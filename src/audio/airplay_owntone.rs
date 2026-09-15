//! OwnTone 29.x process adapter — the AirPlay sender path the design
//! investigation selects.
//!
//! `docs/airplay-sender-design.md` (§4.3, §5.4, §6, §10 step 2) selects
//! OwnTone's maintained daemon as Tributary's shipping AirPlay sender: the
//! daemon owns RAOP/AirPlay-2 pairing, encrypted control, framing and timing,
//! while Tributary decodes the app-owned prepared URI itself and pushes raw
//! s16le 44100 Hz stereo PCM into the daemon's named-pipe input. The daemon
//! never receives the media URL, so the protected loopback ticket never
//! leaves Tributary's process.
//!
//! This module is deliberately **fail-closed** and only ever selected by
//! explicit configuration ([`ENV_SELECT`]); the default output remains the
//! GStreamer `raopsink` adapter (§4.2). Everything the adapter needs is an
//! operator-provisioned, dedicated, Tributary-owned instance: a loopback JSON
//! API, a writable named pipe, and a state directory carrying the ownership
//! marker. Any missing or unsafe condition refuses the load with a localized,
//! actionable message before the receiver, the player, the queue or the
//! output set is read or mutated (§4.3, §4.4, §8).

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use super::airplay_sender::{
    AirplaySender, OpenCancel, OpenOutcome, RecoveryCompletion, RecoveryOutcome, SenderError,
    SenderOpenContext, SenderPosition, SenderSession, SenderWriteOutcome,
};
use super::gstreamer_media::{GstreamerMediaProxy, GstreamerMediaTicket};
use super::{PlayerEvent, PlayerEventGeneration, PlayerState};

/// Selects the OwnTone adapter instead of the default GStreamer `raopsink`
/// adapter. Selection is configuration, never a silent fallback (§4.4).
pub(super) const ENV_SELECT: &str = "TRIBUTARY_AIRPLAY_SENDER";

const ENV_API: &str = "TRIBUTARY_OWNTONE_API";
const ENV_PIPE: &str = "TRIBUTARY_OWNTONE_PIPE";
const ENV_STATE_DIR: &str = "TRIBUTARY_OWNTONE_STATE_DIR";
const ENV_BIN: &str = "TRIBUTARY_OWNTONE_BIN";
const DEFAULT_BIN: &str = "/usr/bin/owntone";

/// Minimum supported daemon major version (design pins 29.3).
const OWNTONE_MIN_MAJOR: u32 = 29;

const API_TIMEOUT: Duration = Duration::from_secs(2);
const OPEN_DEADLINE: Duration = Duration::from_secs(10);
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);
const FIFO_OPEN_POLL: Duration = Duration::from_millis(50);
const POSITION_INTERVAL: Duration = Duration::from_millis(500);

/// Bound on the synchronous cleanup that runs when an open fails or is
/// cancelled: restoration must complete inside it, or the failure becomes the
/// non-clean `RecoveryPending` state (review F3).
const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
/// Bound on the serialized recovery behind a `RecoveryPending` open. It caps
/// the whole recovery — settle-or-restart quiescence plus any restoration
/// attempt — so the `RecoveryCompletion` handle is always terminal.
const RECOVERY_DEADLINE: Duration = Duration::from_secs(30);
/// Poll interval between serialized-recovery restoration attempts.
const RECOVERY_POLL: Duration = Duration::from_millis(200);

/// Token value inside the dedicated instance's ownership record. The record
/// (see [`OwnershipRecord`]) is written by the installation/service record and
/// binds this token to the configured endpoint, pipe, state directory and
/// binary.
const OWNER_TOKEN: &str = "tributary-airplay-owntone-v1";

/// The localized, actionable message every refusal is built from.
fn unavailable(reason: &str) -> SenderError {
    unavailable_in(rust_i18n::locale().as_ref(), reason)
}

/// Build a refusal for an explicit catalog. Split from [`unavailable`] so the
/// every-catalog localization contract is unit-testable without mutating the
/// process locale.
fn unavailable_in(locale: &str, reason: &str) -> SenderError {
    SenderError::Dependency(
        rust_i18n::t!(
            "errors.playback.airplay_owntone_unavailable",
            reason = reason,
            locale = locale
        )
        .into_owned(),
    )
}

