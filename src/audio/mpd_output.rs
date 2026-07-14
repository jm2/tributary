//! MPD audio output using one ordered, persistent protocol session.
//!
//! All TCP I/O, commands, and status polling run on one dedicated worker so
//! GTK-facing methods remain non-blocking and MPD effects cannot overtake one
//! another. Protocol reads are bounded by line, response, idle, and absolute
//! operation limits. URI-bearing commands and raw MPD errors are never logged
//! or retained.

#[cfg(test)]
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, error, info};
use url::Url;

use super::cast_http_server::CastHttpServer;
use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerEventGeneration, PlayerState};
use crate::http_security::{classify_media_uri, MediaUriSecurity};

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

pub struct MpdOutput {
    #[allow(dead_code)]
    display_name: String,
    event_tx: async_channel::Sender<PlayerEvent>,
    event_generation: AtomicU64,
    volume: f64,
    intent_epoch: Arc<AtomicU64>,
    cache: Arc<Mutex<MpdCache>>,
    proxy: ProxyServices,
    worker_tx: mpsc::Sender<WorkerCommand>,
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

#[derive(Debug, Clone, Copy)]
struct MpdFailure {
    operation: &'static str,
    connection_usable: bool,
}

impl MpdFailure {
    const fn new(operation: &'static str) -> Self {
        Self {
            operation,
            connection_usable: false,
        }
    }

    const fn synchronized(operation: &'static str) -> Self {
        Self {
            operation,
            connection_usable: true,
        }
    }
}

fn opaque_mpd_failure<E>(operation: &'static str, _error: E) -> MpdFailure {
    MpdFailure::new(operation)
}

fn mpd_failure_message(failure: MpdFailure) -> String {
    format!("MPD {} failed", failure.operation)
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

trait MpdProxyFactory: Send + Sync + 'static {
    fn start(
        &self,
        runtime: &tokio::runtime::Handle,
        local_addr: SocketAddr,
        upstream: &Url,
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
        self.server.revoke_upstreams();
    }
}

impl MpdProxyFactory for CastMpdProxyFactory {
    fn start(
        &self,
        runtime: &tokio::runtime::Handle,
        local_addr: SocketAddr,
        upstream: &Url,
    ) -> MpdResult<Arc<dyn MpdMediaTicket>> {
        let server = runtime
            .block_on(CastHttpServer::start_on(local_addr))
            .map_err(|error| opaque_mpd_failure("media proxy startup", error))?;
        let uri = server.register_upstream(upstream);
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
        upstream: &Url,
        intent_epoch: &AtomicU64,
    ) -> MpdResult<SessionTicket> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|error| opaque_mpd_failure("media proxy runtime", error))?
            .clone()
            .ok_or_else(|| MpdFailure::new("media proxy runtime"))?;
        let ticket = self.factory.start(&runtime, local_addr, upstream)?;
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

trait MpdConnector: Send + 'static {
    type Connection: MpdTransport + 'static;

    fn connect(&mut self, deadline: OperationDeadline) -> MpdResult<Self::Connection>;
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
    fn delete_id(&mut self, song_id: u64, deadline: OperationDeadline) -> MpdResult<()>;
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

    fn connect(&mut self, deadline: OperationDeadline) -> MpdResult<Self::Connection> {
        MpdConnection::connect(&self.host, self.port, deadline)
    }
}

