//! MPD audio output using one ordered, persistent protocol session.
//!
//! All TCP I/O, commands, and status polling run on one dedicated worker so
//! GTK-facing methods remain non-blocking and MPD effects cannot overtake one
//! another. Protocol reads are bounded by line, response, idle, and absolute
//! operation limits. URI-bearing commands and raw MPD errors are never logged
//! or retained.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6, TcpStream};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::gio::prelude::*;
use gtk::{gio, glib};
use tracing::{debug, error, info};
use url::Url;

use super::cast_http_server::CastHttpServer;
use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerEventGeneration, PlayerState};
use crate::architecture::media::ResolvedHttpRequest;
use crate::http_security::{classify_media_uri, MediaUriSecurity};
use crate::local::resolver::ResolvedLocalMedia;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(8);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);
const WORKER_TICK: Duration = Duration::from_millis(50);
const MAX_GREETING_BYTES: usize = 128;
const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_LINES: usize = 256;
const MAX_URI_BYTES: usize = 32 * 1024;
const MAX_COMMAND_BYTES: usize = MAX_URI_BYTES + 128;
const MAX_RESOLVED_ADDRESSES: usize = 32;
const RESOLUTION_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_PENDING_RESOLVER_REQUESTS: usize = 64;
const MAX_ACTIVE_MPD_RESOLUTIONS: usize = 8;
const MAX_RESOLVER_REQUESTS_PER_TICK: usize = 16;
const MAX_MPD_RESOLVER_HOST_BYTES: usize = 1024;
/// One load-sized command plus a bounded burst of small playback controls.
const MAX_PENDING_WORKER_COMMANDS: usize = 64;

/// Whether this output is allowed to issue MPD's partition-wide playback and
/// option commands. MPD exposes no atomic ownership check for those commands,
/// so playback remains fail-closed until the user explicitly confirms that no
/// other controller or Tributary instance shares the partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpdControlMode {
    Unconfirmed,
    Exclusive,
}

impl From<bool> for MpdControlMode {
    fn from(exclusive_control: bool) -> Self {
        if exclusive_control {
            Self::Exclusive
        } else {
            Self::Unconfirmed
        }
    }
}

pub struct MpdOutput {
    #[allow(dead_code)]
    display_name: String,
    event_tx: async_channel::Sender<PlayerEvent>,
    event_generation: AtomicU64,
    volume: f64,
    control_mode: MpdControlMode,
    intent_epoch: Arc<AtomicU64>,
    cache: Arc<Mutex<MpdCache>>,
    proxy: ProxyServices,
    worker_tx: WorkerCommandSender,
}

#[derive(Clone, Copy)]
struct CommandOwner {
    epoch: u64,
    event_generation: PlayerEventGeneration,
}

struct WorkerCommand {
    owner: CommandOwner,
    kind: CommandKind,
}

// Deliberately not Debug: Load contains a potentially credential-bearing URI.
enum CommandKind {
    Load {
        uri: String,
    },
    ProtectedLoad {
        upstream: Box<Url>,
    },
    ResolvedLoad {
        request: Box<ResolvedHttpRequest>,
    },
    LocalLoad {
        media: ResolvedLocalMedia,
    },
    RejectLoad {
        failure: MpdFailure,
    },
    Play,
    Pause,
    Toggle,
    Stop,
    Seek(u64),
    Shutdown,
    #[cfg(test)]
    PollNow,
    #[cfg(test)]
    Fence(mpsc::Sender<()>),
}

impl CommandKind {
    fn is_load_intent(&self) -> bool {
        matches!(
            self,
            Self::Load { .. }
                | Self::ProtectedLoad { .. }
                | Self::ResolvedLoad { .. }
                | Self::LocalLoad { .. }
                | Self::RejectLoad { .. }
        )
    }

    fn is_transient_control(&self) -> bool {
        matches!(
            self,
            Self::Play | Self::Pause | Self::Toggle | Self::Seek(_)
        )
    }

    fn is_playback_control(&self) -> bool {
        matches!(self, Self::Play | Self::Pause | Self::Toggle)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerEnqueueOutcome {
    Enqueued,
    Superseded,
    Saturated,
    Disconnected,
}

struct PendingWorkerCommands {
    commands: VecDeque<WorkerCommand>,
    newest_epoch: Option<u64>,
    capacity: usize,
    receiver_alive: bool,
}

struct WorkerCommandSender {
    pending: Arc<Mutex<PendingWorkerCommands>>,
    wake_tx: mpsc::SyncSender<()>,
}

struct WorkerCommandReceiver {
    pending: Arc<Mutex<PendingWorkerCommands>>,
    wake_rx: mpsc::Receiver<()>,
}

fn worker_command_channel(capacity: usize) -> (WorkerCommandSender, WorkerCommandReceiver) {
    assert!(capacity > 0, "worker command capacity must be positive");
    let pending = Arc::new(Mutex::new(PendingWorkerCommands {
        commands: VecDeque::with_capacity(capacity),
        newest_epoch: None,
        capacity,
        receiver_alive: true,
    }));
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    (
        WorkerCommandSender {
            pending: Arc::clone(&pending),
            wake_tx,
        },
        WorkerCommandReceiver { pending, wake_rx },
    )
}

impl WorkerCommandSender {
    fn enqueue(&self, command: WorkerCommand) -> WorkerEnqueueOutcome {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !pending.receiver_alive {
            return WorkerEnqueueOutcome::Disconnected;
        }

        match pending.newest_epoch {
            Some(newest_epoch) if command.owner.epoch < newest_epoch => {
                return WorkerEnqueueOutcome::Superseded;
            }
            Some(newest_epoch) if command.owner.epoch > newest_epoch => {
                pending.commands.clear();
                pending.newest_epoch = Some(command.owner.epoch);
            }
            None => pending.newest_epoch = Some(command.owner.epoch),
            Some(_) => {}
        }

        // Below capacity the deque is an exact FIFO. Only a saturated burst
        // may discard intermediate transient intent; lifecycle commands never
        // share an epoch in production and a new epoch atomically purges the
        // obsolete backlog above.
        if pending.commands.len() == pending.capacity {
            if command.kind.is_transient_control() {
                pending.commands.push_back(command);
                compact_saturated_controls(&mut pending.commands);
                while pending.commands.len() > pending.capacity {
                    let Some(oldest_transient) = pending
                        .commands
                        .iter()
                        .position(|queued| queued.kind.is_transient_control())
                    else {
                        break;
                    };
                    let _ = pending.commands.remove(oldest_transient);
                }
            } else {
                compact_saturated_controls(&mut pending.commands);
                if pending.commands.len() == pending.capacity {
                    if let Some(oldest_transient) = pending
                        .commands
                        .iter()
                        .position(|queued| queued.kind.is_transient_control())
                    {
                        let _ = pending.commands.remove(oldest_transient);
                    } else {
                        return WorkerEnqueueOutcome::Saturated;
                    }
                }
                pending.commands.push_back(command);
            }
        } else {
            pending.commands.push_back(command);
        }
        debug_assert!(pending.commands.len() <= pending.capacity);
        // Publish the nonblocking wake while insertion still owns the deque
        // lock. Otherwise the worker can consume a terminal Shutdown, exit,
        // and drop its wake receiver between insertion and try_send(), making
        // an accepted command spuriously report Disconnected. The receiver
        // never holds this lock while waiting for a wake, and try_send never
        // blocks, so GTK-facing enqueue remains a short in-memory operation.
        let outcome = match self.wake_tx.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => WorkerEnqueueOutcome::Enqueued,
            Err(mpsc::TrySendError::Disconnected(())) => {
                pending.receiver_alive = false;
                pending.commands.clear();
                WorkerEnqueueOutcome::Disconnected
            }
        };
        drop(pending);
        outcome
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .commands
            .len()
    }
}

impl WorkerCommandReceiver {
    fn pop_pending(&self) -> Option<WorkerCommand> {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .commands
            .pop_front()
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<WorkerCommand, mpsc::RecvTimeoutError> {
        // A capacity-one wake can remain after the deque has been drained.
        // Keep one absolute timeout so stale wake tokens cannot postpone the
        // worker's periodic status poll.
        let deadline = Instant::now()
            .checked_add(timeout)
            .expect("worker receive deadline representable");
        loop {
            if let Some(command) = self.pop_pending() {
                return Ok(command);
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            match self
                .wake_rx
                .recv_timeout(deadline.saturating_duration_since(now))
            {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(mpsc::RecvTimeoutError::Timeout);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(command) = self.pop_pending() {
                        return Ok(command);
                    }
                    return Err(mpsc::RecvTimeoutError::Disconnected);
                }
            }
        }
    }
}

impl Drop for WorkerCommandReceiver {
    fn drop(&mut self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        pending.receiver_alive = false;
        pending.commands.clear();
    }
}

fn compact_saturated_controls(commands: &mut VecDeque<WorkerCommand>) {
    let mut input = std::mem::take(commands);
    let mut compacted = VecDeque::with_capacity(input.len());
    while let Some(command) = input.pop_front() {
        if matches!(command.kind, CommandKind::Seek(_)) {
            let mut latest = command;
            while input
                .front()
                .is_some_and(|next| matches!(next.kind, CommandKind::Seek(_)))
            {
                latest = input.pop_front().expect("front seek exists");
            }
            compacted.push_back(latest);
        } else if command.kind.is_playback_control() {
            let mut run = VecDeque::from([command]);
            while input
                .front()
                .is_some_and(|next| next.kind.is_playback_control())
            {
                run.push_back(input.pop_front().expect("front playback control exists"));
            }
            fold_playback_controls(run, &mut compacted);
        } else {
            compacted.push_back(command);
        }
    }
    *commands = compacted;
}

fn fold_playback_controls(
    mut run: VecDeque<WorkerCommand>,
    compacted: &mut VecDeque<WorkerCommand>,
) {
    // Map each possible starting state (Stopped, Playing, Paused) to its
    // resulting state. The three commands generate only six transformations,
    // each of which has a canonical sequence of at most two commands.
    let mut transform = [0_usize, 1, 2];
    let mut last_owner = None;
    while let Some(command) = run.pop_front() {
        last_owner = Some(command.owner);
        let operation = match command.kind {
            CommandKind::Play => [1, 1, 1],
            // Pausing an already stopped MPD session leaves it stopped.
            CommandKind::Pause => [0, 2, 2],
            CommandKind::Toggle => [1, 2, 1],
            _ => unreachable!("playback run contains only playback controls"),
        };
        transform = transform.map(|state| operation[state]);
    }

    let owner = last_owner.expect("non-empty playback run has an owner");
    let canonical: &[CommandKind] = match transform {
        [1, 1, 1] => &[CommandKind::Play],
        [0, 2, 2] => &[CommandKind::Pause],
        [1, 2, 1] => &[CommandKind::Toggle],
        // Play then Pause guarantees Paused even from Stopped.
        [2, 2, 2] => &[CommandKind::Play, CommandKind::Pause],
        // Toggle is not involutive for Stopped: two toggles end Paused.
        [2, 1, 2] => &[CommandKind::Toggle, CommandKind::Toggle],
        _ => unreachable!("playback controls produce a known transformation"),
    };
    for kind in canonical {
        compacted.push_back(WorkerCommand {
            owner,
            kind: match kind {
                CommandKind::Play => CommandKind::Play,
                CommandKind::Pause => CommandKind::Pause,
                CommandKind::Toggle => CommandKind::Toggle,
                _ => unreachable!("canonical sequence contains playback controls"),
            },
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MpdAckCode {
    NotList,
    Argument,
    Password,
    Permission,
    Unknown,
    NoExist,
    PlaylistMax,
    System,
    PlaylistLoad,
    UpdateAlready,
    PlayerSync,
    Exist,
}

impl MpdAckCode {
    const fn from_raw(code: u16) -> Option<Self> {
        match code {
            1 => Some(Self::NotList),
            2 => Some(Self::Argument),
            3 => Some(Self::Password),
            4 => Some(Self::Permission),
            5 => Some(Self::Unknown),
            50 => Some(Self::NoExist),
            51 => Some(Self::PlaylistMax),
            52 => Some(Self::System),
            53 => Some(Self::PlaylistLoad),
            54 => Some(Self::UpdateAlready),
            55 => Some(Self::PlayerSync),
            56 => Some(Self::Exist),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedMpdAck {
    Known(MpdAckCode),
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct MpdFailure {
    operation: &'static str,
    connection_usable: bool,
    ack_code: Option<MpdAckCode>,
    user_message: Option<MpdUserMessage>,
}

#[derive(Debug, Clone, Copy)]
enum MpdUserMessage {
    ExclusiveControlRequired,
}

impl MpdFailure {
    const fn new(operation: &'static str) -> Self {
        Self {
            operation,
            connection_usable: false,
            ack_code: None,
            user_message: None,
        }
    }

    const fn synchronized(operation: &'static str) -> Self {
        Self {
            operation,
            connection_usable: true,
            ack_code: None,
            user_message: None,
        }
    }

    const fn ack(operation: &'static str, ack_code: Option<MpdAckCode>) -> Self {
        Self {
            operation,
            connection_usable: true,
            ack_code,
            user_message: None,
        }
    }

    const fn exclusive_control_required() -> Self {
        Self {
            operation: "exclusive control confirmation",
            connection_usable: false,
            ack_code: None,
            user_message: Some(MpdUserMessage::ExclusiveControlRequired),
        }
    }
}

fn opaque_mpd_failure<E>(operation: &'static str, _error: E) -> MpdFailure {
    MpdFailure::new(operation)
}

fn mpd_failure_message(failure: MpdFailure) -> String {
    match failure.user_message {
        Some(MpdUserMessage::ExclusiveControlRequired) => {
            mpd_exclusive_control_required_message(&rust_i18n::locale())
        }
        None => format!("MPD {} failed", failure.operation),
    }
}

fn mpd_exclusive_control_required_message(locale: &str) -> String {
    rust_i18n::t!(
        "errors.playback.mpd_exclusive_control_required",
        locale = locale
    )
    .into_owned()
}

type MpdResult<T> = Result<T, MpdFailure>;

/// One credential-safe route from an opaque ticket to its protected upstream.
///
/// Deliberately not `Debug`: both implementations and the returned URI are
/// security-sensitive transport state. The production implementation owns a
/// dedicated exact-route server, so dropping or revoking this lease cannot
/// affect a ticket installed for another playback generation.
trait MpdMediaTicket: Send + Sync {
    fn uri(&self) -> &str;
    fn revoke(&self);
}

/// Protected upstream state carried through the ordered worker.
///
/// Deliberately not `Debug`: the legacy URL may embed a credential and the
/// typed request owns sensitive headers/private query material.
enum MpdUpstream {
    Legacy(Box<Url>),
    Resolved(Box<ResolvedHttpRequest>),
    Local(ResolvedLocalMedia),
}

impl MpdUpstream {
    #[cfg(test)]
    fn endpoint(&self) -> Option<&Url> {
        match self {
            Self::Legacy(url) => Some(url),
            Self::Resolved(request) => Some(request.endpoint()),
            Self::Local(_) => None,
        }
    }

    fn is_active(&self) -> bool {
        match self {
            Self::Legacy(_) => true,
            Self::Resolved(request) => request.is_active(),
            // Filesystem validation may block on a dead network root, so local
            // authority is revalidated only by the bounded ticket handler
            // immediately before it clones the retained file handle.
            Self::Local(_) => true,
        }
    }
}

trait MpdProxyFactory: Send + Sync + 'static {
    fn start(
        &self,
        runtime: &tokio::runtime::Handle,
        local_addr: SocketAddr,
        upstream: &MpdUpstream,
    ) -> MpdResult<Arc<dyn MpdMediaTicket>>;
}

struct CastMpdProxyFactory;

struct CastMpdMediaTicket {
    server: CastHttpServer,
    uri: String,
}

impl MpdMediaTicket for CastMpdMediaTicket {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn revoke(&self) {
        self.server.revoke_all();
    }
}

impl MpdProxyFactory for CastMpdProxyFactory {
    fn start(
        &self,
        runtime: &tokio::runtime::Handle,
        local_addr: SocketAddr,
        upstream: &MpdUpstream,
    ) -> MpdResult<Arc<dyn MpdMediaTicket>> {
        let server = runtime
            .block_on(CastHttpServer::start_on(local_addr))
            .map_err(|error| opaque_mpd_failure("media proxy startup", error))?;
        if !upstream.is_active() {
            return Err(MpdFailure::new("media source availability"));
        }
        let uri = match upstream {
            MpdUpstream::Legacy(url) => server.register_upstream(url),
            MpdUpstream::Resolved(request) => server
                .register_resolved(request.as_ref().clone())
                .ok_or_else(|| MpdFailure::new("media source availability"))?,
            MpdUpstream::Local(media) => server.register_local(media.clone()),
        };
        // Keep the same command and URI bounds for generated tickets as for
        // direct media. Registration is fail-closed if the route cannot yield
        // a valid MPD argument.
        encode_mpd_arg(&uri)?;
        Ok(Arc::new(CastMpdMediaTicket { server, uri }))
    }
}

struct RegisteredTicket {
    epoch: u64,
    ticket: Arc<dyn MpdMediaTicket>,
}

type TicketRegistry = Arc<Mutex<Option<RegisteredTicket>>>;

#[derive(Clone)]
struct ProxyServices {
    runtime: Arc<Mutex<Option<tokio::runtime::Handle>>>,
    factory: Arc<dyn MpdProxyFactory>,
    current: TicketRegistry,
}

impl ProxyServices {
    fn production() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(None)),
            factory: Arc::new(CastMpdProxyFactory),
            current: Arc::new(Mutex::new(None)),
        }
    }

    fn set_runtime(&self, runtime: tokio::runtime::Handle) {
        let mut slot = self
            .runtime
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *slot = Some(runtime);
    }

    fn start_ticket(
        &self,
        owner: CommandOwner,
        local_addr: SocketAddr,
        upstream: &MpdUpstream,
        intent_epoch: &AtomicU64,
    ) -> MpdResult<SessionTicket> {
        if !upstream.is_active() {
            return Err(MpdFailure::new("media source availability"));
        }
        let runtime = self
            .runtime
            .lock()
            .map_err(|error| opaque_mpd_failure("media proxy runtime", error))?
            .clone()
            .ok_or_else(|| MpdFailure::new("media proxy runtime"))?;
        let ticket = self.factory.start(&runtime, local_addr, upstream)?;
        if !upstream.is_active() {
            ticket.revoke();
            return Err(MpdFailure::new("media source availability"));
        }
        if let Err(failure) = encode_mpd_arg(ticket.uri()) {
            ticket.revoke();
            return Err(failure);
        }

        let mut current = match self.current.lock() {
            Ok(current) => current,
            Err(error) => {
                ticket.revoke();
                return Err(opaque_mpd_failure("media proxy registration", error));
            }
        };
        if !is_current(owner, intent_epoch) {
            drop(current);
            ticket.revoke();
            return Err(MpdFailure::new("media proxy registration"));
        }
        let previous = current.replace(RegisteredTicket {
            epoch: owner.epoch,
            ticket: Arc::clone(&ticket),
        });
        drop(current);
        if let Some(previous) = previous {
            previous.ticket.revoke();
        }

        Ok(SessionTicket {
            epoch: owner.epoch,
            ticket,
            registry: Arc::clone(&self.current),
        })
    }

    /// Revoke a ticket as soon as a replacing load or Stop is requested.
    ///
    /// The epoch comparison and removal happen under one lock. An old worker
    /// cleanup therefore cannot remove a newer registration, while a worker
    /// racing this request either installs first and is revoked here, or sees
    /// the newer intent before it can install.
    fn revoke_before(&self, epoch: u64) {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let ticket = {
            current
                .as_ref()
                .is_some_and(|registered| registered.epoch < epoch)
                .then(|| current.take())
                .flatten()
        };
        drop(current);
        if let Some(ticket) = ticket {
            ticket.ticket.revoke();
        }
    }
}

struct SessionTicket {
    epoch: u64,
    ticket: Arc<dyn MpdMediaTicket>,
    registry: TicketRegistry,
}

impl SessionTicket {
    fn uri(&self) -> &str {
        self.ticket.uri()
    }
}

impl Drop for SessionTicket {
    fn drop(&mut self) {
        self.ticket.revoke();
        let mut current = self
            .registry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if current
            .as_ref()
            .is_some_and(|registered| registered.epoch == self.epoch)
        {
            current.take();
        }
    }
}

#[derive(Clone, Copy)]
struct OperationDeadline {
    end: Instant,
}

impl OperationDeadline {
    fn after(duration: Duration) -> Self {
        Self {
            end: Instant::now()
                .checked_add(duration)
                .unwrap_or_else(Instant::now),
        }
    }

    fn remaining(self, operation: &'static str) -> MpdResult<Duration> {
        let remaining = self.end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(MpdFailure::new(operation))
        } else {
            Ok(remaining)
        }
    }

    fn io_timeout(self, operation: &'static str) -> MpdResult<Duration> {
        Ok(self.remaining(operation)?.min(IO_IDLE_TIMEOUT))
    }

    fn capped(self, duration: Duration) -> Self {
        let cap = Instant::now().checked_add(duration).unwrap_or(self.end);
        Self {
            end: self.end.min(cap),
        }
    }
}

#[derive(Clone, Copy)]
struct WorkerTiming {
    operation: Duration,
    poll: Duration,
    tick: Duration,
}

impl WorkerTiming {
    const fn production() -> Self {
        Self {
            operation: OPERATION_TIMEOUT,
            poll: STATUS_POLL_INTERVAL,
            tick: WORKER_TICK,
        }
    }

    fn deadline(self) -> OperationDeadline {
        OperationDeadline::after(self.operation)
    }
}

#[derive(Clone, Copy)]
struct MpdCache {
    state: PlayerState,
    position_ms: Option<u64>,
}

impl Default for MpdCache {
    fn default() -> Self {
        Self {
            state: PlayerState::Stopped,
            position_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MpdPlaybackState {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, Copy)]
struct MpdStatus {
    state: MpdPlaybackState,
    song_id: Option<u64>,
    position_ms: Option<u64>,
    duration_ms: u64,
    has_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteOutcome {
    Removed,
    AlreadyAbsent,
}

trait MpdConnector: Send + 'static {
    type Connection: MpdTransport + 'static;

    fn connect(
        &mut self,
        owner_epoch: u64,
        intent_epoch: &AtomicU64,
        deadline: OperationDeadline,
    ) -> MpdResult<Self::Connection>;
}

trait MpdTransport {
    fn local_addr(&self) -> MpdResult<SocketAddr>;
    fn repeat_off(&mut self, deadline: OperationDeadline) -> MpdResult<()>;
    fn random_off(&mut self, deadline: OperationDeadline) -> MpdResult<()>;
    fn single_off(&mut self, deadline: OperationDeadline) -> MpdResult<()>;
    fn consume_off(&mut self, deadline: OperationDeadline) -> MpdResult<()>;
    fn add_id(&mut self, uri: &str, deadline: OperationDeadline) -> MpdResult<u64>;
    fn play_id(&mut self, song_id: u64, deadline: OperationDeadline) -> MpdResult<()>;
    fn pause(&mut self, paused: bool, deadline: OperationDeadline) -> MpdResult<()>;
    fn stop(&mut self, deadline: OperationDeadline) -> MpdResult<()>;
    fn seek_id(
        &mut self,
        song_id: u64,
        position_ms: u64,
        deadline: OperationDeadline,
    ) -> MpdResult<()>;
    fn status(&mut self, deadline: OperationDeadline) -> MpdResult<MpdStatus>;
    fn delete_id(&mut self, song_id: u64, deadline: OperationDeadline) -> MpdResult<DeleteOutcome>;
}

struct MpdTcpConnector {
    host: String,
    port: u16,
}

struct MpdConnection {
    reader: BufReader<TcpStream>,
    version: String,
}

impl MpdConnector for MpdTcpConnector {
    type Connection = MpdConnection;

    fn connect(
        &mut self,
        owner_epoch: u64,
        intent_epoch: &AtomicU64,
        deadline: OperationDeadline,
    ) -> MpdResult<Self::Connection> {
        MpdConnection::connect(&self.host, self.port, owner_epoch, intent_epoch, deadline)
    }
}

impl MpdConnection {
    fn connect(
        host: &str,
        port: u16,
        owner_epoch: u64,
        intent_epoch: &AtomicU64,
        deadline: OperationDeadline,
    ) -> MpdResult<Self> {
        let addresses = resolve_mpd_addresses(host, port, owner_epoch, intent_epoch, deadline)?;
        Self::connect_addresses(addresses, deadline)
    }

    fn connect_addresses(
        addresses: Vec<SocketAddr>,
        deadline: OperationDeadline,
    ) -> MpdResult<Self> {
        let address_count = addresses.len();
        for (index, address) in addresses.into_iter().enumerate() {
            let remaining = deadline.remaining("connection")?;
            let addresses_left = u32::try_from(address_count - index).unwrap_or(u32::MAX);
            let fair_slice = (remaining / addresses_left.max(1)).max(Duration::from_millis(1));
            let attempt_deadline = deadline.capped(fair_slice.min(CONNECT_TIMEOUT));
            let timeout = attempt_deadline.remaining("connection")?;
            let Ok(stream) = TcpStream::connect_timeout(&address, timeout) else {
                continue;
            };
            // TCP_NODELAY is a latency optimization, not a correctness
            // requirement; do not discard an otherwise valid connection.
            let _ = stream.set_nodelay(true);
            let mut connection = Self {
                reader: BufReader::new(stream),
                version: String::new(),
            };
            let greeting = connection.read_line(attempt_deadline, MAX_GREETING_BYTES, "greeting");
            let Ok(greeting) = greeting else {
                continue;
            };
            let Ok(version) = parse_greeting(&greeting) else {
                continue;
            };
            connection.version = version;
            return Ok(connection);
        }
        Err(MpdFailure::new("connection"))
    }

    fn read_line(
        &mut self,
        deadline: OperationDeadline,
        max_bytes: usize,
        operation: &'static str,
    ) -> MpdResult<String> {
        let mut bytes = Vec::new();
        loop {
            let timeout = deadline.io_timeout(operation)?;
            self.reader
                .get_mut()
                .set_read_timeout(Some(timeout))
                .map_err(|error| opaque_mpd_failure(operation, error))?;
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    deadline.remaining(operation)?;
                    continue;
                }
                Err(error) => return Err(opaque_mpd_failure(operation, error)),
            };
            deadline.remaining(operation)?;
            if available.is_empty() {
                return Err(MpdFailure::new(operation));
            }
            let take = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            if bytes.len().saturating_add(take) > max_bytes {
                return Err(MpdFailure::new(operation));
            }
            let terminated = available[take - 1] == b'\n';
            bytes.extend_from_slice(&available[..take]);
            self.reader.consume(take);
            if terminated {
                bytes.pop();
                if bytes.last() == Some(&b'\r') {
                    return Err(MpdFailure::new(operation));
                }
                return String::from_utf8(bytes)
                    .map_err(|error| opaque_mpd_failure(operation, error));
            }
        }
    }

    fn write_command(
        &mut self,
        command: &str,
        deadline: OperationDeadline,
        operation: &'static str,
    ) -> MpdResult<()> {
        if command.len() > MAX_COMMAND_BYTES || command.contains(['\r', '\n']) {
            return Err(MpdFailure::new(operation));
        }
        for mut bytes in [command.as_bytes(), b"\n".as_slice()] {
            while !bytes.is_empty() {
                let timeout = deadline.io_timeout(operation)?;
                self.reader
                    .get_mut()
                    .set_write_timeout(Some(timeout))
                    .map_err(|error| opaque_mpd_failure(operation, error))?;
                let written = match self.reader.get_mut().write(bytes) {
                    Ok(written) => written,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                        deadline.remaining(operation)?;
                        continue;
                    }
                    Err(error) => return Err(opaque_mpd_failure(operation, error)),
                };
                if written == 0 {
                    return Err(MpdFailure::new(operation));
                }
                bytes = &bytes[written..];
                deadline.remaining(operation)?;
            }
        }
        let timeout = deadline.io_timeout(operation)?;
        self.reader
            .get_mut()
            .set_write_timeout(Some(timeout))
            .map_err(|error| opaque_mpd_failure(operation, error))?;
        loop {
            match self.reader.get_mut().flush() {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    deadline.remaining(operation)?;
                }
                Err(error) => return Err(opaque_mpd_failure(operation, error)),
            }
        }
        deadline.remaining(operation).map(|_| ())
    }

    /// Parse only MPD's fixed numeric ACK category.
    ///
    /// The command-list offset, command name, and human-readable message are
    /// server-controlled and may contain credentials echoed from a rejected
    /// command. Validate their framing, then discard them immediately.
    fn parse_ack_code(line: &str, expected_command: &str) -> Option<ParsedMpdAck> {
        let framed = line.strip_prefix("ACK [")?;
        let (code, framed) = framed.split_once('@')?;
        let (command_index, framed) = framed.split_once("] {")?;
        let (command, _message) = framed.split_once("} ")?;
        if code.is_empty()
            || command_index != "0"
            || command.is_empty()
            || command != expected_command
            || !code.bytes().all(|byte| byte.is_ascii_digit())
            || command
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'{' | b'}'))
        {
            return None;
        }
        Some(
            code.parse::<u16>()
                .ok()
                .and_then(MpdAckCode::from_raw)
                .map_or(ParsedMpdAck::Unknown, ParsedMpdAck::Known),
        )
    }