/// Whether an explicit `TRIBUTARY_AIRPLAY_SENDER` value selects the OwnTone
/// adapter. Selection is configuration, never a silent fallback (design §4.4),
/// so only the exact configured value counts — and it is split out from the
/// environment read so selection is unit-testable without mutating the
/// process environment.
fn selection_is_owntone(value: Option<&str>) -> bool {
    match value {
        Some(value) => value.eq_ignore_ascii_case("owntone"),
        None => false,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Filesystem + endpoint configuration for the dedicated instance.
#[derive(Debug, Clone)]
struct OwnToneConfig {
    api_base: String,
    pipe_path: PathBuf,
    state_dir: PathBuf,
    binary: PathBuf,
}

impl OwnToneConfig {
    fn from_env() -> Result<Self, SenderError> {
        let api = env_nonempty(ENV_API).ok_or_else(|| unavailable("not configured"))?;
        let pipe = env_nonempty(ENV_PIPE).ok_or_else(|| unavailable("not configured"))?;
        let state = env_nonempty(ENV_STATE_DIR).ok_or_else(|| unavailable("not configured"))?;
        let binary = env_nonempty(ENV_BIN).unwrap_or_else(|| DEFAULT_BIN.to_string());
        let config = Self {
            api_base: api.trim_end_matches('/').to_string(),
            pipe_path: PathBuf::from(pipe),
            state_dir: PathBuf::from(state),
            binary: PathBuf::from(binary),
        };
        config.verify_loopback()?;
        Ok(config)
    }

    /// The JSON API must be loopback-bound: loopback is network isolation, not
    /// authentication (§7), and a non-loopback endpoint is never the dedicated
    /// instance this adapter is allowed to drive.
    fn verify_loopback(&self) -> Result<(), SenderError> {
        let url = url::Url::parse(&self.api_base)
            .map_err(|_| unavailable("the configured API URL is invalid"))?;
        let host = url.host_str().unwrap_or_default();
        if !is_loopback_host(host) {
            return Err(unavailable("the JSON API must be bound to loopback"));
        }
        Ok(())
    }

    fn owner_marker(&self) -> PathBuf {
        self.state_dir.join(".tributary-owner")
    }

    fn lock_path(&self) -> PathBuf {
        self.state_dir.join(".tributary-lock")
    }

    fn takeover_record(&self) -> PathBuf {
        self.state_dir.join(".tributary-takeover.json")
    }

    /// Confirm out of band that the answering daemon is the dedicated
    /// Tributary-owned instance, before any state is read (§4.3, §8). The JSON
    /// API exposes no instance identity, so the ownership record in the
    /// instance's own state directory — the same trust domain as the lock file
    /// — is what distinguishes it from a shared instance that merely looks
    /// healthy. The record must bind the *configured* endpoint, pipe, state
    /// directory and binary: a valid token paired with a foreign API endpoint
    /// is refused here, before any receiver state is read or mutated (review
    /// F5). A constant marker string cannot make that distinction, because it
    /// never ties the answering endpoint to the owned instance.
    fn verify_owned(&self) -> Result<(), SenderError> {
        let body = std::fs::read_to_string(self.owner_marker())
            .map_err(|_| unavailable("the dedicated-instance ownership record is missing"))?;
        let record: OwnershipRecord = serde_json::from_str(&body)
            .map_err(|_| unavailable("the dedicated-instance ownership record is malformed"))?;
        if record.token != OWNER_TOKEN {
            return Err(unavailable(
                "the configured state directory is not a Tributary-owned instance",
            ));
        }
        let matches = record.api_base == self.api_base
            && record.pipe_path == self.pipe_path.to_string_lossy()
            && record.state_dir == self.state_dir.to_string_lossy()
            && record.binary == self.binary.to_string_lossy();
        if !matches {
            return Err(unavailable(
                "the configured endpoint, pipe, state directory or binary does not match the dedicated Tributary-owned instance",
            ));
        }
        Ok(())
    }
}

/// The out-of-band ownership record an installation writes into the dedicated
/// instance's state directory. It binds the token to the exact endpoint, pipe,
/// state directory and binary the adapter is configured with, so a valid token
/// cannot authorize a foreign API endpoint (review F5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct OwnershipRecord {
    token: String,
    api_base: String,
    pipe_path: String,
    state_dir: String,
    binary: String,
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// `true` when this package target has a documented OwnTone acquisition path.
/// Today that is the `.deb` target on Debian/Ubuntu amd64 only (design §8).
fn platform_available() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

/// Parse a daemon version string such as `29.3` into `(major, minor)`.
fn parse_version(raw: &str) -> Option<(u32, u32)> {
    let start = raw.find(|c: char| c.is_ascii_digit())?;
    let core: String = raw[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

/// Normalize a retained discovery identifier (a MAC or `deviceid`) to the
/// numeric value OwnTone uses as an output `id`.
///
/// OwnTone parses the hex MAC to a `u64` and renders it as a decimal string
/// in `/api/outputs`, so a raw-string compare would miss (§4.3).
fn normalize_device_identifier(raw: &str) -> Option<u64> {
    let filtered: String = raw.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if filtered.len() < 6 || filtered.len() > 16 {
        return None;
    }
    u64::from_str_radix(&filtered, 16).ok()
}

/// One output OwnTone reports through `/api/outputs`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnToneOutput {
    id: u64,
    name: String,
    selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MappingFailure {
    MissingIdentifier,
    NoMatch(u64),
    Ambiguous(u64),
}

impl std::fmt::Display for MappingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingIdentifier => write!(
                f,
                "the selected receiver published no retained device identifier"
            ),
            Self::NoMatch(id) => write!(
                f,
                "the selected receiver ({id}) is not present in the daemon's output list"
            ),
            Self::Ambiguous(id) => write!(
                f,
                "the selected receiver ({id}) maps to more than one daemon output"
            ),
        }
    }
}

/// Map the selected receiver onto exactly one OwnTone output by its retained
/// numeric identifier — never by display name (§4.3, §9 item 11). A missing
/// identifier, no match, or an ambiguous match fails closed before any mutating
/// call.
fn map_receiver_to_output(
    outputs: &[OwnToneOutput],
    device_id: Option<&str>,
) -> Result<u64, MappingFailure> {
    let expected = device_id
        .and_then(normalize_device_identifier)
        .ok_or(MappingFailure::MissingIdentifier)?;
    let mut matches = outputs.iter().filter(|output| output.id == expected);
    let first = matches.next().ok_or(MappingFailure::NoMatch(expected))?;
    if matches.next().is_some() {
        return Err(MappingFailure::Ambiguous(expected));
    }
    Ok(first.id)
}

/// Blocking control-plane client for the dedicated instance's loopback JSON
/// API. Every call is deadline-bounded and reports its own localized failure.
struct OwnToneClient {
    http: reqwest::blocking::Client,
    base: String,
}

impl OwnToneClient {
    fn new(base: &str) -> Result<Self, SenderError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .map_err(|_| unavailable("the local HTTP client could not be created"))?;
        Ok(Self {
            http,
            base: base.to_string(),
        })
    }

    fn get_json(&self, path: &str) -> Result<serde_json::Value, SenderError> {
        let response = self
            .http
            .get(format!("{}{}", self.base, path))
            .send()
            .map_err(|_| unavailable("the dedicated daemon is unreachable"))?;
        let response = response
            .error_for_status()
            .map_err(|_| unavailable("the dedicated daemon rejected the request"))?;
        response
            .json()
            .map_err(|_| unavailable("the dedicated daemon sent a malformed response"))
    }

    fn put(&self, path: &str) -> Result<(), SenderError> {
        let response = self
            .http
            .put(format!("{}{}", self.base, path))
            .send()
            .map_err(|_| unavailable("the dedicated daemon is unreachable"))?;
        response
            .error_for_status()
            .map_err(|_| unavailable("the dedicated daemon rejected the request"))?;
        Ok(())
    }

    fn put_json(&self, path: &str, body: &serde_json::Value) -> Result<(), SenderError> {
        let response = self
            .http
            .put(format!("{}{}", self.base, path))
            .json(body)
            .send()
            .map_err(|_| unavailable("the dedicated daemon is unreachable"))?;
        response
            .error_for_status()
            .map_err(|_| unavailable("the dedicated daemon rejected the request"))?;
        Ok(())
    }

    fn version(&self) -> Result<(u32, u32), SenderError> {
        let value = self.get_json("/api/config")?;
        let raw = value
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| unavailable("the dedicated daemon reported no version"))?;
        parse_version(raw).ok_or_else(|| unavailable("the dedicated daemon version is unparseable"))
    }

    fn outputs(&self) -> Result<Vec<OwnToneOutput>, SenderError> {
        let value = self.get_json("/api/outputs")?;
        let array = value
            .get("outputs")
            .and_then(|v| v.as_array())
            .ok_or_else(|| unavailable("the dedicated daemon reported no output list"))?;
        let mut outputs = Vec::with_capacity(array.len());
        for entry in array {
            let Some(id) = entry
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue;
            };
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let selected = entry
                .get("selected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            outputs.push(OwnToneOutput { id, name, selected });
        }
        Ok(outputs)
    }

    /// `PUT /api/outputs/set` rewrites the server-wide enabled set: it enables
    /// exactly `ids` and disables every other output (§4.3).
    fn set_outputs(&self, ids: &[u64]) -> Result<(), SenderError> {
        let body = serde_json::json!({
            "outputs": ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        });
        self.put_json("/api/outputs/set", &body)
    }

    fn clear_queue(&self) -> Result<(), SenderError> {
        self.put("/api/queue/clear")
    }

    /// The daemon's coarse player state (`play`, `pause`, `stop`).
    fn player_state(&self) -> Result<String, SenderError> {
        let value = self.get_json("/api/player")?;
        Ok(value
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    fn player_progress(&self) -> Result<(String, Option<u64>), SenderError> {
        let value = self.get_json("/api/player")?;
        let state = value
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let progress = value.get("item_progress_ms").and_then(|v| v.as_u64());
        Ok((state, progress))
    }

    fn player_control(&self, action: &str) -> Result<(), SenderError> {
        self.put(&format!("/api/player/{action}"))
    }

    fn set_volume(&self, percent: u8) -> Result<(), SenderError> {
        self.put_json(
            "/api/player/volume",
            &serde_json::json!({ "volume": percent }),
        )
    }
}

/// The pre-takeover state persisted before the first mutating step, so a
/// crashed holder's daemon can be restored (§4.3, §9 item 9).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TakeoverRecord {
    enabled_outputs: Vec<u64>,
    selected_output: u64,
}

