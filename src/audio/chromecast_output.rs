//! Chromecast audio output using one ordered Cast V2 worker/session.
//!
//! Chromecast devices are discovered via `_googlecast._tcp.local.` and are
//! controlled with `rust_cast`. Its `Rc`-backed message manager and channel
//! graph remain deliberately non-`Send`, so the transport is constructed,
//! retained, used, and dropped entirely on one dedicated OS thread. Every
//! load/control/poll enters that worker's FIFO command stream.

use std::cell::Cell;
#[cfg(test)]
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{error, info};

use super::cast_http_server::CastHttpServer;
use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerEventGeneration, PlayerState};
use crate::architecture::media::ResolvedHttpRequest;
use crate::local::resolver::ResolvedLocalMedia;

const HEARTBEAT_INTERVAL_SECS: u64 = 5;
const POSITION_POLL_INTERVAL_SECS: u64 = 1;
const CLEANUP_RETRY_INTERVAL_SECS: u64 = 1;
const MAX_CLEANUP_ATTEMPTS: u8 = 3;
const WORKER_TICK_MS: u64 = 100;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(8);
/// Cast V2 carries small protobuf control messages, never media payloads.
///
/// Keep a deliberately generous 1 MiB ceiling while rejecting a peer's
/// advertised length before `rust_cast` can allocate from it. The upstream
/// manager otherwise accepts the complete unsigned 32-bit range.
const MAX_CAST_FRAME_BYTES: u32 = 1024 * 1024;

const CAST_SENDER_ID: &str = "sender-0";
const CAST_RECEIVER_ID: &str = "receiver-0";

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
    connection_usable: bool,
}

impl CastFailure {
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

fn opaque_cast_failure<E>(operation: &'static str, _error: E) -> CastFailure {
    CastFailure::new(operation)
}

fn rust_cast_failure(operation: &'static str, error: rust_cast::errors::Error) -> CastFailure {
    match error {
        // rust_cast reports request-correlated Cast protocol rejections (for
        // example LOAD_FAILED, LOAD_CANCELLED, INVALID_REQUEST, and invalid
        // player state) as Internal. Those responses consume a complete Cast
        // frame, so the stream remains synchronized and remote cleanup is
        // still safe. Namespace errors are likewise rejected before I/O.
        rust_cast::errors::Error::Internal(_) | rust_cast::errors::Error::Namespace(_) => {
            CastFailure::synchronized(operation)
        }
        // Any transport, framing, decoding, or deadline error may leave a
        // partial request or response on the stream. Fail closed instead of
        // issuing cleanup commands on an indeterminate protocol state.
        rust_cast::errors::Error::Io(_)
        | rust_cast::errors::Error::Protobuf(_)
        | rust_cast::errors::Error::Serialization(_)
        | rust_cast::errors::Error::Parsing(_)
        | rust_cast::errors::Error::Dns(_)
        | rust_cast::errors::Error::Tls(_)
        | rust_cast::errors::Error::Timeout(_) => CastFailure::new(operation),
    }
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
    address: SocketAddr,
    timeouts: CastIoTimeouts,
}

type CastTlsStream = rustls::StreamOwned<rustls::ClientConnection, DeadlineTcpStream>;
type CastIo = BoundedCastStream<CastTlsStream>;

struct RustCastTransport {
    connection: rust_cast::channels::connection::ConnectionChannel<'static, CastIo>,
    heartbeat: rust_cast::channels::heartbeat::HeartbeatChannel<'static, CastIo>,
    media: rust_cast::channels::media::MediaChannel<'static, CastIo>,
    receiver: rust_cast::channels::receiver::ReceiverChannel<'static, CastIo>,
    deadline: Rc<DeadlineState>,
    operation_timeout: Duration,
}

#[derive(Clone, Copy)]
struct CastIoTimeouts {
    connect: Duration,
    operation: Duration,
    idle: Duration,
}

impl CastIoTimeouts {
    const fn production() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            operation: OPERATION_TIMEOUT,
            idle: IO_IDLE_TIMEOUT,
        }
    }
}

struct DeadlineState {
    end: Cell<Option<Instant>>,
    idle: Duration,
}

impl DeadlineState {
    fn new(idle: Duration) -> Self {
        Self {
            end: Cell::new(None),
            idle,
        }
    }

    fn arm(self: &Rc<Self>, duration: Duration) -> DeadlineGuard {
        let end = Instant::now()
            .checked_add(duration)
            .unwrap_or_else(Instant::now);
        self.end.set(Some(end));
        DeadlineGuard {
            state: Rc::clone(self),
        }
    }

    fn io_timeout(&self) -> io::Result<Duration> {
        let Some(end) = self.end.get() else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Chromecast I/O attempted without a deadline",
            ));
        };
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Chromecast operation deadline elapsed",
            ));
        }
        Ok(remaining.min(self.idle))
    }
}