    fn response<T>(
        &mut self,
        command: &str,
        deadline: OperationDeadline,
        operation: &'static str,
        mut output: T,
        mut parse: impl FnMut(&mut T, &str) -> MpdResult<()>,
    ) -> MpdResult<T> {
        debug!(operation, "MPD command send");
        self.write_command(command, deadline, operation)?;
        let mut observed = 0_usize;
        let mut lines = 0_usize;
        loop {
            let line = self.read_line(deadline, MAX_LINE_BYTES, operation)?;
            observed = observed.saturating_add(line.len().saturating_add(1));
            lines = lines.saturating_add(1);
            if observed > MAX_RESPONSE_BYTES || lines > MAX_RESPONSE_LINES {
                return Err(MpdFailure::new(operation));
            }
            if line == "OK" {
                return Ok(output);
            }
            if line == "ACK" || line.starts_with("ACK ") {
                // Only a structurally valid ACK for this exact single command
                // proves that the response was consumed and the session is
                // still synchronized. Keep only its fixed numeric category;
                // never retain the command echo or human-readable text.
                let expected_command = command.split_ascii_whitespace().next().unwrap_or_default();
                let ack_code = match Self::parse_ack_code(&line, expected_command) {
                    Some(ParsedMpdAck::Known(code)) => Some(code),
                    Some(ParsedMpdAck::Unknown) => None,
                    None => return Err(MpdFailure::new(operation)),
                };
                return Err(MpdFailure::ack(operation, ack_code));
            }
            parse(&mut output, &line)?;
        }
    }

    fn response_none(
        &mut self,
        command: &str,
        deadline: OperationDeadline,
        operation: &'static str,
    ) -> MpdResult<()> {
        self.response(command, deadline, operation, (), |(), _line| {
            Err(MpdFailure::new(operation))
        })
    }
}

impl MpdTransport for MpdConnection {
    fn local_addr(&self) -> MpdResult<SocketAddr> {
        self.reader
            .get_ref()
            .local_addr()
            .map_err(|error| opaque_mpd_failure("connection address", error))
    }

    fn repeat_off(&mut self, deadline: OperationDeadline) -> MpdResult<()> {
        self.response_none("repeat 0", deadline, "repeat update")
    }

    fn random_off(&mut self, deadline: OperationDeadline) -> MpdResult<()> {
        self.response_none("random 0", deadline, "random update")
    }

    fn single_off(&mut self, deadline: OperationDeadline) -> MpdResult<()> {
        self.response_none("single 0", deadline, "single update")
    }

    fn consume_off(&mut self, deadline: OperationDeadline) -> MpdResult<()> {
        self.response_none("consume 0", deadline, "consume update")
    }

    fn add_id(&mut self, uri: &str, deadline: OperationDeadline) -> MpdResult<u64> {
        let uri = encode_mpd_arg(uri)?;
        let command = format!("addid \"{uri}\"");
        let id = self.response(&command, deadline, "media add", None, |id, line| {
            if let Some(value) = line.strip_prefix("Id: ") {
                if id.is_some() {
                    return Err(MpdFailure::new("media add"));
                }
                *id = Some(
                    value
                        .parse::<u64>()
                        .map_err(|error| opaque_mpd_failure("media add", error))?,
                );
                Ok(())
            } else {
                Err(MpdFailure::new("media add"))
            }
        })?;
        id.ok_or_else(|| MpdFailure::synchronized("media add"))
    }

    fn play_id(&mut self, song_id: u64, deadline: OperationDeadline) -> MpdResult<()> {
        self.response_none(&format!("playid {song_id}"), deadline, "play command")
    }

    fn pause(&mut self, paused: bool, deadline: OperationDeadline) -> MpdResult<()> {
        let value = u8::from(paused);
        self.response_none(&format!("pause {value}"), deadline, "pause command")
    }

    fn stop(&mut self, deadline: OperationDeadline) -> MpdResult<()> {
        self.response_none("stop", deadline, "stop command")
    }

    fn seek_id(
        &mut self,
        song_id: u64,
        position_ms: u64,
        deadline: OperationDeadline,
    ) -> MpdResult<()> {
        let seconds = position_ms / 1000;
        let milliseconds = position_ms % 1000;
        self.response_none(
            &format!("seekid {song_id} {seconds}.{milliseconds:03}"),
            deadline,
            "seek command",
        )
    }

    fn status(&mut self, deadline: OperationDeadline) -> MpdResult<MpdStatus> {
        let raw = self.response(
            "status",
            deadline,
            "status poll",
            RawStatus::default(),
            RawStatus::parse_line,
        )?;
        raw.finish()
    }

    fn delete_id(&mut self, song_id: u64, deadline: OperationDeadline) -> MpdResult<DeleteOutcome> {
        match self.response_none(&format!("deleteid {song_id}"), deadline, "queue ownership") {
            Ok(()) => Ok(DeleteOutcome::Removed),
            Err(MpdFailure {
                ack_code: Some(MpdAckCode::NoExist),
                ..
            }) => Ok(DeleteOutcome::AlreadyAbsent),
            Err(failure) => Err(failure),
        }
    }
}

#[derive(Default)]
struct RawStatus {
    state: Option<MpdPlaybackState>,
    song_id: Option<u64>,
    elapsed_ms: Option<u64>,
    duration_ms: Option<u64>,
    fallback_elapsed_ms: Option<u64>,
    fallback_duration_ms: Option<u64>,
    has_error: bool,
}

impl RawStatus {
    fn parse_line(&mut self, line: &str) -> MpdResult<()> {
        let Some((key, value)) = line.split_once(": ") else {
            return Err(MpdFailure::new("status poll"));
        };
        match key {
            "state" => {
                if self.state.is_some() {
                    return Err(MpdFailure::new("status poll"));
                }
                self.state = Some(match value {
                    "play" => MpdPlaybackState::Playing,
                    "pause" => MpdPlaybackState::Paused,
                    "stop" => MpdPlaybackState::Stopped,
                    _ => return Err(MpdFailure::new("status poll")),
                });
            }
            "songid" => {
                if self.song_id.is_some() {
                    return Err(MpdFailure::new("status poll"));
                }
                self.song_id = Some(parse_u64(value, "status poll")?);
            }
            "elapsed" => {
                if self.elapsed_ms.is_some() {
                    return Err(MpdFailure::new("status poll"));
                }
                self.elapsed_ms = Some(parse_seconds(value, "status poll")?);
            }
            "duration" => {
                if self.duration_ms.is_some() {
                    return Err(MpdFailure::new("status poll"));
                }
                self.duration_ms = Some(parse_seconds(value, "status poll")?);
            }
            "time" => {
                if self.fallback_elapsed_ms.is_some() || self.fallback_duration_ms.is_some() {
                    return Err(MpdFailure::new("status poll"));
                }
                let Some((elapsed, duration)) = value.split_once(':') else {
                    return Err(MpdFailure::new("status poll"));
                };
                self.fallback_elapsed_ms = Some(parse_seconds(elapsed, "status poll")?);
                self.fallback_duration_ms = Some(parse_seconds(duration, "status poll")?);
            }
            "error" => self.has_error = true,
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> MpdResult<MpdStatus> {
        let state = self.state.ok_or_else(|| MpdFailure::new("status poll"))?;
        if matches!(state, MpdPlaybackState::Playing | MpdPlaybackState::Paused)
            && self.song_id.is_none()
        {
            return Err(MpdFailure::new("status poll"));
        }
        Ok(MpdStatus {
            state,
            song_id: self.song_id,
            position_ms: self.elapsed_ms.or(self.fallback_elapsed_ms),
            duration_ms: self.duration_ms.or(self.fallback_duration_ms).unwrap_or(0),
            has_error: self.has_error,
        })
    }
}

fn parse_u64(value: &str, operation: &'static str) -> MpdResult<u64> {
    value
        .parse()
        .map_err(|error| opaque_mpd_failure(operation, error))
}

fn parse_seconds(value: &str, operation: &'static str) -> MpdResult<u64> {
    let (whole, fraction) = value
        .split_once('.')
        .map_or((value, None), |(whole, fraction)| (whole, Some(fraction)));
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(MpdFailure::new(operation));
    }
    let whole = parse_u64(whole, operation)?;
    let mut milliseconds = whole
        .checked_mul(1000)
        .ok_or_else(|| MpdFailure::new(operation))?;
    if let Some(fraction) = fraction {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(MpdFailure::new(operation));
        }
        let mut fraction_ms = 0_u64;
        let mut digits = 0_u32;
        for byte in fraction.bytes().take(3) {
            fraction_ms = fraction_ms * 10 + u64::from(byte - b'0');
            digits += 1;
        }
        while digits < 3 {
            fraction_ms *= 10;
            digits += 1;
        }
        milliseconds = milliseconds
            .checked_add(fraction_ms)
            .ok_or_else(|| MpdFailure::new(operation))?;
    }
    Ok(milliseconds)
}

static MPD_RESOLVER_SERVICE: OnceLock<MpdResult<MpdResolverService>> = OnceLock::new();

struct MpdResolverService {
    request_tx: mpsc::SyncSender<MpdResolverServiceRequest>,
    context: glib::MainContext,
}

#[cfg(test)]
type MpdResolverServiceJob = Box<dyn FnOnce(&glib::MainContext, usize) + Send + 'static>;

enum MpdResolverServiceRequest {
    Resolve(MpdResolverRequest),
    #[cfg(test)]
    Run(MpdResolverServiceJob),
    #[cfg(test)]
    Shutdown(mpsc::Sender<()>),
}

struct MpdResolverRequest {
    host: String,
    port: u16,
    cancellable: gio::Cancellable,
    result_tx: mpsc::Sender<MpdResult<Vec<SocketAddr>>>,
}

impl MpdResolverService {
    fn shared() -> MpdResult<&'static Self> {
        match MPD_RESOLVER_SERVICE.get_or_init(Self::start) {
            Ok(service) => Ok(service),
            Err(failure) => Err(*failure),
        }
    }

    fn start() -> MpdResult<Self> {
        let context = glib::MainContext::new();
        let worker_context = context.clone();
        let (request_tx, request_rx) = mpsc::sync_channel(MAX_PENDING_RESOLVER_REQUESTS);
        std::thread::Builder::new()
            .name("mpd-resolver".to_string())
            .spawn(move || run_mpd_resolver_service(worker_context, request_rx))
            .map_err(|error| opaque_mpd_failure("address resolution", error))?;
        Ok(Self {
            request_tx,
            context,
        })
    }

    fn submit(&self, request: MpdResolverServiceRequest) -> MpdResult<()> {
        self.request_tx
            .try_send(request)
            .map_err(|error| opaque_mpd_failure("address resolution", error))?;
        self.context.wakeup();
        Ok(())
    }

    #[cfg(test)]
    fn shutdown_for_test(&self) {
        let (done_tx, done_rx) = mpsc::channel();
        self.submit(MpdResolverServiceRequest::Shutdown(done_tx))
            .expect("submit resolver shutdown");
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("resolver service stopped");
    }
}

fn run_mpd_resolver_service(
    context: glib::MainContext,
    request_rx: mpsc::Receiver<MpdResolverServiceRequest>,
) {
    let running = context.with_thread_default(|| {
        let active_count = Rc::new(Cell::new(0));
        let mut ingress_alive = true;
        #[cfg(test)]
        let mut shutdown_done: Option<mpsc::Sender<()>> = None;
        'service: loop {
            let mut handled = 0;
            while ingress_alive && handled < MAX_RESOLVER_REQUESTS_PER_TICK {
                let request = match request_rx.try_recv() {
                    Ok(request) => request,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        ingress_alive = false;
                        break;
                    }
                };
                handled += 1;
                match request {
                    MpdResolverServiceRequest::Resolve(request) => {
                        start_mpd_resolution(request, Rc::clone(&active_count));
                    }
                    #[cfg(test)]
                    MpdResolverServiceRequest::Run(job) => {
                        job(&context, active_count.get());
                    }
                    #[cfg(test)]
                    MpdResolverServiceRequest::Shutdown(done_tx) => {
                        shutdown_done = Some(done_tx);
                        ingress_alive = false;
                    }
                }
            }
            if !ingress_alive && active_count.get() == 0 {
                #[cfg(test)]
                if let Some(done_tx) = shutdown_done.take() {
                    let _ = done_tx.send(());
                }
                break 'service;
            }
            // Request submission calls wakeup(), so this remains responsive
            // while continuously dispatching async GIO completions on the
            // one thread that created their gtk-rs callback guards.
            let _ = context.iteration(handled == 0);
        }
    });
    if let Err(context_error) = running {
        error!(error = %context_error, "MPD resolver context stopped");
    }
}

struct MpdResolverOperation {
    enumerator: gio::SocketAddressEnumerator,
    cancellable: gio::Cancellable,
    result_tx: Option<mpsc::Sender<MpdResult<Vec<SocketAddr>>>>,
    addresses: Vec<SocketAddr>,
    active_lease: MpdResolverActiveLease,
}

struct MpdResolverActiveLease {
    active_count: Rc<Cell<usize>>,
    released: bool,
}

impl MpdResolverActiveLease {
    fn acquire(active_count: Rc<Cell<usize>>) -> Option<Self> {
        let active = active_count.get();
        if active >= MAX_ACTIVE_MPD_RESOLUTIONS {
            return None;
        }
        active_count.set(active + 1);
        Some(Self {
            active_count,
            released: false,
        })
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        let active = self.active_count.get();
        debug_assert!(active > 0, "active MPD resolution count underflow");
        self.active_count.set(active.saturating_sub(1));
        self.released = true;
    }
}

impl Drop for MpdResolverActiveLease {
    fn drop(&mut self) {
        self.release();
    }
}

fn start_mpd_resolution(request: MpdResolverRequest, active_count: Rc<Cell<usize>>) {
    if request.cancellable.is_cancelled() || !valid_mpd_resolver_host(&request.host) {
        let _ = request
            .result_tx
            .send(Err(MpdFailure::new("address resolution")));
        return;
    }
    let Some(active_lease) = MpdResolverActiveLease::acquire(active_count) else {
        // Both queued requests and live GIO operations are hard-bounded.
        // Overload therefore fails closed instead of creating an unbounded
        // set of callbacks behind the resolver context.
        let _ = request
            .result_tx
            .send(Err(MpdFailure::new("address resolution")));
        return;
    };
    let connectable = gio::NetworkAddress::new(&request.host, request.port);
    let operation = Rc::new(RefCell::new(MpdResolverOperation {
        enumerator: connectable.enumerate(),
        cancellable: request.cancellable,
        result_tx: Some(request.result_tx),
        addresses: Vec::new(),
        active_lease,
    }));
    request_next_mpd_address(operation);
}

fn request_next_mpd_address(operation: Rc<RefCell<MpdResolverOperation>>) {
    let (enumerator, cancellable) = {
        let state = operation.borrow();
        if state.cancellable.is_cancelled() {
            drop(state);
            finish_mpd_resolution(operation, Err(MpdFailure::new("address resolution")));
            return;
        }
        (state.enumerator.clone(), state.cancellable.clone())
    };
    enumerator.next_async(Some(&cancellable), move |result| {
        complete_mpd_address(operation, result);
    });
}

fn complete_mpd_address(
    operation: Rc<RefCell<MpdResolverOperation>>,
    result: Result<Option<gio::SocketAddress>, glib::Error>,
) {
    let cancelled = operation.borrow().cancellable.is_cancelled();
    if cancelled {
        finish_mpd_resolution(operation, Err(MpdFailure::new("address resolution")));
        return;
    }

    let address = match result {
        Ok(Some(address)) => match gio_address_to_socket_addr(address) {
            Ok(address) => address,
            Err(failure) => {
                finish_mpd_resolution(operation, Err(failure));
                return;
            }
        },
        Ok(None) => {
            let result = {
                let mut operation = operation.borrow_mut();
                if operation.addresses.is_empty() {
                    Err(MpdFailure::new("address resolution"))
                } else {
                    Ok(std::mem::take(&mut operation.addresses))
                }
            };
            finish_mpd_resolution(operation, result);
            return;
        }
        Err(error) => {
            finish_mpd_resolution(
                operation,
                Err(opaque_mpd_failure("address resolution", error)),
            );
            return;
        }
    };

    let full = retain_mpd_address(&mut operation.borrow_mut().addresses, address);
    if full {
        let addresses = std::mem::take(&mut operation.borrow_mut().addresses);
        finish_mpd_resolution(operation, Ok(addresses));
    } else {
        request_next_mpd_address(operation);
    }
}