impl TakeoverRecord {
    fn write(&self, path: &Path) -> Result<(), SenderError> {
        let body = serde_json::to_vec(self)
            .map_err(|_| unavailable("the takeover record could not be serialized"))?;
        std::fs::write(path, body)
            .map_err(|_| unavailable("the takeover record could not be persisted"))
    }

    fn read(path: &Path) -> Option<Self> {
        let body = std::fs::read(path).ok()?;
        serde_json::from_slice(&body).ok()
    }
}

/// Ensure the configured pipe exists and is a FIFO. Missing parents are
/// created; an existing non-FIFO path is refused rather than replaced.
fn ensure_pipe(path: &Path) -> Result<(), SenderError> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_fifo() => Ok(()),
        Ok(_) => Err(unavailable("the configured pipe path is not a FIFO")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| unavailable("the configured pipe has no parent directory"))?;
            std::fs::create_dir_all(parent)
                .map_err(|_| unavailable("the pipe directory could not be created"))?;
            rustix::fs::mkfifoat(rustix::fs::CWD, path, Mode::RUSR | Mode::WUSR)
                .map_err(|_| unavailable("the configured pipe could not be created"))?;
            Ok(())
        }
        Err(_) => Err(unavailable("the configured pipe could not be inspected")),
    }
}

/// Distinguishes a cancellation observed while waiting from a real failure, so
/// the FIFO wait can be aborted ("cancellation must be silent").
enum CancelOrError {
    Cancelled,
    Failed(SenderError),
}

/// Open the pipe write end, waiting (bounded) for the daemon's reader.
///
/// The write end is opened non-blocking first — `ENXIO` means the daemon has
/// not opened the read end yet — then switched to blocking mode so a full pipe
/// produces natural backpressure in the streaming thread instead of an error.
/// The wait is raced against `cancel`, so a Stop or replacement aborts it
/// rather than leaving the open blocked on a daemon that never opens the pipe
/// (review F2).
fn open_pipe_write(
    path: &Path,
    deadline: Instant,
    cancel: &OpenCancel,
) -> Result<OwnedFd, CancelOrError> {
    loop {
        if cancel.is_cancelled() {
            return Err(CancelOrError::Cancelled);
        }
        match rustix::fs::open(
            path,
            OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let mut flags = rustix::fs::fcntl_getfl(&fd).map_err(|_| {
                    CancelOrError::Failed(unavailable("the pipe write end could not be configured"))
                })?;
                flags.remove(OFlags::NONBLOCK);
                rustix::fs::fcntl_setfl(&fd, flags).map_err(|_| {
                    CancelOrError::Failed(unavailable("the pipe write end could not be configured"))
                })?;
                return Ok(fd);
            }
            Err(rustix::io::Errno::NXIO) => {
                if Instant::now() >= deadline {
                    return Err(CancelOrError::Failed(unavailable(
                        "the dedicated daemon is not reading the pipe",
                    )));
                }
                std::thread::sleep(FIFO_OPEN_POLL);
            }
            Err(_) => {
                return Err(CancelOrError::Failed(unavailable(
                    "the pipe write end could not be opened",
                )))
            }
        }
    }
}

/// Per-session shared state, driven by the decode pump and read by the seam.
struct SessionInner {
    client: OwnToneClient,
    config: OwnToneConfig,
    generation: PlayerEventGeneration,
    event_tx: async_channel::Sender<PlayerEvent>,
    recorded: TakeoverRecord,
    /// The app-owned proxy that minted this load's loopback route. The session
    /// adopts the ticket so every terminal path revokes it by identity
    /// (§4.1), matching the GStreamer adapter's contract.
    media_proxy: Arc<GstreamerMediaProxy>,
    media_ticket: Option<Arc<GstreamerMediaTicket>>,
    running: AtomicBool,
    restored: AtomicBool,
    position: Mutex<SenderPosition>,
    state: Mutex<PlayerState>,
    pipeline: Mutex<Option<gst::Pipeline>>,
}