struct DeadlineGuard {
    state: Rc<DeadlineState>,
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        self.state.end.set(None);
    }
}

struct DeadlineTcpStream {
    stream: TcpStream,
    deadline: Rc<DeadlineState>,
}

impl Read for DeadlineTcpStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let timeout = self.deadline.io_timeout()?;
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineTcpStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let timeout = self.deadline.io_timeout()?;
        self.stream.set_write_timeout(Some(timeout))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        let timeout = self.deadline.io_timeout()?;
        self.stream.set_write_timeout(Some(timeout))?;
        self.stream.flush()
    }
}

/// Plaintext Cast framing guard installed immediately above the TLS stream.
///
/// `rust_cast 0.21` reads a four-byte big-endian frame length and immediately
/// calls `Vec::with_capacity(length)`. This adapter withholds that header until
/// all four bytes have been received and the advertised length is within
/// [`MAX_CAST_FRAME_BYTES`]. Accepted headers and payload bytes are then
/// delivered unchanged. Tracking the remaining payload also makes a truncated
/// frame an I/O error instead of allowing its bytes to be parsed as a complete
/// message or the next frame header.
struct BoundedCastStream<S> {
    inner: S,
    header: [u8; 4],
    header_filled: usize,
    header_delivered: usize,
    payload_remaining: Option<u32>,
    poisoned: bool,
}

impl<S> BoundedCastStream<S> {
    const fn new(inner: S) -> Self {
        Self {
            inner,
            header: [0; 4],
            header_filled: 0,
            header_delivered: 0,
            payload_remaining: None,
            poisoned: false,
        }
    }

    fn reset_for_header(&mut self) {
        self.header = [0; 4];
        self.header_filled = 0;
        self.header_delivered = 0;
        self.payload_remaining = None;
    }

    fn framing_error(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message)
    }
}

impl<S: Read> Read for BoundedCastStream<S> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.poisoned {
            return Err(Self::framing_error("Cast frame stream is desynchronized"));
        }

        loop {
            if let Some(remaining) = self.payload_remaining {
                if self.header_delivered < self.header.len() {
                    let available = &self.header[self.header_delivered..];
                    let copied = available.len().min(output.len());
                    output[..copied].copy_from_slice(&available[..copied]);
                    self.header_delivered += copied;
                    return Ok(copied);
                }

                if remaining == 0 {
                    self.reset_for_header();
                    continue;
                }

                let allowed = usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(output.len());
                let read_count = self.inner.read(&mut output[..allowed])?;
                if read_count == 0 {
                    self.poisoned = true;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Cast frame ended before its advertised length",
                    ));
                }
                let Ok(read) = u32::try_from(read_count) else {
                    self.poisoned = true;
                    return Err(Self::framing_error(
                        "Cast frame reader exceeded the validated payload bound",
                    ));
                };
                let Some(remaining) = remaining.checked_sub(read) else {
                    self.poisoned = true;
                    return Err(Self::framing_error(
                        "Cast frame reader exceeded the advertised payload length",
                    ));
                };
                self.payload_remaining = Some(remaining);
                return Ok(read_count);
            }

            while self.header_filled < self.header.len() {
                let read = self.inner.read(&mut self.header[self.header_filled..])?;
                if read == 0 {
                    if self.header_filled == 0 {
                        return Ok(0);
                    }
                    self.poisoned = true;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Cast frame ended inside its length header",
                    ));
                }
                self.header_filled += read;
            }

            let advertised = u32::from_be_bytes(self.header);
            if advertised > MAX_CAST_FRAME_BYTES {
                self.poisoned = true;
                return Err(Self::framing_error(
                    "Cast frame exceeds the inbound control-message limit",
                ));
            }
            self.header_delivered = 0;
            self.payload_remaining = Some(advertised);
        }
    }
}

impl<S: Write> Write for BoundedCastStream<S> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn write_vectored(&mut self, buffers: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.inner.write_vectored(buffers)
    }
}

impl CastConnector for RustCastConnector {
    type Transport = RustCastTransport;

    fn connect(&mut self) -> CastResult<Self::Transport> {
        // Cast devices use self-signed certificates with no verifiable host.
        // This is inherent to Cast V2 and leaves the control channel exposed
        // to endpoint impersonation on a hostile LAN (tracked in P1.6).
        let stream = TcpStream::connect_timeout(&self.address, self.timeouts.connect)
            .map_err(|error| opaque_cast_failure("TCP connection", error))?;
        let _ = stream.set_nodelay(true);

        let mut config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(rust_cast::NoCertificateVerification {}))
            .with_no_client_auth();
        config.key_log = Arc::new(rustls::KeyLogFile::new());
        let server_name = rustls::pki_types::ServerName::IpAddress(self.address.ip().into());
        let connection = rustls::ClientConnection::new(Arc::new(config), server_name)
            .map_err(|error| opaque_cast_failure("TLS connection", error))?;