impl MpdConnection {
    fn connect(host: &str, port: u16, deadline: OperationDeadline) -> MpdResult<Self> {
        let addresses = resolve_mpd_addresses(host, port)?;
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
                // ACK is a complete MPD response terminator, so the session
                // remains synchronized even though the command failed.
                return Err(MpdFailure::synchronized(operation));
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

    fn delete_id(&mut self, song_id: u64, deadline: OperationDeadline) -> MpdResult<()> {
        self.response_none(&format!("deleteid {song_id}"), deadline, "queue ownership")
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

fn resolve_mpd_addresses(host: &str, port: u16) -> MpdResult<Vec<SocketAddr>> {
    let host = strip_optional_ipv6_brackets(host);
    let mut addresses = Vec::new();
    for address in (host, port)
        .to_socket_addrs()
        .map_err(|error| opaque_mpd_failure("address resolution", error))?
    {
        if !addresses.contains(&address) {
            addresses.push(address);
            if addresses.len() == MAX_RESOLVED_ADDRESSES {
                break;
            }
        }
    }
    if addresses.is_empty() {
        Err(MpdFailure::new("address resolution"))
    } else {
        Ok(addresses)
    }
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
    Protected(Box<Url>),
}

fn spawn_mpd_worker<C>(
    connector: C,
    intent_epoch: Arc<AtomicU64>,
    cache: Arc<Mutex<MpdCache>>,
    event_tx: async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
    proxy: ProxyServices,
) -> mpsc::Sender<WorkerCommand>
where
    C: MpdConnector,
{
    let (worker_tx, worker_rx) = mpsc::channel();
    let spawn = std::thread::Builder::new()
        .name("mpd-worker".to_string())
        .spawn(move || {
            run_mpd_worker(
                connector,
                worker_rx,
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

fn run_mpd_worker<C>(
    mut connector: C,
    worker_rx: mpsc::Receiver<WorkerCommand>,
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
            Duration::from_secs(3600)
        };
        match worker_rx.recv_timeout(wait) {
            Ok(command) => {
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
                            MpdMedia::Protected(upstream),
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
    let connection = connector.connect(deadline);
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
    if retire_poisoned_if_stale(&status, active, owner, intent_epoch) {
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
    // Closing the client connection is the only safe action after a status
    // proves that the stable queue id no longer belongs to this session.
    // Global stop/clear commands would destroy the foreign replacement queue.
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
    let failure = if require_delete_proof && removed.is_err() {
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
    if retire_poisoned_if_stale(&status, active, owner, intent_epoch) {
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
    if retire_poisoned_if_stale(&status, active, owner, intent_epoch) {
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
    if retire_poisoned_if_stale(&status, active, owner, intent_epoch) {
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
    // atomic deleteid of our retained stable entry is the completion proof.
    // This works for unknown-duration and very short streams too. An external
    // queue clear makes deleteid fail, and a foreign replacement is untouched.
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
        // deleteid is terminal for this old ownership claim: OK removed our
        // entry and ACK/error means it cannot safely authorize global cleanup.
        active.take();
        return;
    }
    if removed.is_err() {
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
            if status
                .as_ref()
                .err()
                .is_none_or(|failure| failure.connection_usable)
            {
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
            // A foreign or missing current id does not authorize a global
            // stop. Targeting our stable queue id below remains safe.
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
        // OK removed the entry and ACK means it was already absent. Either is
        // terminal for the superseded ownership claim, so never restore it.
        return CleanupOutcome::Stale;
    }
    match removed {
        Ok(())
        | Err(MpdFailure {
            connection_usable: true,
            ..
        }) => failure.map_or(CleanupOutcome::Completed, CleanupOutcome::Failed),
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
        event_tx: async_channel::Sender<PlayerEvent>,
    ) -> Self {
        info!(host = %host, port, name = %display_name, "MPD output configured");
        let intent_epoch = Arc::new(AtomicU64::new(0));
        let cache = Arc::new(Mutex::new(MpdCache::default()));
        let proxy = ProxyServices::production();
        let worker_tx = spawn_mpd_worker(
            MpdTcpConnector {
                host: host.to_string(),
                port,
            },
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
        let connection =
            MpdConnection::connect(host, port, OperationDeadline::after(OPERATION_TIMEOUT))
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
        if self.worker_tx.send(WorkerCommand { owner, kind }).is_err()
            && is_current(owner, &self.intent_epoch)
        {
            fail_current(
                owner,
                MpdFailure::new("worker availability"),
                &self.intent_epoch,
                &self.cache,
                &self.event_tx,
            );
        }
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

    fn load_uri(&self, uri: &str) {
        let owner = self.next_owner();
        self.proxy.revoke_before(owner.epoch);
        // Retire the previous track's cached state immediately. The worker may
        // still be finishing bounded cleanup, and callers such as Previous
        // must not apply that old position to the newly selected track.
        {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            cache.state = PlayerState::Buffering;
            cache.position_ms = None;
        }
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
        let _ = self.worker_tx.send(WorkerCommand {
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
        ownership: Mutex<VecDeque<bool>>,
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
                ownership: Mutex::new(VecDeque::new()),
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

        fn connect(&mut self, _deadline: OperationDeadline) -> MpdResult<Self::Connection> {
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

        fn delete_id(&mut self, song_id: u64, _deadline: OperationDeadline) -> MpdResult<()> {
            self.shared
                .record(Point::Ownership, Action::Delete(song_id))?;
            if self
                .shared
                .ownership
                .lock()
                .expect("ownership lock")
                .pop_front()
                .unwrap_or(true)
            {
                Ok(())
            } else {
                Err(MpdFailure::synchronized("queue ownership"))
            }
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
            upstream: &Url,
        ) -> MpdResult<Arc<dyn MpdMediaTicket>> {
            self.shared
                .starts
                .lock()
                .expect("proxy starts lock")
                .push(local_addr);
            self.shared
                .upstreams
                .lock()
                .expect("proxy upstreams lock")
                .push(upstream.as_str().to_string());
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
        tx: mpsc::Sender<WorkerCommand>,
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
                    poll: Duration::from_secs(3600),
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
                    poll: Duration::from_secs(3600),
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
            let (tx, rx) = mpsc::channel();
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
            self.tx
                .send(WorkerCommand { owner, kind })
                .expect("worker command accepted");
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
        let old = proxy
            .start_ticket(
                old_owner,
                SocketAddr::from((Ipv4Addr::LOCALHOST, 50_001)),
                &Url::parse("https://music.test/old?api_key=secret").expect("old URL"),
                &epoch,
            )
            .expect("old ticket");
        epoch.store(2, Ordering::SeqCst);
        let new_owner = CommandOwner {
            epoch: 2,
            event_generation: PlayerEventGeneration::from_raw(2),
        };
        let new = proxy
            .start_ticket(
                new_owner,
                SocketAddr::from((Ipv4Addr::LOCALHOST, 50_002)),
                &Url::parse("https://music.test/new?api_key=secret").expect("new URL"),
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
        shared
            .ownership
            .lock()
            .expect("ownership lock")
            .push_back(false);

        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        harness.fence(stop);

        assert_eq!(
            shared.actions(),
            vec![Action::Point(Point::Status), Action::Delete(42)]
        );
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
            .ownership
            .lock()
            .expect("ownership lock")
            .push_back(false);

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
    fn foreign_successor_is_ownership_loss_not_end_of_track() {
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
    fn missing_song_id_near_end_is_not_attributed_as_completion() {
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
            .ownership
            .lock()
            .expect("ownership lock")
            .push_back(false);

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
            .ownership
            .lock()
            .expect("ownership lock")
            .push_back(false);

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
    fn load_resets_cached_track_before_the_worker_receives_it() {
        let (event_tx, _event_rx) = async_channel::unbounded();
        let intent_epoch = Arc::new(AtomicU64::new(0));
        let cache = Arc::new(Mutex::new(MpdCache {
            state: PlayerState::Playing,
            position_ms: Some(7_000),
        }));
        let (worker_tx, worker_rx) = mpsc::channel();
        let output = MpdOutput {
            display_name: "test".to_string(),
            event_tx,
            event_generation: AtomicU64::new(7),
            volume: 1.0,
            intent_epoch: Arc::clone(&intent_epoch),
            cache: Arc::clone(&cache),
            proxy: ProxyServices::production(),
            worker_tx,
        };

        output.load_uri("https://music.test/new");

        let snapshot = *cache.lock().expect("cache lock");
        assert_eq!(snapshot.state, PlayerState::Buffering);
        assert_eq!(snapshot.position_ms, None);
        let command = worker_rx.recv().expect("queued load");
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
        let (worker_tx, worker_rx) = mpsc::channel();
        let output = MpdOutput {
            display_name: "test".to_string(),
            event_tx,
            event_generation: AtomicU64::new(1),
            volume: 1.0,
            intent_epoch,
            cache,
            proxy: ProxyServices::production(),
            worker_tx,
        };

        output.load_uri("https://music.test/stream?api_key=worker-secret");
        assert!(matches!(
            worker_rx.recv().expect("protected command").kind,
            CommandKind::ProtectedLoad { upstream }
                if upstream.as_str().contains("api_key=worker-secret")
        ));

        output.load_uri("HTTPS://[malformed");
        assert!(matches!(
            worker_rx.recv().expect("rejected command").kind,
            CommandKind::RejectLoad { failure }
                if failure.operation == "media URI validation"
        ));

        output.load_uri("Albums/Artist/track.flac");
        assert!(matches!(
            worker_rx.recv().expect("direct command").kind,
            CommandKind::Load { uri } if uri == "Albums/Artist/track.flac"
        ));
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
    fn raw_ipv6_host_resolves_without_manual_host_port_formatting() {
        let addresses = resolve_mpd_addresses("::1", 6600).expect("IPv6 loopback resolves");
        assert!(addresses.iter().any(SocketAddr::is_ipv6));
        let bracketed = resolve_mpd_addresses("[::1]", 6600).expect("bracketed IPv6 resolves");
        assert_eq!(addresses, bracketed);
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

    fn connect_test(address: SocketAddr) -> MpdConnection {
        MpdConnection::connect(
            &address.ip().to_string(),
            address.port(),
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
        connection
            .delete_id(id, deadline)
            .expect("targeted delete succeeds");
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
        let message = mpd_failure_message(failure);
        assert_eq!(message, "MPD media add failed");
        assert!(!message.contains("api_key"));
        assert!(!message.contains("secret-token"));
        server.join().expect("fake server stopped");
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
}