impl SessionInner {
    fn publish_state(&self, state: PlayerState) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = state;
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(self.generation, state));
    }

    /// Restore the daemon to the state recorded at takeover and remove the
    /// incomplete-takeover record. Idempotent on success.
    ///
    /// On any failed restoration step the record is **left in place** and the
    /// loopback route is **not** revoked (review F3): a failed mutation unwind
    /// must not erase the evidence a later holder or the supervisor needs, and
    /// it must not release exclusive ownership while the daemon may still be
    /// half-taken-over. The route is revoked only after restoration has
    /// actually completed.
    fn restore(&self) {
        if self.restored.load(Ordering::SeqCst) {
            return;
        }
        if restore_daemon(&self.client, &self.config, &self.recorded).is_err() {
            return;
        }
        // Revoke this load's loopback route by identity only after the daemon
        // has been restored: the route stays valid for every request the
        // daemon might still be applying (§4.1, §4.3).
        if let Some(ticket) = self.media_ticket.as_ref() {
            self.media_proxy.revoke_if_current(ticket);
        }
        self.restored.store(true, Ordering::SeqCst);
    }
}

/// Restore the dedicated daemon to the state recorded before takeover and
/// remove the incomplete-takeover record. Shared by the live-session restore
/// path and the serialized recovery that a `RecoveryPending` open leaves
/// behind. Returns `Err` and leaves the record in place on any failed step.
fn restore_daemon(
    client: &OwnToneClient,
    config: &OwnToneConfig,
    recorded: &TakeoverRecord,
) -> Result<(), SenderError> {
    let mut first_error: Option<SenderError> = None;
    if let Err(error) = client.player_control("stop") {
        warn!(reason = %error.message(), "OwnTone restore: player stop failed");
        first_error.get_or_insert(error);
    }
    if let Err(error) = client.set_outputs(&recorded.enabled_outputs) {
        warn!(reason = %error.message(), "OwnTone restore: enabled-output set failed");
        first_error.get_or_insert(error);
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    if let Err(error) = std::fs::remove_file(config.takeover_record()) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!("OwnTone restore: takeover record removal failed");
            return Err(unavailable("the takeover record could not be cleared"));
        }
    }
    Ok(())
}

/// One live OwnTone session. The decode pump owns the prepared URI; pushed PCM
/// is unused because the pipeline sources its own decoder.
struct OwnToneSession {
    inner: Arc<SessionInner>,
    pump: Option<std::thread::JoinHandle<()>>,
    lock: Option<std::fs::File>,
}