        // rust_cast's high-level CastDevice constructor hides its TcpStream,
        // but its public channel layer accepts any Read + Write transport.
        // Compose those channels over our deadline-enforcing stream so one
        // worker-owned, deliberately non-Send session retains absolute I/O
        // deadlines for every protocol operation.
        let deadline = Rc::new(DeadlineState::new(self.timeouts.idle));
        let stream = rustls::StreamOwned::new(
            connection,
            DeadlineTcpStream {
                stream,
                deadline: Rc::clone(&deadline),
            },
        );
        // Guard the decrypted Cast framing, not the encrypted TCP records, so
        // the peer's advertised protobuf length is rejected before rust_cast
        // sees the header and allocates its receive buffer.
        let stream = BoundedCastStream::new(stream);
        let manager = Rc::new(rust_cast::message_manager::MessageManager::new(stream));
        let connection = rust_cast::channels::connection::ConnectionChannel::new(
            CAST_SENDER_ID,
            Rc::clone(&manager),
        );
        let heartbeat = rust_cast::channels::heartbeat::HeartbeatChannel::new(
            CAST_SENDER_ID,
            CAST_RECEIVER_ID,
            Rc::clone(&manager),
        );
        let media =
            rust_cast::channels::media::MediaChannel::new(CAST_SENDER_ID, Rc::clone(&manager));
        let receiver = rust_cast::channels::receiver::ReceiverChannel::new(
            CAST_SENDER_ID,
            CAST_RECEIVER_ID,
            manager,
        );
        Ok(RustCastTransport {
            connection,
            heartbeat,
            media,
            receiver,
            deadline,
            operation_timeout: self.timeouts.operation,
        })
    }
}

impl RustCastTransport {
    fn with_deadline<T>(
        &self,
        operation: &'static str,
        call: impl FnOnce(&Self) -> Result<T, rust_cast::errors::Error>,
    ) -> CastResult<T> {
        let _guard = self.deadline.arm(self.operation_timeout);
        call(self).map_err(|error| rust_cast_failure(operation, error))
    }
}

impl CastTransport for RustCastTransport {
    fn connect_receiver(&mut self) -> CastResult<()> {
        self.with_deadline("receiver channel connection", |transport| {
            transport.connection.connect(CAST_RECEIVER_ID)
        })
    }

    fn set_volume(&mut self, level: f64) -> CastResult<()> {
        self.with_deadline("volume update", |transport| {
            transport
                .receiver
                .set_volume(level.clamp(0.0, 1.0) as f32)
                .map(|_| ())
        })
    }

    fn launch_receiver(&mut self) -> CastResult<AppSession> {
        use rust_cast::channels::receiver::CastDeviceApp;

        self.with_deadline("receiver launch", |transport| {
            transport
                .receiver
                .launch_app(&CastDeviceApp::DefaultMediaReceiver)
                .map(|app| AppSession {
                    transport_id: app.transport_id,
                    session_id: app.session_id,
                })
        })
    }

    fn connect_app(&mut self, app: &AppSession) -> CastResult<()> {
        self.with_deadline("application channel connection", |transport| {
            transport.connection.connect(app.transport_id.clone())
        })
    }

    fn disconnect_app(&mut self, app: &AppSession) -> CastResult<()> {
        self.with_deadline("application channel disconnection", |transport| {
            transport.connection.disconnect(app.transport_id.clone())
        })
    }

    fn stop_app(&mut self, app: &AppSession) -> CastResult<()> {
        self.with_deadline("receiver application stop", |transport| {
            transport.receiver.stop_app(app.session_id.clone())
        })
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
        self.with_deadline("media load", |transport| {
            transport
                .media
                .load(app.transport_id.clone(), app.session_id.clone(), &media)
                .map(snapshot_from_status)
        })
    }