fn finish_mpd_resolution(
    operation: Rc<RefCell<MpdResolverOperation>>,
    result: MpdResult<Vec<SocketAddr>>,
) {
    let result_tx = {
        let mut operation = operation.borrow_mut();
        operation.active_lease.release();
        operation.result_tx.take()
    };
    if let Some(result_tx) = result_tx {
        let _ = result_tx.send(result);
    }
}

fn retain_mpd_address(addresses: &mut Vec<SocketAddr>, address: SocketAddr) -> bool {
    if !addresses.contains(&address) {
        addresses.push(address);
    }
    addresses.len() == MAX_RESOLVED_ADDRESSES
}

#[derive(Clone, Copy)]
struct MpdResolutionScope<'a> {
    owner_epoch: u64,
    intent_epoch: &'a AtomicU64,
    deadline: OperationDeadline,
}

impl MpdResolutionScope<'_> {
    fn remaining(self) -> MpdResult<Duration> {
        if self.intent_epoch.load(Ordering::SeqCst) != self.owner_epoch {
            return Err(MpdFailure::new("address resolution"));
        }
        self.deadline.remaining("address resolution")
    }
}

fn resolve_mpd_addresses(
    host: &str,
    port: u16,
    owner_epoch: u64,
    intent_epoch: &AtomicU64,
    deadline: OperationDeadline,
) -> MpdResult<Vec<SocketAddr>> {
    let host = strip_optional_ipv6_brackets(host);
    let scope = MpdResolutionScope {
        owner_epoch,
        intent_epoch,
        deadline,
    };
    scope.remaining()?;
    if !valid_mpd_resolver_host(host) {
        return Err(MpdFailure::new("address resolution"));
    }

    // Numeric hosts need neither the platform resolver nor a GLib main
    // context. Scoped IPv6 literals that `IpAddr` does not accept continue
    // through GIO, which preserves the native scope id in its result.
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(address, port)]);
    }

    let service = MpdResolverService::shared()?;
    let cancellable = gio::Cancellable::new();
    let (result_tx, result_rx) = mpsc::channel();
    service.submit(MpdResolverServiceRequest::Resolve(MpdResolverRequest {
        host: host.to_string(),
        port,
        cancellable: cancellable.clone(),
        result_tx,
    }))?;
    wait_for_mpd_resolution(result_rx, &cancellable, scope)
}

fn valid_mpd_resolver_host(host: &str) -> bool {
    !host.is_empty() && host.len() <= MAX_MPD_RESOLVER_HOST_BYTES && !host.contains('\0')
}