impl SessionInner {
    fn pipeline(&self) -> Option<gst::Pipeline> {
        self.pipeline
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// The pump's three exits must not be conflated (§4.3): backpressure is the
/// pipeline's blocking write, terminal session loss is a bus error, and
/// natural end-of-stream is the decoder's own EOS — never inferred from a
/// write result.
fn run_pump(inner: Arc<SessionInner>, pipeline: gst::Pipeline, write_fd: OwnedFd) {
    // The write end is owned here so the natural-EOS path can close it *before*
    // waiting for daemon completion (review F4). If it stayed open across the
    // wait, the daemon's FIFO reader would never observe EOF and a finite track
    // would fall into the drain-deadline failure path instead of `TrackEnded`.
    let mut write_fd = Some(write_fd);
    let Some(bus) = pipeline.bus() else {
        inner.publish_state(PlayerState::Stopped);
        let _ = inner.event_tx.try_send(PlayerEvent::error(
            inner.generation,
            unavailable("the decode pipeline has no bus")
                .message()
                .to_string(),
        ));
        return;
    };

    if pipeline.set_state(gst::State::Playing).is_err() {
        let _ = inner.event_tx.try_send(PlayerEvent::error(
            inner.generation,
            "OwnTone decode pipeline failed to start".to_string(),
        ));
        inner.publish_state(PlayerState::Stopped);
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }
    inner.publish_state(PlayerState::Playing);

    let mut last_position = Instant::now();
    let duration_ms = pipeline
        .query_duration::<gst::ClockTime>()
        .map(|duration| duration.mseconds());
    {
        let mut position = inner.position.lock().unwrap_or_else(|p| p.into_inner());
        position.duration_ms = duration_ms;
    }

    loop {
        if !inner.running.load(Ordering::SeqCst) {
            break;
        }
        if let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(100),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            match message.view() {
                gst::MessageView::Eos(..) => {
                    // Drain and close the actual write end *before* waiting for
                    // the daemon: the reader must see EOF to finish naturally
                    // (review F4). `drop` on the owned descriptor is the close.
                    drop(write_fd.take());
                    natural_completion(&inner, &pipeline);
                    break;
                }
                gst::MessageView::Error(..) => {
                    error!("OwnTone decode pump failed");
                    // Stop the decode pipeline before revoking its loopback
                    // route so an intentional teardown is never mistaken for a
                    // transient fetch error (§4.4).
                    let _ = pipeline.set_state(gst::State::Null);
                    let _ = inner.event_tx.try_send(PlayerEvent::error(
                        inner.generation,
                        "AirPlay playback failed".to_string(),
                    ));
                    inner.publish_state(PlayerState::Stopped);
                    inner.restore();
                    break;
                }
                _ => {}
            }
        }

        if last_position.elapsed() >= POSITION_INTERVAL {
            last_position = Instant::now();
            sample_position(&inner);
        }
    }

    // Stop the pipeline before releasing the write end so an intentional close
    // never surfaces as a transient fetch error (§4.4, `close_session`).
    let _ = pipeline.set_state(gst::State::Null);
    drop(write_fd.take());
}

/// Publish the current daemon position into the observation cache. Position is
/// sampled from the daemon; duration is the decode pipeline's (§7).
fn sample_position(inner: &SessionInner) {
    let Ok((state, progress)) = inner.client.player_progress() else {
        let mut snapshot = inner.position.lock().unwrap_or_else(|p| p.into_inner());
        snapshot.stale = true;
        return;
    };
    let duration_ms = inner
        .pipeline()
        .and_then(|pipeline| pipeline.query_duration::<gst::ClockTime>())
        .map(|duration| duration.mseconds());
    let mut snapshot = inner.position.lock().unwrap_or_else(|p| p.into_inner());
    snapshot.position_ms = progress;
    if duration_ms.is_some() {
        snapshot.duration_ms = duration_ms;
    }
    snapshot.stale = false;
    let state = match state.as_str() {
        "play" => PlayerState::Playing,
        "pause" => PlayerState::Paused,
        _ => PlayerState::Stopped,
    };
    drop(snapshot);
    if let Some(position_ms) = progress {
        let _ = inner.event_tx.try_send(PlayerEvent::position(
            inner.generation,
            position_ms,
            duration_ms.unwrap_or(0),
        ));
    }
    *inner.state.lock().unwrap_or_else(|p| p.into_inner()) = state;
}

/// Natural EOS: close the write end so the daemon sees end-of-input, wait
/// (bounded) for daemon-confirmed completion, restore, then publish exactly one
/// generation-scoped `TrackEnded` (§4.3, §9 item 10). A drain deadline miss or
/// transport loss is terminal failure, never completion.
///
/// The caller has already closed the pipe write end via the owned descriptor it
/// passed in; this function only waits. Completion requires the daemon to
/// report `stop` — `pause` is a user-visible state, not a finished item, and
/// treating any state other than `play` as success reported a paused track as
/// completed (review F4).
fn natural_completion(inner: &SessionInner, pipeline: &gst::Pipeline) {
    pipeline.set_state(gst::State::Null).ok();
    let deadline = Instant::now() + DRAIN_DEADLINE;
    loop {
        match inner.client.player_state() {
            Ok(state) if state == "stop" => break,
            Ok(_) => {}
            Err(_) => {
                let _ = inner.event_tx.try_send(PlayerEvent::error(
                    inner.generation,
                    "AirPlay completion could not be confirmed".to_string(),
                ));
                inner.publish_state(PlayerState::Stopped);
                inner.restore();
                return;
            }
        }
        if Instant::now() >= deadline {
            let _ = inner.event_tx.try_send(PlayerEvent::error(
                inner.generation,
                "AirPlay completion timed out".to_string(),
            ));
            inner.publish_state(PlayerState::Stopped);
            inner.restore();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    inner.restore();
    inner.publish_state(PlayerState::Stopped);
    let _ = inner
        .event_tx
        .try_send(PlayerEvent::ended(inner.generation));
}

impl SenderSession for OwnToneSession {
    fn write_pcm(&mut self, samples: &[u8]) -> SenderWriteOutcome {
        // The pump owns decode and writes through the pipeline, so pushed PCM
        // is unused (the same pipeline-sourced contract the GStreamer adapter
        // documents).
        SenderWriteOutcome::Accepted(samples.len())
    }

    fn set_volume(&mut self, level: f64) {
        let percent = (level.clamp(0.0, 1.0) * 100.0).round() as u8;
        if let Err(error) = self.inner.client.set_volume(percent) {
            debug!(reason = %error.message(), "OwnTone volume change failed");
        }
    }

    fn pause(&mut self) {
        if self.inner.client.player_control("pause").is_ok() {
            self.inner.publish_state(PlayerState::Paused);
        }
    }

    fn resume(&mut self) {
        if self.inner.client.player_control("play").is_ok() {
            self.inner.publish_state(PlayerState::Playing);
        }
    }

    fn flush(&mut self) {
        // The daemon owns buffering; there is no local flush to perform.
    }

    fn observe(&self) -> SenderPosition {
        *self
            .inner
            .position
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn state(&self) -> PlayerState {
        *self.inner.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn close(self: Box<Self>) {
        let this = *self;
        this.inner.running.store(false, Ordering::SeqCst);
        if let Some(pipeline) = this.inner.pipeline() {
            let _ = pipeline.set_state(gst::State::Null);
        }
        if let Some(handle) = this.pump {
            let _ = handle.join();
        }
        this.inner.restore();
        // Dropping the lock file releases the advisory lock only after
        // restoration has completed (§4.3).
        drop(this.lock);
    }
}

/// The OwnTone 29.x transmission path.
pub(super) struct OwnToneSender {
    config: Option<OwnToneConfig>,
}

impl OwnToneSender {
    /// Resolve the adapter from explicit configuration. A missing or invalid
    /// configuration yields a sender whose `probe` fails closed.
    pub(super) fn from_env() -> Self {
        let config = match OwnToneConfig::from_env() {
            Ok(config) => {
                info!(api = %config.api_base, "OwnTone AirPlay sender configured");
                Some(config)
            }
            Err(error) => {
                debug!(reason = %error.message(), "OwnTone AirPlay sender not configured");
                None
            }
        };
        Self { config }
    }

    /// `true` when the process is configured to select this adapter.
    pub(super) fn selected() -> bool {
        selection_is_owntone(std::env::var(ENV_SELECT).ok().as_deref())
    }
}

impl AirplaySender for OwnToneSender {
    fn name(&self) -> &'static str {
        "owntone"
    }

    fn probe(&self) -> Result<(), SenderError> {
        if !platform_available() {
            return Err(unavailable(
                "this platform has no supported OwnTone acquisition path",
            ));
        }
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| unavailable("not configured"))?;
        config.verify_owned()?;
        if !config.binary.is_file() {
            return Err(unavailable("the owntone binary was not found"));
        }
        let client = OwnToneClient::new(&config.api_base)?;
        let (major, _minor) = client.version()?;
        if major < OWNTONE_MIN_MAJOR {
            return Err(unavailable("the dedicated daemon is older than 29.x"));
        }
        ensure_pipe(&config.pipe_path)?;
        Ok(())
    }

    fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome {
        if ctx.cancel.is_cancelled() {
            return OpenOutcome::Cancelled;
        }
        let Some(config) = self.config.clone() else {
            return OpenOutcome::Failed(unavailable("not configured"));
        };
        open(config, ctx)
    }
}

/// Acquire exclusivity, map the receiver, record the pre-takeover state, take
/// the daemon over, and start the decode pump (§4.3).
///
/// Every blocking step is bounded and cancellation is observed between steps;
/// a cancellation or failure after the first mutating RPC unwinds through
/// [`fail_outcome`]/[`cancel_outcome`], which preserve the incomplete-takeover
/// record and serialize recovery when restoration cannot be completed (review
/// F2, review F3).
fn open(config: OwnToneConfig, ctx: &SenderOpenContext) -> OpenOutcome {
    let deadline = Instant::now() + OPEN_DEADLINE;
    let client = match OwnToneClient::new(&config.api_base) {
        Ok(client) => client,
        Err(error) => return OpenOutcome::Failed(error),
    };
    if let Err(error) = config.verify_owned() {
        return OpenOutcome::Failed(error);
    }
    if ctx.cancel.is_cancelled() {
        return OpenOutcome::Cancelled;
    }

    // Exclusivity is locked before the first state read.
    let lock = match open_lock(&config.lock_path()) {
        Ok(lock) => lock,
        Err(error) => return OpenOutcome::Failed(error),
    };
    if rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
        return OpenOutcome::Failed(unavailable(
            "another Tributary session is already using the dedicated daemon",
        ));
    }

    // A record left by a crashed holder means the daemon may be half-taken
    // over. Refuse and let the supervisor (or the next opener) recover rather
    // than adopting it silently (§4.3).
    if config.takeover_record().exists() {
        return OpenOutcome::Failed(unavailable(
            "a previous takeover is incomplete and must be recovered first",
        ));
    }
    if ctx.cancel.is_cancelled() {
        return OpenOutcome::Cancelled;
    }

    // Read phase: nothing has been mutated, so a failure or cancellation here
    // has no takeover to unwind.
    let outputs = match client.outputs() {
        Ok(outputs) => outputs,
        Err(error) => return OpenOutcome::Failed(error),
    };
    let selected = match map_receiver_to_output(&outputs, ctx.target.device_id.as_deref()) {
        Ok(selected) => selected,
        Err(failure) => return OpenOutcome::Failed(unavailable(&failure.to_string())),
    };

    // Never preempt audible playback on the dedicated instance.
    match client.player_state() {
        Ok(state) if state == "play" => {
            return OpenOutcome::Failed(unavailable("the dedicated daemon is already playing"));
        }
        Ok(_) => {}
        Err(error) => return OpenOutcome::Failed(error),
    }
    if ctx.cancel.is_cancelled() {
        return OpenOutcome::Cancelled;
    }

    // Mutation phase: every failure or cancellation from here unwinds.
    let recorded = TakeoverRecord {
        enabled_outputs: outputs
            .iter()
            .filter(|output| output.selected)
            .map(|output| output.id)
            .collect(),
        selected_output: selected,
    };
    if let Err(error) = recorded.write(&config.takeover_record()) {
        return OpenOutcome::Failed(error);
    }

    let mut unsettled = false;
    if ctx.cancel.is_cancelled() {
        return cancel_outcome(client, config, recorded, lock, unsettled);
    }
    if let Err(error) = client.set_outputs(&[selected]) {
        // A mutating RPC that returned an error may still have been applied
        // server-side, so this is not a clean unwind (review F3).
        unsettled = true;
        return fail_outcome(client, config, recorded, lock, unsettled, error);
    }
    if ctx.cancel.is_cancelled() {
        return cancel_outcome(client, config, recorded, lock, unsettled);
    }
    if let Err(error) = client.clear_queue() {
        unsettled = true;
        return fail_outcome(client, config, recorded, lock, unsettled, error);
    }
    if ctx.cancel.is_cancelled() {
        return cancel_outcome(client, config, recorded, lock, unsettled);
    }

    let write_fd = match open_pipe_write(&config.pipe_path, deadline, &ctx.cancel) {
        Ok(fd) => fd,
        Err(CancelOrError::Cancelled) => {
            return cancel_outcome(client, config, recorded, lock, unsettled);
        }
        Err(CancelOrError::Failed(error)) => {
            return fail_outcome(client, config, recorded, lock, unsettled, error);
        }
    };
    if ctx.cancel.is_cancelled() {
        drop(write_fd);
        return cancel_outcome(client, config, recorded, lock, unsettled);
    }

    let pipeline = match build_pipeline(&ctx.prepared_uri, &write_fd) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            return fail_outcome(client, config, recorded, lock, unsettled, error);
        }
    };