    fn play(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()> {
        self.with_deadline("play command", |transport| {
            transport
                .media
                .play(app.transport_id.clone(), media_session_id)
                .map(|_| ())
        })
    }

    fn pause(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()> {
        self.with_deadline("pause command", |transport| {
            transport
                .media
                .pause(app.transport_id.clone(), media_session_id)
                .map(|_| ())
        })
    }

    fn seek(
        &mut self,
        app: &AppSession,
        media_session_id: i32,
        position_ms: u64,
    ) -> CastResult<()> {
        let seconds = (position_ms as f64 / 1000.0).min(f32::MAX as f64) as f32;
        self.with_deadline("seek command", |transport| {
            transport
                .media
                .seek(
                    app.transport_id.clone(),
                    media_session_id,
                    Some(seconds),
                    None,
                )
                .map(|_| ())
        })
    }

    fn stop(&mut self, app: &AppSession, media_session_id: i32) -> CastResult<()> {
        self.with_deadline("stop command", |transport| {
            transport
                .media
                .stop(app.transport_id.clone(), media_session_id)
                .map(|_| ())
        })
    }

    fn heartbeat(&mut self) -> CastResult<()> {
        self.with_deadline("heartbeat", |transport| transport.heartbeat.ping())
    }

    fn status(
        &mut self,
        app: &AppSession,
        media_session_id: i32,
    ) -> CastResult<CastStatusSnapshot> {
        self.with_deadline("status poll", |transport| {
            transport
                .media
                .get_status(app.transport_id.clone(), Some(media_session_id))
                .map(snapshot_from_status)
        })
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
            _ => Duration::from_hours(1),
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
    if discard_poisoned_session_if_stale(active, &result, owner, intent_epoch) {
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
    if discard_poisoned_session_if_stale(active, &loaded, owner, intent_epoch) {
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
            CastFailure::synchronized("media session creation"),
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
                CastFailure::synchronized("media startup"),
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
                CastFailure::synchronized("media startup"),
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
    if failure.connection_usable {
        if let Some(session) = active.as_mut() {
            session.retired = true;
        }
        let _ = cleanup_session(active, owner, intent_epoch);
    } else {
        // A timeout, partial frame, TLS error, or malformed response can leave
        // unread bytes or a half-written request on the Cast stream. Drop the
        // session immediately instead of multiplying one deadline across
        // cleanup calls on a protocol state that is no longer synchronized.
        active.take();
    }
    if is_current(owner, intent_epoch) {
        fail_cast(owner, failure, intent_epoch, current_state, event_tx);
    }
}

/// Return whether an in-flight operation was superseded, dropping a session
/// that the completed operation proved can no longer be used safely.
///
/// A newer intent may arrive while blocking Cast I/O is still within its
/// deadline. Keeping a stream that then reports a transport/protocol failure
/// would make the newer intent spend another full operation budget attempting
/// cleanup on a desynchronized connection.
fn discard_poisoned_session_if_stale<T, U>(
    active: &mut Option<WorkerSession<T>>,
    result: &CastResult<U>,
    owner: CommandOwner,
    intent_epoch: &AtomicU64,
) -> bool {
    if is_current(owner, intent_epoch) {
        return false;
    }
    if result
        .as_ref()
        .is_err_and(|failure| !failure.connection_usable)
    {
        active.take();
    }
    true
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

    if discard_poisoned_session_if_stale(active, &result, owner, intent_epoch) {
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
        if discard_poisoned_session_if_stale(active, &result, owner, intent_epoch) {
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
    if discard_poisoned_session_if_stale(active, &status, owner, intent_epoch) {
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
                CastFailure::synchronized("remote playback"),
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
            Err(failure) if failure.connection_usable => first_failure = Some(failure),
            Err(failure) => return CleanupOutcome::Failed(failure),
        }
        if !is_current(owner, intent_epoch) {
            *active = Some(session);
            return CleanupOutcome::Stale;
        }
    }

    if session.app_connected {
        match session.transport.disconnect_app(&session.app) {
            Ok(()) => session.app_connected = false,
            Err(failure) if failure.connection_usable => {
                first_failure.get_or_insert(failure);
            }
            Err(failure) => return CleanupOutcome::Failed(failure),
        }
        if !is_current(owner, intent_epoch) {
            *active = Some(session);
            return CleanupOutcome::Stale;
        }
    }

    let app_stop = session.transport.stop_app(&session.app);
    if let Err(failure) = app_stop {
        first_failure.get_or_insert(failure);
        if failure.connection_usable {
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
            if session
                .transport
                .stop(&session.app, media_session_id)
                .is_err_and(|failure| !failure.connection_usable)
            {
                return;
            }
        }
        if session.app_connected
            && session
                .transport
                .disconnect_app(&session.app)
                .is_err_and(|failure| !failure.connection_usable)
        {
            return;
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
        address: SocketAddr,
        event_tx: async_channel::Sender<PlayerEvent>,
        initial_volume: f64,
    ) -> Self {
        info!(%address, name = %display_name, "Chromecast output configured");
        let current_state = Arc::new(Mutex::new(PlayerState::Stopped));
        let intent_epoch = Arc::new(AtomicU64::new(0));
        let worker_tx = spawn_cast_worker(
            RustCastConnector {
                address,
                timeouts: CastIoTimeouts::production(),
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
        // Any new load retires every route owned by the previous load, whatever
        // the new track turns out to be. Revoking only inside a registration
        // method would leave credential or retained-file authority alive when
        // playback moves to another media kind.
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

    fn resolve_local_authority(&self, media: ResolvedLocalMedia) -> CastResult<String> {
        self.revoke_proxy_tickets();
        let server = self.ensure_cast_server()?;
        let server = server
            .as_ref()
            .ok_or_else(|| CastFailure::new("local media server startup"))?;
        Ok(server.register_local(media))
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

    /// Revoke every route the current output load is holding.
    ///
    /// Best-effort: a poisoned lock or an unstarted server means there is
    /// nothing to revoke.
    fn revoke_proxy_tickets(&self) {
        if let Ok(guard) = self.cast_server.lock() {
            if let Some(server) = guard.as_ref() {
                server.revoke_playback_routes();
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

    fn load_uri(&self, uri: &str) -> bool {
        let owner = self.next_owner();
        let kind = match self.resolve_uri(uri) {
            Ok(uri) => CommandKind::Load {
                uri,
                volume: self.volume,
            },
            Err(failure) => CommandKind::RejectLoad { failure },
        };
        let _ = self.enqueue(owner, kind);
        true
    }

    fn load_resolved(&self, request: ResolvedHttpRequest) -> bool {
        let owner = self.next_owner();
        let kind = match self.resolve_request(request) {
            Ok(uri) => CommandKind::Load {
                uri,
                volume: self.volume,
            },
            Err(failure) => CommandKind::RejectLoad { failure },
        };
        let _ = self.enqueue(owner, kind);
        true
    }

    fn load_local(&self, media: ResolvedLocalMedia) -> bool {
        let owner = self.next_owner();
        let kind = match self.resolve_local_authority(media) {
            Ok(uri) => CommandKind::Load {
                uri,
                volume: self.volume,
            },
            Err(failure) => CommandKind::RejectLoad { failure },
        };
        let _ = self.enqueue(owner, kind);
        true
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
        // Kill credential and retained-file routes as soon as playback is
        // meant to end. They are not revoked on pause or seek: a Cast device
        // re-fetches with a `Range` header when it seeks, so the current route
        // has to outlive those controls.
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
    use std::sync::atomic::AtomicBool;

    use rust_cast::message_manager::{CastMessage, CastMessagePayload, MessageManager};

    use super::*;

    const TEST_CAST_NAMESPACE: &str = "urn:x-cast:tributary.test";

    struct ObservedCastIo {
        input: std::io::Cursor<Vec<u8>>,
        max_read: usize,
        forbid_read_at: Option<usize>,
        forbidden_read: Arc<AtomicBool>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl ObservedCastIo {
        fn reader(input: Vec<u8>, max_read: usize) -> Self {
            Self {
                input: std::io::Cursor::new(input),
                max_read: max_read.max(1),
                forbid_read_at: None,
                forbidden_read: Arc::new(AtomicBool::new(false)),
                written: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn oversized_peer(
            input: Vec<u8>,
            forbidden_read: Arc<AtomicBool>,
            written: Arc<Mutex<Vec<u8>>>,
        ) -> Self {
            Self {
                input: std::io::Cursor::new(input),
                max_read: usize::MAX,
                forbid_read_at: Some(4),
                forbidden_read,
                written,
            }
        }

        fn writer(written: Arc<Mutex<Vec<u8>>>) -> Self {
            Self {
                input: std::io::Cursor::new(Vec::new()),
                max_read: usize::MAX,
                forbid_read_at: None,
                forbidden_read: Arc::new(AtomicBool::new(false)),
                written,
            }
        }
    }

    impl Read for ObservedCastIo {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let position = usize::try_from(self.input.position()).unwrap_or(usize::MAX);
            if self
                .forbid_read_at
                .is_some_and(|forbidden| position >= forbidden)
            {
                self.forbidden_read.store(true, Ordering::SeqCst);
                return Err(io::Error::other(
                    "test peer payload was read after a rejected Cast header",
                ));
            }

            let before_forbidden = self
                .forbid_read_at
                .map_or(usize::MAX, |forbidden| forbidden.saturating_sub(position));
            let allowed = output.len().min(self.max_read).min(before_forbidden);
            self.input.read(&mut output[..allowed])
        }
    }

    impl Write for ObservedCastIo {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.written
                .lock()
                .expect("test Cast write lock")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn encode_cast_frame(payload: CastMessagePayload) -> Vec<u8> {
        let written = Arc::new(Mutex::new(Vec::new()));
        let stream = BoundedCastStream::new(ObservedCastIo::writer(Arc::clone(&written)));
        MessageManager::new(stream)
            .send(CastMessage {
                namespace: TEST_CAST_NAMESPACE.to_string(),
                source: CAST_RECEIVER_ID.to_string(),
                destination: CAST_SENDER_ID.to_string(),
                payload,
            })
            .expect("encode test Cast frame through the real message manager");

        let frame = written.lock().expect("test Cast write lock").clone();
        assert!(frame.len() >= 4);
        assert_eq!(
            usize::try_from(u32::from_be_bytes(frame[..4].try_into().unwrap())).unwrap(),
            frame.len() - 4
        );
        frame
    }

    fn binary_frame_with_serialized_length(target: usize) -> (Vec<u8>, usize) {
        let mut payload_len = target;
        for _ in 0..8 {
            let frame = encode_cast_frame(CastMessagePayload::Binary(vec![0x5a; payload_len]));
            let serialized_len = frame.len() - 4;
            match serialized_len.cmp(&target) {
                std::cmp::Ordering::Equal => return (frame, payload_len),
                std::cmp::Ordering::Greater => {
                    payload_len = payload_len
                        .checked_sub(serialized_len - target)
                        .expect("test Cast envelope must fit below the frame limit");
                }
                std::cmp::Ordering::Less => payload_len += target - serialized_len,
            }
        }
        panic!("could not construct an exact-boundary Cast frame");
    }

    #[test]
    fn oversized_peer_frame_is_rejected_before_payload_read_or_allocation() {
        let forbidden_read = Arc::new(AtomicBool::new(false));
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut input = (MAX_CAST_FRAME_BYTES + 1).to_be_bytes().to_vec();
        input.push(0x5a);
        let stream = BoundedCastStream::new(ObservedCastIo::oversized_peer(
            input,
            Arc::clone(&forbidden_read),
            written,
        ));

        let error = MessageManager::new(stream)
            .receive()
            .expect_err("oversized Cast header must fail before rust_cast allocates");

        assert!(matches!(
            error,
            rust_cast::errors::Error::Io(error) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(!forbidden_read.load(Ordering::SeqCst));
    }

    #[test]
    fn exact_maximum_cast_frame_is_accepted_by_the_real_message_manager() {
        let target = usize::try_from(MAX_CAST_FRAME_BYTES).unwrap();
        let (frame, payload_len) = binary_frame_with_serialized_length(target);
        let stream = BoundedCastStream::new(ObservedCastIo::reader(frame, 8192));

        let message = MessageManager::new(stream)
            .receive()
            .expect("the exact Cast frame limit remains usable");

        assert!(matches!(
            message.payload,
            CastMessagePayload::Binary(payload) if payload.len() == payload_len
        ));
    }

    #[test]
    fn truncated_cast_headers_and_payloads_fail_closed() {
        let header_stream = BoundedCastStream::new(ObservedCastIo::reader(vec![0, 0, 0], 1));
        let header_error = MessageManager::new(header_stream)
            .receive()
            .expect_err("partial Cast frame header must fail");
        assert!(matches!(
            header_error,
            rust_cast::errors::Error::Io(error)
                if error.kind() == io::ErrorKind::UnexpectedEof
        ));

        let mut payload_frame = encode_cast_frame(CastMessagePayload::String("truncated".into()));
        payload_frame.pop();
        let payload_stream = BoundedCastStream::new(ObservedCastIo::reader(payload_frame, 2));
        let payload_error = MessageManager::new(payload_stream)
            .receive()
            .expect_err("partial Cast frame payload must fail");
        assert!(matches!(
            payload_error,
            rust_cast::errors::Error::Io(error)
                if error.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn consecutive_cast_frames_reset_framing_and_preserve_writes() {
        let first = encode_cast_frame(CastMessagePayload::String("first".into()));
        let second = encode_cast_frame(CastMessagePayload::String("second".into()));
        let mut input = first;
        input.extend_from_slice(&second);
        let stream = BoundedCastStream::new(ObservedCastIo::reader(input, 1));
        let manager = MessageManager::new(stream);

        assert!(matches!(
            manager.receive().expect("receive first Cast frame").payload,
            CastMessagePayload::String(payload) if payload == "first"
        ));
        assert!(matches!(
            manager.receive().expect("receive second Cast frame").payload,
            CastMessagePayload::String(payload) if payload == "second"
        ));
    }

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
        poison_at: Mutex<Option<Point>>,
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
                poison_at: Mutex::new(None),
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
            if self.poison_at.lock().expect("poison lock").as_ref() == Some(&point) {
                return Err(CastFailure::new(point.operation()));
            }
            if self.fail_at.lock().expect("failure lock").as_ref() == Some(&point) {
                return Err(CastFailure::synchronized(point.operation()));
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
                return Err(CastFailure::synchronized(point.operation()));
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
                    heartbeat: Duration::from_hours(1),
                    poll: Duration::from_hours(1),
                    cleanup_retry: Duration::from_millis(10),
                    tick: Duration::from_millis(10),
                },
            )
        }

        fn new_with_timing(shared: Arc<FakeShared>, timing: WorkerTiming) -> Self {
            Self::new_with_connector(FakeConnector { shared }, timing)
        }

        fn new_with_connector<C>(connector: C, timing: WorkerTiming) -> Self
        where
            C: CastConnector,
        {
            let (tx, rx) = mpsc::channel();
            let epoch = Arc::new(AtomicU64::new(0));
            let state = Arc::new(Mutex::new(PlayerState::Stopped));
            let (event_tx, events) = async_channel::unbounded();
            let epoch_for_worker = Arc::clone(&epoch);
            let state_for_worker = Arc::clone(&state);
            let worker = std::thread::spawn(move || {
                run_cast_worker(
                    connector,
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

    struct SilentCastServer {
        address: SocketAddr,
        accepted: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl SilentCastServer {
        fn start(expected_connections: usize) -> Self {
            let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .expect("bind silent Cast server");
            let address = listener.local_addr().expect("silent Cast address");
            let (accepted_tx, accepted) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut streams = Vec::with_capacity(expected_connections);
                for _ in 0..expected_connections {
                    let (stream, _) = listener.accept().expect("accept Cast client");
                    streams.push(stream);
                    accepted_tx.send(()).expect("report accepted Cast client");
                }
                assert_eq!(streams.len(), expected_connections);
                let _ = release_rx.recv_timeout(Duration::from_secs(2));
            });
            Self {
                address,
                accepted,
                release,
                worker: Some(worker),
            }
        }

        fn wait_for_connection(&self) {
            self.accepted
                .recv_timeout(Duration::from_secs(2))
                .expect("silent Cast connection accepted");
        }

        fn finish(mut self) {
            let _ = self.release.send(());
            self.worker
                .take()
                .expect("silent server worker")
                .join()
                .expect("silent server stopped");
        }
    }

    fn short_io_connector(address: SocketAddr) -> RustCastConnector {
        let _ = rustls::crypto::ring::default_provider().install_default();
        RustCastConnector {
            address,
            timeouts: CastIoTimeouts {
                connect: Duration::from_millis(250),
                operation: Duration::from_millis(75),
                idle: Duration::from_millis(50),
            },
        }
    }

    fn quiet_worker_timing() -> WorkerTiming {
        WorkerTiming {
            heartbeat: Duration::from_hours(1),
            poll: Duration::from_hours(1),
            cleanup_retry: Duration::from_millis(10),
            tick: Duration::from_millis(10),
        }
    }

    #[test]
    fn trickled_bytes_cannot_extend_the_absolute_io_deadline() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind trickle server");
        let address = listener.local_addr().expect("trickle server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept trickle client");
            for byte in 0_u8..32 {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        let state = Rc::new(DeadlineState::new(Duration::from_secs(1)));
        let mut stream = DeadlineTcpStream {
            stream: TcpStream::connect(address).expect("connect trickle client"),
            deadline: Rc::clone(&state),
        };
        let started = Instant::now();
        let _guard = state.arm(Duration::from_millis(75));
        let mut bytes = [0_u8; 32];
        assert!(stream.read_exact(&mut bytes).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(stream);
        server.join().expect("trickle server stopped");
    }

    #[test]
    fn silent_receiver_cannot_pin_stop() {
        let server = SilentCastServer::start(1);
        let harness =
            Harness::new_with_connector(short_io_connector(server.address), quiet_worker_timing());
        let load = harness.next_owner(1);
        harness.send(
            load,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        server.wait_for_connection();

        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        harness.fence(stop);
        assert!(harness.events().iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged { generation, state: PlayerState::Stopped }
                if *generation == PlayerEventGeneration::from_raw(2)
        )));

        harness.shutdown();
        server.finish();
    }

    #[test]
    fn silent_receiver_cannot_pin_replacement_load() {
        let server = SilentCastServer::start(2);
        let harness =
            Harness::new_with_connector(short_io_connector(server.address), quiet_worker_timing());
        let first = harness.next_owner(1);
        harness.send(
            first,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        server.wait_for_connection();

        let replacement = harness.next_owner(2);
        harness.send(
            replacement,
            CommandKind::Load {
                uri: "https://music.test/b".to_string(),
                volume: 0.5,
            },
        );
        server.wait_for_connection();
        harness.fence(replacement);
        assert!(harness.events().iter().any(|event| matches!(
            event,
            PlayerEvent::Error { generation, .. }
                if *generation == PlayerEventGeneration::from_raw(2)
        )));

        harness.shutdown();
        server.finish();
    }

    #[test]
    fn silent_receiver_cannot_pin_shutdown() {
        let server = SilentCastServer::start(1);
        let mut harness =
            Harness::new_with_connector(short_io_connector(server.address), quiet_worker_timing());
        let load = harness.next_owner(1);
        harness.send(
            load,
            CommandKind::Load {
                uri: "https://music.test/a".to_string(),
                volume: 0.5,
            },
        );
        server.wait_for_connection();

        let shutdown = harness.next_owner(2);
        harness.send(shutdown, CommandKind::Shutdown);
        let started = Instant::now();
        while !harness
            .worker
            .as_ref()
            .expect("worker handle")
            .is_finished()
            && started.elapsed() < Duration::from_secs(1)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(harness
            .worker
            .as_ref()
            .expect("worker handle")
            .is_finished());
        harness
            .worker
            .take()
            .expect("worker handle")
            .join()
            .expect("worker stopped");
        server.finish();
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
    fn semantic_load_failure_disconnects_and_stops_the_receiver_app() {
        let shared = FakeShared::new();
        *shared.fail_at.lock().expect("failure lock") = Some(Point::Load);
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

        assert!(shared.actions().ends_with(&[
            Action::Point(Point::Load),
            Action::Point(Point::AppDisconnect),
            Action::Point(Point::AppStop),
        ]));
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
    fn poisoned_control_failure_drops_the_session_without_protocol_cleanup() {
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
        *shared.poison_at.lock().expect("poison lock") = Some(Point::Pause);

        harness.send(owner, CommandKind::Pause);
        harness.fence(owner);

        assert_eq!(shared.actions(), vec![Action::Point(Point::Pause)]);
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
    fn superseded_poisoned_control_drops_without_delaying_the_new_intent() {
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
        *shared.poison_at.lock().expect("poison lock") = Some(Point::Pause);
        let (entered, release) = shared.install_gate(Point::Pause);

        harness.send(first, CommandKind::Pause);
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("pause entered");
        let stop = harness.next_owner(2);
        harness.send(stop, CommandKind::Stop);
        release.send(()).expect("release pause");
        harness.fence(stop);

        assert_eq!(shared.actions(), vec![Action::Point(Point::Pause)]);
        assert!(!harness.events().iter().any(|event| matches!(
            event,
            PlayerEvent::Error { generation, .. }
                if *generation == PlayerEventGeneration::from_raw(1)
        )));
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
                heartbeat: Duration::from_hours(1),
                poll: Duration::from_hours(1),
                cleanup_retry: Duration::ZERO,
                tick: Duration::from_hours(1),
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
                heartbeat: Duration::from_hours(1),
                poll: Duration::from_hours(1),
                cleanup_retry: Duration::ZERO,
                tick: Duration::from_hours(1),
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
                heartbeat: Duration::from_hours(1),
                poll: Duration::ZERO,
                cleanup_retry: Duration::from_millis(10),
                tick: Duration::from_hours(1),
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
    fn rust_cast_error_classification_preserves_only_synchronized_failures() {
        let semantic = rust_cast_failure(
            "media load",
            rust_cast::errors::Error::Internal("Failed to load media.".to_string()),
        );
        let timeout = rust_cast_failure(
            "media load",
            rust_cast::errors::Error::Timeout("response deadline elapsed".to_string()),
        );
        let io = rust_cast_failure(
            "media load",
            rust_cast::errors::Error::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "partial Cast frame",
            )),
        );

        assert!(semantic.connection_usable);
        assert!(!timeout.connection_usable);
        assert!(!io.connection_usable);
    }

    #[test]
    fn invalid_local_uri_error_is_secret_free() {
        let (event_tx, events) = async_channel::unbounded();
        let output = ChromecastOutput::new(
            "Living Room",
            "127.0.0.1:8009".parse().unwrap(),
            event_tx,
            1.0,
        );
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
        let output = ChromecastOutput::new(
            "Living Room",
            "127.0.0.1:8009".parse().unwrap(),
            event_tx,
            1.0,
        );
        *output.cast_server.lock().expect("cast server lock") = Some(
            runtime
                .block_on(CastHttpServer::start_on(std::net::SocketAddr::from((
                    std::net::Ipv4Addr::LOCALHOST,
                    0,
                ))))
                .expect("loopback cast server"),
        );
        let explicit_file = tempfile::NamedTempFile::new().expect("legacy explicit file");
        {
            let guard = output.cast_server.lock().expect("cast server lock");
            let server = guard.as_ref().expect("cast server");
            let _ = server.register_file(explicit_file.path());
            assert_eq!(server.registered_route_count(), 1);
        }
        let endpoint = url::Url::parse("https://music.test/clean/track.flac?track=42")
            .expect("clean endpoint");
        let request = ResolvedHttpRequest::new(endpoint.clone()).expect("resolved request");

        let ticket = output.resolve_request(request).expect("proxied request");
        assert_ne!(ticket, endpoint.as_str());
        assert!(ticket.starts_with("http://127.0.0.1:"));
        assert!(!ticket.contains("music.test"));
        assert!(!ticket.contains("track=42"));
        {
            let guard = output.cast_server.lock().expect("cast server lock");
            assert_eq!(
                guard
                    .as_ref()
                    .expect("cast server")
                    .registered_route_count(),
                2,
                "a protected replacement must retain the legacy explicit-file route"
            );
        }

        output.revoke_proxy_tickets();
        let guard = output.cast_server.lock().expect("cast server lock");
        assert_eq!(
            guard
                .as_ref()
                .expect("cast server")
                .registered_route_count(),
            1,
            "output cleanup must revoke only the protected playback route"
        );
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
        let mut output =
            ChromecastOutput::new("Living Room", "127.0.0.1:8009".parse().unwrap(), tx, 1.0);
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
