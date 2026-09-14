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
    AirplaySender, OpenOutcome, SenderError, SenderOpenContext, SenderPosition, SenderSession,
    SenderWriteOutcome,
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

/// Marker content proving the state directory belongs to a Tributary-owned
/// dedicated instance. Written by the installation/service record.
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
    /// API exposes no instance identity, so the ownership marker in the
    /// instance's own state directory is what distinguishes it from a shared
    /// instance that merely looks healthy.
    fn verify_owned(&self) -> Result<(), SenderError> {
        let marker = std::fs::read_to_string(self.owner_marker())
            .map_err(|_| unavailable("the dedicated-instance ownership marker is missing"))?;
        if marker.trim() != OWNER_TOKEN {
            return Err(unavailable(
                "the configured state directory is not a Tributary-owned instance",
            ));
        }
        Ok(())
    }
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

/// Open the pipe write end, waiting (bounded) for the daemon's reader.
///
/// The write end is opened non-blocking first — `ENXIO` means the daemon has
/// not opened the read end yet — then switched to blocking mode so a full pipe
/// produces natural backpressure in the streaming thread instead of an error.
fn open_pipe_write(path: &Path, deadline: Instant) -> Result<OwnedFd, SenderError> {
    loop {
        match rustix::fs::open(
            path,
            OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let mut flags = rustix::fs::fcntl_getfl(&fd)
                    .map_err(|_| unavailable("the pipe write end could not be configured"))?;
                flags.remove(OFlags::NONBLOCK);
                rustix::fs::fcntl_setfl(&fd, flags)
                    .map_err(|_| unavailable("the pipe write end could not be configured"))?;
                return Ok(fd);
            }
            Err(rustix::io::Errno::NXIO) => {
                if Instant::now() >= deadline {
                    return Err(unavailable("the dedicated daemon is not reading the pipe"));
                }
                std::thread::sleep(FIFO_OPEN_POLL);
            }
            Err(_) => return Err(unavailable("the pipe write end could not be opened")),
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
    /// incomplete-takeover record. Idempotent: the first terminal restore wins.
    fn restore(&self) {
        if self.restored.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.client.player_control("stop") {
            warn!(reason = %error.message(), "OwnTone restore: player stop failed");
        }
        if let Err(error) = self.client.set_outputs(&self.recorded.enabled_outputs) {
            warn!(reason = %error.message(), "OwnTone restore: enabled-output set failed");
        }
        if let Err(error) = std::fs::remove_file(self.config.takeover_record()) {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!("OwnTone restore: takeover record removal failed");
            }
        }
        // Revoke this load's loopback route by identity only after the daemon
        // has been restored: the route stays valid for every request the
        // daemon might still be applying (§4.1, §4.3).
        if let Some(ticket) = self.media_ticket.as_ref() {
            self.media_proxy.revoke_if_current(ticket);
        }
    }
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
    drop(write_fd);
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

/// Natural EOS: drain, close the write end so the daemon sees end-of-input,
/// wait (bounded) for daemon-confirmed completion, restore, then publish
/// exactly one generation-scoped `TrackEnded` (§4.3, §9 item 10). A drain
/// deadline miss or transport loss is terminal failure, never completion.
fn natural_completion(inner: &SessionInner, pipeline: &gst::Pipeline) {
    pipeline.set_state(gst::State::Null).ok();
    let deadline = Instant::now() + DRAIN_DEADLINE;
    loop {
        match inner.client.player_state() {
            Ok(state) if state != "play" => break,
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

    fn open_session(&self, ctx: &SenderOpenContext<'_>) -> OpenOutcome {
        if ctx.cancel.is_cancelled() {
            return OpenOutcome::Cancelled;
        }
        let Some(config) = self.config.clone() else {
            return OpenOutcome::Failed(unavailable("not configured"));
        };
        match open(config, ctx) {
            Ok(session) => OpenOutcome::Opened(Box::new(session)),
            Err(error) => OpenOutcome::Failed(error),
        }
    }
}

/// Acquire exclusivity, map the receiver, record the pre-takeover state, take
/// the daemon over, and start the decode pump (§4.3).
fn open(config: OwnToneConfig, ctx: &SenderOpenContext<'_>) -> Result<OwnToneSession, SenderError> {
    let deadline = Instant::now() + OPEN_DEADLINE;
    let client = OwnToneClient::new(&config.api_base)?;
    config.verify_owned()?;

    // Exclusivity is locked before the first state read.
    let lock = open_lock(&config.lock_path())?;
    rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|_| {
        unavailable("another Tributary session is already using the dedicated daemon")
    })?;

    // A record left by a crashed holder means the daemon may be half-taken
    // over. Refuse and let the supervisor (or the next opener) recover rather
    // than adopting it silently (§4.3).
    if config.takeover_record().exists() {
        return Err(unavailable(
            "a previous takeover is incomplete and must be recovered first",
        ));
    }

    let outputs = client.outputs()?;
    let selected = map_receiver_to_output(&outputs, ctx.target.device_id.as_deref())
        .map_err(|failure| unavailable(&failure.to_string()))?;

    // Never preempt audible playback on the dedicated instance.
    if client.player_state()? == "play" {
        return Err(unavailable("the dedicated daemon is already playing"));
    }

    let recorded = TakeoverRecord {
        enabled_outputs: outputs
            .iter()
            .filter(|output| output.selected)
            .map(|output| output.id)
            .collect(),
        selected_output: selected,
    };
    recorded.write(&config.takeover_record())?;

    if let Err(error) = client.set_outputs(&[selected]) {
        let _ = std::fs::remove_file(config.takeover_record());
        return Err(error);
    }
    if let Err(error) = client.clear_queue() {
        let _ = client.set_outputs(&recorded.enabled_outputs);
        let _ = std::fs::remove_file(config.takeover_record());
        return Err(error);
    }

    let write_fd = match open_pipe_write(&config.pipe_path, deadline) {
        Ok(fd) => fd,
        Err(error) => {
            let _ = client.set_outputs(&recorded.enabled_outputs);
            let _ = std::fs::remove_file(config.takeover_record());
            return Err(error);
        }
    };

    let pipeline = match build_pipeline(ctx.prepared_uri, &write_fd) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            let _ = client.set_outputs(&recorded.enabled_outputs);
            let _ = std::fs::remove_file(config.takeover_record());
            return Err(error);
        }
    };

    let inner = Arc::new(SessionInner {
        client,
        config,
        generation: ctx.generation,
        event_tx: ctx.event_tx.clone(),
        recorded,
        media_proxy: Arc::clone(ctx.media_proxy),
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
        .spawn(move || run_pump(pump_inner, pipeline, write_fd))
        .map_err(|_| unavailable("the decode pump could not be started"))?;

    Ok(OwnToneSession {
        inner,
        pump: Some(pump),
        lock: Some(lock),
    })
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
}