    let inner = Arc::new(SessionInner {
        client,
        config,
        generation: ctx.generation,
        event_tx: ctx.event_tx.clone(),
        recorded,
        media_proxy: Arc::clone(&ctx.media_proxy),
        media_ticket: ctx.media_ticket.clone(),
        running: AtomicBool::new(true),
        restored: AtomicBool::new(false),
        position: Mutex::new(SenderPosition::unknown(ctx.generation)),
        state: Mutex::new(PlayerState::Buffering),
        pipeline: Mutex::new(Some(pipeline.clone())),
    });

    let pump_inner = Arc::clone(&inner);
    let pump = std::thread::Builder::new()
        .name("airplay-owntone-pump".to_string())
        .spawn(move || run_pump(pump_inner, pipeline, write_fd));
    let Ok(pump) = pump else {
        // Worker-spawn failure *after* takeover: reclaim the session state and
        // unwind, so a half-taken-over daemon is never leaked (review F3).
        let error = unavailable("the decode pump could not be started");
        return match Arc::try_unwrap(inner) {
            Ok(session) => fail_outcome(
                session.client,
                session.config,
                session.recorded,
                lock,
                unsettled,
                error,
            ),
            Err(inner) => {
                let _ = restore_daemon(&inner.client, &inner.config, &inner.recorded);
                OpenOutcome::Failed(error)
            }
        };
    };

    OpenOutcome::Opened(Box::new(OwnToneSession {
        inner,
        pump: Some(pump),
        lock: Some(lock),
    }))
}

/// Handle a failure after the first mutating RPC. A clean restoration inside
/// the cleanup deadline returns the original failure; otherwise the record is
/// preserved and recovery is serialized behind a `RecoveryPending` failure
/// (review F3).
fn fail_outcome(
    client: OwnToneClient,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    unsettled: bool,
    original: SenderError,
) -> OpenOutcome {
    if !unsettled {
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        if settle_restore(&client, &config, &recorded, deadline) {
            drop(lock);
            return OpenOutcome::Failed(original);
        }
    }
    OpenOutcome::Failed(recovery_pending(client, config, recorded, lock))
}