fn wait_for_mpd_resolution(
    result_rx: mpsc::Receiver<MpdResult<Vec<SocketAddr>>>,
    cancellable: &gio::Cancellable,
    scope: MpdResolutionScope<'_>,
) -> MpdResult<Vec<SocketAddr>> {
    loop {
        let remaining = match scope.remaining() {
            Ok(remaining) => remaining,
            Err(failure) => {
                cancellable.cancel();
                return Err(failure);
            }
        };
        match result_rx.recv_timeout(remaining.min(RESOLUTION_CANCEL_POLL_INTERVAL)) {
            Ok(result) => match scope.remaining() {
                Ok(_) => return result,
                Err(failure) => {
                    cancellable.cancel();
                    return Err(failure);
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cancellable.cancel();
                return Err(MpdFailure::new("address resolution"));
            }
        }
    }
}

fn gio_address_to_socket_addr(address: gio::SocketAddress) -> MpdResult<SocketAddr> {
    let address = address
        .downcast::<gio::InetSocketAddress>()
        .map_err(|_| MpdFailure::new("address resolution"))?;
    let inet_address = IpAddr::from(address.address());
    let port = address.port();
    Ok(match inet_address {
        IpAddr::V4(ip) => SocketAddr::V4(SocketAddrV4::new(ip, port)),
        IpAddr::V6(ip) => SocketAddr::V6(SocketAddrV6::new(
            ip,
            port,
            address.flowinfo(),
            address.scope_id(),
        )),
    })
}

fn strip_optional_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

fn parse_greeting(greeting: &str) -> MpdResult<String> {
    let version = greeting
        .strip_prefix("OK MPD ")
        .filter(|version| {
            version.split('.').all(|component| {
                !component.is_empty() && component.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .ok_or_else(|| MpdFailure::new("greeting"))?;
    Ok(format!("OK MPD {version}"))
}

fn encode_mpd_arg(value: &str) -> MpdResult<String> {
    if value.len() > MAX_URI_BYTES || value.contains(['\r', '\n']) {
        return Err(MpdFailure::new("media URI validation"));
    }
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '"') {
            encoded.push('\\');
        }
        encoded.push(character);
    }
    Ok(encoded)
}

struct WorkerSession<T> {
    connection: T,
    song_id: Option<u64>,
    ticket: Option<SessionTicket>,
    owner: CommandOwner,
    state: PlayerState,
    observed_active: bool,
    last_position_ms: Option<u64>,
    duration_ms: u64,
    last_poll: Instant,
}

#[derive(Debug, Clone, Copy)]
enum CleanupOutcome {
    Completed,
    Stale,
    Failed(MpdFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupKind {
    /// Remove only the queue entry whose stable id belongs to Tributary.
    Targeted,
    /// Stop playback only after a fresh status proves Tributary is current,
    /// then remove the same stable queue id.
    StopOwned,
}

/// Media entering the ordered worker. Deliberately not `Debug`: the protected
/// variant contains the upstream credential that must never reach MPD.
enum MpdMedia {
    Direct(String),
    Protected(MpdUpstream),
}

fn spawn_mpd_worker<C>(
    connector: C,
    control_mode: MpdControlMode,
    intent_epoch: Arc<AtomicU64>,
    cache: Arc<Mutex<MpdCache>>,
    event_tx: async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
    proxy: ProxyServices,
) -> WorkerCommandSender
where
    C: MpdConnector,
{
    let (worker_tx, worker_rx) = worker_command_channel(MAX_PENDING_WORKER_COMMANDS);
    let spawn = std::thread::Builder::new()
        .name("mpd-worker".to_string())
        .spawn(move || {
            run_mpd_worker(
                connector,
                worker_rx,
                control_mode,
                intent_epoch,
                cache,
                event_tx,
                timing,
                proxy,
            );
        });
    if let Err(spawn_error) = spawn {
        error!(error = %spawn_error, "Failed to spawn MPD worker");
    }
    worker_tx
}

#[allow(clippy::too_many_arguments)]
fn run_mpd_worker<C>(
    mut connector: C,
    worker_rx: WorkerCommandReceiver,
    control_mode: MpdControlMode,
    intent_epoch: Arc<AtomicU64>,
    cache: Arc<Mutex<MpdCache>>,
    event_tx: async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
    proxy: ProxyServices,
) where
    C: MpdConnector,
{
    let mut active: Option<WorkerSession<C::Connection>> = None;
    loop {
        let wait = if active.is_some() {
            timing.tick
        } else {
            Duration::from_hours(1)
        };
        match worker_rx.recv_timeout(wait) {
            Ok(command) => {
                // Apply the partition-ownership contract to every load intent,
                // including media rejected before dispatch. This precedes
                // cleanup as well as every connection, MPD, and proxy action.
                if control_mode != MpdControlMode::Exclusive && command.kind.is_load_intent() {
                    fail_current(
                        command.owner,
                        MpdFailure::exclusive_control_required(),
                        &intent_epoch,
                        &cache,
                        &event_tx,
                    );
                    continue;
                }
                let poll_after = match command.kind {
                    CommandKind::Load { uri } => {
                        handle_load(
                            &mut connector,
                            &mut active,
                            command.owner,
                            MpdMedia::Direct(uri),
                            &proxy,
                            &intent_epoch,
                            &cache,
                            &event_tx,
                            timing,
                        );
                        true
                    }
                    CommandKind::ProtectedLoad { upstream } => {
                        handle_load(
                            &mut connector,
                            &mut active,
                            command.owner,
                            MpdMedia::Protected(MpdUpstream::Legacy(upstream)),
                            &proxy,
                            &intent_epoch,
                            &cache,
                            &event_tx,
                            timing,
                        );
                        true
                    }
                    CommandKind::ResolvedLoad { request } => {
                        handle_load(
                            &mut connector,
                            &mut active,
                            command.owner,
                            MpdMedia::Protected(MpdUpstream::Resolved(request)),
                            &proxy,
                            &intent_epoch,
                            &cache,
                            &event_tx,
                            timing,
                        );
                        true
                    }
                    CommandKind::LocalLoad { media } => {
                        handle_load(
                            &mut connector,
                            &mut active,
                            command.owner,
                            MpdMedia::Protected(MpdUpstream::Local(media)),
                            &proxy,
                            &intent_epoch,
                            &cache,
                            &event_tx,
                            timing,
                        );
                        true
                    }
                    CommandKind::RejectLoad { failure } => {
                        let cleanup = cleanup_session(
                            &mut active,
                            command.owner,
                            CleanupKind::Targeted,
                            &intent_epoch,
                            timing,
                        );
                        if !matches!(cleanup, CleanupOutcome::Stale) {
                            fail_current(command.owner, failure, &intent_epoch, &cache, &event_tx);
                        }
                        true
                    }
                    CommandKind::Stop => {
                        match cleanup_session(
                            &mut active,
                            command.owner,
                            CleanupKind::StopOwned,
                            &intent_epoch,
                            timing,
                        ) {
                            CleanupOutcome::Completed => {
                                let _ = publish_state(
                                    command.owner,
                                    PlayerState::Stopped,
                                    None,
                                    &intent_epoch,
                                    &cache,
                                    &event_tx,
                                );
                            }
                            CleanupOutcome::Failed(failure) => fail_current(
                                command.owner,
                                failure,
                                &intent_epoch,
                                &cache,
                                &event_tx,
                            ),
                            CleanupOutcome::Stale => {}
                        }
                        true
                    }
                    CommandKind::Shutdown => {
                        cleanup_unconditionally(&mut active, timing);
                        break;
                    }
                    #[cfg(test)]
                    CommandKind::PollNow => {
                        poll_active(&mut active, true, &intent_epoch, &cache, &event_tx, timing);
                        false
                    }
                    #[cfg(test)]
                    CommandKind::Fence(done) => {
                        let _ = done.send(());
                        false
                    }
                    kind => {
                        handle_control(
                            &mut active,
                            command.owner,
                            kind,
                            &intent_epoch,
                            &cache,
                            &event_tx,
                            timing,
                        );
                        true
                    }
                };
                if poll_after {
                    poll_active(&mut active, false, &intent_epoch, &cache, &event_tx, timing);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                poll_active(&mut active, false, &intent_epoch, &cache, &event_tx, timing);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cleanup_unconditionally(&mut active, timing);
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_load<C>(
    connector: &mut C,
    active: &mut Option<WorkerSession<C::Connection>>,
    owner: CommandOwner,
    media: MpdMedia,
    proxy: &ProxyServices,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) where
    C: MpdConnector,
{
    match cleanup_session(active, owner, CleanupKind::Targeted, intent_epoch, timing) {
        CleanupOutcome::Completed => {}
        CleanupOutcome::Failed(failure) => {
            error!(operation = failure.operation, "Previous MPD cleanup failed");
        }
        CleanupOutcome::Stale => return,
    }
    if !publish_state(
        owner,
        PlayerState::Buffering,
        None,
        intent_epoch,
        cache,
        event_tx,
    ) {
        return;
    }

    let deadline = timing.deadline();
    if !is_current(owner, intent_epoch) {
        return;
    }
    let connection = connector.connect(owner.epoch, intent_epoch, deadline);
    let connection = match connection {
        Ok(connection) => connection,
        Err(failure) => {
            if is_current(owner, intent_epoch) {
                fail_current(owner, failure, intent_epoch, cache, event_tx);
            }
            return;
        }
    };
    *active = Some(WorkerSession {
        connection,
        song_id: None,
        ticket: None,
        owner,
        state: PlayerState::Buffering,
        observed_active: false,
        last_position_ms: None,
        duration_ms: 0,
        last_poll: Instant::now(),
    });
    if !is_current(owner, intent_epoch) {
        return;
    }

    let repeat = active
        .as_mut()
        .expect("connected MPD session recorded")
        .connection
        .repeat_off(deadline);
    if !finish_load_stage(repeat, active, owner, intent_epoch, cache, event_tx, timing) {
        return;
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    // The owned item is appended after the preserved foreign queue. Disable
    // random order so reaching its end cannot select an earlier foreign item.
    let random = active
        .as_mut()
        .expect("connected MPD session recorded")
        .connection
        .random_off(deadline);
    if !finish_load_stage(random, active, owner, intent_epoch, cache, event_tx, timing) {
        return;
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    // `single 1`/`oneshot` can pause at the queue boundary instead of
    // reporting Stopped, which would suppress Tributary's completion event.
    let single = active
        .as_mut()
        .expect("connected MPD session recorded")
        .connection
        .single_off(deadline);
    if !finish_load_stage(single, active, owner, intent_epoch, cache, event_tx, timing) {
        return;
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    // Keep the stable queue id available after natural completion so terminal
    // status can be attributed to this load rather than another MPD client.
    let consume = active
        .as_mut()
        .expect("connected MPD session recorded")
        .connection
        .consume_off(deadline);
    if !finish_load_stage(
        consume,
        active,
        owner,
        intent_epoch,
        cache,
        event_tx,
        timing,
    ) {
        return;
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    let uri = match media {
        MpdMedia::Direct(uri) => uri,
        MpdMedia::Protected(upstream) => {
            let local_addr = active
                .as_ref()
                .expect("connected MPD session recorded")
                .connection
                .local_addr();
            let local_addr = match local_addr {
                Ok(local_addr) => local_addr,
                Err(failure) => {
                    cleanup_then_fail(
                        active,
                        owner,
                        failure,
                        intent_epoch,
                        cache,
                        event_tx,
                        timing,
                    );
                    return;
                }
            };
            let ticket = proxy.start_ticket(owner, local_addr, &upstream, intent_epoch);
            let ticket = match ticket {
                Ok(ticket) => ticket,
                Err(failure) => {
                    cleanup_then_fail(
                        active,
                        owner,
                        failure,
                        intent_epoch,
                        cache,
                        event_tx,
                        timing,
                    );
                    return;
                }
            };
            let uri = ticket.uri().to_string();
            active
                .as_mut()
                .expect("connected MPD session recorded")
                .ticket = Some(ticket);
            uri
        }
    };
    if !is_current(owner, intent_epoch) {
        active.take();
        return;
    }
    let added = active
        .as_mut()
        .expect("connected MPD session recorded")
        .connection
        .add_id(&uri, deadline);
    if let Ok(song_id) = added {
        active
            .as_mut()
            .expect("connected MPD session recorded")
            .song_id = Some(song_id);
    }
    if retire_poisoned_if_stale(&added, active, owner, intent_epoch) {
        return;
    }
    let song_id = match added {
        Ok(song_id) => song_id,
        Err(failure) => {
            cleanup_then_fail(
                active,
                owner,
                failure,
                intent_epoch,
                cache,
                event_tx,
                timing,
            );
            return;
        }
    };
    let played = active
        .as_mut()
        .expect("connected MPD session recorded")
        .connection
        .play_id(song_id, deadline);
    if !finish_load_stage(played, active, owner, intent_epoch, cache, event_tx, timing) {
        return;
    }
    if let Some(session) = active.as_mut() {
        // A successful playid ACK is enough to classify a very short track
        // that reaches Stopped before the first status response.
        session.observed_active = true;
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    let status = active
        .as_mut()
        .expect("connected MPD session recorded")
        .connection
        .status(deadline);
    if retire_status_if_stale(&status, active, owner, intent_epoch) {
        return;
    }
    match status {
        Ok(status) => {
            apply_authoritative_status(
                active,
                owner,
                status,
                intent_epoch,
                cache,
                event_tx,
                timing,
            );
        }
        Err(failure) => cleanup_then_fail(
            active,
            owner,
            failure,
            intent_epoch,
            cache,
            event_tx,
            timing,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_load_stage<T, C>(
    result: MpdResult<T>,
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) -> bool
where
    C: MpdTransport,
{
    if retire_poisoned_if_stale(&result, active, owner, intent_epoch) {
        return false;
    }
    match result {
        Ok(_) => true,
        Err(failure) => {
            cleanup_then_fail(
                active,
                owner,
                failure,
                intent_epoch,
                cache,
                event_tx,
                timing,
            );
            false
        }
    }
}

fn retire_poisoned_if_stale<T, C>(
    result: &MpdResult<T>,
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    intent_epoch: &AtomicU64,
) -> bool {
    if is_current(owner, intent_epoch) {
        return false;
    }
    if matches!(result, Err(failure) if !failure.connection_usable) {
        active.take();
    }
    true
}

fn status_observes_foreign_song(status: &MpdStatus, song_id: u64) -> bool {
    status.song_id.is_some() && status.song_id != Some(song_id)
}

fn retire_status_if_stale<C>(
    result: &MpdResult<MpdStatus>,
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    intent_epoch: &AtomicU64,
) -> bool {
    if is_current(owner, intent_epoch) {
        return false;
    }
    let observed_foreign = result.as_ref().is_ok_and(|status| {
        active
            .as_ref()
            .and_then(|session| session.song_id)
            .is_some_and(|song_id| status_observes_foreign_song(status, song_id))
    });
    if observed_foreign || matches!(result, Err(failure) if !failure.connection_usable) {
        active.take();
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn cleanup_then_fail<C>(
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    failure: MpdFailure,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) where
    C: MpdTransport,
{
    if failure.connection_usable {
        let _ = cleanup_session(active, owner, CleanupKind::Targeted, intent_epoch, timing);
    } else {
        // An I/O timeout, partial write, truncated response, or parser failure
        // may leave unread bytes or a half-command on the stream. Drop it
        // immediately instead of issuing cleanup on a poisoned protocol state.
        active.take();
    }
    if is_current(owner, intent_epoch) {
        fail_current(owner, failure, intent_epoch, cache, event_tx);
    }
}

fn relinquish_then_fail<C>(
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    failure: MpdFailure,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) {
    // Closing the client connection is the only race-free action after a
    // status proves that another song owns the shared player. Deliberately
    // retain our stable queue id: MPD has no conditional delete, so another
    // client could start that id between our status and deleteid calls. A
    // global stop/clear would more directly destroy the foreign replacement.
    // Exclusive-partition work may make orphan cleanup safe later.
    active.take();
    if is_current(owner, intent_epoch) {
        fail_current(owner, failure, intent_epoch, cache, event_tx);
    }
}

#[allow(clippy::too_many_arguments)]
fn delete_owned_then_fail<C>(
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    song_id: u64,
    failure: MpdFailure,
    require_delete_proof: bool,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) where
    C: MpdTransport,
{
    if !is_current(owner, intent_epoch) {
        return;
    }
    let removed = active.as_mut().map_or_else(
        || Err(MpdFailure::new("queue ownership")),
        |session| {
            session.connection.delete_id(
                song_id,
                OperationDeadline::after(timing.operation.min(IO_IDLE_TIMEOUT)),
            )
        },
    );
    let failure = if require_delete_proof && !matches!(removed, Ok(DeleteOutcome::Removed)) {
        MpdFailure::new("remote playback ownership")
    } else {
        failure
    };
    // Whether the targeted delete succeeded, failed cleanly, or poisoned the
    // stream, never issue a global command after this terminal status.
    active.take();
    if is_current(owner, intent_epoch) {
        fail_current(owner, failure, intent_epoch, cache, event_tx);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_control<C>(
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    kind: CommandKind,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) where
    C: MpdTransport,
{
    if !is_current(owner, intent_epoch) {
        return;
    }
    let Some(session) = active.as_ref() else {
        return;
    };
    if session.owner.epoch != owner.epoch {
        return;
    }
    if session.song_id.is_none() {
        return;
    }
    let deadline = timing.deadline();

    // A periodic poll may be almost one interval old. Revalidate the stable
    // current-song id immediately before every control so a replacement made
    // by another MPD client is observed before Tributary changes playback.
    let status = active
        .as_mut()
        .expect("active MPD session checked")
        .connection
        .status(deadline);
    if retire_status_if_stale(&status, active, owner, intent_epoch) {
        return;
    }
    match status {
        Ok(status) => {
            apply_authoritative_status(
                active,
                owner,
                status,
                intent_epoch,
                cache,
                event_tx,
                timing,
            );
        }
        Err(failure) => {
            // A failed ownership query cannot authorize even a global pause.
            // Drop the session without sending any cleanup command.
            active.take();
            fail_current(owner, failure, intent_epoch, cache, event_tx);
            return;
        }
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    let Some(session) = active.as_mut() else {
        return;
    };
    if session.owner.epoch != owner.epoch {
        return;
    }
    let Some(song_id) = session.song_id else {
        return;
    };
    let result = match kind {
        // `playid` begins the selected queue entry and therefore restarts a
        // paused song. Use MPD's explicit resume operation for Paused, but
        // restart the owned queue entry after an external Stop.
        CommandKind::Play => match session.state {
            PlayerState::Stopped => session.connection.play_id(song_id, deadline),
            _ => session.connection.pause(false, deadline),
        },
        CommandKind::Pause => session.connection.pause(true, deadline),
        CommandKind::Toggle => match session.state {
            PlayerState::Playing | PlayerState::Buffering => {
                session.connection.pause(true, deadline)
            }
            PlayerState::Paused => session.connection.pause(false, deadline),
            PlayerState::Stopped => session.connection.play_id(song_id, deadline),
        },
        CommandKind::Seek(position_ms) => {
            session.connection.seek_id(song_id, position_ms, deadline)
        }
        _ => return,
    };
    if retire_poisoned_if_stale(&result, active, owner, intent_epoch) {
        return;
    }
    if let Err(failure) = result {
        cleanup_then_fail(
            active,
            owner,
            failure,
            intent_epoch,
            cache,
            event_tx,
            timing,
        );
        return;
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    let status = active
        .as_mut()
        .expect("active MPD session checked")
        .connection
        .status(deadline);
    if retire_status_if_stale(&status, active, owner, intent_epoch) {
        return;
    }
    match status {
        Ok(status) => {
            apply_authoritative_status(
                active,
                owner,
                status,
                intent_epoch,
                cache,
                event_tx,
                timing,
            );
        }
        Err(failure) => cleanup_then_fail(
            active,
            owner,
            failure,
            intent_epoch,
            cache,
            event_tx,
            timing,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn poll_active<C>(
    active: &mut Option<WorkerSession<C>>,
    force: bool,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) where
    C: MpdTransport,
{
    let Some(session) = active.as_ref() else {
        return;
    };
    let owner = session.owner;
    if session.song_id.is_none() {
        return;
    }
    if !is_current(owner, intent_epoch) || (!force && session.last_poll.elapsed() < timing.poll) {
        return;
    }
    let status = active
        .as_mut()
        .expect("active MPD session checked")
        .connection
        .status(timing.deadline());
    if retire_status_if_stale(&status, active, owner, intent_epoch) {
        return;
    }
    if let Some(session) = active.as_mut() {
        session.last_poll = Instant::now();
    }
    let status = match status {
        Ok(status) => status,
        Err(failure) => {
            cleanup_then_fail(
                active,
                owner,
                failure,
                intent_epoch,
                cache,
                event_tx,
                timing,
            );
            return;
        }
    };
    apply_authoritative_status(active, owner, status, intent_epoch, cache, event_tx, timing);
}

#[allow(clippy::too_many_arguments)]
fn apply_authoritative_status<C>(
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    status: MpdStatus,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) where
    C: MpdTransport,
{
    if !is_current(owner, intent_epoch) {
        return;
    }
    let Some(session) = active.as_ref() else {
        return;
    };
    let Some(song_id) = session.song_id else {
        relinquish_then_fail(
            active,
            owner,
            MpdFailure::new("remote playback ownership"),
            intent_epoch,
            cache,
            event_tx,
        );
        return;
    };

    if status.state != MpdPlaybackState::Stopped {
        if status.song_id == Some(song_id) {
            if status.has_error {
                delete_owned_then_fail(
                    active,
                    owner,
                    song_id,
                    MpdFailure::new("remote playback"),
                    false,
                    intent_epoch,
                    cache,
                    event_tx,
                    timing,
                );
            } else {
                publish_status(active, owner, status, intent_epoch, cache, event_tx);
            }
        } else {
            relinquish_then_fail(
                active,
                owner,
                MpdFailure::new("remote playback ownership"),
                intent_epoch,
                cache,
                event_tx,
            );
        }
        return;
    }

    if status.song_id == Some(song_id) {
        if status.has_error {
            delete_owned_then_fail(
                active,
                owner,
                song_id,
                MpdFailure::new("remote playback"),
                false,
                intent_epoch,
                cache,
                event_tx,
                timing,
            );
        } else {
            // MPD retains the current id on an explicit Stop. Keep the owned
            // session so Play/Toggle can restart it without a reload.
            publish_status(active, owner, status, intent_epoch, cache, event_tx);
        }
        return;
    }
    if status.song_id.is_some() {
        relinquish_then_fail(
            active,
            owner,
            MpdFailure::new("remote playback ownership"),
            intent_epoch,
            cache,
            event_tx,
        );
        return;
    }
    if status.has_error {
        // With no current pointer, atomically target only our retained queue
        // entry before reporting the remote error. Success proves ownership;
        // failure is still safe because no foreign item is mutated.
        delete_owned_then_fail(
            active,
            owner,
            song_id,
            MpdFailure::new("remote playback"),
            true,
            intent_epoch,
            cache,
            event_tx,
            timing,
        );
        return;
    }

    // At the natural end of the queue MPD clears its current pointer and
    // omits songid even with consume disabled. A successful play ACK plus an
    // atomic deleteid that actually removes our retained stable entry is the
    // completion proof. An external Next at the queue boundary has the same
    // observable proof and deliberately shares TrackEnded semantics. An
    // external clear/delete instead returns NoExist and is ownership loss.
    // This remains reliable for unknown-duration and very short streams.
    if !session.observed_active {
        relinquish_then_fail(
            active,
            owner,
            MpdFailure::new("remote playback ownership"),
            intent_epoch,
            cache,
            event_tx,
        );
        return;
    }

    let ownership_deadline = OperationDeadline::after(timing.operation.min(IO_IDLE_TIMEOUT));
    let removed = active
        .as_mut()
        .expect("active MPD session checked")
        .connection
        .delete_id(song_id, ownership_deadline);
    if !is_current(owner, intent_epoch) {
        // Every delete outcome is terminal for this superseded ownership
        // claim; never issue a global command or restore the old session.
        active.take();
        return;
    }
    match removed {
        Ok(DeleteOutcome::Removed) => {}
        Ok(DeleteOutcome::AlreadyAbsent) | Err(_) => {
            relinquish_then_fail(
                active,
                owner,
                MpdFailure::new("remote playback ownership"),
                intent_epoch,
                cache,
                event_tx,
            );
            return;
        }
    }

    active.take().expect("active MPD session checked");
    let published = publish_state(
        owner,
        PlayerState::Stopped,
        None,
        intent_epoch,
        cache,
        event_tx,
    );
    if published {
        emit_if_current(
            owner,
            PlayerEvent::ended(owner.event_generation),
            intent_epoch,
            event_tx,
        );
    }
}

fn publish_status<C>(
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    status: MpdStatus,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) {
    let state = match status.state {
        MpdPlaybackState::Playing => PlayerState::Playing,
        MpdPlaybackState::Paused => PlayerState::Paused,
        MpdPlaybackState::Stopped => PlayerState::Stopped,
    };
    let changed = active
        .as_ref()
        .is_some_and(|session| session.state != state);
    if let Some(session) = active.as_mut() {
        session.state = state;
        session.observed_active |= matches!(
            status.state,
            MpdPlaybackState::Playing | MpdPlaybackState::Paused
        );
        if status.position_ms.is_some() {
            session.last_position_ms = status.position_ms;
        }
        if status.duration_ms > 0 {
            session.duration_ms = status.duration_ms;
        }
        session.last_poll = Instant::now();
    }
    if changed {
        let _ = publish_state(
            owner,
            state,
            status.position_ms,
            intent_epoch,
            cache,
            event_tx,
        );
    } else {
        let mut cache = cache.lock().unwrap_or_else(|poison| poison.into_inner());
        if is_current(owner, intent_epoch) {
            cache.position_ms = status.position_ms;
        }
    }
    if let Some(position_ms) = status.position_ms {
        emit_if_current(
            owner,
            PlayerEvent::position(owner.event_generation, position_ms, status.duration_ms),
            intent_epoch,
            event_tx,
        );
    }
}

fn cleanup_session<C>(
    active: &mut Option<WorkerSession<C>>,
    owner: CommandOwner,
    kind: CleanupKind,
    intent_epoch: &AtomicU64,
    timing: WorkerTiming,
) -> CleanupOutcome
where
    C: MpdTransport,
{
    if !is_current(owner, intent_epoch) {
        return CleanupOutcome::Stale;
    }
    let Some(mut session) = active.take() else {
        return CleanupOutcome::Completed;
    };
    if !is_current(owner, intent_epoch) {
        *active = Some(session);
        return CleanupOutcome::Stale;
    }
    let Some(song_id) = session.song_id else {
        return CleanupOutcome::Completed;
    };

    let deadline = timing.deadline();
    let mut failure = None;
    if kind == CleanupKind::StopOwned {
        let status = session.connection.status(deadline);
        if !is_current(owner, intent_epoch) {
            let can_restore = match &status {
                Ok(status) => !status_observes_foreign_song(status, song_id),
                Err(failure) => failure.connection_usable,
            };
            if can_restore {
                *active = Some(session);
            }
            return CleanupOutcome::Stale;
        }

        match status {
            Ok(status) if status.song_id == Some(song_id) => {
                if status.state != MpdPlaybackState::Stopped {
                    let stopped = session.connection.stop(deadline);
                    if !is_current(owner, intent_epoch) {
                        if stopped
                            .as_ref()
                            .err()
                            .is_none_or(|failure| failure.connection_usable)
                        {
                            *active = Some(session);
                        }
                        return CleanupOutcome::Stale;
                    }
                    match stopped {
                        Ok(()) => {}
                        Err(stop_failure) if stop_failure.connection_usable => {
                            failure = Some(stop_failure);
                        }
                        Err(stop_failure) => return CleanupOutcome::Failed(stop_failure),
                    }
                }
            }
            // A foreign current id means the exclusive-control contract was
            // violated. It still does not authorize either a global stop or a
            // racy targeted delete: another client could select our queued id
            // between this status and deleteid, so deliberately retain it.
            Ok(status) if status.song_id.is_some() => return CleanupOutcome::Completed,
            // No current id authorizes only the targeted cleanup below.
            Ok(_) => {}
            Err(status_failure) if status_failure.connection_usable => {
                // The stream is synchronized, so a targeted delete is safe,
                // but ownership was indeterminate and the Stop itself failed.
                failure = Some(status_failure);
            }
            Err(status_failure) => return CleanupOutcome::Failed(status_failure),
        }
    }

    if !is_current(owner, intent_epoch) {
        *active = Some(session);
        return CleanupOutcome::Stale;
    }
    let removed = session.connection.delete_id(song_id, deadline);
    if !is_current(owner, intent_epoch) {
        // Removed, already absent, rejected, or poisoned are all terminal for
        // the superseded ownership claim, so never restore it.
        return CleanupOutcome::Stale;
    }
    match removed {
        Ok(DeleteOutcome::Removed | DeleteOutcome::AlreadyAbsent) => {
            failure.map_or(CleanupOutcome::Completed, CleanupOutcome::Failed)
        }
        Err(delete_failure) => CleanupOutcome::Failed(failure.unwrap_or(delete_failure)),
    }
}

fn cleanup_unconditionally<C>(active: &mut Option<WorkerSession<C>>, timing: WorkerTiming)
where
    C: MpdTransport,
{
    if let Some(mut session) = active.take() {
        let Some(song_id) = session.song_id else {
            return;
        };
        let deadline = timing.deadline();
        let status = session.connection.status(deadline);
        let can_delete = match status {
            Ok(status) if status.song_id == Some(song_id) => {
                if status.state == MpdPlaybackState::Stopped {
                    true
                } else {
                    session
                        .connection
                        .stop(deadline)
                        .as_ref()
                        .err()
                        .is_none_or(|failure| failure.connection_usable)
                }
            }
            // A foreign current id makes even targeted deletion racy: another
            // client can select our id after this observation.
            Ok(status) if status.song_id.is_some() => false,
            Ok(_) => true,
            Err(failure) => failure.connection_usable,
        };
        if can_delete {
            let _ = session.connection.delete_id(song_id, deadline);
        }
    }
}

fn fail_current(
    owner: CommandOwner,
    failure: MpdFailure,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) {
    if !is_current(owner, intent_epoch) {
        return;
    }
    let message = mpd_failure_message(failure);
    error!(operation = failure.operation, "MPD operation failed");
    if publish_state(
        owner,
        PlayerState::Stopped,
        None,
        intent_epoch,
        cache,
        event_tx,
    ) {
        emit_if_current(
            owner,
            PlayerEvent::error(owner.event_generation, message),
            intent_epoch,
            event_tx,
        );
    }
}

fn publish_state(
    owner: CommandOwner,
    state: PlayerState,
    position_ms: Option<u64>,
    intent_epoch: &AtomicU64,
    cache: &Mutex<MpdCache>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) -> bool {
    if !is_current(owner, intent_epoch) {
        return false;
    }
    {
        let mut cache = cache.lock().unwrap_or_else(|poison| poison.into_inner());
        if !is_current(owner, intent_epoch) {
            return false;
        }
        cache.state = state;
        cache.position_ms = position_ms;
    }
    emit_if_current(
        owner,
        PlayerEvent::state(owner.event_generation, state),
        intent_epoch,
        event_tx,
    );
    true
}

fn emit_if_current(
    owner: CommandOwner,
    event: PlayerEvent,
    intent_epoch: &AtomicU64,
    event_tx: &async_channel::Sender<PlayerEvent>,
) {
    if is_current(owner, intent_epoch) {
        let _ = event_tx.try_send(event);
    }
}

fn is_current(owner: CommandOwner, intent_epoch: &AtomicU64) -> bool {
    intent_epoch.load(Ordering::SeqCst) == owner.epoch
}

impl MpdOutput {
    pub fn new(
        display_name: &str,
        host: &str,
        port: u16,
        control_mode: MpdControlMode,
        event_tx: async_channel::Sender<PlayerEvent>,
    ) -> Self {
        info!(host = %host, port, name = %display_name, ?control_mode, "MPD output configured");
        let intent_epoch = Arc::new(AtomicU64::new(0));
        let cache = Arc::new(Mutex::new(MpdCache::default()));
        let proxy = ProxyServices::production();
        let worker_tx = spawn_mpd_worker(
            MpdTcpConnector {
                host: host.to_string(),
                port,
            },
            control_mode,
            Arc::clone(&intent_epoch),
            Arc::clone(&cache),
            event_tx.clone(),
            WorkerTiming::production(),
            proxy.clone(),
        );
        Self {
            display_name: display_name.to_string(),
            event_tx,
            event_generation: AtomicU64::new(0),
            volume: 1.0,
            control_mode,
            intent_epoch,
            cache,
            proxy,
            worker_tx,
        }
    }

    /// Supply the application runtime used to host exact-route media tickets.
    #[must_use]
    pub fn with_runtime(self, handle: tokio::runtime::Handle) -> Self {
        self.proxy.set_runtime(handle);
        self
    }

    pub fn probe(host: &str, port: u16) -> Result<String, String> {
        let probe_epoch = AtomicU64::new(0);
        let connection = MpdConnection::connect(
            host,
            port,
            0,
            &probe_epoch,
            OperationDeadline::after(OPERATION_TIMEOUT),
        )
        .map_err(mpd_failure_message)?;
        info!(version = %connection.version, "MPD probe successful");
        Ok(connection.version)
    }

    fn event_generation(&self) -> PlayerEventGeneration {
        PlayerEventGeneration::from_raw(self.event_generation.load(Ordering::SeqCst))
    }

    fn next_owner(&self) -> CommandOwner {
        CommandOwner {
            epoch: self.intent_epoch.fetch_add(1, Ordering::SeqCst) + 1,
            event_generation: self.event_generation(),
        }
    }

    fn current_owner(&self) -> CommandOwner {
        CommandOwner {
            epoch: self.intent_epoch.load(Ordering::SeqCst),
            event_generation: self.event_generation(),
        }
    }

    fn enqueue(&self, owner: CommandOwner, kind: CommandKind) {
        match self.worker_tx.enqueue(WorkerCommand { owner, kind }) {
            WorkerEnqueueOutcome::Enqueued | WorkerEnqueueOutcome::Superseded => {}
            WorkerEnqueueOutcome::Saturated => {
                error!("MPD worker command ingress rejected a non-transient command");
            }
            WorkerEnqueueOutcome::Disconnected if is_current(owner, &self.intent_epoch) => {
                fail_current(
                    owner,
                    MpdFailure::new("worker availability"),
                    &self.intent_epoch,
                    &self.cache,
                    &self.event_tx,
                );
            }
            WorkerEnqueueOutcome::Disconnected => {}
        }
    }

    fn begin_load(&self) -> CommandOwner {
        let owner = self.next_owner();
        self.proxy.revoke_before(owner.epoch);
        // Retire the previous track's cached state immediately. The worker may
        // still be finishing bounded cleanup, and callers such as Previous
        // must not apply that old position to the newly selected track.
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cache.state = PlayerState::Buffering;
        cache.position_ms = None;
        owner
    }

    fn ensure_load_allowed(&self) -> bool {
        if self.control_mode == MpdControlMode::Exclusive {
            return true;
        }

        // Reject at the public output boundary before begin_load can advance
        // the epoch or publish optimistic Buffering state. The worker repeats
        // the check as defense in depth for any future/internal caller that
        // bypasses this boundary.
        fail_current(
            self.current_owner(),
            MpdFailure::exclusive_control_required(),
            &self.intent_epoch,
            &self.cache,
            &self.event_tx,
        );
        false
    }
}

impl AudioOutput for MpdOutput {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn output_type(&self) -> OutputType {
        OutputType::Mpd
    }

    fn supports_volume(&self) -> bool {
        false
    }

    fn load_uri(&self, uri: &str) -> bool {
        if !self.ensure_load_allowed() {
            return false;
        }
        let owner = self.begin_load();
        let kind = match encode_mpd_arg(uri) {
            Err(failure) => CommandKind::RejectLoad { failure },
            Ok(_) => match classify_media_uri(uri) {
                MediaUriSecurity::Direct => CommandKind::Load {
                    uri: uri.to_string(),
                },
                MediaUriSecurity::Protected(upstream) => CommandKind::ProtectedLoad { upstream },
                MediaUriSecurity::Reject => CommandKind::RejectLoad {
                    failure: MpdFailure::new("media URI validation"),
                },
            },
        };
        self.enqueue(owner, kind);
        true
    }

    fn load_resolved(&self, request: ResolvedHttpRequest) -> bool {
        if !self.ensure_load_allowed() {
            return false;
        }
        let owner = self.begin_load();
        let kind = if request.is_active() {
            CommandKind::ResolvedLoad {
                request: Box::new(request),
            }
        } else {
            CommandKind::RejectLoad {
                failure: MpdFailure::new("media source availability"),
            }
        };
        self.enqueue(owner, kind);
        true
    }

    fn load_local(&self, media: ResolvedLocalMedia) -> bool {
        if !self.ensure_load_allowed() {
            return false;
        }
        let owner = self.begin_load();
        self.enqueue(owner, CommandKind::LocalLoad { media });
        true
    }

    fn set_event_generation(&self, generation: PlayerEventGeneration) {
        self.event_generation
            .store(generation.as_raw(), Ordering::SeqCst);
    }

    fn play(&self) {
        self.enqueue(self.current_owner(), CommandKind::Play);
    }

    fn pause(&self) {
        self.enqueue(self.current_owner(), CommandKind::Pause);
    }

    fn stop(&self) {
        let owner = self.next_owner();
        self.proxy.revoke_before(owner.epoch);
        {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            cache.state = PlayerState::Stopped;
            cache.position_ms = None;
        }
        self.enqueue(owner, CommandKind::Stop);
    }

    fn toggle_play_pause(&self) {
        self.enqueue(self.current_owner(), CommandKind::Toggle);
    }

    fn seek_to(&self, position_ms: u64) {
        self.enqueue(self.current_owner(), CommandKind::Seek(position_ms));
    }

    fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
    }

    fn volume(&self) -> f64 {
        self.volume
    }

    fn state(&self) -> PlayerState {
        self.cache
            .lock()
            .map(|cache| cache.state)
            .unwrap_or(PlayerState::Stopped)
    }

    fn position_ms(&self) -> Option<u64> {
        self.cache
            .lock()
            .map(|cache| cache.position_ms)
            .unwrap_or(None)
    }
}

impl Drop for MpdOutput {
    fn drop(&mut self) {
        let owner = self.next_owner();
        self.proxy.revoke_before(owner.epoch);
        if let Ok(mut cache) = self.cache.lock() {
            cache.state = PlayerState::Stopped;
            cache.position_ms = None;
        }
        let _ = self.worker_tx.enqueue(WorkerCommand {
            owner,
            kind: CommandKind::Shutdown,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener};
    use std::sync::atomic::AtomicBool;

    fn authorized_local_media() -> (tempfile::TempDir, ResolvedLocalMedia) {
        let root = tempfile::tempdir().expect("temporary local-media root");
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .expect("write local-media marker");
        let path = root.path().join("track.flac");
        std::fs::write(&path, b"local media").expect("write local-media fixture");
        let media = ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
            .expect("retain local-media authority");
        (root, media)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Point {
        Connect,
        Repeat,
        Random,
        Single,
        Consume,
        Add,
        Play,
        Pause,
        Stop,
        Seek,
        Status,
        Ownership,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Action {
        Point(Point),
        Play(u64),
        Pause(bool),
        Seek(u64, u64),
        Delete(u64),
    }

    struct Gate {
        point: Point,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    struct FakeShared {
        actions: Mutex<Vec<Action>>,
        added_uris: Mutex<Vec<String>>,
        gate: Mutex<Option<Gate>>,
        fail_at: Mutex<Option<Point>>,
        poison_at: Mutex<Option<Point>>,
        statuses: Mutex<VecDeque<MpdStatus>>,
        delete_results: Mutex<VecDeque<MpdResult<DeleteOutcome>>>,
        local_addr: Mutex<SocketAddr>,
        fail_local_addr: AtomicBool,
    }

    impl FakeShared {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                actions: Mutex::new(Vec::new()),
                added_uris: Mutex::new(Vec::new()),
                gate: Mutex::new(None),
                fail_at: Mutex::new(None),
                poison_at: Mutex::new(None),
                statuses: Mutex::new(VecDeque::new()),
                delete_results: Mutex::new(VecDeque::new()),
                local_addr: Mutex::new(SocketAddr::from((Ipv4Addr::LOCALHOST, 45_000))),
                fail_local_addr: AtomicBool::new(false),
            })
        }

        fn install_gate(self: &Arc<Self>, point: Point) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            *self.gate.lock().expect("gate lock") = Some(Gate {
                point,
                entered: entered_tx,
                release: release_rx,
            });
            (entered_rx, release_tx)
        }

        fn record(&self, point: Point, action: Action) -> MpdResult<()> {
            self.actions.lock().expect("actions lock").push(action);
            let gate = {
                let mut gate = self.gate.lock().expect("gate lock");
                if gate.as_ref().is_some_and(|gate| gate.point == point) {
                    gate.take()
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                gate.entered
                    .send(())
                    .map_err(|error| opaque_mpd_failure("test gate", error))?;
                gate.release
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|error| opaque_mpd_failure("test gate", error))?;
            }
            if self.fail_at.lock().expect("failure lock").as_ref() == Some(&point) {
                return Err(MpdFailure::synchronized(point.operation()));
            }
            let poisoned = {
                let mut poison = self.poison_at.lock().expect("poison lock");
                if poison.as_ref() == Some(&point) {
                    poison.take();
                    true
                } else {
                    false
                }
            };
            if poisoned {
                return Err(MpdFailure::new(point.operation()));
            }
            Ok(())
        }

        fn actions(&self) -> Vec<Action> {
            self.actions.lock().expect("actions lock").clone()
        }

        fn clear_actions(&self) {
            self.actions.lock().expect("actions lock").clear();
        }

        fn added_uris(&self) -> Vec<String> {
            self.added_uris.lock().expect("added URIs lock").clone()
        }
    }

    impl Point {
        const fn operation(self) -> &'static str {
            match self {
                Self::Connect => "test connection",
                Self::Repeat => "test repeat",
                Self::Random => "test random",
                Self::Single => "test single",
                Self::Consume => "test consume",
                Self::Add => "test add",
                Self::Play => "test play",
                Self::Pause => "test pause",
                Self::Stop => "test stop",
                Self::Seek => "test seek",
                Self::Status => "test status",
                Self::Ownership => "test ownership",
            }
        }
    }

    struct FakeConnector {
        shared: Arc<FakeShared>,
    }

    struct FakeConnection {
        shared: Arc<FakeShared>,
    }

    impl MpdConnector for FakeConnector {
        type Connection = FakeConnection;

        fn connect(
            &mut self,
            _owner_epoch: u64,
            _intent_epoch: &AtomicU64,
            _deadline: OperationDeadline,
        ) -> MpdResult<Self::Connection> {
            self.shared
                .record(Point::Connect, Action::Point(Point::Connect))?;
            Ok(FakeConnection {
                shared: Arc::clone(&self.shared),
            })
        }
    }

    impl MpdTransport for FakeConnection {
        fn local_addr(&self) -> MpdResult<SocketAddr> {
            if self.shared.fail_local_addr.load(Ordering::SeqCst) {
                return Err(MpdFailure::new("connection address"));
            }
            Ok(*self.shared.local_addr.lock().expect("local address lock"))
        }

        fn repeat_off(&mut self, _deadline: OperationDeadline) -> MpdResult<()> {
            self.shared
                .record(Point::Repeat, Action::Point(Point::Repeat))
        }

        fn random_off(&mut self, _deadline: OperationDeadline) -> MpdResult<()> {
            self.shared
                .record(Point::Random, Action::Point(Point::Random))
        }

        fn single_off(&mut self, _deadline: OperationDeadline) -> MpdResult<()> {
            self.shared
                .record(Point::Single, Action::Point(Point::Single))
        }

        fn consume_off(&mut self, _deadline: OperationDeadline) -> MpdResult<()> {
            self.shared
                .record(Point::Consume, Action::Point(Point::Consume))
        }

        fn add_id(&mut self, uri: &str, _deadline: OperationDeadline) -> MpdResult<u64> {
            self.shared
                .added_uris
                .lock()
                .expect("added URIs lock")
                .push(uri.to_string());
            self.shared.record(Point::Add, Action::Point(Point::Add))?;
            Ok(42)
        }

        fn play_id(&mut self, song_id: u64, _deadline: OperationDeadline) -> MpdResult<()> {
            self.shared.record(Point::Play, Action::Play(song_id))
        }

        fn pause(&mut self, paused: bool, _deadline: OperationDeadline) -> MpdResult<()> {
            self.shared.record(Point::Pause, Action::Pause(paused))
        }

        fn stop(&mut self, _deadline: OperationDeadline) -> MpdResult<()> {
            self.shared.record(Point::Stop, Action::Point(Point::Stop))
        }

        fn seek_id(
            &mut self,
            song_id: u64,
            position_ms: u64,
            _deadline: OperationDeadline,
        ) -> MpdResult<()> {
            self.shared
                .record(Point::Seek, Action::Seek(song_id, position_ms))
        }

        fn status(&mut self, _deadline: OperationDeadline) -> MpdResult<MpdStatus> {
            self.shared
                .record(Point::Status, Action::Point(Point::Status))?;
            Ok(self
                .shared
                .statuses
                .lock()
                .expect("statuses lock")
                .pop_front()
                .unwrap_or_else(|| playing_status(0, 10_000)))
        }

        fn delete_id(
            &mut self,
            song_id: u64,
            _deadline: OperationDeadline,
        ) -> MpdResult<DeleteOutcome> {
            self.shared
                .record(Point::Ownership, Action::Delete(song_id))?;
            self.shared
                .delete_results
                .lock()
                .expect("delete results lock")
                .pop_front()
                .unwrap_or(Ok(DeleteOutcome::Removed))
        }
    }

    struct FakeTicketState {
        active: AtomicBool,
    }

    struct FakeMediaTicket {
        uri: String,
        state: Arc<FakeTicketState>,
    }

    impl MpdMediaTicket for FakeMediaTicket {
        fn uri(&self) -> &str {
            &self.uri
        }

        fn revoke(&self) {
            self.state.active.store(false, Ordering::SeqCst);
        }
    }

    struct FakeProxyShared {
        starts: Mutex<Vec<SocketAddr>>,
        upstreams: Mutex<Vec<String>>,
        tickets: Mutex<Vec<Arc<FakeTicketState>>>,
        fail_start: AtomicBool,
        invalid_ticket: AtomicBool,
    }

    impl FakeProxyShared {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                starts: Mutex::new(Vec::new()),
                upstreams: Mutex::new(Vec::new()),
                tickets: Mutex::new(Vec::new()),
                fail_start: AtomicBool::new(false),
                invalid_ticket: AtomicBool::new(false),
            })
        }

        fn active(&self, index: usize) -> bool {
            self.tickets.lock().expect("proxy tickets lock")[index]
                .active
                .load(Ordering::SeqCst)
        }
    }

    struct FakeProxyFactory {
        shared: Arc<FakeProxyShared>,
    }

    impl MpdProxyFactory for FakeProxyFactory {
        fn start(
            &self,
            _runtime: &tokio::runtime::Handle,
            local_addr: SocketAddr,
            upstream: &MpdUpstream,
        ) -> MpdResult<Arc<dyn MpdMediaTicket>> {
            self.shared
                .starts
                .lock()
                .expect("proxy starts lock")
                .push(local_addr);
            if let Some(endpoint) = upstream.endpoint() {
                self.shared
                    .upstreams
                    .lock()
                    .expect("proxy upstreams lock")
                    .push(endpoint.as_str().to_string());
            }
            if self.shared.fail_start.load(Ordering::SeqCst) {
                return Err(MpdFailure::new("media proxy registration"));
            }

            let state = Arc::new(FakeTicketState {
                active: AtomicBool::new(true),
            });
            let index = {
                let mut tickets = self.shared.tickets.lock().expect("proxy tickets lock");
                let index = tickets.len();
                tickets.push(Arc::clone(&state));
                index
            };
            let ticket_addr = SocketAddr::new(local_addr.ip(), 46_000);
            let uri = if self.shared.invalid_ticket.load(Ordering::SeqCst) {
                "http://127.0.0.1/cast/invalid\nroute".to_string()
            } else {
                format!("http://{ticket_addr}/cast/opaque-{index}.flac")
            };
            Ok(Arc::new(FakeMediaTicket { uri, state }))
        }
    }

    fn fake_proxy_services(
        shared: Arc<FakeProxyShared>,
        runtime: &tokio::runtime::Runtime,
    ) -> ProxyServices {
        ProxyServices {
            runtime: Arc::new(Mutex::new(Some(runtime.handle().clone()))),
            factory: Arc::new(FakeProxyFactory { shared }),
            current: Arc::new(Mutex::new(None)),
        }
    }

    fn playing_status(position_ms: u64, duration_ms: u64) -> MpdStatus {
        MpdStatus {
            state: MpdPlaybackState::Playing,
            song_id: Some(42),
            position_ms: Some(position_ms),
            duration_ms,
            has_error: false,
        }
    }

    fn paused_status(position_ms: u64, duration_ms: u64) -> MpdStatus {
        MpdStatus {
            state: MpdPlaybackState::Paused,
            song_id: Some(42),
            position_ms: Some(position_ms),
            duration_ms,
            has_error: false,
        }
    }

    fn stopped_status(position_ms: u64, duration_ms: u64) -> MpdStatus {
        MpdStatus {
            state: MpdPlaybackState::Stopped,
            song_id: Some(42),
            position_ms: Some(position_ms),
            duration_ms,
            has_error: false,
        }
    }

    struct Harness {
        tx: WorkerCommandSender,
        epoch: Arc<AtomicU64>,
        cache: Arc<Mutex<MpdCache>>,
        events: async_channel::Receiver<PlayerEvent>,
        proxy: ProxyServices,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn new(shared: Arc<FakeShared>) -> Self {
            Self::new_with_timing(
                shared,
                WorkerTiming {
                    operation: Duration::from_secs(2),
                    poll: Duration::from_hours(1),
                    tick: Duration::from_millis(10),
                },
            )
        }

        fn new_with_timing(shared: Arc<FakeShared>, timing: WorkerTiming) -> Self {
            Self::new_with_timing_and_proxy(shared, timing, ProxyServices::production())
        }

        fn new_with_proxy(shared: Arc<FakeShared>, proxy: ProxyServices) -> Self {
            Self::new_with_timing_and_proxy(
                shared,
                WorkerTiming {
                    operation: Duration::from_secs(2),
                    poll: Duration::from_hours(1),
                    tick: Duration::from_millis(10),
                },
                proxy,
            )
        }

        fn new_with_timing_and_proxy(
            shared: Arc<FakeShared>,
            timing: WorkerTiming,
            proxy: ProxyServices,
        ) -> Self {
            Self::new_with_mode_and_proxy(shared, timing, proxy, MpdControlMode::Exclusive)
        }

        fn new_unconfirmed_with_proxy(shared: Arc<FakeShared>, proxy: ProxyServices) -> Self {
            Self::new_with_mode_and_proxy(
                shared,
                WorkerTiming {
                    operation: Duration::from_secs(2),
                    poll: Duration::from_hours(1),
                    tick: Duration::from_millis(10),
                },
                proxy,
                MpdControlMode::Unconfirmed,
            )
        }

        fn new_with_mode_and_proxy(
            shared: Arc<FakeShared>,
            timing: WorkerTiming,
            proxy: ProxyServices,
            control_mode: MpdControlMode,
        ) -> Self {
            let (tx, rx) = worker_command_channel(MAX_PENDING_WORKER_COMMANDS);
            let epoch = Arc::new(AtomicU64::new(0));
            let cache = Arc::new(Mutex::new(MpdCache::default()));
            let (event_tx, events) = async_channel::unbounded();
            let epoch_for_worker = Arc::clone(&epoch);
            let cache_for_worker = Arc::clone(&cache);
            let worker_proxy = proxy.clone();
            let worker = std::thread::spawn(move || {
                run_mpd_worker(
                    FakeConnector { shared },
                    rx,
                    control_mode,
                    epoch_for_worker,
                    cache_for_worker,
                    event_tx,
                    timing,
                    worker_proxy,
                );
            });
            Self {
                tx,
                epoch,
                cache,
                events,
                proxy,
                worker: Some(worker),
            }
        }

        fn next_owner(&self, generation: u64) -> CommandOwner {
            CommandOwner {
                epoch: self.epoch.fetch_add(1, Ordering::SeqCst) + 1,
                event_generation: PlayerEventGeneration::from_raw(generation),
            }
        }

        fn next_replacing_owner(&self, generation: u64) -> CommandOwner {
            let owner = self.next_owner(generation);
            self.proxy.revoke_before(owner.epoch);
            owner
        }

        fn send(&self, owner: CommandOwner, kind: CommandKind) {
            assert_eq!(
                self.tx.enqueue(WorkerCommand { owner, kind }),
                WorkerEnqueueOutcome::Enqueued,
                "worker command accepted"
            );
        }

        fn fence(&self, owner: CommandOwner) {
            let (done_tx, done_rx) = mpsc::channel();
            self.send(owner, CommandKind::Fence(done_tx));
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("worker reached fence");
        }

        fn events(&self) -> Vec<PlayerEvent> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }

        fn cache(&self) -> MpdCache {
            *self.cache.lock().expect("cache lock")
        }

        fn shutdown(mut self) {
            let owner = self.next_owner(999);
            self.send(owner, CommandKind::Shutdown);
            self.worker
                .take()
                .expect("worker handle")
                .join()
                .expect("worker stopped");
        }
    }

    fn protected_load(uri: &str) -> CommandKind {
        CommandKind::ProtectedLoad {
            upstream: Box::new(Url::parse(uri).expect("protected test URL")),
        }
    }

    fn assert_unconfirmed_load_has_no_side_effect(kind: CommandKind) {
        let shared = FakeShared::new();
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let harness = Harness::new_unconfirmed_with_proxy(Arc::clone(&shared), proxy);
        let owner = harness.next_owner(17);

        harness.send(owner, kind);
        harness.fence(owner);

        assert!(shared.actions().is_empty(), "no MPD operation is allowed");
        assert!(shared.added_uris().is_empty(), "no queue URI is allowed");
        assert!(
            proxy_shared
                .starts
                .lock()
                .expect("proxy starts lock")
                .is_empty(),
            "no protected-media ticket is allowed"
        );
        assert_eq!(harness.cache().state, PlayerState::Stopped);
        let events = harness.events();
        assert!(matches!(
            events.as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { message, .. }
            ] if message == &mpd_exclusive_control_required_message(&rust_i18n::locale())
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                state: PlayerState::Buffering | PlayerState::Playing | PlayerState::Paused,
                ..
            }
        )));
        harness.shutdown();
    }

    #[test]
    fn unconfirmed_partition_rejects_load_before_any_connection_state_or_ticket_action() {
        assert_unconfirmed_load_has_no_side_effect(protected_load(
            "https://music.test/private.flac?token=must-not-leave",
        ));
    }

    #[test]
    fn unconfirmed_partition_rejects_preclassified_failure_before_cleanup() {
        assert_unconfirmed_load_has_no_side_effect(CommandKind::RejectLoad {
            failure: MpdFailure::new("media URI validation"),
        });
    }

    #[test]
    fn exclusive_control_requirement_is_localized_for_every_catalog() {
        let english = mpd_exclusive_control_required_message("en");
        assert!(!english.is_empty());
        for locale in rust_i18n::available_locales!() {
            let localized = mpd_exclusive_control_required_message(&locale);
            assert!(!localized.is_empty(), "{locale}");
            if locale != "en" {
                assert_ne!(localized, english, "{locale} must not fall back to English");
            }
        }
    }

    fn queue_test_owner(epoch: u64) -> CommandOwner {
        CommandOwner {
            epoch,
            event_generation: PlayerEventGeneration::from_raw(epoch),
        }
    }

    #[test]
    fn worker_ingress_preserves_exact_fifo_below_capacity() {
        let (tx, rx) = worker_command_channel(5);
        let owner = queue_test_owner(1);
        for kind in [
            CommandKind::Seek(1_000),
            CommandKind::Seek(2_000),
            CommandKind::Play,
            CommandKind::Toggle,
        ] {
            assert_eq!(
                tx.enqueue(WorkerCommand { owner, kind }),
                WorkerEnqueueOutcome::Enqueued
            );
        }
        assert_eq!(tx.pending_len(), 4);

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("first seek")
                .kind,
            CommandKind::Seek(1_000)
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("second seek")
                .kind,
            CommandKind::Seek(2_000)
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).expect("play").kind,
            CommandKind::Play
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("toggle")
                .kind,
            CommandKind::Toggle
        ));
    }

    #[test]
    fn saturated_worker_ingress_compacts_controls_without_crossing_barriers() {
        let (tx, rx) = worker_command_channel(8);
        let owner = queue_test_owner(1);
        let (done_tx, _done_rx) = mpsc::channel();
        for kind in [
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
            CommandKind::Play,
            CommandKind::Toggle,
            CommandKind::Seek(1_000),
            CommandKind::Pause,
            CommandKind::Toggle,
            CommandKind::Fence(done_tx),
            CommandKind::Toggle,
        ] {
            assert_eq!(
                tx.enqueue(WorkerCommand { owner, kind }),
                WorkerEnqueueOutcome::Enqueued
            );
        }
        assert_eq!(tx.pending_len(), 8);
        assert_eq!(
            tx.enqueue(WorkerCommand {
                owner,
                kind: CommandKind::Toggle,
            }),
            WorkerEnqueueOutcome::Enqueued
        );
        assert_eq!(tx.pending_len(), 8);

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("load barrier")
                .kind,
            CommandKind::Load { .. }
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("folded pre-seek play")
                .kind,
            CommandKind::Play
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("folded pre-seek pause")
                .kind,
            CommandKind::Pause
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("seek barrier")
                .kind,
            CommandKind::Seek(1_000)
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("folded post-seek control")
                .kind,
            CommandKind::Play
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("test fence")
                .kind,
            CommandKind::Fence(_)
        ));
        for _ in 0..2 {
            assert!(matches!(
                rx.recv_timeout(Duration::from_secs(1))
                    .expect("even toggle retained")
                    .kind,
                CommandKind::Toggle
            ));
        }
    }

    #[test]
    fn saturated_worker_ingress_evicts_oldest_transient_for_latest_intent() {
        let (tx, rx) = worker_command_channel(4);
        let owner = queue_test_owner(1);
        for kind in [
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
            CommandKind::Seek(1_000),
            CommandKind::Toggle,
            CommandKind::Seek(2_000),
        ] {
            assert_eq!(
                tx.enqueue(WorkerCommand { owner, kind }),
                WorkerEnqueueOutcome::Enqueued
            );
        }
        assert_eq!(
            tx.enqueue(WorkerCommand {
                owner,
                kind: CommandKind::Toggle,
            }),
            WorkerEnqueueOutcome::Enqueued
        );
        assert_eq!(tx.pending_len(), 4);

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("load retained")
                .kind,
            CommandKind::Load { .. }
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("oldest retained control")
                .kind,
            CommandKind::Toggle
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("latest seek retained")
                .kind,
            CommandKind::Seek(2_000)
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("newest intent retained")
                .kind,
            CommandKind::Toggle
        ));
    }

    #[test]
    fn newer_epoch_purges_backlog_and_late_old_work_cannot_reenter() {
        let (tx, rx) = worker_command_channel(4);
        let old = queue_test_owner(1);
        for kind in [
            CommandKind::Load {
                uri: "https://music.test/old".to_string(),
            },
            CommandKind::Play,
            CommandKind::Seek(1_000),
            CommandKind::Pause,
        ] {
            assert_eq!(
                tx.enqueue(WorkerCommand { owner: old, kind }),
                WorkerEnqueueOutcome::Enqueued
            );
        }

        let replacement = queue_test_owner(2);
        assert_eq!(
            tx.enqueue(WorkerCommand {
                owner: replacement,
                kind: CommandKind::Stop,
            }),
            WorkerEnqueueOutcome::Enqueued
        );
        assert_eq!(tx.pending_len(), 1);
        assert_eq!(
            tx.enqueue(WorkerCommand {
                owner: old,
                kind: CommandKind::Toggle,
            }),
            WorkerEnqueueOutcome::Superseded
        );
        assert_eq!(tx.pending_len(), 1);
        let command = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("replacement stop");
        assert_eq!(command.owner.epoch, replacement.epoch);
        assert!(matches!(command.kind, CommandKind::Stop));
    }

    #[test]
    fn worker_ingress_reports_a_dropped_receiver_without_buffering() {
        let (tx, rx) = worker_command_channel(4);
        drop(rx);
        assert_eq!(
            tx.enqueue(WorkerCommand {
                owner: queue_test_owner(1),
                kind: CommandKind::Play,
            }),
            WorkerEnqueueOutcome::Disconnected
        );
        assert_eq!(tx.pending_len(), 0);
    }

    #[test]
    fn protected_load_adds_only_an_opaque_ticket_bound_to_the_ipv4_mpd_route() {
        let shared = FakeShared::new();
        let local_addr = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 44), 51_234));
        *shared.local_addr.lock().expect("local address lock") = local_addr;
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
        let owner = harness.next_replacing_owner(1);
        const SECRET: &str = "https://music.test/stream.flac?api_key=credential-must-not-reach-mpd";

        harness.send(owner, protected_load(SECRET));
        harness.fence(owner);

        assert_eq!(
            *proxy_shared.starts.lock().expect("proxy starts lock"),
            vec![local_addr]
        );
        assert_eq!(
            proxy_shared
                .upstreams
                .lock()
                .expect("proxy upstreams lock")
                .as_slice(),
            [SECRET]
        );
        let added = shared.added_uris();
        assert_eq!(added.len(), 1);
        assert!(added[0].starts_with("http://192.0.2.44:46000/cast/opaque-"));
        assert!(!added[0].contains("api_key"));
        assert!(!added[0].contains("credential-must-not-reach-mpd"));
        assert!(proxy_shared.active(0));

        harness.shutdown();
        assert!(!proxy_shared.active(0), "shutdown must revoke the route");
    }

    #[test]
    fn resolved_load_adds_only_the_route_ticket_and_never_the_clean_endpoint() {
        let shared = FakeShared::new();
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
        let owner = harness.next_replacing_owner(1);
        let endpoint =
            Url::parse("https://music.test/clean/track.flac?track=42").expect("clean endpoint");
        let request = ResolvedHttpRequest::new(endpoint.clone()).expect("resolved request");

        harness.send(
            owner,
            CommandKind::ResolvedLoad {
                request: Box::new(request),
            },
        );
        harness.fence(owner);

        let added = shared.added_uris();
        assert_eq!(added.len(), 1);
        assert!(added[0].starts_with("http://127.0.0.1:46000/cast/opaque-"));
        assert_ne!(added[0], endpoint.as_str());
        assert!(!added[0].contains("music.test"));
        assert!(!added[0].contains("track=42"));
        assert!(proxy_shared.active(0));

        harness.shutdown();
        assert!(!proxy_shared.active(0));
    }

    #[test]
    fn local_load_reaches_mpd_only_as_an_opaque_handle_backed_ticket() {
        let shared = FakeShared::new();
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
        let owner = harness.next_replacing_owner(1);
        let (_root, media) = authorized_local_media();

        harness.send(owner, CommandKind::LocalLoad { media });
        harness.fence(owner);

        assert_eq!(
            *proxy_shared.starts.lock().expect("proxy starts lock"),
            vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 45_000))]
        );
        assert!(
            proxy_shared
                .upstreams
                .lock()
                .expect("proxy upstreams lock")
                .is_empty(),
            "local authority must not become an upstream URL"
        );
        let added = shared.added_uris();
        assert_eq!(added.len(), 1);
        assert!(added[0].starts_with("http://127.0.0.1:46000/cast/opaque-"));
        assert!(!added[0].contains("track.flac"));
        assert!(proxy_shared.active(0));

        harness.shutdown();
        assert!(!proxy_shared.active(0));
    }

    #[test]
    fn protected_load_preserves_the_ipv6_mpd_route_in_the_ticket() {
        let shared = FakeShared::new();
        let local_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 51_235);
        *shared.local_addr.lock().expect("local address lock") = local_addr;
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
        let owner = harness.next_replacing_owner(1);

        harness.send(
            owner,
            protected_load("https://music.test/stream?X-Plex-Token=secret"),
        );
        harness.fence(owner);

        assert_eq!(
            *proxy_shared.starts.lock().expect("proxy starts lock"),
            vec![local_addr]
        );
        assert!(shared
            .added_uris()
            .into_iter()
            .all(|uri| uri.starts_with("http://[::1]:46000/cast/opaque-")));
        harness.shutdown();
    }

    #[test]
    fn protected_load_fails_closed_without_runtime_or_connection_address() {
        for fail_address in [false, true] {
            let shared = FakeShared::new();
            shared.fail_local_addr.store(fail_address, Ordering::SeqCst);
            let proxy_shared = FakeProxyShared::new();
            let runtime = tokio::runtime::Runtime::new().expect("test runtime");
            let proxy = ProxyServices {
                runtime: Arc::new(Mutex::new(fail_address.then(|| runtime.handle().clone()))),
                factory: Arc::new(FakeProxyFactory {
                    shared: Arc::clone(&proxy_shared),
                }),
                current: Arc::new(Mutex::new(None)),
            };
            let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
            let owner = harness.next_replacing_owner(1);
            harness.send(
                owner,
                protected_load("https://music.test/stream?api_key=secret"),
            );
            harness.fence(owner);

            assert!(shared.added_uris().is_empty());
            assert!(proxy_shared
                .tickets
                .lock()
                .expect("proxy tickets lock")
                .is_empty());
            assert!(harness.events().iter().any(|event| matches!(
                event,
                PlayerEvent::Error { message, .. }
                    if !message.contains("secret") && !message.contains("api_key")
            )));
            harness.shutdown();
        }
    }

    #[test]
    fn proxy_start_and_ticket_registration_failures_never_reach_addid() {
        for invalid_ticket in [false, true] {
            let shared = FakeShared::new();
            let proxy_shared = FakeProxyShared::new();
            proxy_shared
                .fail_start
                .store(!invalid_ticket, Ordering::SeqCst);
            proxy_shared
                .invalid_ticket
                .store(invalid_ticket, Ordering::SeqCst);
            let runtime = tokio::runtime::Runtime::new().expect("test runtime");
            let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
            let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
            let owner = harness.next_replacing_owner(1);
            harness.send(
                owner,
                protected_load("https://music.test/stream?session-id=secret"),
            );
            harness.fence(owner);

            assert!(shared.added_uris().is_empty());
            if invalid_ticket {
                assert!(!proxy_shared.active(0));
            } else {
                assert!(proxy_shared
                    .tickets
                    .lock()
                    .expect("proxy tickets lock")
                    .is_empty());
            }
            harness.shutdown();
        }
    }

    #[test]
    fn ticket_survives_pause_seek_and_restartable_remote_stop_but_not_user_stop() {
        let shared = FakeShared::new();
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
        let owner = harness.next_replacing_owner(1);
        harness.send(
            owner,
            protected_load("https://music.test/stream?api_key=secret"),
        );
        harness.fence(owner);
        assert!(proxy_shared.active(0));

        harness.send(owner, CommandKind::Pause);
        harness.fence(owner);
        assert!(proxy_shared.active(0));
        harness.send(owner, CommandKind::Seek(7_000));
        harness.fence(owner);
        assert!(proxy_shared.active(0));

        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(stopped_status(7_000, 10_000));
        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);
        assert!(proxy_shared.active(0));

        let stop_owner = harness.next_replacing_owner(2);
        assert!(
            !proxy_shared.active(0),
            "Stop revokes before worker cleanup"
        );
        harness.send(stop_owner, CommandKind::Stop);
        harness.fence(stop_owner);
        harness.shutdown();
    }

    #[test]
    fn protected_ticket_is_revoked_on_add_play_status_and_control_failures() {
        for failure_point in [Point::Add, Point::Play, Point::Status] {
            let shared = FakeShared::new();
            *shared.fail_at.lock().expect("failure lock") = Some(failure_point);
            let proxy_shared = FakeProxyShared::new();
            let runtime = tokio::runtime::Runtime::new().expect("test runtime");
            let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
            let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
            let owner = harness.next_replacing_owner(1);
            harness.send(
                owner,
                protected_load("https://music.test/stream?api_key=secret"),
            );
            harness.fence(owner);

            assert!(!proxy_shared.active(0), "failure at {failure_point:?}");
            harness.shutdown();
        }

        let shared = FakeShared::new();
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
        let owner = harness.next_replacing_owner(1);
        harness.send(
            owner,
            protected_load("https://music.test/stream?api_key=secret"),
        );
        harness.fence(owner);
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Seek);
        harness.send(owner, CommandKind::Seek(5_000));
        harness.fence(owner);

        assert!(!proxy_shared.active(0));
        harness.shutdown();
    }

    #[test]
    fn replacement_load_revokes_the_old_ticket_before_the_new_command_runs() {
        let shared = FakeShared::new();
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
        let old_owner = harness.next_replacing_owner(1);
        harness.send(
            old_owner,
            protected_load("https://music.test/stream?api_key=old-secret"),
        );
        harness.fence(old_owner);
        assert!(proxy_shared.active(0));

        let new_owner = harness.next_replacing_owner(2);
        assert!(!proxy_shared.active(0));
        harness.send(
            new_owner,
            CommandKind::Load {
                uri: "https://radio.test/live.mp3".to_string(),
            },
        );
        harness.fence(new_owner);
        assert_eq!(
            proxy_shared
                .tickets
                .lock()
                .expect("proxy tickets lock")
                .len(),
            1
        );
        harness.shutdown();
    }

    #[test]
    fn natural_eos_and_remote_ownership_loss_revoke_protected_tickets() {
        for ownership_loss in [false, true] {
            let shared = FakeShared::new();
            let proxy_shared = FakeProxyShared::new();
            let runtime = tokio::runtime::Runtime::new().expect("test runtime");
            let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
            let harness = Harness::new_with_proxy(Arc::clone(&shared), proxy);
            let owner = harness.next_replacing_owner(1);
            harness.send(
                owner,
                protected_load("https://music.test/stream?api_key=secret"),
            );
            harness.fence(owner);

            let terminal = if ownership_loss {
                let mut foreign = playing_status(1_000, 10_000);
                foreign.song_id = Some(99);
                foreign
            } else {
                let mut ended = stopped_status(10_000, 10_000);
                ended.song_id = None;
                ended
            };
            shared
                .statuses
                .lock()
                .expect("statuses lock")
                .push_back(terminal);
            harness.send(owner, CommandKind::PollNow);
            harness.fence(owner);

            assert!(!proxy_shared.active(0));
            harness.shutdown();
        }
    }

    #[test]
    fn stale_ticket_drop_cannot_revoke_a_newer_generation_ticket() {
        let proxy_shared = FakeProxyShared::new();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let proxy = fake_proxy_services(Arc::clone(&proxy_shared), &runtime);
        let epoch = AtomicU64::new(1);
        let old_owner = CommandOwner {
            epoch: 1,
            event_generation: PlayerEventGeneration::from_raw(1),
        };
        let old_upstream = MpdUpstream::Legacy(Box::new(
            Url::parse("https://music.test/old?api_key=secret").expect("old URL"),
        ));
        let old = proxy
            .start_ticket(
                old_owner,
                SocketAddr::from((Ipv4Addr::LOCALHOST, 50_001)),
                &old_upstream,
                &epoch,
            )
            .expect("old ticket");
        epoch.store(2, Ordering::SeqCst);
        let new_owner = CommandOwner {
            epoch: 2,
            event_generation: PlayerEventGeneration::from_raw(2),
        };
        let new_upstream = MpdUpstream::Legacy(Box::new(
            Url::parse("https://music.test/new?api_key=secret").expect("new URL"),
        ));
        let new = proxy
            .start_ticket(
                new_owner,
                SocketAddr::from((Ipv4Addr::LOCALHOST, 50_002)),
                &new_upstream,
                &epoch,
            )
            .expect("new ticket");

        assert!(!proxy_shared.active(0));
        assert!(proxy_shared.active(1));
        drop(old);
        assert!(proxy_shared.active(1));
        assert_eq!(
            proxy
                .current
                .lock()
                .expect("ticket registry lock")
                .as_ref()
                .map(|registered| registered.epoch),
            Some(2)
        );
        drop(new);
        assert!(!proxy_shared.active(1));
    }

    #[test]
    fn load_publishes_authoritative_state_position_and_cache() {
        let shared = FakeShared::new();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(playing_status(1_250, 125_750));
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Connect),
                Action::Point(Point::Repeat),
                Action::Point(Point::Random),
                Action::Point(Point::Single),
                Action::Point(Point::Consume),
                Action::Point(Point::Add),
                Action::Play(42),
                Action::Point(Point::Status),
            ]
        );
        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Buffering,
                    ..
                },
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing,
                    ..
                },
                PlayerEvent::PositionChanged {
                    position_ms: 1_250,
                    duration_ms: 125_750,
                    ..
                }
            ]
        ));
        assert_eq!(harness.cache().state, PlayerState::Playing);
        assert_eq!(harness.cache().position_ms, Some(1_250));
        harness.shutdown();
    }

    #[test]
    fn immediate_unknown_duration_completion_is_not_reported_as_an_error() {
        let shared = FakeShared::new();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(MpdStatus {
                state: MpdPlaybackState::Stopped,
                song_id: None,
                position_ms: None,
                duration_ms: 0,
                has_error: false,
            });
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/short".to_string(),
            },
        );
        harness.fence(owner);

        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Buffering,
                    ..
                },
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::TrackEnded { .. }
            ]
        ));
        assert!(shared.actions().contains(&Action::Delete(42)));
        harness.shutdown();
    }

    #[test]
    fn controls_remain_fifo_and_publish_only_returned_status() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        {
            let mut statuses = shared.statuses.lock().expect("statuses lock");
            statuses.push_back(playing_status(0, 10_000));
            statuses.push_back(paused_status(2_000, 10_000));
            statuses.push_back(paused_status(2_000, 10_000));
            statuses.push_back(paused_status(7_000, 10_000));
            statuses.push_back(paused_status(7_000, 10_000));
            statuses.push_back(playing_status(7_000, 10_000));
        }

        let (entered, release) = shared.install_gate(Point::Pause);
        harness.send(owner, CommandKind::Pause);
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("pause entered");
        harness.send(owner, CommandKind::Seek(7_000));
        harness.send(owner, CommandKind::Play);
        release.send(()).expect("release pause");
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Status),
                Action::Pause(true),
                Action::Point(Point::Status),
                Action::Point(Point::Status),
                Action::Seek(42, 7_000),
                Action::Point(Point::Status),
                Action::Point(Point::Status),
                Action::Pause(false),
                Action::Point(Point::Status),
            ]
        );
        let events = harness.events();
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                state: PlayerState::Paused,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                state: PlayerState::Playing,
                ..
            }
        )));
        assert_eq!(harness.cache().position_ms, Some(7_000));
        harness.shutdown();
    }

    #[test]
    fn every_control_revalidates_and_relinquishes_a_foreign_current_song() {
        for kind in [
            CommandKind::Play,
            CommandKind::Pause,
            CommandKind::Toggle,
            CommandKind::Seek(7_000),
        ] {
            let shared = FakeShared::new();
            let harness = Harness::new(Arc::clone(&shared));
            let owner = harness.next_owner(1);
            harness.send(
                owner,
                CommandKind::Load {
                    uri: "https://music.test/a".to_string(),
                },
            );
            harness.fence(owner);
            shared.clear_actions();
            let _ = harness.events();

            let mut foreign = playing_status(1_000, 10_000);
            foreign.song_id = Some(99);
            shared
                .statuses
                .lock()
                .expect("statuses lock")
                .push_back(foreign);
            harness.send(owner, kind);
            harness.fence(owner);

            assert_eq!(shared.actions(), vec![Action::Point(Point::Status)]);
            assert!(matches!(
                harness.events().as_slice(),
                [
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    },
                    PlayerEvent::Error { .. }
                ]
            ));
            harness.shutdown();
        }
    }

    #[test]
    fn failed_control_cleans_only_the_owned_queue_id() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Seek);

        harness.send(owner, CommandKind::Seek(7_000));
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Status),
                Action::Seek(42, 7_000),
                Action::Delete(42),
            ]
        );
        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::PositionChanged { .. },
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn failed_control_preflight_never_sends_cleanup() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Status);

        harness.send(owner, CommandKind::Pause);
        harness.fence(owner);

        assert_eq!(shared.actions(), vec![Action::Point(Point::Status)]);
        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn superseded_delayed_add_never_plays_or_publishes_the_old_load() {
        let shared = FakeShared::new();
        let (entered, release) = shared.install_gate(Point::Add);
        let harness = Harness::new(Arc::clone(&shared));
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a?token=secret".to_string(),
            },
        );
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("old add entered");
        let second = harness.next_owner(2);
        harness.send(
            second,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
            },
        );
        release.send(()).expect("release old add");
        harness.fence(second);

        assert_eq!(
            shared
                .actions()
                .iter()
                .filter(|action| matches!(action, Action::Play(42)))
                .count(),
            1
        );
        let events = harness.events();
        assert!(!events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Playing }
                if *generation == PlayerEventGeneration::from_raw(1)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Playing }
                if *generation == PlayerEventGeneration::from_raw(2)
        )));
        harness.shutdown();
    }

    #[test]
    fn superseded_poisoned_add_is_dropped_before_new_load_cleanup() {
        let shared = FakeShared::new();
        *shared.poison_at.lock().expect("poison lock") = Some(Point::Add);
        let (entered, release) = shared.install_gate(Point::Add);
        let harness = Harness::new(Arc::clone(&shared));
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("old add entered");
        let second = harness.next_owner(2);
        harness.send(
            second,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
            },
        );
        release.send(()).expect("release poisoned add");
        harness.fence(second);

        let actions = shared.actions();
        let first_add = actions
            .iter()
            .position(|action| *action == Action::Point(Point::Add))
            .expect("first add recorded");
        let replacement_connect = actions
            .iter()
            .enumerate()
            .skip(first_add + 1)
            .find_map(|(index, action)| (*action == Action::Point(Point::Connect)).then_some(index))
            .expect("replacement connected");
        assert!(!actions[first_add + 1..replacement_connect]
            .iter()
            .any(|action| matches!(action, Action::Point(Point::Stop))));

        let events = harness.events();
        assert!(!events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                generation,
                state: PlayerState::Stopped,
            } | PlayerEvent::Error { generation, .. }
                if *generation == PlayerEventGeneration::from_raw(1)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                generation,
                state: PlayerState::Playing,
            } if *generation == PlayerEventGeneration::from_raw(2)
        )));
        harness.shutdown();
    }

    #[test]
    fn explicit_stop_is_authoritative_and_never_emits_eos() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let load = harness.next_owner(1);
        harness.send(
            load,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(load);
        shared.clear_actions();
        let _ = harness.events();

        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        harness.fence(stop);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Status),
                Action::Point(Point::Stop),
                Action::Delete(42),
            ]
        );
        assert!(matches!(
            harness.events().as_slice(),
            [PlayerEvent::StateChanged {
                state: PlayerState::Stopped,
                ..
            }]
        ));
        assert_eq!(harness.cache().position_ms, None);
        harness.shutdown();
    }

    #[test]
    fn explicit_stop_does_not_stop_or_clear_a_foreign_current_song() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let load = harness.next_owner(1);
        harness.send(
            load,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(load);
        shared.clear_actions();
        let _ = harness.events();

        let mut foreign = playing_status(1_000, 10_000);
        foreign.song_id = Some(99);
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(foreign);

        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        harness.fence(stop);

        assert_eq!(shared.actions(), vec![Action::Point(Point::Status)]);
        assert!(matches!(
            harness.events().as_slice(),
            [PlayerEvent::StateChanged {
                state: PlayerState::Stopped,
                ..
            }]
        ));
        harness.shutdown();
    }

    #[test]
    fn explicit_stop_does_not_treat_argument_or_permission_ack_as_absence() {
        for ack_code in [MpdAckCode::Argument, MpdAckCode::Permission] {
            let shared = FakeShared::new();
            let harness = Harness::new(Arc::clone(&shared));
            let load = harness.next_owner(1);
            harness.send(
                load,
                CommandKind::Load {
                    uri: "https://music.test/a".to_string(),
                },
            );
            harness.fence(load);
            shared.clear_actions();
            let _ = harness.events();

            let mut no_current = stopped_status(1_000, 10_000);
            no_current.song_id = None;
            shared
                .statuses
                .lock()
                .expect("statuses lock")
                .push_back(no_current);
            shared
                .delete_results
                .lock()
                .expect("delete results lock")
                .push_back(Err(MpdFailure::ack("queue ownership", Some(ack_code))));

            let stop = harness.next_owner(2);
            harness.send(stop, CommandKind::Stop);
            harness.fence(stop);

            assert_eq!(
                shared.actions(),
                vec![Action::Point(Point::Status), Action::Delete(42)]
            );
            assert!(matches!(
                harness.events().as_slice(),
                [
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    },
                    PlayerEvent::Error { .. }
                ]
            ));
            harness.shutdown();
        }
    }

    #[test]
    fn replacement_load_preserves_the_foreign_queue_and_tolerates_a_missing_old_id() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(first);
        shared.clear_actions();
        let _ = harness.events();
        shared
            .delete_results
            .lock()
            .expect("delete results lock")
            .push_back(Ok(DeleteOutcome::AlreadyAbsent));

        let second = harness.next_owner(2);
        harness.send(
            second,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
            },
        );
        harness.fence(second);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Delete(42),
                Action::Point(Point::Connect),
                Action::Point(Point::Repeat),
                Action::Point(Point::Random),
                Action::Point(Point::Single),
                Action::Point(Point::Consume),
                Action::Point(Point::Add),
                Action::Play(42),
                Action::Point(Point::Status),
            ]
        );
        assert!(shared
            .actions()
            .iter()
            .all(|action| { !matches!(action, Action::Point(Point::Stop)) }));
        assert!(harness.events().iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                generation,
                state: PlayerState::Playing,
            } if *generation == PlayerEventGeneration::from_raw(2)
        )));
        harness.shutdown();
    }

    #[test]
    fn natural_stop_removes_owned_entry_and_emits_eos_exactly_once() {
        let shared = FakeShared::new();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(playing_status(9_000, 10_000));
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        let mut completed = stopped_status(10_000, 10_000);
        completed.song_id = None;
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(completed);

        harness.send(owner, CommandKind::PollNow);
        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);
        let events = harness.events();
        assert!(matches!(
            events.as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::TrackEnded { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn external_next_at_queue_end_shares_natural_completion_semantics() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared.clear_actions();

        // With consume disabled, both natural exhaustion and an external
        // Next at the queue boundary leave no current pointer while retaining
        // our stable id. MPD exposes no discriminator, so both advance the
        // Tributary queue once targeted deletion proves the id still existed.
        let mut externally_advanced = stopped_status(1_000, 10_000);
        externally_advanced.song_id = None;
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(externally_advanced);

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![Action::Point(Point::Status), Action::Delete(42)]
        );
        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::TrackEnded { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn early_remote_stop_remains_restartable_without_eos() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(stopped_status(1_000, 10_000));

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);
        let events = harness.events();
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                state: PlayerState::Stopped,
                ..
            }
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })));
        assert_eq!(harness.cache().state, PlayerState::Stopped);
        assert_eq!(harness.cache().position_ms, Some(1_000));

        shared.clear_actions();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(stopped_status(1_000, 10_000));
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(playing_status(0, 10_000));
        harness.send(owner, CommandKind::Toggle);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Status),
                Action::Play(42),
                Action::Point(Point::Status),
            ]
        );
        assert!(harness.events().iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                state: PlayerState::Playing,
                ..
            }
        )));
        harness.shutdown();
    }

    #[test]
    fn stopped_foreign_song_is_lost_ownership_not_completion() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared.clear_actions();
        let mut foreign = stopped_status(10_000, 10_000);
        foreign.song_id = Some(99);
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(foreign);

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(shared.actions(), vec![Action::Point(Point::Status)]);
        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn foreign_successor_deliberately_retains_owned_entry_without_global_side_effects() {
        let shared = FakeShared::new();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(playing_status(9_500, 10_000));
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared.clear_actions();

        let mut foreign = playing_status(0, 20_000);
        foreign.song_id = Some(99);
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(foreign);
        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        // Neither a global Stop nor even targeted deleteid is conditional on
        // the foreign song remaining current. Retain our orphan rather than
        // race another client's next command.
        assert_eq!(shared.actions(), vec![Action::Point(Point::Status)]);
        let events = harness.events();
        assert!(matches!(
            events.as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        assert!(!events
            .iter()
            .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })));
        harness.shutdown();
    }

    #[test]
    fn external_queue_loss_or_rejected_delete_is_not_completion() {
        for delete_result in [
            Ok(DeleteOutcome::AlreadyAbsent),
            Err(MpdFailure::ack(
                "queue ownership",
                Some(MpdAckCode::Argument),
            )),
            Err(MpdFailure::ack(
                "queue ownership",
                Some(MpdAckCode::Permission),
            )),
        ] {
            let shared = FakeShared::new();
            shared
                .statuses
                .lock()
                .expect("statuses lock")
                .push_back(playing_status(9_500, 10_000));
            let harness = Harness::new(Arc::clone(&shared));
            let owner = harness.next_owner(1);
            harness.send(
                owner,
                CommandKind::Load {
                    uri: "https://music.test/a".to_string(),
                },
            );
            harness.fence(owner);
            let _ = harness.events();
            shared.clear_actions();
            let mut missing = stopped_status(10_000, 10_000);
            missing.song_id = None;
            shared
                .statuses
                .lock()
                .expect("statuses lock")
                .push_back(missing);
            shared
                .delete_results
                .lock()
                .expect("delete results lock")
                .push_back(delete_result);

            harness.send(owner, CommandKind::PollNow);
            harness.fence(owner);

            assert_eq!(
                shared.actions(),
                vec![Action::Point(Point::Status), Action::Delete(42)]
            );
            let events = harness.events();
            assert!(matches!(
                events.as_slice(),
                [
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    },
                    PlayerEvent::Error { .. }
                ]
            ));
            assert!(!events
                .iter()
                .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })));
            harness.shutdown();
        }
    }

    #[test]
    fn foreign_song_error_is_relinquished_without_global_cleanup() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared.clear_actions();
        let mut foreign = playing_status(1_000, 10_000);
        foreign.song_id = Some(99);
        foreign.has_error = true;
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(foreign);

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(shared.actions(), vec![Action::Point(Point::Status)]);
        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn missing_current_id_error_targets_only_the_owned_entry_and_never_emits_eos() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared.clear_actions();
        let mut failed = stopped_status(1_000, 10_000);
        failed.song_id = None;
        failed.has_error = true;
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(failed);

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![Action::Point(Point::Status), Action::Delete(42)]
        );
        let events = harness.events();
        assert!(matches!(
            events.as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        assert!(!events
            .iter()
            .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })));
        harness.shutdown();
    }

    #[test]
    fn missing_current_id_error_without_owned_entry_reports_ownership_loss() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared.clear_actions();
        let mut failed = stopped_status(1_000, 10_000);
        failed.song_id = None;
        failed.has_error = true;
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(failed);
        shared
            .delete_results
            .lock()
            .expect("delete results lock")
            .push_back(Ok(DeleteOutcome::AlreadyAbsent));

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![Action::Point(Point::Status), Action::Delete(42)]
        );
        let events = harness.events();
        let message = events
            .iter()
            .find_map(|event| match event {
                PlayerEvent::Error { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .expect("terminal ownership error");
        assert_eq!(message, "MPD remote playback ownership failed");
        assert!(!events
            .iter()
            .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })));
        harness.shutdown();
    }

    #[test]
    fn status_failure_targets_only_owned_entry_and_emits_one_terminal_error() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Status);

        harness.send(owner, CommandKind::PollNow);
        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![Action::Point(Point::Status), Action::Delete(42)]
        );
        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn ambiguous_status_failure_drops_poisoned_session_without_cleanup_commands() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        *shared.poison_at.lock().expect("poison lock") = Some(Point::Status);

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(shared.actions(), vec![Action::Point(Point::Status)]);
        assert!(matches!(
            harness.events().as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn load_failure_is_buffering_then_stopped_then_url_free_error() {
        let shared = FakeShared::new();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Add);
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a?api_key=secret-token".to_string(),
            },
        );
        harness.fence(owner);
        let events = harness.events();

        assert!(matches!(
            events.as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Buffering,
                    ..
                },
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));
        let message = events
            .iter()
            .find_map(|event| match event {
                PlayerEvent::Error { message, .. } => Some(message),
                _ => None,
            })
            .expect("error message");
        assert!(!message.contains("api_key"));
        assert!(!message.contains("secret-token"));
        harness.shutdown();
    }

    #[test]
    fn stale_terminal_poll_cannot_end_a_newer_load() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(first);
        let _ = harness.events();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(stopped_status(10_000, 10_000));
        let (entered, release) = shared.install_gate(Point::Status);
        harness.send(first, CommandKind::PollNow);
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("old status entered");
        let second = harness.next_owner(2);
        harness.send(
            second,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
            },
        );
        release.send(()).expect("release old status");
        harness.fence(second);

        let events = harness.events();
        assert!(!events.iter().any(|event| matches!(
            event,
            PlayerEvent::TrackEnded { generation }
                if *generation == PlayerEventGeneration::from_raw(1)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Playing }
                if *generation == PlayerEventGeneration::from_raw(2)
        )));
        harness.shutdown();
    }

    #[test]
    fn stale_foreign_status_before_replacement_load_does_not_delete_the_old_id() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(first);
        let _ = harness.events();
        shared.clear_actions();

        let mut foreign = playing_status(1_000, 10_000);
        foreign.song_id = Some(99);
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(foreign);
        let (entered, release) = shared.install_gate(Point::Status);
        harness.send(first, CommandKind::PollNow);
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("old status entered");

        let second = harness.next_owner(2);
        harness.send(
            second,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
            },
        );
        release.send(()).expect("release old status");
        harness.fence(second);

        let actions = shared.actions();
        assert_eq!(actions.first(), Some(&Action::Point(Point::Status)));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::Delete(42))),
            "stale foreign ownership proof must retain the old queue id: {actions:?}"
        );
        assert!(actions.contains(&Action::Point(Point::Connect)));
        assert!(harness.events().iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                generation,
                state: PlayerState::Playing,
            } if *generation == PlayerEventGeneration::from_raw(2)
        )));
        harness.shutdown();
    }

    #[test]
    fn shutdown_cleans_active_session_without_events() {
        let shared = FakeShared::new();
        let mut harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();

        let shutdown = harness.next_owner(2);
        harness.send(shutdown, CommandKind::Shutdown);
        harness
            .worker
            .take()
            .expect("worker handle")
            .join()
            .expect("worker stopped");
        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Status),
                Action::Point(Point::Stop),
                Action::Delete(42),
            ]
        );
        assert!(harness.events().is_empty());
    }

    #[test]
    fn shutdown_retains_owned_entry_when_a_foreign_song_is_current() {
        let shared = FakeShared::new();
        let mut harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        let mut foreign = playing_status(1_000, 10_000);
        foreign.song_id = Some(99);
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(foreign);

        let shutdown = harness.next_owner(2);
        harness.send(shutdown, CommandKind::Shutdown);
        harness
            .worker
            .take()
            .expect("worker handle")
            .join()
            .expect("worker stopped");

        assert_eq!(shared.actions(), vec![Action::Point(Point::Status)]);
        assert!(harness.events().is_empty());
    }

    #[test]
    fn mpd_argument_encoding_rejects_newlines() {
        assert!(encode_mpd_arg("song.flac\ndelete 0").is_err());
        assert!(encode_mpd_arg("song.flac\rstop").is_err());
    }

    #[test]
    fn mpd_argument_encoding_escapes_quotes_and_backslashes() {
        assert_eq!(
            encode_mpd_arg(r#"it's a "test"\song"#).expect("valid argument"),
            r#"it's a \"test\"\\song"#
        );
    }

    #[test]
    fn unconfirmed_public_loads_reject_before_begin_load_and_remain_retryable() {
        let (event_tx, event_rx) = async_channel::unbounded();
        let intent_epoch = Arc::new(AtomicU64::new(0));
        let cache = Arc::new(Mutex::new(MpdCache::default()));
        let (worker_tx, worker_rx) = worker_command_channel(MAX_PENDING_WORKER_COMMANDS);
        let output = MpdOutput {
            display_name: "legacy".to_string(),
            event_tx,
            event_generation: AtomicU64::new(9),
            volume: 1.0,
            control_mode: MpdControlMode::Unconfirmed,
            intent_epoch: Arc::clone(&intent_epoch),
            cache: Arc::clone(&cache),
            proxy: ProxyServices::production(),
            worker_tx,
        };

        assert!(!output.load_uri("file:///music/retry.flac"));
        let request = ResolvedHttpRequest::new(
            Url::parse("https://music.test/retry.flac").expect("resolved endpoint"),
        )
        .expect("active resolved request");
        assert!(!output.load_resolved(request));

        assert_eq!(intent_epoch.load(Ordering::SeqCst), 0);
        let snapshot = *cache.lock().expect("cache lock");
        assert_eq!(snapshot.state, PlayerState::Stopped);
        assert_eq!(snapshot.position_ms, None);
        assert!(
            worker_rx.pop_pending().is_none(),
            "rejected loads never reach the worker"
        );
        for _ in 0..2 {
            assert!(matches!(
                event_rx.try_recv(),
                Ok(PlayerEvent::StateChanged {
                    generation,
                    state: PlayerState::Stopped,
                }) if generation.as_raw() == 9
            ));
            assert!(matches!(
                event_rx.try_recv(),
                Ok(PlayerEvent::Error {
                    generation,
                    message,
                }) if generation.as_raw() == 9
                    && message == mpd_exclusive_control_required_message(&rust_i18n::locale())
            ));
        }
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn load_resets_cached_track_before_the_worker_receives_it() {
        let (event_tx, _event_rx) = async_channel::unbounded();
        let intent_epoch = Arc::new(AtomicU64::new(0));
        let cache = Arc::new(Mutex::new(MpdCache {
            state: PlayerState::Playing,
            position_ms: Some(7_000),
        }));
        let (worker_tx, worker_rx) = worker_command_channel(MAX_PENDING_WORKER_COMMANDS);
        let output = MpdOutput {
            display_name: "test".to_string(),
            event_tx,
            event_generation: AtomicU64::new(7),
            volume: 1.0,
            control_mode: MpdControlMode::Exclusive,
            intent_epoch: Arc::clone(&intent_epoch),
            cache: Arc::clone(&cache),
            proxy: ProxyServices::production(),
            worker_tx,
        };

        assert!(output.load_uri("https://music.test/new"));

        let snapshot = *cache.lock().expect("cache lock");
        assert_eq!(snapshot.state, PlayerState::Buffering);
        assert_eq!(snapshot.position_ms, None);
        let command = worker_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("queued load");
        assert_eq!(command.owner.epoch, 1);
        assert_eq!(command.owner.event_generation.as_raw(), 7);
        assert!(matches!(
            command.kind,
            CommandKind::Load { uri } if uri == "https://music.test/new"
        ));
    }

    #[test]
    fn load_uri_separates_protected_rejected_and_direct_media_before_the_worker() {
        let (event_tx, _event_rx) = async_channel::unbounded();
        let intent_epoch = Arc::new(AtomicU64::new(0));
        let cache = Arc::new(Mutex::new(MpdCache::default()));
        let (worker_tx, worker_rx) = worker_command_channel(MAX_PENDING_WORKER_COMMANDS);
        let output = MpdOutput {
            display_name: "test".to_string(),
            event_tx,
            event_generation: AtomicU64::new(1),
            volume: 1.0,
            control_mode: MpdControlMode::Exclusive,
            intent_epoch,
            cache,
            proxy: ProxyServices::production(),
            worker_tx,
        };

        assert!(output.load_uri("https://music.test/stream?api_key=worker-secret"));
        assert!(matches!(
            worker_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("protected command")
                .kind,
            CommandKind::ProtectedLoad { upstream }
                if upstream.as_str().contains("api_key=worker-secret")
        ));

        assert!(output.load_uri("HTTPS://[malformed"));
        assert!(matches!(
            worker_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("rejected command")
                .kind,
            CommandKind::RejectLoad { failure }
                if failure.operation == "media URI validation"
        ));

        assert!(output.load_uri("Albums/Artist/track.flac"));
        assert!(matches!(
            worker_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("direct command")
                .kind,
            CommandKind::Load { uri } if uri == "Albums/Artist/track.flac"
        ));
    }

    #[test]
    fn typed_load_enters_the_ordered_worker_without_serializing_its_endpoint() {
        let (event_tx, _event_rx) = async_channel::unbounded();
        let (worker_tx, worker_rx) = worker_command_channel(MAX_PENDING_WORKER_COMMANDS);
        let output = MpdOutput {
            display_name: "test".to_string(),
            event_tx,
            event_generation: AtomicU64::new(3),
            volume: 1.0,
            control_mode: MpdControlMode::Exclusive,
            intent_epoch: Arc::new(AtomicU64::new(0)),
            cache: Arc::new(Mutex::new(MpdCache::default())),
            proxy: ProxyServices::production(),
            worker_tx,
        };
        let endpoint =
            Url::parse("https://music.test/clean/track.flac?track=42").expect("clean endpoint");
        let request = ResolvedHttpRequest::new(endpoint.clone()).expect("resolved request");

        assert!(output.load_resolved(request));

        let command = worker_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("resolved command");
        assert_eq!(command.owner.epoch, 1);
        assert_eq!(command.owner.event_generation.as_raw(), 3);
        match command.kind {
            CommandKind::ResolvedLoad { request } => {
                assert_eq!(request.endpoint(), &endpoint);
            }
            _ => panic!("typed request must stay typed through the MPD worker boundary"),
        }
    }

    #[test]
    fn status_parses_fractional_and_legacy_time() {
        let mut status = RawStatus::default();
        status.parse_line("state: play").expect("state");
        status.parse_line("songid: 42").expect("song id");
        status.parse_line("elapsed: 1.250").expect("elapsed");
        status.parse_line("duration: 9.750").expect("duration");
        let status = status.finish().expect("valid status");
        assert_eq!(status.position_ms, Some(1_250));
        assert_eq!(status.duration_ms, 9_750);

        let mut fallback = RawStatus::default();
        fallback.parse_line("state: pause").expect("state");
        fallback.parse_line("songid: 7").expect("song id");
        fallback.parse_line("time: 2:11").expect("legacy time");
        let fallback = fallback.finish().expect("valid fallback");
        assert_eq!(fallback.position_ms, Some(2_000));
        assert_eq!(fallback.duration_ms, 11_000);
    }

    #[test]
    fn seconds_parser_is_checked_and_uses_millisecond_precision() {
        assert_eq!(parse_seconds("1", "test").expect("integer"), 1_000);
        assert_eq!(parse_seconds("1.2", "test").expect("tenths"), 1_200);
        assert_eq!(
            parse_seconds("1.2345", "test").expect("sub-millisecond truncation"),
            1_234
        );
        for malformed in ["", ".5", "1.", "1.2.3", "-1", "NaN"] {
            assert!(parse_seconds(malformed, "test").is_err(), "{malformed}");
        }
        assert!(parse_seconds("18446744073709552", "test").is_err());
        assert!(parse_seconds("18446744073709551.999", "test").is_err());
    }

    #[test]
    fn status_rejects_duplicate_authoritative_fields() {
        let mut status = RawStatus::default();
        status.parse_line("state: play").expect("first state");
        assert!(status.parse_line("state: pause").is_err());

        let mut status = RawStatus::default();
        status.parse_line("songid: 42").expect("first id");
        assert!(status.parse_line("songid: 43").is_err());
    }

    #[test]
    fn greeting_requires_the_exact_bounded_protocol_shape() {
        assert_eq!(
            parse_greeting("OK MPD 0.24.0").expect("valid greeting"),
            "OK MPD 0.24.0"
        );
        for invalid in [
            "OK MPD",
            "OK MPD ",
            "OK MPD dev",
            "OK MPD .",
            "OK MPD 0..24",
            "OKAY MPD 0.24.0",
        ] {
            assert!(parse_greeting(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn status_discards_remote_error_text() {
        let mut status = RawStatus::default();
        status.parse_line("state: stop").expect("state");
        status
            .parse_line("error: https://music.test/a?token=secret")
            .expect("error marker");
        let status = status.finish().expect("typed status retained");
        assert!(status.has_error);
        let message = mpd_failure_message(MpdFailure::new("remote playback"));
        assert_eq!(message, "MPD remote playback failed");
        assert!(!message.contains("secret"));
    }

    #[test]
    fn ack_parser_retains_only_official_codes_from_strict_frames() {
        for (raw, expected) in [
            (1, MpdAckCode::NotList),
            (2, MpdAckCode::Argument),
            (3, MpdAckCode::Password),
            (4, MpdAckCode::Permission),
            (5, MpdAckCode::Unknown),
            (50, MpdAckCode::NoExist),
            (51, MpdAckCode::PlaylistMax),
            (52, MpdAckCode::System),
            (53, MpdAckCode::PlaylistLoad),
            (54, MpdAckCode::UpdateAlready),
            (55, MpdAckCode::PlayerSync),
            (56, MpdAckCode::Exist),
        ] {
            let line =
                format!("ACK [{raw}@0] {{deleteid}} https://music.test/a?token=server-secret");
            assert_eq!(
                MpdConnection::parse_ack_code(&line, "deleteid"),
                Some(ParsedMpdAck::Known(expected))
            );
        }

        for malformed_or_mismatched in [
            "ACK",
            "ACK [50@0] {deleteid}",
            "ACK [50@x] {deleteid} missing",
            "ACK [x@0] {deleteid} missing",
            "ACK [50@0] {delete id} missing",
            "ACK [50@1] {deleteid} wrong command index",
            "ACK [50@0] {addid} wrong command",
        ] {
            assert_eq!(
                MpdConnection::parse_ack_code(malformed_or_mismatched, "deleteid"),
                None,
                "{malformed_or_mismatched}"
            );
        }
        for future_code in [
            "ACK [57@0] {deleteid} future code",
            "ACK [65536@0] {deleteid} oversized code",
        ] {
            assert_eq!(
                MpdConnection::parse_ack_code(future_code, "deleteid"),
                Some(ParsedMpdAck::Unknown),
                "{future_code}"
            );
        }
    }

    struct ResolverCallbackDropProbe {
        dropped_tx: mpsc::Sender<std::thread::ThreadId>,
    }

    impl Drop for ResolverCallbackDropProbe {
        fn drop(&mut self) {
            let _ = self.dropped_tx.send(std::thread::current().id());
        }
    }

    fn resolution_scope(
        owner_epoch: u64,
        intent_epoch: &AtomicU64,
        duration: Duration,
    ) -> MpdResolutionScope<'_> {
        MpdResolutionScope {
            owner_epoch,
            intent_epoch,
            deadline: OperationDeadline::after(duration),
        }
    }

    #[test]
    fn numeric_hosts_bypass_dns_and_preserve_raw_or_bracketed_ipv6() {
        let intent_epoch = AtomicU64::new(7);
        let addresses = resolve_mpd_addresses(
            "::1",
            6600,
            7,
            &intent_epoch,
            OperationDeadline::after(Duration::from_secs(1)),
        )
        .expect("IPv6 loopback resolves");
        assert!(addresses.iter().any(SocketAddr::is_ipv6));
        let bracketed = resolve_mpd_addresses(
            "[::1]",
            6600,
            7,
            &intent_epoch,
            OperationDeadline::after(Duration::from_secs(1)),
        )
        .expect("bracketed IPv6 resolves");
        assert_eq!(addresses, bracketed);
    }

    #[test]
    fn resolver_rejects_empty_nul_and_overlong_hosts_before_submission() {
        let intent_epoch = AtomicU64::new(1);
        for host in [
            String::new(),
            "invalid\0host".to_string(),
            "a".repeat(MAX_MPD_RESOLVER_HOST_BYTES + 1),
        ] {
            assert!(resolve_mpd_addresses(
                &host,
                6600,
                1,
                &intent_epoch,
                OperationDeadline::after(Duration::from_secs(1)),
            )
            .is_err());
        }
    }

    #[test]
    fn resolver_preserves_order_deduplicates_and_stops_at_address_cap() {
        let expected = (1..=MAX_RESOLVED_ADDRESSES)
            .map(|last| SocketAddr::from((Ipv4Addr::new(192, 0, 2, last as u8), 6600)))
            .collect::<Vec<_>>();
        let mut addresses = Vec::new();
        assert!(!retain_mpd_address(&mut addresses, expected[0]));
        assert!(!retain_mpd_address(&mut addresses, expected[0]));
        for (index, address) in expected.iter().copied().enumerate().skip(1) {
            assert_eq!(
                retain_mpd_address(&mut addresses, address),
                index + 1 == MAX_RESOLVED_ADDRESSES
            );
        }
        assert_eq!(addresses, expected);
    }

    #[test]
    fn resolver_deadline_cancels_inflight_gio_work() {
        let (_held_result_tx, result_rx) = mpsc::channel();
        let cancellable = gio::Cancellable::new();
        let intent_epoch = AtomicU64::new(1);
        let started = Instant::now();
        let result = wait_for_mpd_resolution(
            result_rx,
            &cancellable,
            resolution_scope(1, &intent_epoch, Duration::from_millis(25)),
        );

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(cancellable.is_cancelled());
    }

    #[test]
    fn resolver_result_disconnect_cancels_inflight_gio_work() {
        let (result_tx, result_rx) = mpsc::channel();
        drop(result_tx);
        let cancellable = gio::Cancellable::new();
        let intent_epoch = AtomicU64::new(1);

        assert!(wait_for_mpd_resolution(
            result_rx,
            &cancellable,
            resolution_scope(1, &intent_epoch, Duration::from_secs(1)),
        )
        .is_err());
        assert!(cancellable.is_cancelled());
    }

    #[test]
    fn newer_epoch_cancels_inflight_resolution_before_deadline() {
        let (_held_result_tx, result_rx) = mpsc::channel();
        let cancellable = gio::Cancellable::new();
        let intent_epoch = AtomicU64::new(1);
        let (replace_tx, replace_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let worker_epoch = &intent_epoch;
            scope.spawn(move || {
                replace_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("resolution wait started");
                worker_epoch.store(2, Ordering::SeqCst);
            });
            let started = Instant::now();
            replace_tx.send(()).expect("replace playback epoch");
            let result = wait_for_mpd_resolution(
                result_rx,
                &cancellable,
                resolution_scope(1, &intent_epoch, Duration::from_secs(5)),
            );
            assert!(result.is_err());
            assert!(started.elapsed() < Duration::from_secs(2));
        });

        assert!(cancellable.is_cancelled());
    }

    #[test]
    fn resolver_service_dispatches_real_gio_on_its_private_context() {
        let service = MpdResolverService::start().expect("resolver service starts");
        let cancellable = gio::Cancellable::new();
        let (result_tx, result_rx) = mpsc::channel();
        service
            .submit(MpdResolverServiceRequest::Resolve(MpdResolverRequest {
                host: "127.0.0.1".to_string(),
                port: 6600,
                cancellable: cancellable.clone(),
                result_tx,
            }))
            .expect("submit numeric GIO enumeration");
        let intent_epoch = AtomicU64::new(1);
        let addresses = wait_for_mpd_resolution(
            result_rx,
            &cancellable,
            resolution_scope(1, &intent_epoch, Duration::from_secs(1)),
        )
        .expect("numeric GIO enumeration succeeds");
        assert_eq!(
            addresses,
            vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 6600))]
        );
        service.shutdown_for_test();
    }

    #[test]
    fn resolver_service_caps_active_work_and_dispatches_under_queued_load() {
        let service = MpdResolverService::start().expect("resolver service starts");
        let (gate_armed_tx, gate_armed_rx) = mpsc::channel();
        let (gate_release_tx, gate_release_rx) = mpsc::channel();
        service
            .submit(MpdResolverServiceRequest::Run(Box::new(
                move |_context, active| {
                    let _ = gate_armed_tx.send(active);
                    let _ = gate_release_rx.recv_timeout(Duration::from_secs(2));
                },
            )))
            .expect("submit service gate");
        assert_eq!(
            gate_armed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("resolver gate armed"),
            0
        );

        let cancelled = gio::Cancellable::new();
        cancelled.cancel();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
        service
            .submit(MpdResolverServiceRequest::Resolve(MpdResolverRequest {
                host: "127.0.0.1".to_string(),
                port: 6600,
                cancellable: cancelled,
                result_tx: cancelled_tx,
            }))
            .expect("submit pre-cancelled resolution");

        let submitted = MAX_ACTIVE_MPD_RESOLUTIONS + 4;
        let mut result_receivers = Vec::new();
        for _ in 0..submitted {
            let (result_tx, result_rx) = mpsc::channel();
            service
                .submit(MpdResolverServiceRequest::Resolve(MpdResolverRequest {
                    host: "127.0.0.1".to_string(),
                    port: 6600,
                    cancellable: gio::Cancellable::new(),
                    result_tx,
                }))
                .expect("submit queued resolution");
            result_receivers.push(result_rx);
        }
        // The first valid operation will occupy a slot, but its final send
        // must not be required to release that slot.
        drop(result_receivers.remove(0));

        let (observed_tx, observed_rx) = mpsc::channel();
        service
            .submit(MpdResolverServiceRequest::Run(Box::new(
                move |_context, active| {
                    let _ = observed_tx.send(active);
                },
            )))
            .expect("submit active-count observation");
        gate_release_tx.send(()).expect("release resolver gate");
        assert_eq!(
            observed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("active count observed before dispatch"),
            MAX_ACTIVE_MPD_RESOLUTIONS
        );

        assert!(cancelled_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pre-cancelled request rejected")
            .is_err());
        let mut succeeded = 0;
        let mut rejected = 0;
        for result_rx in result_receivers {
            match result_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("queued resolution completed")
            {
                Ok(_) => succeeded += 1,
                Err(_) => rejected += 1,
            }
        }
        assert_eq!(succeeded, MAX_ACTIVE_MPD_RESOLUTIONS - 1);
        assert_eq!(rejected, submitted - MAX_ACTIVE_MPD_RESOLUTIONS);

        let drain_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let (active_tx, active_rx) = mpsc::channel();
            service
                .submit(MpdResolverServiceRequest::Run(Box::new(
                    move |_context, active| {
                        let _ = active_tx.send(active);
                    },
                )))
                .expect("query active resolution count");
            if active_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("active count query")
                == 0
            {
                break;
            }
            assert!(
                Instant::now() < drain_deadline,
                "active resolutions drained"
            );
        }

        let (followup_tx, followup_rx) = mpsc::channel();
        service
            .submit(MpdResolverServiceRequest::Resolve(MpdResolverRequest {
                host: "127.0.0.1".to_string(),
                port: 6600,
                cancellable: gio::Cancellable::new(),
                result_tx: followup_tx,
            }))
            .expect("submit follow-up resolution");
        assert!(followup_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("free slot accepts follow-up")
            .is_ok());
        service.shutdown_for_test();
    }

    #[test]
    fn cancelled_service_callback_still_dispatches_and_drops_on_resolver_thread() {
        let service = MpdResolverService::start().expect("resolver service starts");
        let cancellable = gio::Cancellable::new();
        let callback_cancellable = cancellable.clone();
        let (armed_tx, armed_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel::<()>();
        let (dispatched_tx, dispatched_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();

        service
            .submit(MpdResolverServiceRequest::Run(Box::new(
                move |_context, _active| {
                    let service_thread = std::thread::current().id();
                    let drop_probe = ResolverCallbackDropProbe { dropped_tx };
                    let callback_cancel_state = callback_cancellable.clone();
                    let connectable = gio::NetworkAddress::new("127.0.0.1", 6600);
                    connectable.enumerate().next_async(
                        Some(&callback_cancellable),
                        move |_result| {
                            let receiver_was_dropped = result_tx.send(()).is_err();
                            let _ = dispatched_tx.send((
                                std::thread::current().id(),
                                receiver_was_dropped,
                                callback_cancel_state.is_cancelled(),
                            ));
                            let _ = &drop_probe;
                        },
                    );
                    let _ = armed_tx.send(service_thread);
                    // Hold the service before its next context iteration so the
                    // caller can cancel and retire the result receiver first.
                    let _ = release_rx.recv_timeout(Duration::from_secs(1));
                },
            )))
            .expect("submit resolver service test callback");

        let service_thread = armed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("service callback armed");
        drop(result_rx);
        cancellable.cancel();
        release_tx.send(()).expect("release resolver service");
        let (callback_thread, receiver_was_dropped, callback_saw_cancel) = dispatched_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("late callback dispatched");
        assert_eq!(callback_thread, service_thread);
        assert!(receiver_was_dropped);
        assert!(callback_saw_cancel);
        assert_eq!(
            dropped_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("callback resources dropped"),
            service_thread
        );
        service.shutdown_for_test();
    }

    #[test]
    fn gio_inet_addresses_convert_with_ipv6_flowinfo_and_scope() {
        let ipv4: gio::InetSocketAddress = glib::Object::builder()
            .property(
                "address",
                gio::InetAddress::from(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 44))),
            )
            .property("port", 6600_u32)
            .build();
        assert_eq!(
            gio_address_to_socket_addr(ipv4.upcast()).expect("IPv4 GIO address converts"),
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, 44), 6600))
        );

        let ipv6: gio::InetSocketAddress = glib::Object::builder()
            .property(
                "address",
                gio::InetAddress::from(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            )
            .property("port", 6600_u32)
            .property("flowinfo", 0x1234_u32)
            .property("scope-id", 42_u32)
            .build();
        assert_eq!(
            gio_address_to_socket_addr(ipv6.upcast()).expect("IPv6 GIO address converts"),
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 6600, 0x1234, 42,))
        );
    }

    fn scripted_server(steps: Vec<(String, Vec<u8>)>) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake MPD server");
        let address = listener.local_addr().expect("fake server address");
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept MPD client");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("server read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .expect("server write timeout");
            stream
                .write_all(b"OK MPD 0.24.0\n")
                .expect("write MPD greeting");
            let mut reader = BufReader::new(stream.try_clone().expect("clone test stream"));
            for (expected, response) in steps {
                let mut command = String::new();
                reader.read_line(&mut command).expect("read MPD command");
                assert_eq!(command.strip_suffix('\n'), Some(expected.as_str()));
                stream.write_all(&response).expect("write MPD response");
                stream.flush().expect("flush MPD response");
            }
        });
        (address, worker)
    }

    fn read_test_command(reader: &mut BufReader<TcpStream>, expected: &str) {
        let mut command = String::new();
        reader.read_line(&mut command).expect("read MPD command");
        assert_eq!(command.strip_suffix('\n'), Some(expected));
    }

    fn write_test_response(stream: &mut TcpStream, response: &[u8]) {
        stream.write_all(response).expect("write MPD response");
        stream.flush().expect("flush MPD response");
    }

    #[test]
    fn held_ack_keeps_enqueue_nonblocking_and_commands_fifo() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake MPD server");
        let address = listener.local_addr().expect("fake server address");
        let (pause_seen_tx, pause_seen_rx) = mpsc::channel();
        let (commands_queued_tx, commands_queued_rx) = mpsc::channel();
        let (pipeline_checked_tx, pipeline_checked_rx) = mpsc::channel();
        let (release_ack_tx, release_ack_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept MPD client");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("server read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .expect("server write timeout");
            write_test_response(&mut stream, b"OK MPD 0.24.0\n");
            let mut reader = BufReader::new(stream.try_clone().expect("clone test stream"));

            for command in ["repeat 0", "random 0", "single 0", "consume 0"] {
                read_test_command(&mut reader, command);
                write_test_response(&mut stream, b"OK\n");
            }
            read_test_command(&mut reader, "addid \"https://music.test/a\"");
            write_test_response(&mut stream, b"Id: 42\nOK\n");
            read_test_command(&mut reader, "playid 42");
            write_test_response(&mut stream, b"OK\n");
            read_test_command(&mut reader, "status");
            write_test_response(
                &mut stream,
                b"songid: 42\nelapsed: 0.000\nduration: 10.000\nstate: play\nOK\n",
            );

            read_test_command(&mut reader, "status");
            write_test_response(
                &mut stream,
                b"songid: 42\nelapsed: 0.000\nduration: 10.000\nstate: play\nOK\n",
            );
            read_test_command(&mut reader, "pause 1");
            pause_seen_tx.send(()).expect("pause observed");
            commands_queued_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("later commands queued");

            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .expect("short pipeline probe timeout");
            let mut probe = [0_u8; 1];
            match stream.peek(&mut probe) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Ok(0) => panic!("MPD worker disconnected while its ACK was held"),
                Ok(_) => panic!("MPD worker pipelined a later command before the held ACK"),
                Err(error) => panic!("unexpected pipeline probe error: {error}"),
            }
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("restore server read timeout");
            pipeline_checked_tx.send(()).expect("pipeline checked");
            release_ack_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release held ACK");
            write_test_response(&mut stream, b"OK\n");

            read_test_command(&mut reader, "status");
            write_test_response(
                &mut stream,
                b"songid: 42\nelapsed: 0.000\nduration: 10.000\nstate: pause\nOK\n",
            );
            read_test_command(&mut reader, "status");
            write_test_response(
                &mut stream,
                b"songid: 42\nelapsed: 0.000\nduration: 10.000\nstate: pause\nOK\n",
            );
            read_test_command(&mut reader, "seekid 42 7.000");
            write_test_response(&mut stream, b"OK\n");
            read_test_command(&mut reader, "status");
            write_test_response(
                &mut stream,
                b"songid: 42\nelapsed: 7.000\nduration: 10.000\nstate: pause\nOK\n",
            );
            read_test_command(&mut reader, "status");
            write_test_response(
                &mut stream,
                b"songid: 42\nelapsed: 7.000\nduration: 10.000\nstate: pause\nOK\n",
            );
            read_test_command(&mut reader, "pause 0");
            write_test_response(&mut stream, b"OK\n");
            read_test_command(&mut reader, "status");
            write_test_response(
                &mut stream,
                b"songid: 42\nelapsed: 7.000\nduration: 10.000\nstate: play\nOK\n",
            );

            // Shutdown revalidates once. A foreign current id deliberately
            // suppresses global Stop and targeted deletion during teardown.
            read_test_command(&mut reader, "status");
            write_test_response(
                &mut stream,
                b"songid: 99\nelapsed: 0.000\nduration: 10.000\nstate: play\nOK\n",
            );
        });

        let (worker_tx, worker_rx) = worker_command_channel(MAX_PENDING_WORKER_COMMANDS);
        let intent_epoch = Arc::new(AtomicU64::new(1));
        let cache = Arc::new(Mutex::new(MpdCache::default()));
        let (event_tx, _events) = async_channel::unbounded();
        let worker_epoch = Arc::clone(&intent_epoch);
        let worker_cache = Arc::clone(&cache);
        let worker = std::thread::spawn(move || {
            run_mpd_worker(
                MpdTcpConnector {
                    host: address.ip().to_string(),
                    port: address.port(),
                },
                worker_rx,
                MpdControlMode::Exclusive,
                worker_epoch,
                worker_cache,
                event_tx,
                WorkerTiming {
                    operation: Duration::from_secs(2),
                    poll: Duration::from_hours(1),
                    tick: Duration::from_millis(10),
                },
                ProxyServices::production(),
            );
        });
        let owner = queue_test_owner(1);
        assert_eq!(
            worker_tx.enqueue(WorkerCommand {
                owner,
                kind: CommandKind::Load {
                    uri: "https://music.test/a".to_string(),
                },
            }),
            WorkerEnqueueOutcome::Enqueued
        );
        let (loaded_tx, loaded_rx) = mpsc::channel();
        assert_eq!(
            worker_tx.enqueue(WorkerCommand {
                owner,
                kind: CommandKind::Fence(loaded_tx),
            }),
            WorkerEnqueueOutcome::Enqueued
        );
        loaded_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("load reached fence");

        assert_eq!(
            worker_tx.enqueue(WorkerCommand {
                owner,
                kind: CommandKind::Pause,
            }),
            WorkerEnqueueOutcome::Enqueued
        );
        pause_seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server held pause ACK");
        let (done_tx, done_rx) = mpsc::channel();
        let enqueue_started = Instant::now();
        for kind in [
            CommandKind::Seek(7_000),
            CommandKind::Play,
            CommandKind::Fence(done_tx),
        ] {
            assert_eq!(
                worker_tx.enqueue(WorkerCommand { owner, kind }),
                WorkerEnqueueOutcome::Enqueued
            );
        }
        assert_eq!(worker_tx.pending_len(), 3);
        assert!(
            enqueue_started.elapsed() < Duration::from_millis(250),
            "GTK-facing enqueue path waited for the held ACK"
        );
        commands_queued_tx
            .send(())
            .expect("tell server commands are queued");
        pipeline_checked_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server observed no pipelined command");
        release_ack_tx.send(()).expect("release held ACK");
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("queued controls reached fence");
        assert_eq!(cache.lock().expect("cache lock").position_ms, Some(7_000));

        let shutdown = queue_test_owner(2);
        intent_epoch.store(shutdown.epoch, Ordering::SeqCst);
        assert_eq!(
            worker_tx.enqueue(WorkerCommand {
                owner: shutdown,
                kind: CommandKind::Shutdown,
            }),
            WorkerEnqueueOutcome::Enqueued
        );
        worker.join().expect("worker stopped");
        server.join().expect("fake server stopped");
    }

    fn connect_test(address: SocketAddr) -> MpdConnection {
        let intent_epoch = AtomicU64::new(0);
        MpdConnection::connect(
            &address.ip().to_string(),
            address.port(),
            0,
            &intent_epoch,
            OperationDeadline::after(Duration::from_secs(2)),
        )
        .expect("connect to fake MPD server")
    }

    #[test]
    fn tcp_session_reuses_connection_and_parses_typed_responses() {
        let (address, server) = scripted_server(vec![
            ("repeat 0".to_string(), b"OK\n".to_vec()),
            ("random 0".to_string(), b"OK\n".to_vec()),
            ("single 0".to_string(), b"OK\n".to_vec()),
            ("consume 0".to_string(), b"OK\n".to_vec()),
            (
                "addid \"https://music.test/a\"".to_string(),
                b"Id: 42\nOK\n".to_vec(),
            ),
            ("playid 42".to_string(), b"OK\n".to_vec()),
            (
                "status".to_string(),
                b"duration: 125.750\nsongid: 42\nelapsed: 1.250\nstate: play\nOK\n".to_vec(),
            ),
            ("deleteid 42".to_string(), b"OK\n".to_vec()),
        ]);
        let mut connection = connect_test(address);
        let deadline = OperationDeadline::after(Duration::from_secs(2));
        connection.repeat_off(deadline).expect("repeat succeeds");
        connection.random_off(deadline).expect("random succeeds");
        connection.single_off(deadline).expect("single succeeds");
        connection.consume_off(deadline).expect("consume succeeds");
        let id = connection
            .add_id("https://music.test/a", deadline)
            .expect("add succeeds");
        assert_eq!(id, 42);
        connection.play_id(id, deadline).expect("play succeeds");
        let status = connection.status(deadline).expect("status succeeds");
        assert_eq!(status.state, MpdPlaybackState::Playing);
        assert_eq!(status.song_id, Some(42));
        assert_eq!(status.position_ms, Some(1_250));
        assert_eq!(status.duration_ms, 125_750);
        assert_eq!(
            connection
                .delete_id(id, deadline)
                .expect("targeted delete succeeds"),
            DeleteOutcome::Removed
        );
        server.join().expect("fake server stopped");
    }

    #[test]
    fn ack_echoing_credentials_is_an_opaque_failure() {
        let secret_uri = "https://music.test/a?api_key=secret-token";
        let (address, server) = scripted_server(vec![(
            format!("addid \"{secret_uri}\""),
            format!("ACK [50@0] {{addid}} {secret_uri}\n").into_bytes(),
        )]);
        let mut connection = connect_test(address);
        let failure = connection
            .add_id(secret_uri, OperationDeadline::after(Duration::from_secs(2)))
            .expect_err("ACK rejected");
        assert_eq!(failure.ack_code, Some(MpdAckCode::NoExist));
        let debug = format!("{failure:?}");
        let message = mpd_failure_message(failure);
        assert_eq!(message, "MPD media add failed");
        assert!(!debug.contains("api_key"));
        assert!(!debug.contains("secret-token"));
        assert!(!message.contains("api_key"));
        assert!(!message.contains("secret-token"));
        server.join().expect("fake server stopped");
    }

    #[test]
    fn targeted_delete_distinguishes_absence_from_argument_and_permission_rejections() {
        let (address, server) = scripted_server(vec![
            ("deleteid 41".to_string(), b"OK\n".to_vec()),
            (
                "deleteid 42".to_string(),
                b"ACK [50@0] {deleteid} No such song\n".to_vec(),
            ),
            (
                "deleteid 43".to_string(),
                b"ACK [2@0] {deleteid} Bad argument\n".to_vec(),
            ),
            (
                "deleteid 44".to_string(),
                b"ACK [4@0] {deleteid} Permission denied\n".to_vec(),
            ),
            (
                "deleteid 45".to_string(),
                b"ACK [57@0] {deleteid} Future error\n".to_vec(),
            ),
        ]);
        let mut connection = connect_test(address);
        let deadline = OperationDeadline::after(Duration::from_secs(2));

        assert_eq!(
            connection.delete_id(41, deadline).expect("delete succeeds"),
            DeleteOutcome::Removed
        );
        assert_eq!(
            connection
                .delete_id(42, deadline)
                .expect("missing id is classified"),
            DeleteOutcome::AlreadyAbsent
        );
        let argument = connection
            .delete_id(43, deadline)
            .expect_err("argument rejection is not absence");
        assert!(argument.connection_usable);
        assert_eq!(argument.ack_code, Some(MpdAckCode::Argument));
        let permission = connection
            .delete_id(44, deadline)
            .expect_err("permission rejection is not absence");
        assert!(permission.connection_usable);
        assert_eq!(permission.ack_code, Some(MpdAckCode::Permission));
        let unknown = connection
            .delete_id(45, deadline)
            .expect_err("unknown rejection is not absence");
        assert!(unknown.connection_usable);
        assert_eq!(unknown.ack_code, None);
        server.join().expect("fake server stopped");
    }

    #[test]
    fn malformed_or_mismatched_ack_poisons_the_connection() {
        for (song_id, response) in [
            (41, "ACK\n"),
            (42, "ACK [50@1] {deleteid} wrong index\n"),
            (43, "ACK [50@0] {addid} wrong command\n"),
        ] {
            let (address, server) = scripted_server(vec![(
                format!("deleteid {song_id}"),
                response.as_bytes().to_vec(),
            )]);
            let mut connection = connect_test(address);
            let failure = connection
                .delete_id(song_id, OperationDeadline::after(Duration::from_secs(2)))
                .expect_err("uncorrelated ACK must fail closed");
            assert!(!failure.connection_usable);
            assert_eq!(failure.ack_code, None);
            server.join().expect("fake server stopped");
        }
    }

    #[test]
    fn okay_prefix_is_not_a_success_terminator() {
        let (address, server) = scripted_server(vec![("repeat 0".to_string(), b"OKAY\n".to_vec())]);
        let mut connection = connect_test(address);
        assert!(connection
            .repeat_off(OperationDeadline::after(Duration::from_secs(2)))
            .is_err());
        server.join().expect("fake server stopped");
    }

    #[test]
    fn addid_rejects_duplicate_identifiers() {
        let (address, server) = scripted_server(vec![(
            "addid \"https://music.test/a\"".to_string(),
            b"Id: 42\nId: 43\nOK\n".to_vec(),
        )]);
        let mut connection = connect_test(address);
        assert!(connection
            .add_id(
                "https://music.test/a",
                OperationDeadline::after(Duration::from_secs(2)),
            )
            .is_err());
        server.join().expect("fake server stopped");
    }

    #[test]
    fn seek_command_formats_all_u64_milliseconds_exactly() {
        let seconds = u64::MAX / 1000;
        let milliseconds = u64::MAX % 1000;
        let (address, server) = scripted_server(vec![(
            format!("seekid 42 {seconds}.{milliseconds:03}"),
            b"OK\n".to_vec(),
        )]);
        let mut connection = connect_test(address);
        connection
            .seek_id(
                42,
                u64::MAX,
                OperationDeadline::after(Duration::from_secs(2)),
            )
            .expect("seek succeeds");
        server.join().expect("fake server stopped");
    }

    #[test]
    fn oversized_protocol_line_is_rejected() {
        let mut response = vec![b'x'; MAX_LINE_BYTES + 1];
        response.push(b'\n');
        let (address, server) = scripted_server(vec![("status".to_string(), response)]);
        let mut connection = connect_test(address);
        assert!(connection
            .status(OperationDeadline::after(Duration::from_secs(2)))
            .is_err());
        server.join().expect("fake server stopped");
    }

    #[test]
    fn response_line_count_is_bounded() {
        let response = b"volume: 1\n".repeat(MAX_RESPONSE_LINES + 1);
        let (address, server) = scripted_server(vec![("status".to_string(), response)]);
        let mut connection = connect_test(address);
        assert!(connection
            .status(OperationDeadline::after(Duration::from_secs(2)))
            .is_err());
        server.join().expect("fake server stopped");
    }

    #[test]
    fn silent_response_respects_absolute_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake MPD server");
        let address = listener.local_addr().expect("fake server address");
        let (command_seen_tx, command_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept MPD client");
            stream.write_all(b"OK MPD 0.24.0\n").expect("greeting");
            let mut byte = [0_u8; 1];
            while byte[0] != b'\n' {
                stream.read_exact(&mut byte).expect("read command byte");
            }
            command_seen_tx.send(()).expect("command observed");
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        let mut connection = connect_test(address);
        let started = Instant::now();
        assert!(connection
            .repeat_off(OperationDeadline::after(Duration::from_millis(75)))
            .is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        command_seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server saw command");
        release_tx.send(()).expect("release server");
        server.join().expect("fake server stopped");
    }

    #[test]
    fn trickling_response_cannot_extend_the_absolute_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake MPD server");
        let address = listener.local_addr().expect("fake server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept MPD client");
            stream.write_all(b"OK MPD 0.24.0\n").expect("greeting");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut command = String::new();
            reader.read_line(&mut command).expect("read command");
            assert_eq!(command, "status\n");
            for byte in b"volume: 1\n" {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                let _ = stream.flush();
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        let mut connection = connect_test(address);
        let started = Instant::now();
        assert!(connection
            .status(OperationDeadline::after(Duration::from_millis(75)))
            .is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(connection);
        server.join().expect("fake server stopped");
    }

    #[test]
    fn connection_falls_back_to_a_later_resolved_address() {
        let good_listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind good fake server");
        let good_address = good_listener.local_addr().expect("good server address");
        let bad_listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind temporary bad address");
        let bad_address = bad_listener.local_addr().expect("bad address");
        drop(bad_listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = good_listener.accept().expect("accept fallback client");
            stream.write_all(b"OK MPD 0.24.0\n").expect("greeting");
        });
        let connection = MpdConnection::connect_addresses(
            vec![bad_address, good_address],
            OperationDeadline::after(Duration::from_secs(2)),
        )
        .expect("second address connects");
        assert_eq!(connection.version, "OK MPD 0.24.0");
        server.join().expect("fake server stopped");
    }

    #[test]
    fn slow_first_greeting_preserves_budget_for_later_address() {
        let slow_listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind slow fake server");
        let slow_address = slow_listener.local_addr().expect("slow server address");
        let good_listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind good fake server");
        let good_address = good_listener.local_addr().expect("good server address");
        let (slow_accepted_tx, slow_accepted_rx) = mpsc::channel();
        let (release_slow_tx, release_slow_rx) = mpsc::channel();

        let slow_server = std::thread::spawn(move || {
            let (_stream, _) = slow_listener.accept().expect("accept slow client");
            slow_accepted_tx.send(()).expect("report slow client");
            let _ = release_slow_rx.recv_timeout(Duration::from_secs(3));
        });
        let good_server = std::thread::spawn(move || {
            let (mut stream, _) = good_listener.accept().expect("accept fallback client");
            stream.write_all(b"OK MPD 0.24.0\n").expect("greeting");
        });

        let connection = MpdConnection::connect_addresses(
            vec![slow_address, good_address],
            OperationDeadline::after(Duration::from_secs(2)),
        )
        .expect("silent first greeting leaves time for the second address");
        assert_eq!(connection.version, "OK MPD 0.24.0");
        slow_accepted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first address accepted the client");

        release_slow_tx.send(()).expect("release slow server");
        slow_server.join().expect("slow fake server stopped");
        good_server.join().expect("good fake server stopped");
    }

    #[test]
    fn real_ipv6_loopback_connects_and_reads_greeting_when_available() {
        let Ok(listener) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            // IPv6 can be disabled by the test host or container. Once this
            // capability check succeeds, every later assertion is mandatory.
            return;
        };
        let address = listener.local_addr().expect("IPv6 server address");
        assert!(address.is_ipv6());
        let server = std::thread::spawn(move || {
            let (mut stream, peer) = listener.accept().expect("accept IPv6 client");
            assert!(peer.is_ipv6());
            stream.write_all(b"OK MPD 0.24.0\n").expect("greeting");
        });

        let connection = connect_test(address);
        assert_eq!(connection.version, "OK MPD 0.24.0");
        assert!(connection
            .local_addr()
            .expect("IPv6 client address")
            .is_ipv6());
        server.join().expect("IPv6 fake server stopped");
    }
}
