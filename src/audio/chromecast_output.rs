//! Chromecast audio output using one ordered Cast V2 worker/session.
//!
//! Chromecast devices are discovered via `_googlecast._tcp.local.` and are
//! controlled with `rust_cast`. The crate's `CastDevice` is deliberately
//! non-`Send`, so it is constructed, retained, used, and dropped entirely on
//! one dedicated OS thread. Every load/control/poll enters that worker's FIFO
//! command stream.

#[cfg(test)]
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{error, info};

use super::cast_http_server::CastHttpServer;
use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerEventGeneration, PlayerState};
use crate::architecture::media::ResolvedHttpRequest;

const HEARTBEAT_INTERVAL_SECS: u64 = 5;
const POSITION_POLL_INTERVAL_SECS: u64 = 1;
const CLEANUP_RETRY_INTERVAL_SECS: u64 = 1;
const MAX_CLEANUP_ATTEMPTS: u8 = 3;
const WORKER_TICK_MS: u64 = 100;

/// Chromecast audio output — streams to a Cast V2 device.
pub struct ChromecastOutput {
    #[allow(dead_code)]
    display_name: String,
    event_tx: async_channel::Sender<PlayerEvent>,
    event_generation: AtomicU64,
    volume: f64,
    current_state: Arc<Mutex<PlayerState>>,
    cast_server: Arc<Mutex<Option<CastHttpServer>>>,
    rt_handle: Option<tokio::runtime::Handle>,
    intent_epoch: Arc<AtomicU64>,
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

// Deliberately not Debug: Load contains credential-bearing media URLs.
enum CommandKind {
    Load {
        uri: String,
        volume: f64,
    },
    RejectLoad {
        failure: CastFailure,
    },
    Play,
    Pause,
    Toggle,
    Stop,
    Seek(u64),
    Volume(f64),
    Shutdown,
    #[cfg(test)]
    PollNow,
    #[cfg(test)]
    Fence(mpsc::Sender<()>),
    #[cfg(test)]
    Hold {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    },
}

#[derive(Clone, Copy)]
struct WorkerTiming {
    heartbeat: Duration,
    poll: Duration,
    cleanup_retry: Duration,
    tick: Duration,
}

impl WorkerTiming {
    fn production() -> Self {
        Self {
            heartbeat: Duration::from_secs(HEARTBEAT_INTERVAL_SECS),
            poll: Duration::from_secs(POSITION_POLL_INTERVAL_SECS),
            cleanup_retry: Duration::from_secs(CLEANUP_RETRY_INTERVAL_SECS),
            tick: Duration::from_millis(WORKER_TICK_MS),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CastFailure {
    operation: &'static str,
}

impl CastFailure {
    const fn new(operation: &'static str) -> Self {
        Self { operation }
    }
}

fn opaque_cast_failure<E>(operation: &'static str, _error: E) -> CastFailure {
    CastFailure::new(operation)
}

type CastResult<T> = Result<T, CastFailure>;

#[derive(Clone)]
struct AppSession {
    transport_id: String,
    session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalReason {
    Finished,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Copy)]
struct CastStatusSnapshot {
    media_session_id: Option<i32>,
    state: Option<PlayerState>,
    position_ms: Option<u64>,
    duration_ms: u64,
    terminal: Option<TerminalReason>,
}

impl CastStatusSnapshot {
    #[cfg(test)]
    const fn loaded(media_session_id: i32) -> Self {
        Self {
            media_session_id: Some(media_session_id),
            state: Some(PlayerState::Playing),
            position_ms: Some(0),
            duration_ms: 0,
            terminal: None,
        }
    }
}

/// Factory moved into the worker. Its transport may remain non-`Send` because
/// it is created and destroyed inside that same worker thread.
trait CastConnector: Send + 'static {
    type Transport: CastTransport + 'static;

    fn connect(&mut self) -> CastResult<Self::Transport>;
}

trait CastTransport {
    fn connect_receiver(&mut self) -> CastResult<()>;
    fn set_volume(&mut self, level: f64) -> CastResult<()>;
    fn launch_receiver(&mut self) -> CastResult<AppSession>;
    fn connect_app(&mut self, app: &AppSession) -> CastResult<()>;
    fn disconnect_app(&mut self, app: &AppSession) -> CastResult<()>;
    fn stop_app(&mut self, app: &AppSession) -> CastResult<()>;
    fn load(&mut self, app: &AppSession, uri: &str) -> CastResult<CastStatusSnapshot>;
    fn play(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()>;
    fn pause(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()>;
    fn seek(&mut self, app: &AppSession, media_session_id: i32, position_ms: u64)
        -> CastResult<()>;
    fn stop(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()>;
    fn heartbeat(&mut self) -> CastResult<()>;
    fn status(&mut self, app: &AppSession, media_session_id: i32)
        -> CastResult<CastStatusSnapshot>;
}

struct RustCastConnector {
    host: String,
    port: u16,
}

struct RustCastTransport {
    device: rust_cast::CastDevice<'static>,
}

impl CastConnector for RustCastConnector {
    type Transport = RustCastTransport;

    fn connect(&mut self) -> CastResult<Self::Transport> {
        // Cast devices use self-signed certificates with no verifiable host.
        // This is inherent to Cast V2 and leaves the control channel exposed
        // to endpoint impersonation on a hostile LAN (tracked in P1.6).
        let device: rust_cast::CastDevice<'static> =
            rust_cast::CastDevice::connect_without_host_verification(self.host.clone(), self.port)
                .map_err(|error| opaque_cast_failure("TLS connection", error))?;
        Ok(RustCastTransport { device })
    }
}

impl CastTransport for RustCastTransport {
    fn connect_receiver(&mut self) -> CastResult<()> {
        self.device
            .connection
            .connect("receiver-0")
            .map_err(|error| opaque_cast_failure("receiver channel connection", error))
    }

    fn set_volume(&mut self, level: f64) -> CastResult<()> {
        self.device
            .receiver
            .set_volume(level.clamp(0.0, 1.0) as f32)
            .map(|_| ())
            .map_err(|error| opaque_cast_failure("volume update", error))
    }

    fn launch_receiver(&mut self) -> CastResult<AppSession> {
        use rust_cast::channels::receiver::CastDeviceApp;

        self.device
            .receiver
            .launch_app(&CastDeviceApp::DefaultMediaReceiver)
            .map(|app| AppSession {
                transport_id: app.transport_id,
                session_id: app.session_id,
            })
            .map_err(|error| opaque_cast_failure("receiver launch", error))
    }

    fn connect_app(&mut self, app: &AppSession) -> CastResult<()> {
        self.device
            .connection
            .connect(app.transport_id.clone())
            .map_err(|error| opaque_cast_failure("application channel connection", error))
    }

    fn disconnect_app(&mut self, app: &AppSession) -> CastResult<()> {
        self.device
            .connection
            .disconnect(app.transport_id.clone())
            .map_err(|error| opaque_cast_failure("application channel disconnection", error))
    }

    fn stop_app(&mut self, app: &AppSession) -> CastResult<()> {
        self.device
            .receiver
            .stop_app(app.session_id.clone())
            .map_err(|error| opaque_cast_failure("receiver application stop", error))
    }

    fn load(&mut self, app: &AppSession, uri: &str) -> CastResult<CastStatusSnapshot> {
        use rust_cast::channels::media::{Media, StreamType};

        let stream_type = if is_live_uri(uri) {
            StreamType::Live
        } else {
            StreamType::Buffered
        };
        let media = Media {
            content_id: uri.to_string(),
            content_type: guess_content_type(uri).to_string(),
            stream_type,
            duration: None,
            metadata: None,
        };
        self.device
            .media
            .load(app.transport_id.clone(), app.session_id.clone(), &media)
            .map(snapshot_from_status)
            .map_err(|error| opaque_cast_failure("media load", error))
    }

    fn play(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()> {
        self.device
            .media
            .play(app.transport_id.clone(), media_session_id)
            .map(|_| ())
            .map_err(|error| opaque_cast_failure("play command", error))
    }

    fn pause(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()> {
        self.device
            .media
            .pause(app.transport_id.clone(), media_session_id)
            .map(|_| ())
            .map_err(|error| opaque_cast_failure("pause command", error))
    }

    fn seek(
        &mut self,
        app: &AppSession,
        media_session_id: i32,
        position_ms: u64,
    ) -> CastResult<()> {
        let seconds = (position_ms as f64 / 1000.0).min(f32::MAX as f64) as f32;
        self.device
            .media
            .seek(
                app.transport_id.clone(),
                media_session_id,
                Some(seconds),
                None,
            )
            .map(|_| ())
            .map_err(|error| opaque_cast_failure("seek command", error))
    }

    fn stop(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()> {
        self.device
            .media
            .stop(app.transport_id.clone(), media_session_id)
            .map(|_| ())
            .map_err(|error| opaque_cast_failure("stop command", error))
    }

    fn heartbeat(&mut self) -> CastResult<()> {
        self.device
            .heartbeat
            .ping()
            .map_err(|error| opaque_cast_failure("heartbeat", error))
    }

    fn status(
        &mut self,
        app: &AppSession,
        media_session_id: i32,
    ) -> CastResult<CastStatusSnapshot> {
        self.device
            .media
            .get_status(app.transport_id.clone(), Some(media_session_id))
            .map(snapshot_from_status)
            .map_err(|error| opaque_cast_failure("status poll", error))
    }
}

fn snapshot_from_status(status: rust_cast::channels::media::Status) -> CastStatusSnapshot {
    use rust_cast::channels::media::{IdleReason, PlayerState as CastPlayerState};

    let Some(entry) = status.entries.first() else {
        return CastStatusSnapshot {
            media_session_id: None,
            state: None,
            position_ms: None,
            duration_ms: 0,
            terminal: Some(TerminalReason::Stopped),
        };
    };

    let state = match entry.player_state {
        CastPlayerState::Playing => Some(PlayerState::Playing),
        CastPlayerState::Paused => Some(PlayerState::Paused),
        CastPlayerState::Buffering => Some(PlayerState::Buffering),
        // An IDLE entry without an idle reason is emitted while a receiver is
        // starting or loading. Treat it as Buffering so a delayed receiver
        // cannot leave Tributary reporting an earlier Playing state.
        CastPlayerState::Idle => Some(PlayerState::Buffering),
    };
    let terminal = match entry.idle_reason {
        Some(IdleReason::Finished) => Some(TerminalReason::Finished),
        Some(IdleReason::Cancelled | IdleReason::Interrupted) => Some(TerminalReason::Stopped),
        Some(IdleReason::Error) => Some(TerminalReason::Error),
        None => None,
    };

    CastStatusSnapshot {
        media_session_id: Some(entry.media_session_id),
        state,
        position_ms: entry.current_time.map(seconds_to_millis),
        duration_ms: entry
            .media
            .as_ref()
            .and_then(|media| media.duration)
            .map(seconds_to_millis)
            .unwrap_or(0),
        terminal,
    }
}

fn seconds_to_millis(seconds: f32) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    (f64::from(seconds) * 1000.0).min(u64::MAX as f64) as u64
}

struct WorkerSession<T> {
    transport: T,
    app: AppSession,
    app_connected: bool,
    media_session_id: Option<i32>,
    owner: CommandOwner,
    state: PlayerState,
    retired: bool,
    last_cleanup_attempt: Option<Instant>,
    cleanup_attempts: u8,
    last_heartbeat: Instant,
    last_poll: Instant,
}

#[derive(Debug, Clone, Copy)]
enum CleanupOutcome {
    Completed,
    Stale,
    Failed(CastFailure),
}

fn spawn_cast_worker<C>(
    connector: C,
    intent_epoch: Arc<AtomicU64>,
    current_state: Arc<Mutex<PlayerState>>,
    event_tx: async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) -> mpsc::Sender<WorkerCommand>
where
    C: CastConnector,
{
    let (worker_tx, worker_rx) = mpsc::channel();
    let spawn = std::thread::Builder::new()
        .name("chromecast-worker".to_string())
        .spawn(move || {
            run_cast_worker(
                connector,
                worker_rx,
                intent_epoch,
                current_state,
                event_tx,
                timing,
            );
        });
    if let Err(spawn_error) = spawn {
        error!(error = %spawn_error, "Failed to spawn Chromecast worker");
    }
    worker_tx
}

fn run_cast_worker<C>(
    mut connector: C,
    worker_rx: mpsc::Receiver<WorkerCommand>,
    intent_epoch: Arc<AtomicU64>,
    current_state: Arc<Mutex<PlayerState>>,
    event_tx: async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) where
    C: CastConnector,
{
    let mut active: Option<WorkerSession<C::Transport>> = None;

    loop {
        let wait = match active.as_ref() {
            Some(session) if session.retired => session
                .last_cleanup_attempt
                .map_or(Duration::ZERO, |last_attempt| {
                    timing.cleanup_retry.saturating_sub(last_attempt.elapsed())
                }),
            Some(session) if session.media_session_id.is_some() => timing.tick,
            _ => Duration::from_secs(3600),
        };
        match worker_rx.recv_timeout(wait) {
            Ok(command) => {
                let poll_after_command = match command.kind {
                    CommandKind::Load { uri, volume } => {
                        handle_load(
                            &mut connector,
                            &mut active,
                            command.owner,
                            uri,
                            volume,
                            &intent_epoch,
                            &current_state,
                            &event_tx,
                        );
                        true
                    }
                    CommandKind::RejectLoad { failure } => {
                        match cleanup_session(&mut active, command.owner, &intent_epoch) {
                            CleanupOutcome::Completed => fail_cast(
                                command.owner,
                                failure,
                                &intent_epoch,
                                &current_state,
                                &event_tx,
                            ),
                            CleanupOutcome::Failed(cleanup_failure) => {
                                let reported = if active.is_some() {
                                    cleanup_failure
                                } else {
                                    error!(
                                        operation = cleanup_failure.operation,
                                        "Previous Chromecast cleanup stage failed after receiver stop"
                                    );
                                    failure
                                };
                                fail_cast(
                                    command.owner,
                                    reported,
                                    &intent_epoch,
                                    &current_state,
                                    &event_tx,
                                );
                            }
                            CleanupOutcome::Stale => {}
                        }
                        true
                    }
                    CommandKind::Stop => {
                        match cleanup_session(&mut active, command.owner, &intent_epoch) {
                            CleanupOutcome::Completed => {
                                set_state_and_emit(
                                    command.owner,
                                    PlayerState::Stopped,
                                    &intent_epoch,
                                    &current_state,
                                    &event_tx,
                                );
                            }
                            CleanupOutcome::Failed(failure) => fail_cast(
                                command.owner,
                                failure,
                                &intent_epoch,
                                &current_state,
                                &event_tx,
                            ),
                            CleanupOutcome::Stale => {}
                        }
                        true
                    }
                    CommandKind::Shutdown => {
                        cleanup_unconditionally(&mut active);
                        break;
                    }
                    #[cfg(test)]
                    CommandKind::PollNow => {
                        poll_active(
                            &mut active,
                            true,
                            &intent_epoch,
                            &current_state,
                            &event_tx,
                            timing,
                        );
                        false
                    }
                    #[cfg(test)]
                    CommandKind::Fence(done) => {
                        let _ = done.send(());
                        false
                    }
                    #[cfg(test)]
                    CommandKind::Hold { entered, release } => {
                        let _ = entered.send(());
                        let _ = release.recv_timeout(Duration::from_secs(2));
                        false
                    }
                    kind => {
                        handle_control(
                            &mut active,
                            command.owner,
                            kind,
                            &intent_epoch,
                            &current_state,
                            &event_tx,
                        );
                        true
                    }
                };
                if active.as_ref().is_some_and(|session| session.retired) {
                    retry_retired_cleanup(&mut active, &intent_epoch, timing);
                } else if poll_after_command {
                    poll_active(
                        &mut active,
                        false,
                        &intent_epoch,
                        &current_state,
                        &event_tx,
                        timing,
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if active.as_ref().is_some_and(|session| session.retired) {
                    retry_retired_cleanup(&mut active, &intent_epoch, timing);
                } else {
                    poll_active(
                        &mut active,
                        false,
                        &intent_epoch,
                        &current_state,
                        &event_tx,
                        timing,
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cleanup_unconditionally(&mut active);
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_load<C>(
    connector: &mut C,
    active: &mut Option<WorkerSession<C::Transport>>,
    owner: CommandOwner,
    uri: String,
    volume: f64,
    intent_epoch: &AtomicU64,
    current_state: &Mutex<PlayerState>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) where
    C: CastConnector,
{
    match cleanup_session(active, owner, intent_epoch) {
        CleanupOutcome::Completed => {}
        CleanupOutcome::Failed(failure) => {
            if active.is_some() {
                fail_cast(owner, failure, intent_epoch, current_state, event_tx);
                return;
            }
            error!(
                operation = failure.operation,
                "Previous Chromecast cleanup stage failed after receiver stop"
            );
        }
        CleanupOutcome::Stale => return,
    }
    if !set_state_and_emit(
        owner,
        PlayerState::Buffering,
        intent_epoch,
        current_state,
        event_tx,
    ) {
        return;
    }

    if !is_current(owner, intent_epoch) {
        return;
    }
    let transport = connector.connect();
    if !is_current(owner, intent_epoch) {
        return;
    }
    let mut transport = match transport {
        Ok(transport) => transport,
        Err(failure) => {
            fail_cast(owner, failure, intent_epoch, current_state, event_tx);
            return;
        }
    };

    if !is_current(owner, intent_epoch) {
        return;
    }
    let result = transport.connect_receiver();
    if !finish_stage(result, owner, intent_epoch, current_state, event_tx) {
        return;
    }

    if !is_current(owner, intent_epoch) {
        return;
    }
    let result = transport.set_volume(volume);
    if !finish_stage(result, owner, intent_epoch, current_state, event_tx) {
        return;
    }

    if !is_current(owner, intent_epoch) {
        return;
    }
    let app = transport.launch_receiver();
    let app = match app {
        Ok(app) => app,
        Err(failure) => {
            if is_current(owner, intent_epoch) {
                fail_cast(owner, failure, intent_epoch, current_state, event_tx);
            }
            return;
        }
    };

    // Record ownership before checking whether launch was superseded. A
    // successful launch creates a receiver app even if its caller became
    // stale while waiting, and the next queued intent must be able to stop it.
    *active = Some(WorkerSession {
        transport,
        app,
        app_connected: false,
        media_session_id: None,
        owner,
        state: PlayerState::Buffering,
        retired: false,
        last_cleanup_attempt: None,
        cleanup_attempts: 0,
        last_heartbeat: Instant::now(),
        last_poll: Instant::now(),
    });

    if !is_current(owner, intent_epoch) {
        return;
    }
    let result = {
        let session = active.as_mut().expect("launched session recorded");
        session.transport.connect_app(&session.app)
    };
    if result.is_ok() {
        active
            .as_mut()
            .expect("launched session recorded")
            .app_connected = true;
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    if let Err(failure) = result {
        cleanup_then_fail(
            active,
            owner,
            failure,
            intent_epoch,
            current_state,
            event_tx,
        );
        return;
    }

    info!(
        content_type = guess_content_type(&uri),
        "Chromecast: loading media"
    );
    if !is_current(owner, intent_epoch) {
        return;
    }
    let loaded = {
        let session = active.as_mut().expect("connected session recorded");
        session.transport.load(&session.app, &uri)
    };
    if let Ok(status) = loaded.as_ref() {
        if let Some(media_session_id) = status.media_session_id {
            active
                .as_mut()
                .expect("connected session recorded")
                .media_session_id = Some(media_session_id);
        }
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(failure) => {
            cleanup_then_fail(
                active,
                owner,
                failure,
                intent_epoch,
                current_state,
                event_tx,
            );
            return;
        }
    };
    if loaded.media_session_id.is_none() {
        cleanup_then_fail(
            active,
            owner,
            CastFailure::new("media session creation"),
            intent_epoch,
            current_state,
            event_tx,
        );
        return;
    }

    let initial_state = loaded.state.unwrap_or(PlayerState::Buffering);
    active.as_mut().expect("loaded session recorded").state = initial_state;

    match loaded.terminal {
        Some(TerminalReason::Finished) => {
            if let Some(session) = active.as_mut() {
                session.media_session_id = None;
            }
            if let CleanupOutcome::Failed(failure) = cleanup_session(active, owner, intent_epoch) {
                error!(operation = failure.operation, "Chromecast cleanup failed");
            }
            if set_state_and_emit(
                owner,
                PlayerState::Stopped,
                intent_epoch,
                current_state,
                event_tx,
            ) {
                emit_if_current(
                    owner,
                    PlayerEvent::ended(owner.event_generation),
                    intent_epoch,
                    event_tx,
                );
            }
            return;
        }
        Some(TerminalReason::Stopped) => {
            if let Some(session) = active.as_mut() {
                session.media_session_id = None;
            }
            cleanup_then_fail(
                active,
                owner,
                CastFailure::new("media startup"),
                intent_epoch,
                current_state,
                event_tx,
            );
            return;
        }
        Some(TerminalReason::Error) => {
            cleanup_then_fail(
                active,
                owner,
                CastFailure::new("media startup"),
                intent_epoch,
                current_state,
                event_tx,
            );
            return;
        }
        None => {}
    }

    if initial_state != PlayerState::Buffering {
        let _ = set_state_and_emit(owner, initial_state, intent_epoch, current_state, event_tx);
    }
    if let Some(position_ms) = loaded.position_ms {
        emit_if_current(
            owner,
            PlayerEvent::position(owner.event_generation, position_ms, loaded.duration_ms),
            intent_epoch,
            event_tx,
        );
    }
}

fn finish_stage<T>(
    result: CastResult<T>,
    owner: CommandOwner,
    intent_epoch: &AtomicU64,
    current_state: &Mutex<PlayerState>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) -> bool {
    if !is_current(owner, intent_epoch) {
        return false;
    }
    match result {
        Ok(_) => true,
        Err(failure) => {
            fail_cast(owner, failure, intent_epoch, current_state, event_tx);
            false
        }
    }
}

fn cleanup_then_fail<T>(
    active: &mut Option<WorkerSession<T>>,
    owner: CommandOwner,
    failure: CastFailure,
    intent_epoch: &AtomicU64,
    current_state: &Mutex<PlayerState>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) where
    T: CastTransport,
{
    if let Some(session) = active.as_mut() {
        session.retired = true;
    }
    let _ = cleanup_session(active, owner, intent_epoch);
    if is_current(owner, intent_epoch) {
        fail_cast(owner, failure, intent_epoch, current_state, event_tx);
    }
}

fn handle_control<T>(
    active: &mut Option<WorkerSession<T>>,
    owner: CommandOwner,
    kind: CommandKind,
    intent_epoch: &AtomicU64,
    current_state: &Mutex<PlayerState>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) where
    T: CastTransport,
{
    if !is_current(owner, intent_epoch) {
        return;
    }
    let Some(session) = active.as_mut() else {
        return;
    };
    if session.owner.epoch != owner.epoch {
        return;
    }
    if session.retired {
        return;
    }
    let Some(media_session_id) = session.media_session_id else {
        return;
    };

    let (result, new_state) = match kind {
        CommandKind::Play => (
            session.transport.play(&session.app, media_session_id),
            Some(PlayerState::Playing),
        ),
        CommandKind::Pause => (
            session.transport.pause(&session.app, media_session_id),
            Some(PlayerState::Paused),
        ),
        CommandKind::Toggle => {
            if matches!(session.state, PlayerState::Playing | PlayerState::Buffering) {
                (
                    session.transport.pause(&session.app, media_session_id),
                    Some(PlayerState::Paused),
                )
            } else {
                (
                    session.transport.play(&session.app, media_session_id),
                    Some(PlayerState::Playing),
                )
            }
        }
        CommandKind::Seek(position_ms) => (
            session
                .transport
                .seek(&session.app, media_session_id, position_ms),
            None,
        ),
        CommandKind::Volume(level) => (session.transport.set_volume(level), None),
        _ => return,
    };

    if !is_current(owner, intent_epoch) {
        return;
    }
    if let Err(failure) = result {
        cleanup_then_fail(
            active,
            owner,
            failure,
            intent_epoch,
            current_state,
            event_tx,
        );
        return;
    }

    if let Some(new_state) = new_state {
        if let Some(session) = active.as_mut() {
            session.state = new_state;
        }
        let _ = set_state_and_emit(owner, new_state, intent_epoch, current_state, event_tx);
    }
}

fn poll_active<T>(
    active: &mut Option<WorkerSession<T>>,
    force: bool,
    intent_epoch: &AtomicU64,
    current_state: &Mutex<PlayerState>,
    event_tx: &async_channel::Sender<PlayerEvent>,
    timing: WorkerTiming,
) where
    T: CastTransport,
{
    let Some(session) = active.as_ref() else {
        return;
    };
    let owner = session.owner;
    if session.retired {
        return;
    }
    let Some(media_session_id) = session.media_session_id else {
        return;
    };
    if !is_current(owner, intent_epoch) {
        return;
    }

    let heartbeat_due = force || session.last_heartbeat.elapsed() >= timing.heartbeat;
    if heartbeat_due {
        let result = active
            .as_mut()
            .expect("active session checked")
            .transport
            .heartbeat();
        if !is_current(owner, intent_epoch) {
            return;
        }
        if let Err(failure) = result {
            cleanup_then_fail(
                active,
                owner,
                failure,
                intent_epoch,
                current_state,
                event_tx,
            );
            return;
        }
        if let Some(session) = active.as_mut() {
            session.last_heartbeat = Instant::now();
        }
    }

    let Some(session) = active.as_ref() else {
        return;
    };
    if !force && session.last_poll.elapsed() < timing.poll {
        return;
    }
    if !is_current(owner, intent_epoch) {
        return;
    }
    let status = {
        let session = active.as_mut().expect("active session checked");
        session.transport.status(&session.app, media_session_id)
    };
    if !is_current(owner, intent_epoch) {
        return;
    }
    let status = match status {
        Ok(status) => status,
        Err(failure) => {
            cleanup_then_fail(
                active,
                owner,
                failure,
                intent_epoch,
                current_state,
                event_tx,
            );
            return;
        }
    };
    if let Some(session) = active.as_mut() {
        session.last_poll = Instant::now();
        if let Some(media_session_id) = status.media_session_id {
            session.media_session_id = Some(media_session_id);
        }
    }

    match status.terminal {
        Some(TerminalReason::Finished) => {
            if let Some(session) = active.as_mut() {
                session.media_session_id = None;
            }
            if let CleanupOutcome::Failed(failure) = cleanup_session(active, owner, intent_epoch) {
                error!(operation = failure.operation, "Chromecast cleanup failed");
            }
            if set_state_and_emit(
                owner,
                PlayerState::Stopped,
                intent_epoch,
                current_state,
                event_tx,
            ) {
                emit_if_current(
                    owner,
                    PlayerEvent::ended(owner.event_generation),
                    intent_epoch,
                    event_tx,
                );
            }
            return;
        }
        Some(TerminalReason::Stopped) => {
            if let Some(session) = active.as_mut() {
                session.media_session_id = None;
            }
            if let CleanupOutcome::Failed(failure) = cleanup_session(active, owner, intent_epoch) {
                error!(operation = failure.operation, "Chromecast cleanup failed");
            }
            let _ = set_state_and_emit(
                owner,
                PlayerState::Stopped,
                intent_epoch,
                current_state,
                event_tx,
            );
            return;
        }
        Some(TerminalReason::Error) => {
            cleanup_then_fail(
                active,
                owner,
                CastFailure::new("remote playback"),
                intent_epoch,
                current_state,
                event_tx,
            );
            return;
        }
        None => {}
    }

    if let Some(position_ms) = status.position_ms {
        emit_if_current(
            owner,
            PlayerEvent::position(owner.event_generation, position_ms, status.duration_ms),
            intent_epoch,
            event_tx,
        );
    }
    if let Some(state) = status.state {
        let changed = active
            .as_ref()
            .is_some_and(|session| session.state != state);
        if changed {
            if let Some(session) = active.as_mut() {
                session.state = state;
            }
            let _ = set_state_and_emit(owner, state, intent_epoch, current_state, event_tx);
        }
    }
}

fn cleanup_session<T>(
    active: &mut Option<WorkerSession<T>>,
    owner: CommandOwner,
    intent_epoch: &AtomicU64,
) -> CleanupOutcome
where
    T: CastTransport,
{
    if !is_current(owner, intent_epoch) {
        return CleanupOutcome::Stale;
    }
    let Some(mut session) = active.take() else {
        return CleanupOutcome::Completed;
    };
    session.retired = true;
    let mut first_failure = None;

    if !is_current(owner, intent_epoch) {
        *active = Some(session);
        return CleanupOutcome::Stale;
    }
    if let Some(media_session_id) = session.media_session_id {
        match session.transport.stop(&session.app, media_session_id) {
            Ok(()) => session.media_session_id = None,
            Err(failure) => first_failure = Some(failure),
        }
        if !is_current(owner, intent_epoch) {
            *active = Some(session);
            return CleanupOutcome::Stale;
        }
    }

    if session.app_connected {
        match session.transport.disconnect_app(&session.app) {
            Ok(()) => session.app_connected = false,
            Err(failure) => {
                first_failure.get_or_insert(failure);
            }
        }
        if !is_current(owner, intent_epoch) {
            *active = Some(session);
            return CleanupOutcome::Stale;
        }
    }

    let app_stop = session.transport.stop_app(&session.app);
    if let Err(failure) = app_stop {
        first_failure.get_or_insert(failure);
        session.last_cleanup_attempt = Some(Instant::now());
        session.cleanup_attempts = session.cleanup_attempts.saturating_add(1);
        if session.cleanup_attempts < MAX_CLEANUP_ATTEMPTS {
            *active = Some(session);
        } else {
            error!(
                attempts = MAX_CLEANUP_ATTEMPTS,
                "Abandoning unreachable Chromecast receiver application"
            );
        }
    }
    if !is_current(owner, intent_epoch) {
        return CleanupOutcome::Stale;
    }

    match first_failure {
        Some(failure) => CleanupOutcome::Failed(failure),
        None => CleanupOutcome::Completed,
    }
}

fn retry_retired_cleanup<T>(
    active: &mut Option<WorkerSession<T>>,
    intent_epoch: &AtomicU64,
    timing: WorkerTiming,
) where
    T: CastTransport,
{
    let Some(session) = active.as_ref() else {
        return;
    };
    if !session.retired {
        return;
    }
    if session
        .last_cleanup_attempt
        .is_some_and(|last_attempt| last_attempt.elapsed() < timing.cleanup_retry)
    {
        return;
    }
    let owner = CommandOwner {
        epoch: intent_epoch.load(Ordering::SeqCst),
        event_generation: session.owner.event_generation,
    };
    let _ = cleanup_session(active, owner, intent_epoch);
}

fn cleanup_unconditionally<T>(active: &mut Option<WorkerSession<T>>)
where
    T: CastTransport,
{
    if let Some(mut session) = active.take() {
        if let Some(media_session_id) = session.media_session_id {
            let _ = session.transport.stop(&session.app, media_session_id);
        }
        if session.app_connected {
            let _ = session.transport.disconnect_app(&session.app);
        }
        let _ = session.transport.stop_app(&session.app);
    }
}

fn fail_cast(
    owner: CommandOwner,
    failure: CastFailure,
    intent_epoch: &AtomicU64,
    current_state: &Mutex<PlayerState>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) {
    fail_message(
        owner,
        cast_failure_message(failure),
        intent_epoch,
        current_state,
        event_tx,
    );
}

fn cast_failure_message(failure: CastFailure) -> String {
    format!("Chromecast {} failed", failure.operation)
}

fn fail_message(
    owner: CommandOwner,
    message: String,
    intent_epoch: &AtomicU64,
    current_state: &Mutex<PlayerState>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) {
    if !is_current(owner, intent_epoch) {
        return;
    }
    error!(operation = %message, "Chromecast operation failed");
    if set_state_and_emit(
        owner,
        PlayerState::Stopped,
        intent_epoch,
        current_state,
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

fn set_state_and_emit(
    owner: CommandOwner,
    state: PlayerState,
    intent_epoch: &AtomicU64,
    current_state: &Mutex<PlayerState>,
    event_tx: &async_channel::Sender<PlayerEvent>,
) -> bool {
    if !is_current(owner, intent_epoch) {
        return false;
    }
    {
        let mut current = current_state.lock().unwrap_or_else(|p| p.into_inner());
        if !is_current(owner, intent_epoch) {
            return false;
        }
        *current = state;
        if !is_current(owner, intent_epoch) {
            *current = PlayerState::Stopped;
            return false;
        }
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

/// How a track URI must be handed to a Cast device.
///
/// Deliberately not `Debug`: `Proxied` holds a credential-bearing URL, and the
/// entire purpose of this type is that the URL never gets printed or sent to a
/// receiver.
#[derive(Clone, PartialEq, Eq)]
enum CastMedia {
    /// A local file. The receiver cannot read `file://`, so serve it over the
    /// LAN HTTP server.
    LocalFile(std::path::PathBuf),
    /// A URI that claims to be a local file but is not a usable path. It must
    /// be rejected, never forwarded — a malformed `file://` URI is not something
    /// to hand a receiver.
    InvalidLocalUri,
    /// A remote stream carrying a credential. Tributary fetches it and hands
    /// the receiver an opaque ticket instead.
    Proxied(Box<url::Url>),
    /// Anything else — internet radio, plain unauthenticated HTTP. There is no
    /// secret to protect, and relaying a live radio stream through this process
    /// would buy nothing.
    Direct(String),
}

/// Decide how a URI reaches the receiver.
///
/// This is the security boundary. A Subsonic, Jellyfin, or Plex stream URL
/// carries the user's token in its query string — and under Subsonic's
/// plaintext auth mode it carries `p=enc:<hex>`, the user's actual *password*,
/// which unlike a token cannot be revoked. Passing such a URL to a Cast device
/// publishes that credential to hardware Tributary does not control, over a LAN
/// it does not control, where it also lands in the device's media session.
fn classify_cast_uri(uri: &str) -> CastMedia {
    // The *declared* scheme is decided before parsing, because parsing can
    // fail. A malformed `file://` URI must still be rejected as a bad local
    // path — falling through to "pass it to the device" would forward a URI we
    // could not even parse.
    //
    // Compared case-insensitively: URL schemes are case-insensitive, and while
    // `Url::parse` normalizes them, this check runs *before* parsing and has to
    // catch `FILE://[bad` too.
    let declares_local_file = uri
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"));

    let Ok(parsed) = url::Url::parse(uri) else {
        return if declares_local_file {
            CastMedia::InvalidLocalUri
        } else {
            // Nothing we can read a credential out of; pass it through as before.
            CastMedia::Direct(uri.to_string())
        };
    };

    if declares_local_file || parsed.scheme() == "file" {
        return match parsed.to_file_path() {
            Ok(path) => CastMedia::LocalFile(path),
            Err(()) => CastMedia::InvalidLocalUri,
        };
    }

    if matches!(parsed.scheme(), "http" | "https")
        && crate::http_security::url_carries_credentials(&parsed)
    {
        return CastMedia::Proxied(Box::new(parsed));
    }

    CastMedia::Direct(uri.to_string())
}

impl ChromecastOutput {
    pub fn new(
        display_name: &str,
        host: &str,
        port: u16,
        event_tx: async_channel::Sender<PlayerEvent>,
        initial_volume: f64,
    ) -> Self {
        info!(host = %host, port, name = %display_name, "Chromecast output configured");
        let current_state = Arc::new(Mutex::new(PlayerState::Stopped));
        let intent_epoch = Arc::new(AtomicU64::new(0));
        let worker_tx = spawn_cast_worker(
            RustCastConnector {
                host: host.to_string(),
                port,
            },
            Arc::clone(&intent_epoch),
            Arc::clone(&current_state),
            event_tx.clone(),
            WorkerTiming::production(),
        );

        Self {
            display_name: display_name.to_string(),
            event_tx,
            event_generation: AtomicU64::new(0),
            volume: initial_volume.clamp(0.0, 1.0),
            current_state,
            cast_server: Arc::new(Mutex::new(None)),
            rt_handle: tokio::runtime::Handle::try_current().ok(),
            intent_epoch,
            worker_tx,
        }
    }

    #[must_use]
    pub fn with_runtime(mut self, handle: tokio::runtime::Handle) -> Self {
        self.rt_handle = Some(handle);
        self
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

    fn enqueue(&self, owner: CommandOwner, kind: CommandKind) -> bool {
        if self.worker_tx.send(WorkerCommand { owner, kind }).is_ok() {
            return true;
        }
        if is_current(owner, &self.intent_epoch) {
            let _ = set_state_and_emit(
                owner,
                PlayerState::Stopped,
                &self.intent_epoch,
                &self.current_state,
                &self.event_tx,
            );
            emit_if_current(
                owner,
                PlayerEvent::error(owner.event_generation, "Chromecast worker unavailable"),
                &self.intent_epoch,
                &self.event_tx,
            );
        }
        false
    }

    /// Resolve a track URI into something it is safe to hand to a Cast device.
    ///
    /// Three cases:
    ///
    /// - `file://` — the receiver cannot read local files, so serve it over the
    ///   LAN HTTP server.
    /// - A credential-bearing `http(s)://` URL — a Subsonic, Jellyfin, or Plex
    ///   stream URL carries the user's token in its query string, and with
    ///   Subsonic's plaintext mode it carries the user's *password*. Handing
    ///   that to the device would publish the credential to hardware we do not
    ///   control. Proxy it instead, so the device only ever sees an opaque
    ///   ticket.
    /// - Anything else (internet radio, plain unauthenticated HTTP) — pass
    ///   through untouched. There is no secret to protect, and relaying a live
    ///   radio stream through this process would buy nothing.
    fn resolve_uri(&self, uri: &str) -> CastResult<String> {
        // Any new load retires the previous credential ticket, whatever the new
        // track turns out to be. Revoking only inside `register_upstream` would
        // leave a ticket alive when a credentialed track is followed by an
        // unauthenticated one (radio, or a local file): the device could keep
        // replaying the protected stream long after playback moved on.
        self.revoke_proxy_tickets();

        match classify_cast_uri(uri) {
            CastMedia::LocalFile(path) => self.resolve_local_file(&path),
            CastMedia::InvalidLocalUri => Err(CastFailure::new("local media URI validation")),
            CastMedia::Proxied(url) => {
                let server = self.ensure_cast_server()?;
                let server = server
                    .as_ref()
                    .ok_or_else(|| CastFailure::new("media proxy startup"))?;
                Ok(server.register_upstream(&url))
            }
            CastMedia::Direct(uri) => Ok(uri),
        }
    }

    /// Put a typed authenticated request behind the LAN proxy unconditionally.
    /// No endpoint string from this path is ever eligible for a direct Cast
    /// device load.
    fn resolve_request(&self, request: ResolvedHttpRequest) -> CastResult<String> {
        self.revoke_proxy_tickets();
        if !request.is_active() {
            return Err(CastFailure::new("media source availability"));
        }
        let server = self.ensure_cast_server()?;
        let server = server
            .as_ref()
            .ok_or_else(|| CastFailure::new("media proxy startup"))?;
        server
            .register_resolved(request)
            .ok_or_else(|| CastFailure::new("media source availability"))
    }

    fn resolve_local_file(&self, file_path: &std::path::Path) -> CastResult<String> {
        let file_path = file_path.to_path_buf();
        // A regular file, not merely something that exists: `file://` parses to
        // the filesystem root, and serving a directory to a receiver is never
        // what the user meant.
        if !file_path.is_file() {
            return Err(CastFailure::new("local media lookup"));
        }
        let file_path = file_path
            .canonicalize()
            .map_err(|error| opaque_cast_failure("local media canonicalization", error))?;

        let server = self.ensure_cast_server()?;
        let server = server
            .as_ref()
            .ok_or_else(|| CastFailure::new("local media server startup"))?;
        Ok(server.register_file(&file_path))
    }

    /// Revoke every credential-bearing ticket the proxy is holding.
    ///
    /// Best-effort: a poisoned lock or an unstarted server means there is
    /// nothing to revoke.
    fn revoke_proxy_tickets(&self) {
        if let Ok(guard) = self.cast_server.lock() {
            if let Some(server) = guard.as_ref() {
                server.revoke_upstreams();
            }
        }
    }

    /// Start the LAN media server if it is not already running.
    fn ensure_cast_server(&self) -> CastResult<std::sync::MutexGuard<'_, Option<CastHttpServer>>> {
        let mut server_guard = self
            .cast_server
            .lock()
            .map_err(|error| opaque_cast_failure("media server state", error))?;

        if server_guard.is_none() {
            let runtime = self
                .rt_handle
                .as_ref()
                .ok_or_else(|| CastFailure::new("media server startup"))?;
            let server = runtime
                .block_on(CastHttpServer::start())
                .map_err(|error| opaque_cast_failure("media server startup", error))?;
            info!(addr = %server.addr(), "Cast HTTP server started");
            *server_guard = Some(server);
        }

        Ok(server_guard)
    }
}

impl AudioOutput for ChromecastOutput {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn output_type(&self) -> OutputType {
        OutputType::Chromecast
    }

    fn supports_volume(&self) -> bool {
        true
    }

    fn load_uri(&self, uri: &str) {
        let owner = self.next_owner();
        let kind = match self.resolve_uri(uri) {
            Ok(uri) => CommandKind::Load {
                uri,
                volume: self.volume,
            },
            Err(failure) => CommandKind::RejectLoad { failure },
        };
        let _ = self.enqueue(owner, kind);
    }

    fn load_resolved(&self, request: ResolvedHttpRequest) {
        let owner = self.next_owner();
        let kind = match self.resolve_request(request) {
            Ok(uri) => CommandKind::Load {
                uri,
                volume: self.volume,
            },
            Err(failure) => CommandKind::RejectLoad { failure },
        };
        let _ = self.enqueue(owner, kind);
    }

    fn set_event_generation(&self, generation: PlayerEventGeneration) {
        self.event_generation
            .store(generation.as_raw(), Ordering::SeqCst);
    }

    fn play(&self) {
        let _ = self.enqueue(self.current_owner(), CommandKind::Play);
    }

    fn pause(&self) {
        let _ = self.enqueue(self.current_owner(), CommandKind::Pause);
    }

    fn stop(&self) {
        // Kill the credential ticket as soon as playback is meant to end. It is
        // not revoked on pause or seek: a Cast device re-fetches with a `Range`
        // header when it seeks, so a ticket has to outlive those.
        self.revoke_proxy_tickets();
        let _ = self.enqueue(self.next_owner(), CommandKind::Stop);
    }

    fn toggle_play_pause(&self) {
        let _ = self.enqueue(self.current_owner(), CommandKind::Toggle);
    }

    fn seek_to(&self, position_ms: u64) {
        let _ = self.enqueue(self.current_owner(), CommandKind::Seek(position_ms));
    }

    fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
        let _ = self.enqueue(self.current_owner(), CommandKind::Volume(self.volume));
    }

    fn volume(&self) -> f64 {
        self.volume
    }

    fn state(&self) -> PlayerState {
        self.current_state
            .lock()
            .map(|state| *state)
            .unwrap_or(PlayerState::Stopped)
    }

    fn position_ms(&self) -> Option<u64> {
        None
    }
}

impl Drop for ChromecastOutput {
    fn drop(&mut self) {
        let owner = self.next_owner();
        let _ = self.worker_tx.send(WorkerCommand {
            owner,
            kind: CommandKind::Shutdown,
        });
    }
}

fn is_live_uri(uri: &str) -> bool {
    uri.contains("/radio/")
        || uri_path_extension(uri).is_some_and(|extension| {
            extension.eq_ignore_ascii_case("m3u8") || extension.eq_ignore_ascii_case("pls")
        })
}

fn uri_path_extension(uri: &str) -> Option<&str> {
    let path = uri.split('?').next().unwrap_or(uri);
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
}

fn guess_content_type(uri: &str) -> &'static str {
    let extension = uri_path_extension(uri).unwrap_or("");
    if extension.eq_ignore_ascii_case("mp3") {
        "audio/mpeg"
    } else if extension.eq_ignore_ascii_case("flac") {
        "audio/flac"
    } else if extension.eq_ignore_ascii_case("ogg") || extension.eq_ignore_ascii_case("oga") {
        "audio/ogg"
    } else if extension.eq_ignore_ascii_case("opus") {
        "audio/opus"
    } else if extension.eq_ignore_ascii_case("wav") {
        "audio/wav"
    } else if extension.eq_ignore_ascii_case("aac") || extension.eq_ignore_ascii_case("m4a") {
        "audio/mp4"
    } else if extension.eq_ignore_ascii_case("aiff") || extension.eq_ignore_ascii_case("aif") {
        "audio/aiff"
    } else if extension.eq_ignore_ascii_case("m3u8") {
        "application/x-mpegURL"
    } else if extension.eq_ignore_ascii_case("pls") {
        "audio/x-scpls"
    } else {
        "audio/mpeg"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Point {
        Connect,
        ReceiverConnect,
        Volume,
        Launch,
        AppConnect,
        AppDisconnect,
        AppStop,
        Load,
        Play,
        Pause,
        Seek,
        Stop,
        Heartbeat,
        Status,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Action {
        Point(Point),
        Seek(u64),
        Stop(i32),
        Volume(f64),
    }

    struct Gate {
        point: Point,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    struct FakeShared {
        actions: Mutex<Vec<Action>>,
        gate: Mutex<Option<Gate>>,
        fail_at: Mutex<Option<Point>>,
        fail_once_at: Mutex<Option<Point>>,
        notification: Mutex<Option<(Point, mpsc::Sender<()>)>>,
        load_statuses: Mutex<VecDeque<CastStatusSnapshot>>,
        statuses: Mutex<VecDeque<CastStatusSnapshot>>,
    }

    impl FakeShared {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                actions: Mutex::new(Vec::new()),
                gate: Mutex::new(None),
                fail_at: Mutex::new(None),
                fail_once_at: Mutex::new(None),
                notification: Mutex::new(None),
                load_statuses: Mutex::new(VecDeque::new()),
                statuses: Mutex::new(VecDeque::new()),
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

        fn record(&self, point: Point, action: Action) -> CastResult<()> {
            self.actions.lock().expect("actions lock").push(action);
            if let Some((notify_point, sender)) = self
                .notification
                .lock()
                .expect("notification lock")
                .as_ref()
            {
                if *notify_point == point {
                    let _ = sender.send(());
                }
            }
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
                    .map_err(|_| CastFailure::new("test gate entry"))?;
                gate.release
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|_| CastFailure::new("test gate release"))?;
            }
            if self.fail_at.lock().expect("failure lock").as_ref() == Some(&point) {
                return Err(CastFailure::new(point.operation()));
            }
            let fail_once = {
                let mut fail_once_at = self.fail_once_at.lock().expect("one-shot failure lock");
                if fail_once_at.as_ref() == Some(&point) {
                    *fail_once_at = None;
                    true
                } else {
                    false
                }
            };
            if fail_once {
                return Err(CastFailure::new(point.operation()));
            }
            Ok(())
        }

        fn notify_on(&self, point: Point) -> mpsc::Receiver<()> {
            let (sender, receiver) = mpsc::channel();
            *self.notification.lock().expect("notification lock") = Some((point, sender));
            receiver
        }

        fn actions(&self) -> Vec<Action> {
            self.actions.lock().expect("actions lock").clone()
        }

        fn clear_actions(&self) {
            self.actions.lock().expect("actions lock").clear();
        }
    }

    impl Point {
        const fn operation(self) -> &'static str {
            match self {
                Self::Connect => "test connect",
                Self::ReceiverConnect => "test receiver connection",
                Self::Volume => "test volume",
                Self::Launch => "test launch",
                Self::AppConnect => "test app connection",
                Self::AppDisconnect => "test app disconnection",
                Self::AppStop => "test app stop",
                Self::Load => "test load",
                Self::Play => "test play",
                Self::Pause => "test pause",
                Self::Seek => "test seek",
                Self::Stop => "test stop",
                Self::Heartbeat => "test heartbeat",
                Self::Status => "test status",
            }
        }
    }

    struct FakeConnector {
        shared: Arc<FakeShared>,
    }

    struct FakeTransport {
        shared: Arc<FakeShared>,
    }

    impl CastConnector for FakeConnector {
        type Transport = FakeTransport;

        fn connect(&mut self) -> CastResult<Self::Transport> {
            self.shared
                .record(Point::Connect, Action::Point(Point::Connect))?;
            Ok(FakeTransport {
                shared: Arc::clone(&self.shared),
            })
        }
    }

    impl CastTransport for FakeTransport {
        fn connect_receiver(&mut self) -> CastResult<()> {
            self.shared.record(
                Point::ReceiverConnect,
                Action::Point(Point::ReceiverConnect),
            )
        }

        fn set_volume(&mut self, level: f64) -> CastResult<()> {
            self.shared.record(Point::Volume, Action::Volume(level))
        }

        fn launch_receiver(&mut self) -> CastResult<AppSession> {
            self.shared
                .record(Point::Launch, Action::Point(Point::Launch))?;
            Ok(AppSession {
                transport_id: "transport".to_string(),
                session_id: "app-session".to_string(),
            })
        }

        fn connect_app(&mut self, _app: &AppSession) -> CastResult<()> {
            self.shared
                .record(Point::AppConnect, Action::Point(Point::AppConnect))
        }

        fn disconnect_app(&mut self, _app: &AppSession) -> CastResult<()> {
            self.shared
                .record(Point::AppDisconnect, Action::Point(Point::AppDisconnect))
        }

        fn stop_app(&mut self, _app: &AppSession) -> CastResult<()> {
            self.shared
                .record(Point::AppStop, Action::Point(Point::AppStop))
        }

        fn load(&mut self, _app: &AppSession, _uri: &str) -> CastResult<CastStatusSnapshot> {
            self.shared
                .record(Point::Load, Action::Point(Point::Load))?;
            Ok(self
                .shared
                .load_statuses
                .lock()
                .expect("load statuses lock")
                .pop_front()
                .unwrap_or_else(|| CastStatusSnapshot::loaded(42)))
        }

        fn play(&mut self, _app: &AppSession, _media_session_id: i32) -> CastResult<()> {
            self.shared.record(Point::Play, Action::Point(Point::Play))
        }

        fn pause(&mut self, _app: &AppSession, _media_session_id: i32) -> CastResult<()> {
            self.shared
                .record(Point::Pause, Action::Point(Point::Pause))
        }

        fn seek(
            &mut self,
            _app: &AppSession,
            _media_session_id: i32,
            position_ms: u64,
        ) -> CastResult<()> {
            self.shared.record(Point::Seek, Action::Seek(position_ms))
        }

        fn stop(&mut self, _app: &AppSession, media_session_id: i32) -> CastResult<()> {
            self.shared
                .record(Point::Stop, Action::Stop(media_session_id))
        }

        fn heartbeat(&mut self) -> CastResult<()> {
            self.shared
                .record(Point::Heartbeat, Action::Point(Point::Heartbeat))
        }

        fn status(
            &mut self,
            _app: &AppSession,
            _media_session_id: i32,
        ) -> CastResult<CastStatusSnapshot> {
            self.shared
                .record(Point::Status, Action::Point(Point::Status))?;
            Ok(self
                .shared
                .statuses
                .lock()
                .expect("statuses lock")
                .pop_front()
                .unwrap_or_else(|| CastStatusSnapshot::loaded(42)))
        }
    }

    struct Harness {
        tx: mpsc::Sender<WorkerCommand>,
        epoch: Arc<AtomicU64>,
        events: async_channel::Receiver<PlayerEvent>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn new(shared: Arc<FakeShared>) -> Self {
            Self::new_with_timing(
                shared,
                WorkerTiming {
                    heartbeat: Duration::from_secs(3600),
                    poll: Duration::from_secs(3600),
                    cleanup_retry: Duration::from_millis(10),
                    tick: Duration::from_millis(10),
                },
            )
        }

        fn new_with_timing(shared: Arc<FakeShared>, timing: WorkerTiming) -> Self {
            let (tx, rx) = mpsc::channel();
            let epoch = Arc::new(AtomicU64::new(0));
            let state = Arc::new(Mutex::new(PlayerState::Stopped));
            let (event_tx, events) = async_channel::unbounded();
            let epoch_for_worker = Arc::clone(&epoch);
            let state_for_worker = Arc::clone(&state);
            let worker = std::thread::spawn(move || {
                run_cast_worker(
                    FakeConnector { shared },
                    rx,
                    epoch_for_worker,
                    state_for_worker,
                    event_tx,
                    timing,
                );
            });
            Self {
                tx,
                epoch,
                events,
                worker: Some(worker),
            }
        }

        fn next_owner(&self, generation: u64) -> CommandOwner {
            CommandOwner {
                epoch: self.epoch.fetch_add(1, Ordering::SeqCst) + 1,
                event_generation: PlayerEventGeneration::from_raw(generation),
            }
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

    #[test]
    fn delayed_load_is_superseded_before_any_later_side_effect() {
        let shared = FakeShared::new();
        let (entered, release) = shared.install_gate(Point::Connect);
        let harness = Harness::new(Arc::clone(&shared));
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a?api_key=secret".to_string(),
                volume: 0.5,
            },
        );
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("first connect entered");
        let second = harness.next_owner(2);
        harness.send(
            second,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
                volume: 0.5,
            },
        );
        release.send(()).expect("release first connect");
        harness.fence(second);

        let actions = shared.actions();
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, Action::Point(Point::Connect)))
                .count(),
            2
        );
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, Action::Point(Point::Load)))
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
    fn stop_waiting_behind_load_uses_the_returned_media_id() {
        let shared = FakeShared::new();
        let (entered, release) = shared.install_gate(Point::Load);
        let harness = Harness::new(Arc::clone(&shared));
        let load = harness.next_owner(1);
        harness.send(
            load,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("load entered");
        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        release.send(()).expect("release load");
        harness.fence(stop);

        assert!(shared.actions().contains(&Action::Stop(42)));
        let events = harness.events();
        assert!(!events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Playing }
                if *generation == PlayerEventGeneration::from_raw(1)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Stopped }
                if *generation == PlayerEventGeneration::from_raw(2)
        )));
        harness.shutdown();
    }

    #[test]
    fn superseded_app_connect_cleans_the_partial_receiver() {
        let shared = FakeShared::new();
        let (entered, release) = shared.install_gate(Point::AppConnect);
        let harness = Harness::new(Arc::clone(&shared));
        let load = harness.next_owner(1);
        harness.send(
            load,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("app connection entered");
        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        release.send(()).expect("release app connection");
        harness.fence(stop);

        let actions = shared.actions();
        assert!(!actions.contains(&Action::Point(Point::Load)));
        assert!(actions.ends_with(&[
            Action::Point(Point::AppDisconnect),
            Action::Point(Point::AppStop),
        ]));
        let events = harness.events();
        assert!(!events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Playing }
                | PlayerEvent::Error { generation, .. }
                if *generation == PlayerEventGeneration::from_raw(1)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Stopped }
                if *generation == PlayerEventGeneration::from_raw(2)
        )));
        harness.shutdown();
    }

    #[test]
    fn superseded_delayed_stop_retains_cleanup_for_the_new_load() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(first);
        shared.clear_actions();
        let _ = harness.events();

        let (entered, release) = shared.install_gate(Point::Stop);
        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("stop entered");
        let next = harness.next_owner(3);
        harness.send(
            next,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
                volume: 0.5,
            },
        );
        release.send(()).expect("release stop");
        harness.fence(next);

        let actions = shared.actions();
        let app_stop = actions
            .iter()
            .position(|action| *action == Action::Point(Point::AppStop))
            .expect("old app stopped");
        let new_load = actions
            .iter()
            .rposition(|action| *action == Action::Point(Point::Load))
            .expect("new media loaded");
        assert!(app_stop < new_load);
        let events = harness.events();
        assert!(!events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Stopped }
                if *generation == PlayerEventGeneration::from_raw(2)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Playing }
                if *generation == PlayerEventGeneration::from_raw(3)
        )));
        harness.shutdown();
    }

    #[test]
    fn controls_remain_fifo_behind_a_delayed_command() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();

        let (entered, release) = shared.install_gate(Point::Pause);
        harness.send(owner, CommandKind::Pause);
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("pause entered");
        harness.send(owner, CommandKind::Seek(7_000));
        harness.send(owner, CommandKind::Volume(0.25));
        harness.send(owner, CommandKind::Play);
        release.send(()).expect("release pause");
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Pause),
                Action::Seek(7_000),
                Action::Volume(0.25),
                Action::Point(Point::Play),
            ]
        );
        harness.shutdown();
    }

    #[test]
    fn current_failure_is_buffering_then_stopped_then_url_free_error() {
        let shared = FakeShared::new();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Launch);
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a?api_key=secret-token".to_string(),
                volume: 0.5,
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
        let rendered = events
            .iter()
            .find_map(|event| match event {
                PlayerEvent::Error { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .expect("error event");
        assert!(!rendered.contains("api_key"));
        assert!(!rendered.contains("secret-token"));
        harness.shutdown();
    }

    #[test]
    fn control_failure_cleans_remote_media_before_error() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Pause);

        harness.send(owner, CommandKind::Pause);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Pause),
                Action::Stop(42),
                Action::Point(Point::AppDisconnect),
                Action::Point(Point::AppStop),
            ]
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
    fn explicit_stop_failure_is_reported_after_best_effort_cleanup() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Stop);

        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        harness.fence(stop);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Stop(42),
                Action::Point(Point::AppDisconnect),
                Action::Point(Point::AppStop),
            ]
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
    fn failed_app_stop_retries_without_post_error_playback_events() {
        let shared = FakeShared::new();
        let harness = Harness::new_with_timing(
            Arc::clone(&shared),
            WorkerTiming {
                heartbeat: Duration::from_secs(3600),
                poll: Duration::from_secs(3600),
                cleanup_retry: Duration::ZERO,
                tick: Duration::from_secs(3600),
            },
        );
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();

        let app_stops = shared.notify_on(Point::AppStop);
        let (app_stop_entered, app_stop_release) = shared.install_gate(Point::AppStop);
        *shared.fail_once_at.lock().expect("one-shot failure lock") = Some(Point::AppStop);
        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        app_stop_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("first app stop entered");
        app_stops
            .recv_timeout(Duration::from_secs(2))
            .expect("first app stop attempted");

        let (hold_entered_tx, hold_entered_rx) = mpsc::channel();
        let (hold_release_tx, hold_release_rx) = mpsc::channel();
        harness.send(
            stop,
            CommandKind::Hold {
                entered: hold_entered_tx,
                release: hold_release_rx,
            },
        );
        app_stop_release.send(()).expect("release first app stop");
        hold_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("queued command entered");
        app_stops
            .try_recv()
            .expect("cleanup retried before queued command");
        hold_release_tx.send(()).expect("release queued command");
        harness.fence(stop);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Stop(42),
                Action::Point(Point::AppDisconnect),
                Action::Point(Point::AppStop),
                Action::Point(Point::AppStop),
            ]
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
    fn permanent_app_stop_failure_does_not_block_a_replacement_load_forever() {
        let shared = FakeShared::new();
        let harness = Harness::new_with_timing(
            Arc::clone(&shared),
            WorkerTiming {
                heartbeat: Duration::from_secs(3600),
                poll: Duration::from_secs(3600),
                cleanup_retry: Duration::ZERO,
                tick: Duration::from_secs(3600),
            },
        );
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(first);
        shared.clear_actions();
        let _ = harness.events();

        let app_stops = shared.notify_on(Point::AppStop);
        *shared.fail_at.lock().expect("failure lock") = Some(Point::AppStop);
        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        for _ in 0..MAX_CLEANUP_ATTEMPTS {
            app_stops
                .recv_timeout(Duration::from_secs(2))
                .expect("bounded app stop attempt");
        }
        harness.fence(stop);
        assert_eq!(
            shared
                .actions()
                .iter()
                .filter(|action| **action == Action::Point(Point::AppStop))
                .count(),
            usize::from(MAX_CLEANUP_ATTEMPTS)
        );
        let stop_events = harness.events();
        assert!(matches!(
            stop_events.as_slice(),
            [
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                },
                PlayerEvent::Error { .. }
            ]
        ));

        let replacement = harness.next_owner(3);
        harness.send(
            replacement,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(replacement);
        assert!(harness.events().iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Playing }
                if *generation == PlayerEventGeneration::from_raw(3)
        )));
        harness.shutdown();
    }

    #[test]
    fn heartbeat_failure_cleans_remote_media_before_error() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Heartbeat);

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Heartbeat),
                Action::Stop(42),
                Action::Point(Point::AppDisconnect),
                Action::Point(Point::AppStop),
            ]
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
    fn status_failure_cleans_remote_media_before_error() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Status);

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Heartbeat),
                Action::Point(Point::Status),
                Action::Stop(42),
                Action::Point(Point::AppDisconnect),
                Action::Point(Point::AppStop),
            ]
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
    fn terminal_error_status_cleans_remote_media_before_error() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        shared.clear_actions();
        let _ = harness.events();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(CastStatusSnapshot {
                terminal: Some(TerminalReason::Error),
                ..CastStatusSnapshot::loaded(42)
            });

        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);

        assert_eq!(
            shared.actions(),
            vec![
                Action::Point(Point::Heartbeat),
                Action::Point(Point::Status),
                Action::Stop(42),
                Action::Point(Point::AppDisconnect),
                Action::Point(Point::AppStop),
            ]
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
    fn initial_buffering_result_does_not_publish_playing() {
        let shared = FakeShared::new();
        shared
            .load_statuses
            .lock()
            .expect("load statuses lock")
            .push_back(CastStatusSnapshot {
                media_session_id: Some(42),
                state: Some(PlayerState::Buffering),
                position_ms: Some(250),
                duration_ms: 1_000,
                terminal: None,
            });
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);

        let events = harness.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    PlayerEvent::StateChanged {
                        state: PlayerState::Buffering,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                state: PlayerState::Playing,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::PositionChanged {
                position_ms: 250,
                duration_ms: 1_000,
                ..
            }
        )));
        harness.shutdown();
    }

    #[test]
    fn terminal_error_load_result_cleans_remote_media() {
        let shared = FakeShared::new();
        shared
            .load_statuses
            .lock()
            .expect("load statuses lock")
            .push_back(CastStatusSnapshot {
                terminal: Some(TerminalReason::Error),
                ..CastStatusSnapshot::loaded(42)
            });
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);

        assert!(shared
            .actions()
            .windows(2)
            .any(|pair| { pair == [Action::Point(Point::Load), Action::Stop(42)] }));
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
                PlayerEvent::Error { .. }
            ]
        ));
        harness.shutdown();
    }

    #[test]
    fn queued_commands_do_not_starve_due_status_polls() {
        let shared = FakeShared::new();
        let (entered, release) = shared.install_gate(Point::Load);
        let harness = Harness::new_with_timing(
            Arc::clone(&shared),
            WorkerTiming {
                heartbeat: Duration::from_secs(3600),
                poll: Duration::ZERO,
                cleanup_retry: Duration::from_millis(10),
                tick: Duration::from_secs(3600),
            },
        );
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("load entered");
        harness.send(owner, CommandKind::Pause);
        harness.send(owner, CommandKind::Seek(7_000));
        release.send(()).expect("release load");
        harness.fence(owner);

        let actions = shared.actions();
        let pause = actions
            .iter()
            .position(|action| *action == Action::Point(Point::Pause))
            .expect("pause action");
        let seek = actions
            .iter()
            .position(|action| *action == Action::Seek(7_000))
            .expect("seek action");
        assert!(actions[pause + 1..seek].contains(&Action::Point(Point::Status)));
        harness.shutdown();
    }

    #[test]
    fn raw_cast_error_context_is_discarded() {
        let failure = opaque_cast_failure(
            "media load",
            "request failed for https://music.test/cast/token-secret?api_key=query-secret",
        );
        let message = cast_failure_message(failure);
        assert_eq!(message, "Chromecast media load failed");
        assert!(!message.contains("token-secret"));
        assert!(!message.contains("query-secret"));
    }

    #[test]
    fn invalid_local_uri_error_is_secret_free() {
        let (event_tx, events) = async_channel::unbounded();
        let output = ChromecastOutput::new("Living Room", "127.0.0.1", 8009, event_tx, 1.0);
        output.load_uri("file://[cast-secret-token");
        let owner = output.current_owner();
        let (done_tx, done_rx) = mpsc::channel();
        assert!(output.enqueue(owner, CommandKind::Fence(done_tx)));
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker reached fence");

        let rendered = std::iter::from_fn(|| events.try_recv().ok())
            .find_map(|event| match event {
                PlayerEvent::Error { message, .. } => Some(message),
                _ => None,
            })
            .expect("error event");
        assert_eq!(rendered, "Chromecast local media URI validation failed");
        assert!(!rendered.contains("cast-secret-token"));
        assert!(!rendered.contains("file://"));
    }

    #[test]
    fn typed_request_resolution_cannot_fall_through_to_a_direct_cast_load() {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let (event_tx, _events) = async_channel::unbounded();
        let output = ChromecastOutput::new("Living Room", "127.0.0.1", 8009, event_tx, 1.0);
        *output.cast_server.lock().expect("cast server lock") = Some(
            runtime
                .block_on(CastHttpServer::start_on(std::net::SocketAddr::from((
                    std::net::Ipv4Addr::LOCALHOST,
                    0,
                ))))
                .expect("loopback cast server"),
        );
        let endpoint = url::Url::parse("https://music.test/clean/track.flac?track=42")
            .expect("clean endpoint");
        let request = ResolvedHttpRequest::new(endpoint.clone()).expect("resolved request");

        let ticket = output.resolve_request(request).expect("proxied request");
        assert_ne!(ticket, endpoint.as_str());
        assert!(ticket.starts_with("http://127.0.0.1:"));
        assert!(!ticket.contains("music.test"));
        assert!(!ticket.contains("track=42"));
    }

    #[test]
    fn stale_finished_poll_cannot_end_a_newer_load() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(first);
        let _ = harness.events();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(CastStatusSnapshot {
                terminal: Some(TerminalReason::Finished),
                ..CastStatusSnapshot::loaded(42)
            });
        let (entered, release) = shared.install_gate(Point::Status);
        harness.send(first, CommandKind::PollNow);
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("status entered");
        let second = harness.next_owner(2);
        harness.send(
            second,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
                volume: 0.5,
            },
        );
        release.send(()).expect("release status");
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
    fn finished_status_emits_track_ended_exactly_once() {
        let shared = FakeShared::new();
        let harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared
            .statuses
            .lock()
            .expect("statuses lock")
            .push_back(CastStatusSnapshot {
                terminal: Some(TerminalReason::Finished),
                ..CastStatusSnapshot::loaded(42)
            });
        harness.send(owner, CommandKind::PollNow);
        harness.send(owner, CommandKind::PollNow);
        harness.fence(owner);
        let events = harness.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
                .count(),
            1
        );
        harness.shutdown();
    }

    #[test]
    fn shutdown_cleans_active_media_without_emitting_events() {
        let shared = FakeShared::new();
        let mut harness = Harness::new(Arc::clone(&shared));
        let owner = harness.next_owner(1);
        harness.send(
            owner,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        harness.fence(owner);
        let _ = harness.events();
        shared.clear_actions();
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
                Action::Stop(42),
                Action::Point(Point::AppDisconnect),
                Action::Point(Point::AppStop),
            ]
        );
        assert!(harness.events().is_empty());
    }

    #[test]
    fn test_chromecast_output_name_type_volume_and_initial_state() {
        let (tx, _rx) = async_channel::unbounded();
        let mut output = ChromecastOutput::new("Living Room", "127.0.0.1", 8009, tx, 1.0);
        assert_eq!(output.name(), "Living Room");
        assert_eq!(output.output_type(), OutputType::Chromecast);
        assert!(output.supports_volume());
        assert_eq!(output.state(), PlayerState::Stopped);
        assert!(output.position_ms().is_none());
        output.set_volume(1.5);
        assert!((output.volume() - 1.0).abs() < f64::EPSILON);
        output.set_volume(-0.5);
        assert!(output.volume().abs() < f64::EPSILON);
    }

    #[test]
    fn test_guess_content_type() {
        assert_eq!(
            guess_content_type("http://example.com/song.mp3"),
            "audio/mpeg"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.flac"),
            "audio/flac"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.ogg"),
            "audio/ogg"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.opus"),
            "audio/opus"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.wav"),
            "audio/wav"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.m4a"),
            "audio/mp4"
        );
        assert_eq!(
            guess_content_type("http://example.com/stream.m3u8"),
            "application/x-mpegURL"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.flac?token=abc"),
            "audio/flac"
        );
        assert_eq!(
            guess_content_type("http://example.com/stream"),
            "audio/mpeg"
        );
    }

    #[test]
    fn idle_status_without_reason_maps_to_buffering() {
        use rust_cast::channels::media::{PlayerState as CastPlayerState, Status, StatusEntry};

        let snapshot = snapshot_from_status(Status {
            request_id: 1,
            entries: vec![StatusEntry {
                media_session_id: 42,
                media: None,
                playback_rate: 0.0,
                player_state: CastPlayerState::Idle,
                current_item_id: None,
                loading_item_id: None,
                preloaded_item_id: None,
                idle_reason: None,
                extended_status: None,
                current_time: None,
                supported_media_commands: 0,
            }],
        });

        assert_eq!(snapshot.state, Some(PlayerState::Buffering));
        assert_eq!(snapshot.terminal, None);
    }

    // ── P1.6: credentials must never reach a Cast device ──────────────

    fn classify(uri: &str) -> CastMedia {
        classify_cast_uri(uri)
    }

    /// The bug this exists for. Every one of these URLs used to be handed to
    /// the Chromecast verbatim, credential and all.
    #[test]
    fn a_credential_bearing_stream_is_always_proxied() {
        let leaks = [
            // Plex: the account-wide token.
            "https://plex.test/library/parts/1/a.flac?X-Plex-Token=plex-secret",
            // Jellyfin: the access token.
            "https://jellyfin.test/Audio/1/stream?api_key=jellyfin-secret",
            // Subsonic token auth: token + salt.
            "https://sub.test/rest/stream.view?u=me&t=tok-secret&s=salt&c=Tributary&id=1",
            // Subsonic PLAINTEXT auth: this is the user's password, hex-encoded
            // and trivially reversible. It cannot be revoked.
            "https://sub.test/rest/stream.view?u=me&p=enc%3A70617373&c=Tributary&id=1",
            // DAAP session id.
            "http://daap.test:3689/databases/1/items/2.mp3?session-id=daap-secret",
            // Credentials in user-info rather than the query.
            "https://me:hunter2@music.test/stream.flac",
        ];

        for uri in leaks {
            match classify(uri) {
                CastMedia::Proxied(_) => {}
                CastMedia::Direct(passed_through) => panic!(
                    "credential-bearing URL was handed straight to the device: {passed_through}"
                ),
                CastMedia::LocalFile(_) | CastMedia::InvalidLocalUri => {
                    panic!("remote URL misclassified as local: {uri}")
                }
            }
        }
    }

    /// The receiver must see an opaque ticket, never the secret.
    #[test]
    fn a_proxied_stream_never_exposes_the_secret_to_the_receiver() {
        let uri = "https://sub.test/rest/stream.view?u=me&p=enc%3A70617373&c=Tributary&id=1";
        let CastMedia::Proxied(upstream) = classify(uri) else {
            panic!("must be proxied");
        };

        // The upstream URL is what *Tributary* fetches, in-process. What the
        // device is given is a ticket minted by the LAN server, which contains
        // only a random UUID — asserted here by construction: the ticket is
        // built from the server address and a fresh v4 UUID, never from `uri`.
        assert!(upstream.as_str().contains("enc%3A70617373"));

        // And the classification itself must not be Direct, which is the only
        // path that would ever return `uri` to a caller.
        assert!(!matches!(classify(uri), CastMedia::Direct(_)));
    }

    /// Internet radio has no secret, and relaying a live stream through this
    /// process would buy nothing. It must still pass straight through.
    #[test]
    fn an_unauthenticated_stream_is_not_relayed() {
        for uri in [
            "http://radio.test/stream.mp3",
            "https://radio.test/listen?bitrate=128&format=mp3",
            // An ordinary `s`/`p` pair on a non-Subsonic service is not a
            // credential and must not trigger the proxy.
            "https://radio.test/search?s=jazz&p=2",
        ] {
            // `CastMedia` has no `Debug` on purpose — a credential must not be
            // printable — so match rather than `assert_eq!`.
            match classify(uri) {
                CastMedia::Direct(passed_through) => assert_eq!(passed_through, uri),
                _ => panic!("{uri} should be passed through untouched"),
            }
        }
    }

    #[test]
    fn a_local_file_is_still_served_from_disk() {
        // Built from a real absolute path: `Url::from_file_path` rejects a bare
        // POSIX path on Windows, which needs a drive letter.
        let path = std::env::temp_dir().join("song.flac");
        let uri = url::Url::from_file_path(&path)
            .expect("file URL")
            .to_string();
        assert!(matches!(classify(&uri), CastMedia::LocalFile(_)));
    }

    /// A `file://` URI that does not parse must be rejected, not forwarded.
    /// Classifying on the parse result alone dropped it into the pass-through
    /// arm, which handed the device a URI we could not even read — caught by
    /// `invalid_local_uri_error_is_secret_free`.
    ///
    /// The scheme comparison is case-insensitive, so `FILE://` cannot sneak
    /// past it into the pass-through arm either.
    #[test]
    fn an_unparseable_local_uri_is_rejected_rather_than_forwarded() {
        for uri in [
            "file://[not-a-url",
            "file://[cast-secret-token",
            "FILE://[not-a-url",
        ] {
            assert!(
                matches!(classify(uri), CastMedia::InvalidLocalUri),
                "{uri} must be rejected, not passed to the device"
            );
        }
    }
}