/// Handle a cancellation after the first mutating RPC. A clean restoration
/// returns `Cancelled` silently; an unsettled mutation or failed restoration
/// returns the non-clean `RecoveryPending` failure (review F2, review F3).
fn cancel_outcome(
    client: OwnToneClient,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    unsettled: bool,
) -> OpenOutcome {
    if !unsettled {
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        if settle_restore(&client, &config, &recorded, deadline) {
            drop(lock);
            return OpenOutcome::Cancelled;
        }
    }
    OpenOutcome::Failed(recovery_pending(client, config, recorded, lock))
}

/// Attempt restoration until `deadline`. Used as the "settle" half of
/// settle-or-restart: a mutating RPC that settled on its own is compensated by
/// re-running restoration, after which ownership can be released cleanly.
fn settle_restore(
    client: &OwnToneClient,
    config: &OwnToneConfig,
    recorded: &TakeoverRecord,
    deadline: Instant,
) -> bool {
    loop {
        if restore_daemon(client, config, recorded).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(RECOVERY_POLL);
    }
}

/// Build the non-clean recovery-pending failure and start the serialized
/// recovery that keeps retrying restoration — and leaves the incomplete-
/// takeover record for the supervisor — until the recovery deadline. The
/// recovery owns the advisory lock until its terminal outcome, so no next
/// opener can interleave with an unsettled request (review F3).
fn recovery_pending(
    client: OwnToneClient,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
) -> SenderError {
    let completion = RecoveryCompletion::default();
    let worker = completion.clone();
    let message = unavailable("recovery is pending for the dedicated daemon")
        .message()
        .to_string();
    let spawned = std::thread::Builder::new()
        .name("airplay-owntone-recovery".to_string())
        .spawn(move || {
            // Hold the advisory lock until recovery is terminal so ownership is
            // never released early.
            let _lock_guard = lock;
            let deadline = Instant::now() + RECOVERY_DEADLINE;
            loop {
                if restore_daemon(&client, &config, &recorded).is_ok() {
                    worker.resolve(RecoveryOutcome::Restored);
                    return;
                }
                if Instant::now() >= deadline {
                    worker.resolve(RecoveryOutcome::RestorationFailed {
                        message: "restoration did not complete before the recovery deadline"
                            .to_string(),
                    });
                    return;
                }
                std::thread::sleep(RECOVERY_POLL);
            }
        });
    if spawned.is_err() {
        // No recovery owner exists; report a terminal failure now rather than
        // leaving a waiter pending. The record remains in place for the
        // supervisor.
        completion.resolve(RecoveryOutcome::RestorationFailed {
            message: "the serialized recovery could not be started".to_string(),
        });
    }
    SenderError::RecoveryPending {
        message,
        completion,
    }
}

/// Build the headless decode pipeline that writes s16le 44100 Hz stereo PCM
/// into the pipe's write end with `fdsink` (§4.3). The loopback source policy
/// keeps a protected ticket's URI inside Tributary's process on direct
/// routing, exactly as the GStreamer adapter does.
fn build_pipeline(uri: &str, write_fd: &OwnedFd) -> Result<gst::Pipeline, SenderError> {
    let _ = gst::init();
    let description = format!(
        "uridecodebin name=decoder uri=\"{}\" ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=2 ! fdsink name=sink fd={}",
        uri.replace('"', "\\\""),
        write_fd.as_raw_fd(),
    );
    let element = gst::parse::launch(&description)
        .map_err(|_| unavailable("the decode pipeline could not be constructed"))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| unavailable("the decode pipeline is not a pipeline"))?;
    if let Some(decoder) = pipeline.by_name("decoder") {
        super::Player::install_loopback_http_source_policy(&decoder);
    }
    Ok(pipeline)
}

/// Open or create the advisory lock file and return it with the lock not yet
/// taken (the caller flocks it). The file descriptor stays open for the
/// session's lifetime.
fn open_lock(path: &Path) -> Result<std::fs::File, SenderError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| unavailable("the instance state directory could not be created"))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|_| unavailable("the instance lock file could not be opened"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_accepts_common_forms() {
        assert_eq!(parse_version("29.3"), Some((29, 3)));
        assert_eq!(parse_version("29.10"), Some((29, 10)));
        assert_eq!(parse_version("v29.3"), Some((29, 3)));
        assert_eq!(parse_version("29"), Some((29, 0)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn device_identifier_normalizes_to_numeric_id() {
        assert_eq!(
            normalize_device_identifier("8EE58A500A56"),
            Some(0x8EE58A500A56)
        );
        assert_eq!(
            normalize_device_identifier("8e:e5:8a:50:0a:56"),
            Some(0x8EE58A500A56)
        );
        assert_eq!(
            normalize_device_identifier("8E-E5-8A-50-0A-56"),
            Some(0x8EE58A500A56)
        );
        assert_eq!(normalize_device_identifier("ABCD"), None);
        assert_eq!(normalize_device_identifier("not-a-mac"), None);
        assert_eq!(normalize_device_identifier(""), None);
    }

    #[test]
    fn mapping_uses_the_identifier_never_the_name() {
        let outputs = vec![
            OwnToneOutput {
                id: 0x8EE58A500A56,
                name: "Living Room".to_string(),
                selected: false,
            },
            OwnToneOutput {
                id: 0x112233445566,
                name: "Living Room".to_string(),
                selected: true,
            },
        ];
        // Two same-named receivers resolve by identifier to the exact one.
        assert_eq!(
            map_receiver_to_output(&outputs, Some("8EE58A500A56")),
            Ok(0x8EE58A500A56)
        );
        assert_eq!(
            map_receiver_to_output(&outputs, Some("112233445566")),
            Ok(0x112233445566)
        );
    }

    #[test]
    fn mapping_fails_closed_without_an_exact_single_match() {
        let outputs = vec![OwnToneOutput {
            id: 0x112233445566,
            name: "Kitchen".to_string(),
            selected: false,
        }];
        assert_eq!(
            map_receiver_to_output(&outputs, None),
            Err(MappingFailure::MissingIdentifier)
        );
        assert_eq!(
            map_receiver_to_output(&outputs, Some("not-a-mac")),
            Err(MappingFailure::MissingIdentifier)
        );
        assert_eq!(
            map_receiver_to_output(&outputs, Some("8EE58A500A56")),
            Err(MappingFailure::NoMatch(0x8EE58A500A56))
        );
    }

    #[test]
    fn loopback_hosts_are_recognized() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("owntone.example"));
    }

    #[test]
    fn takeover_record_round_trips() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("record.json");
        let record = TakeoverRecord {
            enabled_outputs: vec![1, 2],
            selected_output: 2,
        };
        record.write(&path).expect("write");
        assert_eq!(TakeoverRecord::read(&path).as_ref(), Some(&record));
    }

    #[test]
    fn unavailable_message_names_the_reason_in_every_catalog() {
        let english = unavailable_in("en", "not configured").message().to_string();
        assert!(english.contains("not configured"), "{english}");
        assert!(english.contains("OwnTone"), "{english}");

        for locale in rust_i18n::available_locales!() {
            let message = unavailable_in(&locale, "not configured");
            let message = message.message();
            assert!(!message.is_empty(), "{locale} is empty");
            assert!(message.contains("not configured"), "{locale}: {message}");
            if locale != "en" {
                assert_ne!(message, english, "{locale} must not fall back to English");
            }
        }
    }

    #[test]
    fn selection_is_explicit_and_exact() {
        assert!(selection_is_owntone(Some("owntone")));
        assert!(selection_is_owntone(Some("OwnTone")));
        assert!(selection_is_owntone(Some("OWNTONE")));
        assert!(!selection_is_owntone(Some("raopsink")));
        assert!(!selection_is_owntone(Some("")));
        assert!(!selection_is_owntone(None));
    }

    #[test]
    fn unconfigured_sender_probe_fails_closed() {
        let sender = OwnToneSender { config: None };
        let error = sender.probe().expect_err("unconfigured sender must refuse");
        let expected = if platform_available() {
            "not configured"
        } else {
            "no supported OwnTone acquisition path"
        };
        assert!(error.message().contains(expected), "{}", error.message());
    }

    #[test]
    fn loopback_verification_accepts_only_loopback_endpoints() {
        let config = |api: &str| OwnToneConfig {
            api_base: api.to_string(),
            pipe_path: PathBuf::from("/run/tributary/airplay.pcm"),
            state_dir: PathBuf::from("/run/tributary"),
            binary: PathBuf::from("/usr/bin/owntone"),
        };
        assert!(config("http://127.0.0.1:3689").verify_loopback().is_ok());
        assert!(config("http://localhost:3689").verify_loopback().is_ok());
        assert!(config("http://[::1]:3689").verify_loopback().is_ok());
        assert!(config("http://192.168.1.10:3689")
            .verify_loopback()
            .is_err());
        assert!(config("https://owntone.example:3689")
            .verify_loopback()
            .is_err());
        assert!(config("not a url").verify_loopback().is_err());
    }

    #[test]
    fn ensure_pipe_creates_a_fifo_and_refuses_a_regular_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pipe = directory.path().join("airplay.pcm");
        ensure_pipe(&pipe).expect("create fifo");
        use std::os::unix::fs::FileTypeExt;
        assert!(std::fs::symlink_metadata(&pipe)
            .expect("stat")
            .file_type()
            .is_fifo());

        let file = directory.path().join("regular");
        std::fs::write(&file, b"not a fifo").expect("write");
        assert!(ensure_pipe(&file).is_err());
    }

    fn owned_config(dir: &Path) -> OwnToneConfig {
        OwnToneConfig {
            api_base: "http://127.0.0.1:3689".to_string(),
            pipe_path: dir.join("airplay.pcm"),
            state_dir: dir.to_path_buf(),
            binary: PathBuf::from("/usr/bin/owntone"),
        }
    }

    fn write_owner_record(config: &OwnToneConfig) {
        let record = OwnershipRecord {
            token: OWNER_TOKEN.to_string(),
            api_base: config.api_base.clone(),
            pipe_path: config.pipe_path.to_string_lossy().into_owned(),
            state_dir: config.state_dir.to_string_lossy().into_owned(),
            binary: config.binary.to_string_lossy().into_owned(),
        };
        std::fs::write(
            config.owner_marker(),
            serde_json::to_vec(&record).expect("serialize record"),
        )
        .expect("write record");
    }

    /// F5: the ownership record binds the token to the configured endpoint,
    /// pipe, state directory and binary. A valid marker paired with a foreign
    /// API endpoint is refused before any state read or mutation.
    #[test]
    fn ownership_record_binds_endpoint_pipe_state_and_binary() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = owned_config(directory.path());
        write_owner_record(&config);
        assert!(config.verify_owned().is_ok());

        let foreign_endpoint = OwnToneConfig {
            api_base: "http://127.0.0.1:9999".to_string(),
            ..config.clone()
        };
        assert!(foreign_endpoint.verify_owned().is_err());

        let foreign_pipe = OwnToneConfig {
            pipe_path: directory.path().join("other.pcm"),
            ..config.clone()
        };
        assert!(foreign_pipe.verify_owned().is_err());

        let foreign_binary = OwnToneConfig {
            binary: PathBuf::from("/usr/bin/not-owntone"),
            ..config.clone()
        };
        assert!(foreign_binary.verify_owned().is_err());
    }

    #[test]
    fn ownership_record_rejects_a_foreign_token_and_a_missing_record() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = owned_config(directory.path());
        assert!(config.verify_owned().is_err());

        std::fs::write(config.owner_marker(), "not a tributary token").expect("write marker");
        assert!(config.verify_owned().is_err());
    }

    /// F3: a failed restoration leaves the incomplete-takeover record in place
    /// so the supervisor can retry; it is never erased by an unwind that did
    /// not actually complete.
    #[test]
    fn failed_restoration_preserves_the_takeover_record() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = owned_config(directory.path());
        let recorded = TakeoverRecord {
            enabled_outputs: vec![1],
            selected_output: 2,
        };
        recorded
            .write(&config.takeover_record())
            .expect("write takeover record");

        // An unreachable daemon makes every restoration step fail.
        let client = OwnToneClient::new("http://127.0.0.1:1").expect("client");
        assert!(restore_daemon(&client, &config, &recorded).is_err());
        assert!(
            config.takeover_record().exists(),
            "a failed restore must not clear the takeover record"
        );
    }
}
