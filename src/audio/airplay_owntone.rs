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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use super::airplay_sender::{
    AirplaySender, OpenCancel, OpenOutcome, RecoveryCompletion, RecoveryOutcome, SenderError,
    SenderOpenContext, SenderPosition, SenderSession, SenderWriteOutcome, SessionGate,
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

/// Oldest daemon release the adapter's pinned behaviour was verified against
/// (design pins 29.3: pipe autostart/autostop and completion semantics).
const OWNTONE_MIN_VERSION: (u32, u32) = (29, 3);
/// The only verified major series. A later major is unverified, so it is
/// refused before any daemon mutation rather than assumed compatible.
const OWNTONE_VERIFIED_MAJOR: u32 = 29;

const API_TIMEOUT: Duration = Duration::from_secs(2);
const OPEN_DEADLINE: Duration = Duration::from_secs(10);
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);
/// Cadence of the post-EOF drain's daemon observations.
const DRAIN_POLL: Duration = Duration::from_millis(100);
/// How long the daemon's reported item progress must stand still — while it
/// still reports `play` after end-of-input — before a finite item counts as
/// rendered. Pinned OwnTone 29.3 advances `pos_ms` only for samples it read
/// (`player.c: source_read` → `event_read`) and, once a pipe that is not
/// autostarted runs dry, waits instead of streaming (`inputs/pipe.c: play` →
/// `input_wait`), so a stalled progress under `play` is a drained pipe.
const COMPLETION_PROGRESS_STALL: Duration = Duration::from_secs(1);
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
/// Poll interval for the process-global retained-recovery supervisor (review
/// T2). The supervisor keeps the advisory lock held and retries until
/// settlement, so it never busy-spins.
const RECOVERY_SUPERVISOR_POLL: Duration = Duration::from_millis(500);
/// Bound on terminating the owned instance during quiescence (review R2).
const QUIESCE_TERMINATE_DEADLINE: Duration = Duration::from_secs(5);
/// Bound on confirming the owned instance is gone after `SIGKILL` before
/// quiescence is declared failed (review S3). `SIGKILL` cannot be ignored, so
/// a process still present after this bound is not dying and must not be
/// mistaken for a quiesced instance.
const QUIESCE_KILL_DEADLINE: Duration = Duration::from_secs(5);
/// Bound on the owned instance coming back after quiescence (review R2).
const QUIESCE_RESTART_DEADLINE: Duration = Duration::from_secs(15);

/// Token value inside the dedicated instance's ownership record. The record
/// (see [`OwnershipRecord`]) is written by the installation/service record and
/// binds this token to the configured endpoint, pipe, state directory and
/// binary.
const OWNER_TOKEN: &str = "tributary-airplay-owntone-v1";

/// The localized, actionable message every refusal is built from. `reason`
/// names a key under `errors.playback.airplay_owntone_reason`, so the reason
/// is rendered from the same catalog as the wrapper and a translated sentence
/// never carries an English clause (PR #270 review).
fn unavailable(reason: &str) -> SenderError {
    unavailable_in(rust_i18n::locale().as_ref(), reason)
}

/// Build a refusal for an explicit catalog. Split from [`unavailable`] so the
/// every-catalog localization contract is unit-testable without mutating the
/// process locale.
fn unavailable_in(locale: &str, reason: &str) -> SenderError {
    let key = format!("{REASON_CATALOG}.{reason}");
    let reason_text = rust_i18n::t!(key.as_str(), locale = locale);
    debug_assert!(
        !reason_text.contains(REASON_CATALOG),
        "missing catalog entry for OwnTone refusal reason {reason}"
    );
    unavailable_raw_in(locale, reason_text.as_ref())
}

/// The catalog map holding every refusal reason.
const REASON_CATALOG: &str = "errors.playback.airplay_owntone_reason";

/// A refusal whose reason carries the daemon output identifier the catalog
/// entry names as `%{id}`.
fn unavailable_with_id(reason: &str, id: u64) -> SenderError {
    unavailable_with_id_in(rust_i18n::locale().as_ref(), reason, id)
}

fn unavailable_with_id_in(locale: &str, reason: &str, id: u64) -> SenderError {
    let key = format!("{REASON_CATALOG}.{reason}");
    let reason_text = rust_i18n::t!(key.as_str(), id = id, locale = locale);
    debug_assert!(
        !reason_text.contains(REASON_CATALOG),
        "missing catalog entry for OwnTone refusal reason {reason}"
    );
    unavailable_raw_in(locale, reason_text.as_ref())
}

fn unavailable_raw_in(locale: &str, reason: &str) -> SenderError {
    SenderError::Dependency(
        rust_i18n::t!(
            "errors.playback.airplay_owntone_unavailable",
            reason = reason,
            locale = locale
        )
        .into_owned(),
    )
}

/// The catalog map holding the terminal failures a live session publishes.
const RUNTIME_CATALOG: &str = "errors.playback.airplay_runtime";

/// A terminal failure published through `PlayerEvent::error` after a session
/// opened. It is rendered from the selected catalog at publication and never
/// carried as an English literal (PR #270 review, round 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeFailure {
    /// The decode pipeline failed or could not be driven.
    PlaybackFailed,
    /// The daemon stopped answering while the item drained.
    CompletionUnconfirmed,
    /// The active-drain budget ran out before the daemon reported completion.
    CompletionTimedOut,
    /// The item completed but the daemon could not be restored.
    RestorationFailed,
}

impl RuntimeFailure {
    const fn key(self) -> &'static str {
        match self {
            Self::PlaybackFailed => "playback_failed",
            Self::CompletionUnconfirmed => "completion_unconfirmed",
            Self::CompletionTimedOut => "completion_timed_out",
            Self::RestorationFailed => "restoration_failed",
        }
    }

    fn message(self) -> String {
        self.message_in(rust_i18n::locale().as_ref())
    }

    fn message_in(self, locale: &str) -> String {
        let key = format!("{RUNTIME_CATALOG}.{}", self.key());
        let text = rust_i18n::t!(key.as_str(), locale = locale);
        debug_assert!(
            !text.contains(RUNTIME_CATALOG),
            "missing catalog entry for OwnTone runtime failure {}",
            self.key()
        );
        text.into_owned()
    }
}

/// Whether an explicit `TRIBUTARY_AIRPLAY_SENDER` value selects the OwnTone
/// adapter. Selection is configuration, never a silent fallback (design §4.4),
/// so only the exact configured value counts — and it is split out from the
/// environment read so selection is unit-testable without mutating the
/// process environment.
fn selection_is_owntone(value: Option<&str>) -> bool {
    match value {
        Some(value) => value.trim().eq_ignore_ascii_case("owntone"),
        None => false,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Why explicit configuration was refused: the catalog key of the reason. Kept
/// as a key rather than a rendered message so the refusal is reported in the
/// locale selected when it is shown, and so `probe` can name the operator's
/// actual setup mistake instead of a generic "not configured" (PR #270 review,
/// round 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigRefusal(&'static str);

const fn unavailable_config(reason: &'static str) -> ConfigRefusal {
    ConfigRefusal(reason)
}

impl ConfigRefusal {
    fn error(self) -> SenderError {
        unavailable(self.0)
    }
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
    fn from_env() -> Result<Self, ConfigRefusal> {
        Self::from_values(
            env_nonempty(ENV_API),
            env_nonempty(ENV_PIPE),
            env_nonempty(ENV_STATE_DIR),
            env_nonempty(ENV_BIN),
        )
    }

    /// Resolve the configuration from already-read values. Split from
    /// [`Self::from_env`] so every refusal is unit-testable without mutating
    /// the process environment.
    fn from_values(
        api: Option<String>,
        pipe: Option<String>,
        state: Option<String>,
        binary: Option<String>,
    ) -> Result<Self, ConfigRefusal> {
        let api = api.ok_or(unavailable_config("not_configured"))?;
        let pipe = pipe.ok_or(unavailable_config("not_configured"))?;
        let state = state.ok_or(unavailable_config("not_configured"))?;
        let binary = binary.unwrap_or_else(|| DEFAULT_BIN.to_string());
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
    fn verify_loopback(&self) -> Result<(), ConfigRefusal> {
        let url = url::Url::parse(&self.api_base)
            .map_err(|_| unavailable_config("configured_api_url_is_invalid"))?;
        let host = url.host_str().unwrap_or_default();
        if is_loopback_host(host) && !is_literal_loopback_host(host) {
            // "localhost" resolves to *both* loopback families, so the process
            // the kernel match finds and the address the HTTP client actually
            // dials can be different listeners (review T5). Require the literal
            // address the client will use, so the observed family is the
            // endpoint's family by construction.
            return Err(unavailable_config(
                "json_api_loopback_host_must_be_a_literal_address",
            ));
        }
        if !is_loopback_host(host) {
            return Err(unavailable_config("json_api_must_be_bound_to_loopback"));
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

    /// Read and validate the on-disk ownership record only: the token must be
    /// ours and the record must bind the *configured* endpoint, pipe, state
    /// directory and binary (review F5). This is the local, non-blocking half
    /// of the ownership gate, safe to run synchronously on the GTK caller.
    fn verify_owned_record(&self) -> Result<(), SenderError> {
        let body = std::fs::read_to_string(self.owner_marker())
            .map_err(|_| unavailable("dedicated_instance_ownership_record_is_missing"))?;
        let record: OwnershipRecord = serde_json::from_str(&body)
            .map_err(|_| unavailable("dedicated_instance_ownership_record_is_malformed"))?;
        if record.token != OWNER_TOKEN {
            return Err(unavailable(
                "configured_state_directory_is_not_a_tributary_owned_instance",
            ));
        }
        // The configured endpoint is normalized without its trailing slash;
        // the record is compared the same way, so an installation that writes
        // the URL exactly as it exports it is not refused.
        let matches = record.api_base.trim_end_matches('/') == self.api_base
            && record.pipe_path == self.pipe_path.to_string_lossy()
            && record.state_dir == self.state_dir.to_string_lossy()
            && record.binary == self.binary.to_string_lossy();
        if !matches {
            return Err(unavailable(
                "configured_instance_does_not_match_owned_record",
            ));
        }
        Ok(())
    }

    /// The supervisor restart command the installation record supplied, if
    /// any (review R2).
    fn restart_command(&self) -> Option<String> {
        let body = std::fs::read_to_string(self.owner_marker()).ok()?;
        let record: OwnershipRecord = serde_json::from_str(&body).ok()?;
        record.restart_command
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
    ///
    /// A matching record is still not proof that the *answering process* is
    /// ours: an old record stays valid if the dedicated daemon stops and a
    /// shared instance binds the same port. The kernel's view of the listener
    /// closes that gap (review R5), so this runs on the load worker — the
    /// `/proc` walk is filesystem I/O, not GTK work.
    fn verify_owned(&self) -> Result<(), SenderError> {
        self.verify_owned_record()?;
        verify_daemon_process(self)
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
    /// Optional supervisor restart command. The installation/service record
    /// writes the documented per-user service's restart invocation here; the
    /// adapter executes it after terminating the owned instance during bounded
    /// quiescence (review R2). Absent means the environment is expected to
    /// bring the instance back on its own and the adapter waits for it.
    #[serde(default)]
    restart_command: Option<String>,
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// A loopback host that names exactly one address family, so the `/proc`
/// listener match and the HTTP client dial the same endpoint (review T5). The
/// name `localhost` is deliberately excluded: it is ambiguous.
fn is_literal_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "[::1]")
}

// ---------------------------------------------------------------------------
// Out-of-band process identity and bounded quiescence (review R2, R5)
// ---------------------------------------------------------------------------

/// A process bound to the dedicated instance's loopback API port, resolved
/// through the kernel rather than the JSON API — which exposes no instance
/// identity at all (design §4.3, review R5).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListenerProcess {
    pid: u32,
    /// Kernel process start time, from `/proc/<pid>/stat` field 22. Paired
    /// with the pid it is a stable identity: a recycled pid has a different
    /// start time, so a signal can never reach a foreign successor of the
    /// dedicated instance (review S2).
    start_time: u64,
    exe: PathBuf,
    /// The process's NUL-separated argument vector. Kept as discrete arguments
    /// (not a whitespace-joined string) so the effective configuration option
    /// and its value can be parsed without an argument boundary being forged
    /// with a space (review U5).
    argv: Vec<String>,
}

impl ListenerProcess {
    /// The stable kernel identity a signal must re-verify immediately before
    /// delivery (review S2).
    fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            start_time: self.start_time,
        }
    }
}

/// A pid paired with the kernel start time that makes it stable across pid
/// reuse (review S2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
}

/// The local port of a JSON API base URL.
fn api_port(api_base: &str) -> Option<u16> {
    let url = url::Url::parse(api_base).ok()?;
    url.port_or_known_default()
}

/// The loopback host of a JSON API base URL.
fn api_host(api_base: &str) -> Option<String> {
    let url = url::Url::parse(api_base).ok()?;
    url.host_str().map(str::to_string)
}

/// Acceptable `/proc/net/tcp{,6}` `HEXADDR` renderings for the configured
/// loopback host. The kernel stores IPv4 addresses as a little-endian `u32`
/// and IPv6 addresses as four little-endian `u32` words, so a plain
/// big-endian hex rendering would not match. `localhost` accepts both
/// loopback families. Port-only matching was the S2 defect: a listener on a
/// *different* loopback address that happens to share the port was treated as
/// the dedicated endpoint.
fn expected_local_addrs(host: &str) -> Vec<String> {
    fn ipv4(octets: [u8; 4]) -> String {
        format!("{:08X}", u32::from_le_bytes(octets))
    }
    fn ipv6(segments: [u16; 8]) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for chunk in segments.chunks(2) {
            let word = ((chunk[0] as u32) << 16) | chunk[1] as u32;
            let _ = write!(out, "{:08X}", word.swap_bytes());
        }
        out
    }
    match host {
        "127.0.0.1" => vec![ipv4([127, 0, 0, 1])],
        "::1" | "[::1]" => vec![ipv6([0, 0, 0, 0, 0, 0, 0, 1])],
        "localhost" => vec![ipv4([127, 0, 0, 1]), ipv6([0, 0, 0, 0, 0, 0, 0, 1])],
        _ => Vec::new(),
    }
}

/// Parse a `/proc/net/tcp` / `/proc/net/tcp6` table (identical layout) and
/// return the socket inodes LISTENing on `port` at one of `addrs`. The
/// local-address column is `HEXADDR:HEXPORT`, the state column is `0A` for
/// `TCP_LISTEN`, and the inode is the tenth whitespace-separated column. Both
/// the bound address and the port must match the configured endpoint, so a
/// listener sharing only the port is never confused with the dedicated
/// instance (review S2).
fn listening_inodes(table: &str, port: u16, addrs: &[String]) -> Vec<u64> {
    let mut inodes = Vec::new();
    for line in table.lines().skip(1) {
        // Columns: `sl local_address rem_address st ... inode`. The slot is
        // `0:`, so `nth(1)` is `HEXADDR:HEXPORT`, `nth(3)` is the state and
        // `nth(9)` is the inode.
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(local) = fields.get(1) else { continue };
        let Some(state) = fields.get(3) else { continue };
        if *state != "0A" {
            continue;
        }
        let Some((addr, port_hex)) = local.rsplit_once(':') else {
            continue;
        };
        let Ok(local_port) = u16::from_str_radix(port_hex, 16) else {
            continue;
        };
        if local_port != port || !addrs.iter().any(|expected| expected == addr) {
            continue;
        }
        if let Some(inode) = fields.get(9).and_then(|value| value.parse::<u64>().ok()) {
            inodes.push(inode);
        }
    }
    inodes
}

/// `/proc/<pid>/cmdline` is NUL-separated; split it into discrete arguments so
/// binding compares whole arguments, never substrings (review U5).
fn parse_argv(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// The fail-closed kernel observation of a process (review T5). A `/proc`
/// read that fails for any reason other than "the pid does not exist" is
/// **unobserved**, never silently collapsed into absence: an unreadable or
/// inaccessible state table must not be reported as a quiesced process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessObservation {
    /// The process exists; the payload is its `stat` state letter (`Z` for a
    /// zombie, which is terminal for our purposes).
    Live(char),
    /// The kernel reports no such process (`/proc/<pid>/stat` is absent).
    Gone,
    /// The state could not be read or parsed, so absence is not proven.
    Unobserved,
}

/// Observe a process's kernel state without conflating absence with a failed
/// read (review T5).
fn observe_process(pid: u32) -> ProcessObservation {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match stat
            .rfind(')')
            .and_then(|close| stat[close + 1..].split_whitespace().next())
            .and_then(|field| field.chars().next())
        {
            Some(state) => ProcessObservation::Live(state),
            None => ProcessObservation::Unobserved,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProcessObservation::Gone,
        Err(_) => ProcessObservation::Unobserved,
    }
}

/// The kernel start time (field 22 of `/proc/<pid>/stat`) that, paired with
/// the pid, gives a stable process identity across pid reuse (review S2).
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Skip the parenthesised `comm`; the next fields are state, ppid, ...,
    // starttime (field 22), which is the 20th field after `comm`.
    let close = stat.rfind(')')?;
    stat[close + 1..].split_whitespace().nth(19)?.parse().ok()
}

/// Read a process's executable path, command line and stable start time out of
/// band.
fn read_process(pid: u32) -> Option<ListenerProcess> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let argv = parse_argv(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?);
    let start_time = process_start_time(pid)?;
    Some(ListenerProcess {
        pid,
        start_time,
        exe,
        argv,
    })
}

/// The process currently LISTENing on the configured loopback API endpoint —
/// both the configured address and port must match (review S2).
/// The inodes of every listening socket bound to the endpoint's address and
/// port, whoever holds them.
fn listening_socket_inodes(api_base: &str) -> Vec<u64> {
    let Some(port) = api_port(api_base) else {
        return Vec::new();
    };
    let addrs = api_host(api_base)
        .map(|host| expected_local_addrs(&host))
        .unwrap_or_default();
    if addrs.is_empty() {
        return Vec::new();
    }
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(table) {
            inodes.extend(listening_inodes(&text, port, &addrs));
        }
    }
    inodes
}

/// Whether anything still listens on the endpoint. Unlike
/// [`listener_process`] this needs no owning process: a socket held only by
/// the exiting sibling threads of a zombie thread-group leader still counts.
fn endpoint_is_bound(api_base: &str) -> bool {
    !listening_socket_inodes(api_base).is_empty()
}

/// After the dedicated instance was signalled, wait until nothing listens on
/// its endpoint. The kernel reports a multithreaded process's thread-group
/// leader as a zombie while its sibling threads are still exiting, and those
/// threads still hold the listening socket; a restart spawned in that window
/// fails to bind and the instance never comes back within the restart
/// deadline (observed on CI, 2026-09-17).
fn wait_for_endpoint_release(api_base: &str, deadline: Instant) -> Result<(), SenderError> {
    while endpoint_is_bound(api_base) {
        if Instant::now() >= deadline {
            return Err(unavailable(
                "dedicated_daemon_did_not_release_its_endpoint_after_terminating",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn listener_process(api_base: &str) -> Option<ListenerProcess> {
    let inodes = listening_socket_inodes(api_base);
    if inodes.is_empty() {
        return None;
    }
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            let Some(inode) = target
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
            else {
                continue;
            };
            if inodes
                .iter()
                .any(|candidate| candidate.to_string() == inode)
            {
                return read_process(pid);
            }
        }
    }
    None
}

/// The dedicated instance's launch configuration file name inside its state
/// directory (review T5, review U5).
const OWNTONE_CONFIG_FILE: &str = "owntone.conf";

#[path = "airplay_owntone_config.rs"]
mod dedicated_config;

/// The effective configuration file named by a process's launch arguments.
///
/// OwnTone takes its configuration through an explicit option (`-c <file>`,
/// `--config <file>`, `--config=<file>`, or attached `-c<file>`). That value is
/// the instance's **effective** configuration; a bare path argument that
/// happens to name the state directory or the config file is not (review U5).
/// Ambiguity — no option, more than one, or an empty value — fails closed by
/// returning `None`.
fn effective_config_argument(argv: &[String]) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    let mut index = 0;
    while index < argv.len() {
        let arg = argv[index].as_str();
        let value = if arg == "-c" || arg == "--config" {
            index += 1;
            argv.get(index).cloned()
        } else if let Some(value) = arg.strip_prefix("--config=") {
            Some(value.to_string())
        } else if let Some(value) = arg.strip_prefix("-c") {
            (!value.is_empty()).then(|| value.to_string())
        } else {
            None
        };
        if let Some(value) = value {
            if value.is_empty() || found.is_some() {
                return None;
            }
            found = Some(PathBuf::from(value));
        }
        index += 1;
    }
    found
}

/// Read the launched configuration and require it to bind OwnTone's pipe input
/// to `pipe_path`. The effective configuration is the instance's own trust
/// domain, so the FIFO the adapter writes is authoritative only when the daemon
/// is configured to read exactly that FIFO (review U5).
fn config_binds_pipe(config_path: &Path, pipe_path: &Path) -> bool {
    dedicated_config::binds_pipe(config_path, pipe_path)
}

/// `true` when the process's launch binds the dedicated instance: its effective
/// configuration option names the canonical launch file inside the configured
/// state directory, that file is a regular (non-symlink) file, and it binds
/// OwnTone's pipe input to the adapter's configured FIFO (review R5, S2, T5,
/// U5).
///
/// The check is **effective and canonical**: only the argument of the
/// configuration option counts, both the configuration file and the state
/// directory are resolved through symlinks, and a symlinked launch file or a
/// `..` traversal out of the state directory is refused. A foreign daemon that
/// merely holds the same port and mentions the state directory cannot pass.
fn cmdline_binds_instance(argv: &[String], state_dir: &Path, pipe_path: &Path) -> bool {
    if state_dir.as_os_str().is_empty() || pipe_path.as_os_str().is_empty() {
        return false;
    }
    let Some(config) = effective_config_argument(argv) else {
        return false;
    };
    // Resolve symlinks: a lexically-correct name that resolves outside the
    // state directory is a foreign configuration (review U5).
    let Ok(canonical_state) = std::fs::canonicalize(state_dir) else {
        return false;
    };
    let Ok(canonical_config) = std::fs::canonicalize(&config) else {
        return false;
    };
    let expected_config = canonical_state.join(OWNTONE_CONFIG_FILE);
    if canonical_config != expected_config {
        return false;
    }
    // The expected name must itself be the regular file, not a symlink to a
    // foreign configuration (review U5).
    match std::fs::symlink_metadata(&expected_config) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        _ => return false,
    }
    config_binds_pipe(&canonical_config, pipe_path)
}

/// Compare an executable path with the configured binary, resolving symlinks
/// so a `/bin`-vs-`/usr/bin` split is not a false mismatch.
fn same_binary(actual: &Path, configured: &Path) -> bool {
    let actual = std::fs::canonicalize(actual).unwrap_or_else(|_| actual.to_path_buf());
    let configured = std::fs::canonicalize(configured).unwrap_or_else(|_| configured.to_path_buf());
    actual == configured
}

/// `true` when `process` is the configured dedicated binary *and* its command
/// line binds the configured state directory (review R5).
fn process_is_owned(process: &ListenerProcess, config: &OwnToneConfig) -> bool {
    same_binary(&process.exe, &config.binary)
        && cmdline_binds_instance(&process.argv, &config.state_dir, &config.pipe_path)
}

/// Confirm the process answering on the configured endpoint is the dedicated
/// Tributary-owned daemon. A matching ownership record alone is insufficient:
/// an old record stays valid if the dedicated daemon stops and a shared
/// instance binds the same port (review R5).
fn verify_daemon_process(config: &OwnToneConfig) -> Result<(), SenderError> {
    owned_listener(config).map(|_| ())
}

/// The endpoint's listener, proven to be the dedicated Tributary-owned daemon.
fn owned_listener(config: &OwnToneConfig) -> Result<ListenerProcess, SenderError> {
    let Some(process) = listener_process(&config.api_base) else {
        return Err(unavailable(
            "no_process_is_bound_to_the_configured_dedicated_instance_endpoint",
        ));
    };
    if !same_binary(&process.exe, &config.binary) {
        return Err(unavailable("endpoint_process_is_not_the_owntone_binary"));
    }
    if !cmdline_binds_instance(&process.argv, &config.state_dir, &config.pipe_path) {
        return Err(unavailable("endpoint_process_is_not_the_owned_instance"));
    }
    Ok(process)
}

/// The socket inode a `/proc/<pid>/fd` entry points at, if it is a socket.
fn socket_inode(fd: &Path) -> Option<u64> {
    let target = std::fs::read_link(fd).ok()?;
    target
        .to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Whether the exact process `identity` names (pid and kernel start time) is
/// alive and holds every one of `inodes`.
fn process_holds_sockets(identity: ProcessIdentity, inodes: &[u64]) -> bool {
    if process_start_time(identity.pid) != Some(identity.start_time) {
        return false;
    }
    let Ok(fds) = std::fs::read_dir(format!("/proc/{}/fd", identity.pid)) else {
        return false;
    };
    let held: Vec<u64> = fds
        .flatten()
        .filter_map(|fd| socket_inode(&fd.path()))
        .collect();
    inodes.iter().all(|inode| held.contains(inode))
}

/// The process authority a control client re-proves before every mutating
/// request (PR #270 review, round 11). `open_preflight` proves once that the
/// endpoint's listener is the dedicated instance, but the client outlives that
/// proof through the session and its recovery: if the daemon exits and another
/// local process binds the same loopback address and port, a later
/// `outputs/set`, queue, volume or playback request would reach it on the
/// strength of the URL alone — the stale-ownership-record scenario the process
/// check exists to prevent.
struct ProcessGuard {
    config: OwnToneConfig,
    /// The owned listener last proven in full. While that exact process still
    /// holds every listening socket of the endpoint a mutation needs no second
    /// `/proc` walk; anything else (it exited, was restarted, or a second
    /// listener joined the endpoint) repeats the full ownership proof.
    verified: Mutex<Option<ProcessIdentity>>,
}

impl ProcessGuard {
    fn new(config: &OwnToneConfig) -> Self {
        Self {
            config: config.clone(),
            verified: Mutex::new(None),
        }
    }

    fn authorize(&self) -> Result<(), SenderError> {
        let inodes = listening_socket_inodes(&self.config.api_base);
        if inodes.is_empty() {
            return Err(unavailable(
                "no_process_is_bound_to_the_configured_dedicated_instance_endpoint",
            ));
        }
        let mut verified = self.verified.lock().unwrap_or_else(|p| p.into_inner());
        if verified.is_some_and(|identity| process_holds_sockets(identity, &inodes)) {
            return Ok(());
        }
        *verified = None;
        let identity = owned_listener(&self.config)?.identity();
        if !process_holds_sockets(identity, &inodes) {
            // A second listener shares the endpoint: the kernel may hand this
            // connection to either one.
            return Err(unavailable("endpoint_process_is_not_the_owned_instance"));
        }
        *verified = Some(identity);
        Ok(())
    }
}

/// Re-verify, immediately before signalling, that the exact process the
/// listener was resolved from is still the dedicated Tributary-owned instance
/// (review S2). The pid/start-time identity guards against pid reuse and the
/// full ownership binding (binary + component-exact state directory) guards
/// against a foreign same-binary successor adopting the endpoint.
fn verify_signal_target(
    process: &ListenerProcess,
    config: &OwnToneConfig,
) -> Result<(), SenderError> {
    let Some(current) = read_process(process.pid) else {
        return Err(unavailable("dedicated_daemon_is_no_longer_running"));
    };
    if current.identity() != process.identity() {
        return Err(unavailable("daemon_process_identity_changed"));
    }
    if !process_is_owned(&current, config) {
        return Err(unavailable("endpoint_process_is_not_the_owned_instance"));
    }
    Ok(())
}

/// Re-verify `identity` immediately before a signal (review T5). `Ok(true)`
/// when the same live process is still present, `Ok(false)` when it is gone
/// (already quiesced), and `Err` when it was replaced (pid reuse) or could not
/// be observed — never a silent success.
fn identity_still_ours(identity: ProcessIdentity) -> Result<bool, SenderError> {
    match observe_process(identity.pid) {
        ProcessObservation::Gone => Ok(false),
        ProcessObservation::Unobserved => Err(unavailable(
            "dedicated_daemon_process_state_could_not_be_observed",
        )),
        ProcessObservation::Live(_) => {
            if process_start_time(identity.pid) == Some(identity.start_time) {
                Ok(true)
            } else {
                Err(unavailable("daemon_process_identity_changed"))
            }
        }
    }
}

/// A stable delivery handle for the exact process the listener was resolved
/// from (review U5). On Linux this is a `pidfd`: signals sent through it are
/// delivered to that process and can never reach a recycled pid, so there is no
/// check-to-signal window at all. On other Unix targets the numeric pid is
/// re-bound to the observed start time immediately before each delivery.
struct SignalHandle {
    identity: ProcessIdentity,
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
}

impl SignalHandle {
    /// Open a stable handle for `identity`, proving it still names the observed
    /// process. `Ok(None)` when the process is already gone.
    fn open(identity: ProcessIdentity) -> Result<Option<Self>, SenderError> {
        #[cfg(target_os = "linux")]
        {
            use rustix::process::{pidfd_open, Pid, PidfdFlags};
            let Some(pid) = Pid::from_raw(identity.pid as i32) else {
                return Err(unavailable("dedicated_daemon_process_id_is_invalid"));
            };
            match pidfd_open(pid, PidfdFlags::empty()) {
                Ok(pidfd) => {
                    // Prove the handle names the observed process, not a
                    // successor that reused the pid in the interval.
                    if process_start_time(identity.pid) != Some(identity.start_time) {
                        return Err(unavailable("daemon_process_identity_changed"));
                    }
                    Ok(Some(Self { identity, pidfd }))
                }
                Err(rustix::io::Errno::SRCH) => Ok(None),
                Err(_) => Err(unavailable(
                    "a_stable_handle_to_the_dedicated_daemon_could_not_be_opened",
                )),
            }
        }
    }

    /// Deliver `signal` through the handle. `Ok(false)` when the process is
    /// already gone (quiescence satisfied).
    fn send(&self, signal: rustix::process::Signal) -> Result<bool, SenderError> {
        #[cfg(target_os = "linux")]
        {
            match rustix::process::pidfd_send_signal(&self.pidfd, signal) {
                Ok(()) => Ok(true),
                Err(rustix::io::Errno::SRCH) => Ok(false),
                Err(_) => Err(unavailable("dedicated_daemon_could_not_be_signalled")),
            }
        }
    }
}

/// Signal the process named by the stable `identity`, escalating to `SIGKILL`
/// at `deadline`, and **confirm** the process is gone (or a zombie). Delivery
/// goes through a stable handle — a Linux `pidfd` on Linux — so a pid reaped
/// and reused during the wait is never signalled as the old instance (review
/// S2, review U5). A `/proc` read that cannot prove absence is an error, not
/// quiescence.
fn signal_and_wait(identity: ProcessIdentity, deadline: Duration) -> Result<(), SenderError> {
    use rustix::process::Signal;
    let Some(handle) = SignalHandle::open(identity)? else {
        // Already gone: quiescence is satisfied.
        return Ok(());
    };
    if !handle.send(Signal::TERM)? {
        return Ok(());
    }
    let end = Instant::now() + deadline;
    loop {
        match observe_process(identity.pid) {
            ProcessObservation::Gone | ProcessObservation::Live('Z') => return Ok(()),
            ProcessObservation::Live(_) => {}
            ProcessObservation::Unobserved => {
                return Err(unavailable(
                    "dedicated_daemon_process_state_could_not_be_observed",
                ));
            }
        }
        if Instant::now() >= end {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Escalate through the same stable handle: it still names the original
    // process even if the pid was recycled during the wait (review U5).
    if !handle.send(Signal::KILL)? {
        return Ok(());
    }
    let kill_end = Instant::now() + QUIESCE_KILL_DEADLINE;
    loop {
        match observe_process(identity.pid) {
            ProcessObservation::Gone | ProcessObservation::Live('Z') => return Ok(()),
            ProcessObservation::Live(_) => {}
            ProcessObservation::Unobserved => {
                return Err(unavailable(
                    "dedicated_daemon_process_state_could_not_be_observed",
                ));
            }
        }
        if Instant::now() >= kill_end {
            return Err(unavailable(
                "dedicated_daemon_did_not_terminate_after_sigkill",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Terminate the dedicated instance, re-verifying its full authority and
/// stable identity immediately before signalling (review S2), and confirming
/// exit after `SIGKILL` before declaring quiescence (review S3).
fn terminate_process(
    process: &ListenerProcess,
    config: &OwnToneConfig,
    deadline: Duration,
) -> Result<(), SenderError> {
    verify_signal_target(process, config)?;
    signal_and_wait(process.identity(), deadline)
}

/// Run the supervisor's restart command recorded by the installation (review
/// R2). Detached: the daemon outlives this process.
fn spawn_restart_command(command: &str) -> Result<(), SenderError> {
    use std::process::{Command, Stdio};
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut child| {
            // Reap the restart command so a quiescence never leaves a zombie.
            let _ = std::thread::Builder::new()
                .name("airplay-owntone-reap".to_string())
                .spawn(move || {
                    let _ = child.wait();
                });
        })
        .map_err(|_| unavailable("dedicated_daemon_could_not_be_restarted"))
}

/// Wait (bounded) until the configured endpoint is again served by an owned
/// instance after quiescence. The **same** still-live process that was just
/// terminated never satisfies the wait: its stable identity is excluded, so a
/// failed termination cannot be mistaken for a restart (review S3).
fn wait_for_owned_listener(
    config: &OwnToneConfig,
    deadline: Instant,
    previous: Option<ProcessIdentity>,
) -> Result<(), SenderError> {
    loop {
        if let Some(process) = listener_process(&config.api_base) {
            if process_is_owned(&process, config) && Some(process.identity()) != previous {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(unavailable(
                "dedicated_daemon_did_not_come_back_after_quiescence",
            ));
        }
        std::thread::sleep(RECOVERY_POLL);
    }
}

/// Bounded server-side quiescence before any compensation or terminal delivery
/// (review R2). An unsettled mutating RPC cannot be retracted by releasing the
/// OS lock: the daemon may still apply a late `outputs/set`, `queue/add`, or
/// `player/play`. Terminating and restarting the dedicated instance drops every
/// connection and cancels any in-flight request, after which restoration runs
/// against a daemon that cannot replay an old generation's mutation.
fn quiesce_daemon(config: &OwnToneConfig) -> Result<(), SenderError> {
    let Some(process) = listener_process(&config.api_base) else {
        return restart_exited_daemon(config);
    };
    // Full authority, not just the binary: a same-binary shared instance that
    // happens to hold the endpoint must never be terminated (review S2).
    if !process_is_owned(&process, config) {
        return Err(unavailable("endpoint_process_is_not_the_owned_instance"));
    }
    let previous = process.identity();
    terminate_process(&process, config, QUIESCE_TERMINATE_DEADLINE)?;
    // Termination is observed on the leader; the endpoint is released by the
    // last thread. Never race the restart against that release.
    wait_for_endpoint_release(
        &config.api_base,
        Instant::now() + QUIESCE_TERMINATE_DEADLINE,
    )?;
    if let Some(command) = config.restart_command() {
        spawn_restart_command(&command)?;
    }
    wait_for_owned_listener(
        config,
        Instant::now() + QUIESCE_RESTART_DEADLINE,
        Some(previous),
    )
}

/// Quiescence when no process can be named on the endpoint: the dedicated
/// instance already exited (it crashed after the takeover). A dead daemon
/// cannot replay an old generation's mutation, so it is already quiesced —
/// what is missing is the instance restoration needs. Refusing here left an
/// installation that relies on the documented `restart_command` with its
/// takeover record, instance lock and media route retained forever, because
/// inline and supervisor recovery both retry through this same function (PR
/// #270 review, round 11).
///
/// A socket that is still bound is either the exited instance's last threads
/// releasing it or a listener this user cannot inspect: the release gets its
/// bounded window, and a holder that stays is foreign — nothing is started
/// over it.
fn restart_exited_daemon(config: &OwnToneConfig) -> Result<(), SenderError> {
    if endpoint_is_bound(&config.api_base) {
        wait_for_endpoint_release(
            &config.api_base,
            Instant::now() + QUIESCE_TERMINATE_DEADLINE,
        )
        .map_err(|_| unavailable("endpoint_process_is_not_the_owned_instance"))?;
    }
    if let Some(command) = config.restart_command() {
        spawn_restart_command(&command)?;
    }
    wait_for_owned_listener(config, Instant::now() + QUIESCE_RESTART_DEADLINE, None)
}

/// `true` when this package target has a documented OwnTone acquisition path.
/// Today that is the `.deb` target on Debian/Ubuntu amd64 only (design §8).
fn platform_available() -> bool {
    // Emitted by build.rs for x86_64 Linux; the daemon-backed regressions are
    // gated on the same cfg.
    cfg!(owntone_host)
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

impl MappingFailure {
    /// The localized refusal for this mapping outcome. The English `Display`
    /// text is for logs; user-facing refusals render the catalog entry with
    /// the output identifier as a parameter (PR #270 review, round 9).
    fn refusal(&self) -> SenderError {
        match self {
            Self::MissingIdentifier => unavailable("receiver_published_no_retained_identifier"),
            Self::NoMatch(id) => unavailable_with_id("receiver_not_in_daemon_output_list", *id),
            Self::Ambiguous(id) => unavailable_with_id("receiver_maps_to_multiple_outputs", *id),
        }
    }
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

/// The outcome of one mutating request primitive, distinguishing a refusal
/// by the process guard — which happens strictly before transmission — from
/// a failure at or after it (PR #270 review, round 12). A takeover step
/// proves the guard twice (once by [`OwnToneClient::takeover_step`], once at
/// the transmission boundary inside the primitive), and the endpoint can
/// stop being the owned instance between the two: reporting that refusal as
/// unsettled unwound a request that was never sent through serialized
/// recovery and restarted a daemon holding nothing of ours.
#[derive(Debug)]
enum MutationOutcome {
    /// The guard refused before the request was transmitted: the daemon
    /// holds nothing of ours to replay, so the failure settles with its
    /// own reason.
    Refused(SenderError),
    /// The request was transmitted, or its failure is not provably
    /// pre-send: whether the daemon applied it is unknown.
    Unsettled(SenderError),
}

impl MutationOutcome {
    /// The localized, user-actionable message to surface verbatim.
    fn message(&self) -> &str {
        match self {
            Self::Refused(error) | Self::Unsettled(error) => error.message(),
        }
    }

    /// Collapse the distinction where no unwind decision depends on it.
    fn into_error(self) -> SenderError {
        match self {
            Self::Refused(error) | Self::Unsettled(error) => error,
        }
    }
}

/// Blocking control-plane client for the dedicated instance's loopback JSON
/// API. Every call is deadline-bounded and reports its own localized failure.
struct OwnToneClient {
    http: reqwest::blocking::Client,
    base: String,
    /// Present on the client a session drives the dedicated instance with;
    /// absent only where no instance authority exists to prove.
    guard: Option<ProcessGuard>,
}

impl OwnToneClient {
    fn new(base: &str) -> Result<Self, SenderError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(API_TIMEOUT)
            // Ownership is verified for this literal loopback endpoint only.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| unavailable("local_http_client_could_not_be_created"))?;
        Ok(Self {
            http,
            base: base.to_string(),
            guard: None,
        })
    }

    /// The client a session uses: every mutating request first re-proves that
    /// the endpoint is still served by the dedicated Tributary-owned daemon.
    fn for_owned_instance(config: &OwnToneConfig) -> Result<Self, SenderError> {
        let mut client = Self::new(&config.api_base)?;
        client.guard = Some(ProcessGuard::new(config));
        Ok(client)
    }

    /// Refuse a mutation the verified daemon would not be the one to receive.
    fn authorize_mutation(&self) -> Result<(), SenderError> {
        self.guard.as_ref().map_or(Ok(()), ProcessGuard::authorize)
    }

    /// Run one takeover mutation and say whether its failure left a request
    /// unsettled. A refusal by the process guard precedes transmission: the
    /// daemon holds nothing of ours to replay, so it unwinds cleanly with its
    /// own reason. Counting it as unsettled terminated and restarted a healthy
    /// daemon to settle a request that was never sent, and hid the reason
    /// behind "recovery is pending" (pre-review audit, round 11). The step
    /// bodies re-prove the guard at the transmission boundary, where the
    /// endpoint can have stopped being the owned instance since this outer
    /// check, so a refusal carried by the primitive is classified exactly the
    /// same way — settled, with its own reason (PR #270 review, round 12).
    fn takeover_step(
        &self,
        step: impl FnOnce(&Self) -> Result<(), MutationOutcome>,
    ) -> Result<(), (SenderError, bool)> {
        self.authorize_mutation().map_err(|error| (error, false))?;
        step(self).map_err(|outcome| match outcome {
            MutationOutcome::Refused(error) => (error, false),
            MutationOutcome::Unsettled(error) => (error, true),
        })
    }

    fn require_success(response: &reqwest::blocking::Response) -> Result<(), SenderError> {
        if response.status().is_success() {
            Ok(())
        } else {
            // error_for_status accepts redirects; those cannot confirm an
            // observation or mutation, especially a recovery restoration.
            Err(unavailable("dedicated_daemon_rejected_the_request"))
        }
    }

    fn get_json(&self, path: &str) -> Result<serde_json::Value, SenderError> {
        let response = self
            .http
            .get(format!("{}{}", self.base, path))
            .send()
            .map_err(|_| unavailable("dedicated_daemon_is_unreachable"))?;
        Self::require_success(&response)?;
        response
            .json()
            .map_err(|_| unavailable("dedicated_daemon_sent_a_malformed_response"))
    }

    fn put(&self, path: &str) -> Result<(), MutationOutcome> {
        self.authorize_mutation()
            .map_err(MutationOutcome::Refused)?;
        let response = self
            .http
            .put(format!("{}{}", self.base, path))
            .send()
            .map_err(|_| {
                MutationOutcome::Unsettled(unavailable("dedicated_daemon_is_unreachable"))
            })?;
        Self::require_success(&response).map_err(MutationOutcome::Unsettled)?;
        Ok(())
    }

    fn put_json(&self, path: &str, body: &serde_json::Value) -> Result<(), MutationOutcome> {
        self.authorize_mutation()
            .map_err(MutationOutcome::Refused)?;
        let response = self
            .http
            .put(format!("{}{}", self.base, path))
            .json(body)
            .send()
            .map_err(|_| {
                MutationOutcome::Unsettled(unavailable("dedicated_daemon_is_unreachable"))
            })?;
        Self::require_success(&response).map_err(MutationOutcome::Unsettled)?;
        Ok(())
    }

    fn version(&self) -> Result<(u32, u32), SenderError> {
        let value = self.get_json("/api/config")?;
        let raw = value
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| unavailable("dedicated_daemon_reported_no_version"))?;
        parse_version(raw).ok_or_else(|| unavailable("dedicated_daemon_version_is_unparseable"))
    }

    fn outputs(&self) -> Result<Vec<OwnToneOutput>, SenderError> {
        let value = self.get_json("/api/outputs")?;
        let array = value
            .get("outputs")
            .and_then(|v| v.as_array())
            .ok_or_else(|| unavailable("dedicated_daemon_reported_no_output_list"))?;
        let mut outputs = Vec::with_capacity(array.len());
        for entry in array {
            let id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| unavailable("dedicated_daemon_reported_an_invalid_output_id"))?;
            if outputs.iter().any(|output: &OwnToneOutput| output.id == id) {
                return Err(unavailable(
                    "dedicated_daemon_reported_duplicate_output_ids",
                ));
            }
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let selected = entry
                .get("selected")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| unavailable("dedicated_daemon_reported_invalid_output_selection"))?;
            outputs.push(OwnToneOutput { id, name, selected });
        }
        Ok(outputs)
    }

    /// `PUT /api/outputs/set` rewrites the server-wide enabled set: it enables
    /// exactly `ids` and disables every other output (§4.3).
    fn set_outputs(&self, ids: &[u64]) -> Result<(), MutationOutcome> {
        let body = serde_json::json!({
            "outputs": ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        });
        self.put_json("/api/outputs/set", &body)
    }

    fn clear_queue(&self) -> Result<(), MutationOutcome> {
        self.put("/api/queue/clear")
    }

    /// Validate the authority-bearing state for both takeover and drain polling.
    fn decode_player_state(value: &serde_json::Value) -> Result<String, SenderError> {
        match value.get("state").and_then(|v| v.as_str()) {
            Some(state @ ("play" | "pause" | "stop")) => Ok(state.to_string()),
            _ => Err(unavailable(
                "dedicated_daemon_reported_an_invalid_player_state",
            )),
        }
    }

    /// The daemon's coarse player state (`play`, `pause`, `stop`).
    fn player_state(&self) -> Result<String, SenderError> {
        Self::decode_player_state(&self.get_json("/api/player")?)
    }

    fn player_progress(&self) -> Result<(String, Option<u64>), SenderError> {
        let value = self.get_json("/api/player")?;
        let state = Self::decode_player_state(&value)?;
        let progress = value.get("item_progress_ms").and_then(|v| v.as_u64());
        Ok((state, progress))
    }

    fn player_control(&self, action: &str) -> Result<(), SenderError> {
        self.put(&format!("/api/player/{action}"))
            .map_err(MutationOutcome::into_error)
    }

    fn set_volume(&self, percent: u8) -> Result<(), MutationOutcome> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("volume", &percent.to_string())
            .finish();
        self.put(&format!("/api/player/volume?{query}"))
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
            .map_err(|_| unavailable("takeover_record_could_not_be_serialized"))?;
        // A truncating write that fails part-way (ENOSPC, EIO, a crash) leaves
        // an unreadable record, and an unreadable record refuses every later
        // load until someone removes it by hand. Nothing has been sent to the
        // daemon when this runs, so the record is staged beside its final
        // name, synced and renamed into place; a failure removes the stage and
        // leaves no record at all (pre-review audit, round 11). A failure
        // after the rename removes the final record too: a reported failure
        // must never leave a record a later load would misread as
        // crashed-takeover evidence and quiesce a daemon over, even though
        // that load sent no mutation (PR #270 review, round 12).
        let stage = path.with_extension("json.partial");
        let mut renamed = false;
        let staged = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&stage)?;
            std::io::Write::write_all(&mut file, &body)?;
            file.sync_all()?;
            std::fs::rename(&stage, path)?;
            renamed = true;
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if staged.is_err() {
            let _ = std::fs::remove_file(&stage);
            if renamed {
                let _ = std::fs::remove_file(path);
            }
        }
        staged.map_err(|_| unavailable("takeover_record_could_not_be_persisted"))
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
        Ok(_) => Err(unavailable("configured_pipe_path_is_not_a_fifo")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| unavailable("configured_pipe_has_no_parent_directory"))?;
            std::fs::create_dir_all(parent)
                .map_err(|_| unavailable("pipe_directory_could_not_be_created"))?;
            rustix::fs::mkfifoat(rustix::fs::CWD, path, Mode::RUSR | Mode::WUSR)
                .map_err(|_| unavailable("configured_pipe_could_not_be_created"))?;
            Ok(())
        }
        Err(_) => Err(unavailable("configured_pipe_could_not_be_inspected")),
    }
}

/// The identity of the owned FIFO, bound on the load worker at the same
/// verification step that proves the dedicated daemon scans this pathname
/// ([`OwnToneConfig::verify_owned`]). The writer descriptor acquired later
/// must resolve to this exact object: a pathname substituted in the interval
/// — a regular file, a symlink to a foreign file, a swapped parent directory —
/// is refused before any PCM can be written (AM1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PipeIdentity {
    device: u64,
    inode: u64,
}

/// Inspect the configured pipe pathname without following symlinks and
/// require a FIFO. The returned identity is what the writer must match.
fn verify_pipe_identity(path: &Path) -> Result<PipeIdentity, SenderError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| unavailable("configured_pipe_could_not_be_inspected"))?;
    if !metadata.file_type().is_fifo() {
        return Err(unavailable("configured_pipe_path_is_not_a_fifo"));
    }
    Ok(PipeIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Bind a freshly opened writer to the verified FIFO. The check is made on
/// the descriptor (`fstat`), never on the pathname, so a substitution between
/// verification and acquisition cannot slip through. A descriptor that is not
/// that FIFO is closed untouched: nothing is written, truncated, removed or
/// replaced at the pathname (AM1).
fn bind_pipe_writer(fd: OwnedFd, expected: PipeIdentity) -> Result<OwnedFd, SenderError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let file = std::fs::File::from(fd);
    let metadata = file
        .metadata()
        .map_err(|_| unavailable("pipe_write_end_could_not_be_inspected"))?;
    if !metadata.file_type().is_fifo() {
        return Err(unavailable("configured_pipe_path_is_not_a_fifo"));
    }
    if (metadata.dev(), metadata.ino()) != (expected.device, expected.inode) {
        return Err(unavailable("configured_pipe_was_replaced"));
    }
    Ok(OwnedFd::from(file))
}

/// Distinguishes a cancellation observed while waiting from a real failure, so
/// the FIFO wait can be aborted ("cancellation must be silent").
enum CancelOrError {
    Cancelled,
    Failed(SenderError),
}

/// Open the pipe write end, waiting (bounded) for the daemon's reader.
///
/// Keep the write end nonblocking: `ENXIO` means no reader yet. `fdsink`
/// handles partial writes and EAGAIN with cancellable polling. A blocking
/// write could strand pipeline shutdown if the daemon stops draining (AD1).
/// The wait is raced against `cancel`, so a Stop or replacement aborts it
/// rather than leaving the open blocked on a daemon that never opens the pipe
/// (review F2).
///
/// Every successful descriptor is bound to `expected` before it is returned
/// (AM1): `NOFOLLOW` refuses a symlink planted at the pathname outright,
/// `NOCTTY` keeps a substituted terminal device from becoming the controlling
/// terminal before the descriptor check can refuse it, and
/// [`bind_pipe_writer`] refuses any object other than the verified FIFO.
fn open_pipe_write(
    path: &Path,
    expected: PipeIdentity,
    deadline: Instant,
    cancel: &OpenCancel,
) -> Result<OwnedFd, CancelOrError> {
    loop {
        if cancel.is_cancelled() {
            return Err(CancelOrError::Cancelled);
        }
        match rustix::fs::open(
            path,
            OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NOCTTY,
            Mode::empty(),
        ) {
            Ok(fd) => return bind_pipe_writer(fd, expected).map_err(CancelOrError::Failed),
            Err(rustix::io::Errno::LOOP) => {
                return Err(CancelOrError::Failed(unavailable(
                    "configured_pipe_path_is_a_symlink",
                )))
            }
            Err(rustix::io::Errno::NXIO) => {
                if Instant::now() >= deadline {
                    return Err(CancelOrError::Failed(unavailable(
                        "dedicated_daemon_is_not_reading_the_pipe",
                    )));
                }
                std::thread::sleep(FIFO_OPEN_POLL);
            }
            Err(_) => {
                return Err(CancelOrError::Failed(unavailable(
                    "pipe_write_end_could_not_be_opened",
                )))
            }
        }
    }
}

/// The serialized activation/cancellation decision shared by the load path
/// (which accepts a current load) and teardown (which cancels the load). A
/// single mutex makes the two a defined boundary: an activation that loses the
/// race to a cancellation is refused *before* it transmits `player/play`, and
/// a cancellation that follows an accepted activation knows a play may have
/// been transmitted and must be covered by restoration (review S4).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ActivationState {
    /// Activation reserved this session; the pump may observe after confirmation.
    accepted: bool,
    /// Teardown cancelled this session; no activation may be accepted and no
    /// pump may start.
    cancelled: bool,
}

/// Test-only instrumentation for the terminal/publication ordering regressions
/// (review Z1/Z2). It adds no ordering of its own: it can only *observe* that a
/// terminal `restore` is about to contend on the settlement boundary, and park
/// a publication that already holds that boundary *before* its effects run, so
/// a regression can place the terminal contender deterministically behind it
/// instead of sleeping and hoping.
#[cfg(test)]
#[derive(Default)]
struct SessionProbe {
    /// Signalled, once, by a `restore` immediately before it contends on
    /// `mutation_lock`.
    restore_attempt: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// The publication holding the settlement boundary.
    publication_hold: Mutex<PublicationHold>,
    /// Signalled on every live `observe` call, so a regression can wait for a
    /// running worker's own observation/cache refresh to become visible
    /// deterministically instead of sleeping or asserting after teardown.
    observe: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

#[cfg(test)]
#[derive(Default)]
struct PublicationHold {
    /// Signalled, once, once the publication has acquired the boundary and is
    /// about to run the caller's effects.
    entered: Option<std::sync::mpsc::Sender<()>>,
    /// Parked on before the caller's effects run; the test releases it.
    release: Option<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl SessionInner {
    /// Arm the terminal-contender arrival probe: the next `restore` signals just
    /// before it attempts the settlement boundary.
    fn arm_restore_attempt_probe(&self, tx: std::sync::mpsc::Sender<()>) {
        *self
            .probe
            .restore_attempt
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(tx);
    }

    /// Arm the publication probe: the next `publish_under_boundary` signals that
    /// it holds the boundary and then parks before running the caller's effects.
    fn arm_publication_probe(
        &self,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) {
        let mut hold = self
            .probe
            .publication_hold
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        hold.entered = Some(entered);
        hold.release = Some(release);
    }

    fn note_restore_attempt(&self) {
        let attempt = self
            .probe
            .restore_attempt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(tx) = attempt {
            let _ = tx.send(());
        }
    }

    fn note_publication_entered(&self) {
        let mut hold = self
            .probe
            .publication_hold
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = hold.entered.take() {
            let _ = tx.send(());
        }
        if let Some(release) = hold.release.take() {
            let _ = release.recv();
        }
    }

    /// Arm the worker-observation probe: every later `observe` call signals, so
    /// a regression can synchronize on the worker's own cache refresh.
    fn arm_observe_probe(&self, tx: std::sync::mpsc::Sender<()>) {
        *self.probe.observe.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
    }

    fn note_observe(&self) {
        let probe = self.probe.observe.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = probe.as_ref() {
            let _ = tx.send(());
        }
    }
}

/// Per-session shared state, driven by the decode pump and read by the seam.
struct SessionInner {
    client: Arc<OwnToneClient>,
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
    /// The serialized acceptance/cancellation boundary. The decode pump stays
    /// inert until activation finishes. First PCM is driven by activation
    /// itself under the shared Stop gate; cancellation refuses late activation
    /// (review R4, review S4).
    activation: Mutex<ActivationState>,
    /// The load path's Stop/start boundary (review U3). `activate_and_play`
    /// authorizes first PCM or a resume RPC through this gate. Stop prevents
    /// a new effect; teardown settles an already-authorized effect.
    gate: Arc<SessionGate>,
    /// The load's cancellation currency, so the pump can abort its activation
    /// wait the moment the load is cancelled or replaced (review R4).
    cancel: OpenCancel,
    restored: AtomicBool,
    /// Set once a `player/pause` has been accepted by the daemon. Pinned
    /// OwnTone's pipe input `stop` runs on pause and clears
    /// `pipe_autostart_id`, so from then on the item is a plain source that
    /// will never autostop at EOF: natural completion must then be decided
    /// from stalled progress instead of waiting for the daemon's `stop`
    /// ([`CompletionTracker`]). Never cleared.
    autostart_lost: AtomicBool,
    /// The session's **control epoch**: the count of accepted control
    /// transitions (`pause`/`play`) published under [`Self::mutation_lock`].
    /// A completion observation is bound to the epoch it was sampled under;
    /// an observation that a transition overtook while it was in flight
    /// describes a state the user has already left and proves nothing, and
    /// the decision to enter terminal restoration re-validates the epoch under
    /// the same lock (refinery R11: an accepted pause must never turn an
    /// older `play` sample into a completed, restored item).
    control_epoch: AtomicU64,
    /// The session's **terminal** flag. Set under [`Self::mutation_lock`] at
    /// the start of [`Self::restore`], i.e. *before* the daemon is restored, so
    /// the terminal transition and any concurrent control transmission are one
    /// serialized decision. Once set, [`Self::transmit_mutation`] refuses every
    /// later control RPC: a control that was queued or waiting for the
    /// settlement boundary while the pump restored can no longer transmit a
    /// `player/play`/`player/pause`/`volume` against the already-restored
    /// output selection (review W1). It is never cleared — a restored session
    /// is terminal and a fresh load builds a new [`SessionInner`].
    terminal: AtomicBool,
    /// The session's **settlement boundary**. Every daemon-mutating RPC the
    /// live session transmits (control RPCs and the restoring `player/stop` +
    /// `outputs/set`) takes this lock, and [`Self::restore`] holds it across
    /// its quiesce-and-restore sequence. This is what makes "is anything
    /// outstanding?" and "transmit this effect" a single ordered decision
    /// instead of a check-to-effect race: a control that is in flight while
    /// the pump restores can no longer transmit between the pump's quiescence
    /// and its own transmission (review U1).
    mutation_lock: Mutex<()>,
    /// Count of transmitted daemon mutations whose outcome is not yet proven.
    /// Raised **before** transmission and lowered only on a confirmed success;
    /// a failure (or a timeout that may still be applied server-side) stays
    /// counted until a successful quiescence drops every in-flight request.
    /// The count — never a bare boolean — is what stops a later successful
    /// restoration from being accepted as proof that an earlier timed-out
    /// mutation settled (review T1, review U1).
    unsettled: AtomicUsize,
    position: Mutex<SenderPosition>,
    state: Mutex<PlayerState>,
    pipeline: Mutex<Option<gst::Pipeline>>,
    /// Test-only ordering instrumentation (review Z1/Z2). Compiled out of
    /// production builds.
    #[cfg(test)]
    probe: SessionProbe,
}

impl SessionInner {
    fn publish_state(&self, state: PlayerState) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = state;
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(self.generation, state));
    }

    /// Transmit one daemon-mutating RPC under the session's settlement
    /// boundary. The outstanding count is raised **before** the effect is
    /// transmitted and lowered only on a confirmed success, so a concurrent
    /// [`Self::restore`] can never observe "nothing outstanding" while an
    /// effect is still in flight, and a failed or timed-out effect stays
    /// recorded until a confirmed quiescence settles it (review U1).
    ///
    /// **Terminal sessions refuse every control transmission (review W1).**
    /// [`Self::restore`] sets [`Self::terminal`] under this same lock before it
    /// restores the daemon, so a control that is queued or waiting for the
    /// boundary while the terminal transition runs acquires the lock only
    /// *after* the session is terminal and is refused here — it can never
    /// transmit a late `player/play`/`pause`/`volume` against the restored
    /// output selection. The refusal is fail-closed and does not touch the
    /// outstanding count (nothing was transmitted).
    #[cfg(test)]
    fn transmit_mutation<F>(&self, effect: F) -> Result<(), SenderError>
    where
        F: FnOnce() -> Result<(), SenderError>,
    {
        self.transmit_under_boundary(effect, None, false)
    }

    /// Transmit one daemon-mutating RPC and, on a confirmed success, publish the
    /// control state it produces **inside the same settlement boundary** (review
    /// X1). Holding [`Self::mutation_lock`] across both the transmission and the
    /// publication is the deterministic barrier the review requires: a control
    /// that succeeds publishes its `Playing`/`Paused` before it releases the
    /// lock, and [`Self::restore`] can latch `terminal` only while holding that
    /// same lock. No successful control publication can therefore trail a
    /// terminal `Stopped`/`TrackEnded`, and a control that observes a latched
    /// terminal transition publishes nothing. Publishing *after* releasing the
    /// lock (a bare `terminal` check) would leave a fresh check-to-effect race
    /// between the RPC settling and the state becoming visible.
    fn transmit_mutation_publishing<F>(
        &self,
        effect: F,
        on_success: PlayerState,
    ) -> Result<(), SenderError>
    where
        F: FnOnce() -> Result<(), SenderError>,
    {
        self.transmit_under_boundary(effect, Some(on_success), false)
    }

    /// Shared body of [`Self::transmit_mutation`] and
    /// [`Self::transmit_mutation_publishing`]: raise the outstanding count
    /// **before** transmission, lower it only on confirmed success, and publish
    /// an optional control state before the boundary is released.
    fn transmit_under_boundary<F>(
        &self,
        effect: F,
        on_success: Option<PlayerState>,
        terminal_on_failure: bool,
    ) -> Result<(), SenderError>
    where
        F: FnOnce() -> Result<(), SenderError>,
    {
        #[cfg(test)]
        if self
            .config
            .pipe_path
            .with_extension("park-terminal")
            .exists()
        {
            std::fs::write(
                self.config
                    .pipe_path
                    .with_extension("terminal-control-waiting"),
                "",
            )
            .unwrap();
        }
        let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.terminal.load(Ordering::SeqCst) {
            return Err(unavailable(
                "airplay_session_is_no_longer_accepting_control",
            ));
        }
        if terminal_on_failure && (self.cancel.is_cancelled() || self.gate.is_stopped()) {
            return Err(unavailable("airplay_session_was_stopped"));
        }
        self.unsettled.fetch_add(1, Ordering::SeqCst);
        match effect() {
            Ok(()) => {
                self.unsettled.fetch_sub(1, Ordering::SeqCst);
                if let Some(state) = on_success {
                    // Still unterminated: `restore` latches `terminal` only
                    // under this same lock, so it cannot have interleaved
                    // between the transmission and here (review X1).
                    if !self.gate.publish_if_live(|| self.publish_state(state)) {
                        return Err(unavailable("airplay_session_was_stopped"));
                    }
                    // The accepted transition and everything the drain wait
                    // derives from it are one decision under this boundary.
                    if state == PlayerState::Paused {
                        // The daemon's pipe input stopped: the item is no
                        // longer autostarted and will not autostop at EOF
                        // (see `Self::autostart_lost`).
                        self.autostart_lost.store(true, Ordering::SeqCst);
                    }
                    self.control_epoch.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            }
            Err(error) => {
                if terminal_on_failure {
                    // Latch refusal before releasing the transmission boundary:
                    // no queued control may overtake an uncertain mutation.
                    // Keep the outstanding count for close()'s quiescence.
                    self.terminal.store(true, Ordering::SeqCst);
                    self.running.store(false, Ordering::SeqCst);
                    self.gate.publish_if_live(|| {
                        if !self.cancel.is_cancelled() {
                            let _ = self.event_tx.try_send(PlayerEvent::error(
                                self.generation,
                                error.message().to_string(),
                            ));
                            self.publish_state(PlayerState::Stopped);
                        }
                    });
                }
                Err(error)
            }
        }
    }

    /// Run a caller-supplied publication (the worker's own cache/event write)
    /// under the settlement boundary, suppressing it once the session is
    /// terminal (review Y1). This is what makes the *caller's* publication
    /// atomic with the terminal transition instead of a check-then-effect race
    /// the caller could lose after the boundary was released: [`Self::restore`]
    /// latches `terminal` under this same lock, so a publication either runs
    /// before the latch or observes it and reports `false`.
    fn publish_under_boundary(&self, publish: &mut dyn FnMut(PlayerState)) -> bool {
        let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.terminal.load(Ordering::SeqCst) {
            return false;
        }
        // Test-only: signal that this publication holds the boundary and, when
        // armed, park before running the caller's effects so a terminal
        // contender can be placed deterministically behind it (review Z2).
        #[cfg(test)]
        self.note_publication_entered();
        self.gate.publish_if_live(|| publish(PlayerState::Playing))
    }

    /// Latch the terminal transition **and** publish the terminal `Stopped` as
    /// one serialized decision (review Y1). A terminal failure that published
    /// `Stopped` before `restore` latched `terminal` left a gap in which a
    /// control queued on the settlement boundary could transmit successfully
    /// and publish `Playing`/`Paused` *after* the terminal `Stopped`. Taking
    /// the boundary for the latch and the publication together closes it: a
    /// control acquires the boundary either before the latch (and publishes
    /// before the `Stopped`) or after it (and is refused by
    /// `transmit_mutation`). Idempotent with [`Self::restore`], which latches
    /// the same flag under the same lock.
    fn publish_terminal(&self, failure: RuntimeFailure) {
        let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
        self.terminal.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        self.gate.publish_if_live(|| {
            if !self.cancel.is_cancelled() {
                let _ = self
                    .event_tx
                    .try_send(PlayerEvent::error(self.generation, failure.message()));
                self.publish_state(PlayerState::Stopped);
            }
        });
    }

    /// Register uncertainty around an effect whose transmission is not routed
    /// through [`Self::transmit_mutation`] — the restoring RPCs — so a failed
    /// restoration is never forgotten (review U1).
    fn mark_unsettled(&self) {
        self.unsettled.fetch_add(1, Ordering::SeqCst);
    }

    /// The number of transmitted mutations that are not yet proven settled.
    /// Exposed for the fault-injection regressions.
    #[cfg(test)]
    fn unsettled_count(&self) -> usize {
        self.unsettled.load(Ordering::SeqCst)
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
    ///
    /// Returns the restoration outcome so callers cannot mistake a failed
    /// restore for a clean teardown: `Ok(())` once the record is cleared and
    /// the route revoked by identity, `Err` when the record and route are
    /// retained for serialized recovery (review R3).
    ///
    /// **Settlement is not forgotten (review U1).** A failed restoration may
    /// itself have transmitted a `PUT` that is still outstanding. The
    /// outstanding count is raised before the restoring RPCs are transmitted
    /// and is only cleared by a confirmed quiescence, so a later `restore`
    /// (the close path re-invokes it after joining the pump) cannot succeed
    /// and release the lock while an earlier timed-out restoration is still
    /// live: it must quiesce first.
    fn restore(&self) -> Result<(), SenderError> {
        // Test-only: signal that this terminal transition has reached the
        // settlement boundary (before it contends on the lock), so a regression
        // can synchronize on real arrival instead of a sleep (review Z2).
        #[cfg(test)]
        self.note_restore_attempt();
        let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
        self.restore_locked()
    }

    /// Restore for a natural completion decided under control epoch `epoch`.
    /// The epoch is re-validated under the settlement boundary, atomically
    /// with the decision to latch `terminal`: a control transition accepted
    /// since the deciding observation (an accepted pause, or a pause/resume
    /// pair) makes that completion void — the drain wait resumes — instead of
    /// restoring and reporting `TrackEnded` for an item the user just paused
    /// (refinery R11).
    fn restore_completion(&self, epoch: u64) -> Result<(), CompletionRestore> {
        #[cfg(test)]
        self.note_restore_attempt();
        let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
        let paused = *self.state.lock().unwrap_or_else(|p| p.into_inner()) == PlayerState::Paused;
        if self.control_epoch.load(Ordering::SeqCst) != epoch || paused {
            return Err(CompletionRestore::Superseded);
        }
        self.restore_locked().map_err(CompletionRestore::Failed)
    }

    /// Shared body of [`Self::restore`] and [`Self::restore_completion`];
    /// the caller holds [`Self::mutation_lock`].
    fn restore_locked(&self) -> Result<(), SenderError> {
        // Terminal transition, taken under the settlement boundary **before**
        // any restoring RPC. This is what makes "this session is finished" and
        // "a control may transmit" one ordered decision instead of a
        // check-to-effect race: a control that is queued on `mutation_lock`
        // while this runs observes `terminal` on acquire and is refused (review
        // W1). Latch it even when restoration later fails — a failed teardown
        // is still terminal for the session. `running` is cleared so the pump's
        // loop and activation gate stop as well.
        self.terminal.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        if self.restored.load(Ordering::SeqCst) {
            // Restoration already completed. A mutation whose outcome is still
            // unproven — transmitted while the terminal transition was being
            // taken, or left outstanding by a timed-out restoring RPC — must
            // still be settled before ownership can be released: a bare
            // `restored` check would short-circuit over it and drop the lock
            // (review W1). Quiescence drops every in-flight request; if it
            // cannot be established, fail closed and retain the lock.
            if self.unsettled.load(Ordering::SeqCst) > 0 {
                quiesce_daemon(&self.config)?;
                self.unsettled.store(0, Ordering::SeqCst);
            }
            return Ok(());
        }
        // A transmitted RPC that failed or timed out may still be outstanding.
        // Quiescence (terminate/restart, dropping every in-flight request) is
        // then mandatory before restoration can be trusted; if it cannot be
        // established, retain custody rather than releasing (review T1). The
        // count is cleared only by that confirmed quiescence.
        if self.unsettled.load(Ordering::SeqCst) > 0 {
            quiesce_daemon(&self.config)?;
            self.unsettled.store(0, Ordering::SeqCst);
        }
        // Register uncertainty before transmitting the restoring RPCs: a
        // restoration step that fails leaves the count raised, so the next
        // restore must quiesce before it can release (review U1).
        self.mark_unsettled();
        match restore_daemon(&self.client, &self.config, &self.recorded) {
            Ok(()) => {
                self.unsettled.fetch_sub(1, Ordering::SeqCst);
                // Release this load's loopback route by identity only after
                // the daemon has been restored: the route stays valid for every
                // request the daemon might still be applying (§4.1, §4.3). The
                // identity-bound `take_and_release` is the single release
                // primitive, so a ticket that was moved into recovery custody
                // during a superseded open is also removed from custody here
                // rather than stranded (review S5).
                if let Some(ticket) = self.media_ticket.as_ref() {
                    self.media_proxy.take_and_release(ticket);
                }
                self.restored.store(true, Ordering::SeqCst);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

/// Why a natural completion did not restore.
enum CompletionRestore {
    /// A control transition was accepted after the deciding observation.
    Superseded,
    Failed(SenderError),
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
    if let Err(outcome) = client.set_outputs(&recorded.enabled_outputs) {
        warn!(reason = %outcome.message(), "OwnTone restore: enabled-output set failed");
        first_error.get_or_insert_with(|| outcome.into_error());
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    if let Err(error) = std::fs::remove_file(config.takeover_record()) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!("OwnTone restore: takeover record removal failed");
            return Err(unavailable("takeover_record_could_not_be_cleared"));
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

    /// Hold the decode pipeline while the daemon pauses. Pinned pause stops
    /// the pipe input and re-arms its watcher (`inputs/pipe.c: stop` →
    /// `pipe_watch_reset`), which closes and reopens the daemon's reader: a
    /// PCM write that lands in that reader-less window gets `EPIPE`, which
    /// `fdsink` reports as a fatal stream error and would end the session.
    /// Pausing the pipeline first keeps the writer idle across that window;
    /// [`Self::release_pipeline`] lets it write again once the daemon has
    /// accepted `play` and reopened its input. A pipeline that is not
    /// playing (never activated, or already torn down) is left alone.
    fn hold_pipeline(&self) {
        if let Some(pipeline) = self.pipeline() {
            if pipeline.state(gst::ClockTime::ZERO).1 == gst::State::Playing {
                let _ = pipeline.set_state(gst::State::Paused);
            }
        }
    }

    /// Counterpart of [`Self::hold_pipeline`], after an accepted resume.
    fn release_pipeline(&self) {
        if let Some(pipeline) = self.pipeline() {
            if pipeline.state(gst::ClockTime::ZERO).1 == gst::State::Paused {
                let _ = pipeline.set_state(gst::State::Playing);
            }
        }
    }

    /// Serialize first PCM (pipe autostart) or resume with the current load's
    /// Stop boundary. Initial playback starts decoding inside the settlement
    /// boundary and waits for daemon-confirmed play before publishing Playing.
    /// The pump only observes/drains an already-started pipeline; it cannot
    /// start a cancelled generation independently (AC1).
    fn activate_and_play(&self) -> bool {
        let mut state = self.activation.lock().unwrap_or_else(|p| p.into_inner());
        // Re-check the load's cancellation currency inside the same lock a Stop
        // cancels through: a Stop that raced the worker's currentness check has
        // already cancelled this token, so the play is refused before it is
        // transmitted (review T3).
        if self.cancel.is_cancelled() {
            state.cancelled = true;
        }
        let first_start = !state.accepted;
        if !activation_decide(&mut state, self.running.load(Ordering::SeqCst)) {
            return false;
        }
        // Authorize and transmit the play through the load path's shared
        // Stop/start boundary: the load path's Stop takes the same gate, so the
        // check and the effect are one serialized decision with no window for a
        // Stop to land between them (review U3). The settlement boundary records
        // the RPC as outstanding until it settles, so a concurrent restore
        // cannot release ownership while it is in flight (review U1).
        let played = self.gate.start(|| {
            // Transmit the accepted start and publish `Playing` in one bounded
            // critical section, so a terminal transition cannot land between
            // the successful play and its state publication (review X1).
            match self.transmit_mutation_publishing(
                || {
                    if let Some(pipeline) = self.pipeline().filter(|_| first_start) {
                        self.start_pipe(&pipeline)
                    } else {
                        self.client.player_control("play")?;
                        // The daemon reopened its pipe input: let the decoder
                        // held across the pause write again.
                        self.release_pipeline();
                        Ok(())
                    }
                },
                PlayerState::Playing,
            ) {
                Ok(()) => true,
                Err(error) => {
                    // Refusal/cancellation is a silent false result to the
                    // worker; a live failure remains visible. Serialize the
                    // decision AND send with Stop, in settlement -> gate order.
                    let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
                    self.gate.publish_if_live(|| {
                        if !self.cancel.is_cancelled() && !self.terminal.load(Ordering::SeqCst) {
                            let _ = self.event_tx.try_send(PlayerEvent::error(
                                self.generation,
                                error.message().to_string(),
                            ));
                        }
                    });
                    false
                }
            }
        });
        if played {
            return true;
        }
        // Stop decoding before releasing activation: the pump owns the write
        // descriptor and may return immediately once it observes refusal.
        if let Some(pipeline) = self.pipeline() {
            let _ = pipeline.set_state(gst::State::Null);
        }
        // Either a failed play or a Stop that won the boundary: refuse
        // activation so the inert pump returns, and never report `Playing`
        // (review U3).
        state.accepted = false;
        state.cancelled = true;
        // A session that already went terminal published its own terminal
        // state; a late `Stopped` must not follow `Stopped`/`TrackEnded`
        // (review X1). A non-terminal refusal still reports the truthful
        // `Stopped` (review U3).
        if !self.terminal.load(Ordering::SeqCst) {
            self.publish_state(PlayerState::Stopped);
        }
        false
    }

    /// Pinned OwnTone pipe_read_cb starts a scanned pipe only after bytes
    /// arrive. Ordinary play on the queue cleared by open cannot do that.
    /// This runs under both SessionGate and mutation_lock, so Stop either
    /// prevents the first PCM effect or teardown settles it before release.
    fn start_pipe(&self, pipeline: &gst::Pipeline) -> Result<(), SenderError> {
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| unavailable("decode_pipeline_failed_to_start"))?;
        let deadline = Instant::now() + OPEN_DEADLINE;
        loop {
            if self.cancel.is_cancelled() || self.gate.is_stopped() {
                return Err(unavailable("pipe_activation_was_cancelled"));
            }
            if self.client.player_state()? == "play" {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(unavailable("dedicated_daemon_did_not_autostart_the_pipe"));
            }
            std::thread::sleep(FIFO_OPEN_POLL);
        }
    }

    /// Cancel activation: after this returns, no activation can be accepted.
    /// Idempotent and safe from any thread; paired with the same mutex the
    /// acceptance uses so an in-flight `resume` and a concurrent teardown
    /// observe a single ordering (review S4).
    fn cancel_activation(&self) {
        let mut state = self.activation.lock().unwrap_or_else(|p| p.into_inner());
        state.cancelled = true;
    }

    /// The pump's final serialized check: `true` only while activation is
    /// accepted and no cancellation has won. Taken under the same mutex as
    /// acceptance and cancellation, so the pump's start decision cannot
    /// interleave with a teardown that is cancelling it (review S4).
    fn activation_live(&self) -> bool {
        let state = self.activation.lock().unwrap_or_else(|p| p.into_inner());
        state.accepted && !state.cancelled
    }
}

/// Pure acceptance decision shared by [`SessionInner::activate_and_play`]:
/// accept activation only while the session is running and no cancellation has
/// won the serialized boundary (review S4). Split out so the boundary is
/// deterministically unit-testable without a live session.
fn activation_decide(state: &mut ActivationState, running: bool) -> bool {
    if state.cancelled || !running {
        return false;
    }
    state.accepted = true;
    true
}

/// Pure activation gate, split from [`wait_for_activation`] so the pump's
/// "inert until accepted" barrier is deterministically unit-testable without
/// a GStreamer pipeline (review R4, S4). Returns `true` only when the load
/// accepted activation before the deadline and was neither cancelled nor torn
/// down while waiting. The decision is read under the activation mutex, so the
/// gate observes the same serialized boundary as
/// [`SessionInner::activate_and_play`] and [`SessionInner::cancel_activation`].
fn activation_gate(
    activation: &Mutex<ActivationState>,
    running: &AtomicBool,
    cancel: &OpenCancel,
    deadline: Instant,
    poll: Duration,
) -> bool {
    loop {
        {
            let state = activation.lock().unwrap_or_else(|p| p.into_inner());
            if state.cancelled {
                return false;
            }
            if state.accepted {
                return true;
            }
        }
        if cancel.is_cancelled() || !running.load(Ordering::SeqCst) {
            return false;
        }
        if Instant::now() >= deadline {
            return false;
        }
        if !poll.is_zero() {
            std::thread::sleep(poll);
        }
    }
}

/// Wait (bounded) for the load path to accept activation. A load that is
/// cancelled, torn down, or never accepted inside the bound leaves the pump
/// inert: it returns `false` without starting the pipeline, publishing a start
/// event, or driving the daemon (review R4).
fn wait_for_activation(inner: &SessionInner) -> bool {
    activation_gate(
        &inner.activation,
        &inner.running,
        &inner.cancel,
        Instant::now() + OPEN_DEADLINE,
        FIFO_OPEN_POLL,
    )
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

    // The pump is spawned inside `open`, before the load path has accepted the
    // session, so it must stay inert until that acceptance arrives. A load
    // cancelled or superseded before activation returns here without touching
    // the pipeline or the daemon (review R4).
    if !wait_for_activation(&inner) {
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }
    // Activation owns pipeline startup. A cancelled activation never lets
    // this observer independently start or restart the pipeline.
    if !inner.activation_live() || inner.cancel.is_cancelled() {
        // Activation may already have started decoding. Stop it before this
        // observer releases the descriptor, even if close is still pending.
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }

    let Some(bus) = pipeline.bus() else {
        // A pump that cannot run is terminal for the session: latch terminal
        // and publish `Stopped` together, so no control can publish after it
        // (review Y1).
        let _ = pipeline.set_state(gst::State::Null);
        error!("OwnTone decode pipeline has no bus");
        inner.publish_terminal(RuntimeFailure::PlaybackFailed);
        return;
    };

    // Activation started the pipeline, confirmed daemon playback and
    // published `Playing` while holding the shared first-effect boundary.
    // Never restart it here — Stop may already have won after activation
    // released that boundary — and never republish it: the pump has no
    // control state of its own, so a late `Playing` from here could only
    // overwrite a control the command worker has accepted since (refinery
    // R13). The test-only park below marks the point where that
    // republication used to run, so a regression can prove nothing is
    // published here across an accepted pause.
    #[cfg(test)]
    park_at_sentinel(&inner, "park-startup", "startup-parked", "startup-release");

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
                    // Close the write end before the restoring `player/stop`,
                    // exactly as the EOS path does: the daemon's reset pipe
                    // watcher must never find this writer still attached.
                    drop(write_fd.take());
                    // Terminal failure and publication share the Stop gate.
                    inner.publish_terminal(RuntimeFailure::PlaybackFailed);
                    // A failed restore is not a clean teardown; the session
                    // close path installs serialized recovery and retains the
                    // record (review R3).
                    let _ = inner.restore();
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
    // The observation is bound to the control epoch it was sampled under
    // (refinery R13): a control accepted while the request was in flight
    // makes the sampled state history, and the state write below must not
    // undo that control's own publication.
    let epoch = inner.control_epoch.load(Ordering::SeqCst);
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
    // Publish the observed state under the settlement boundary, and only if
    // no control was accepted since the sample was taken; a terminal
    // session's state is owned by its terminal publication.
    let _boundary = inner
        .mutation_lock
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if inner.terminal.load(Ordering::SeqCst) || inner.control_epoch.load(Ordering::SeqCst) != epoch
    {
        return;
    }
    *inner.state.lock().unwrap_or_else(|p| p.into_inner()) = state;
}

/// The daemon's own autostop: it reports `stop` for a finite item once its
/// autostarted pipe hit EOF. `pause` is a user-visible state, not a finished
/// item (review F4). Kept as a predicate so the pause-is-not-completion
/// regression is unit-testable.
fn daemon_completion_reached(state: &str) -> bool {
    state == "stop"
}

/// Decides completion from successive `/api/player` observations after the
/// writer closed.
///
/// Pinned OwnTone 29.3 autostops a pipe only while it is *autostarted*. A
/// `player/pause` stops the pipe input (`inputs/pipe.c: stop` →
/// `pipe_watch_reset`, which clears `pipe_autostart_id`), and the following
/// `player/play` restarts the same item as a plain source (`setup`:
/// `is_autostarted = (source->id == pipe_autostart_id)` is now false). Such a
/// pipe never emits EOF: at end-of-input `play` loops in `input_wait` and the
/// daemon keeps reporting `play`. Its `pos_ms` keeps counting across the
/// resume (`source_restart` resumes at the paused position) and stops moving
/// once the pipe is dry, so a `play` whose progress has not moved for
/// [`COMPLETION_PROGRESS_STALL`] is a rendered item: the caller then issues
/// the restoring `player/stop` itself. `pause` never completes.
struct CompletionTracker {
    /// The control epoch every retained sample was taken under. A sample
    /// from any other epoch starts the history over: a pause/resume pair
    /// that happened entirely between two polls (refinery R12) is a control
    /// transition exactly like one observed mid-request, and the interval
    /// spent paused must never count as continuous play.
    history_epoch: Option<u64>,
    last_state: String,
    last_progress: Option<u64>,
    unchanged_since: Option<Instant>,
}

impl CompletionTracker {
    fn new() -> Self {
        Self {
            history_epoch: None,
            last_state: String::new(),
            last_progress: None,
            unchanged_since: None,
        }
    }

    /// Forget every sample: a control transition was accepted, so no stall
    /// interval measured before it may carry over.
    fn reset(&mut self) {
        self.history_epoch = None;
        self.last_state.clear();
        self.last_progress = None;
        self.unchanged_since = None;
    }

    /// `stall_completes` is whether stalled progress may complete the item:
    /// true only once the daemon's autostart binding was lost (a pause was
    /// accepted). It is read afresh on every observation because a pause can
    /// be accepted after the writer closed, while this wait is already
    /// running (refinery R10): a value captured when the wait began would
    /// leave a resumed, drained pipe to time out as a failure. An autostarted
    /// pipe is expected to autostop; a `play` that merely stalls is then a
    /// fault and falls to the drain deadline.
    ///
    /// The stall is measured under continuous `play`: a state change (a
    /// resume after a pause that outlived the drain) restarts the clock, so a
    /// tail the daemon is about to read is never cut off as "already stalled".
    /// A `play` that reports no progress at all is not evidence of a drained
    /// pipe: it keeps waiting and falls to the drain deadline.
    fn observe(
        &mut self,
        state: &str,
        progress: Option<u64>,
        now: Instant,
        stall_completes: bool,
        epoch: u64,
    ) -> bool {
        if daemon_completion_reached(state) {
            return true;
        }
        if self.history_epoch != Some(epoch) {
            // First sample under this epoch: whatever was measured before it
            // belongs to a control state the user has left.
            self.reset();
            self.history_epoch = Some(epoch);
        }
        match self.unchanged_since {
            Some(since) if progress == self.last_progress && state == self.last_state => {
                stall_completes
                    && state == "play"
                    && progress.is_some()
                    && now.duration_since(since) >= COMPLETION_PROGRESS_STALL
            }
            _ => {
                self.last_state = state.to_string();
                self.last_progress = progress;
                self.unchanged_since = Some(now);
                false
            }
        }
    }
}

/// Bounds the *active* drain. Only time the daemon spends in an active state
/// is charged against [`DRAIN_DEADLINE`]: a healthy `pause` observation
/// suspends the budget (a paused item is the user's to resume, however long
/// they leave it — refinery R14), and the first active observation after a
/// pause — the resume — starts a fresh budget, so time spent paused never
/// shortens the drain of a just-resumed tail. One budget spans a whole
/// [`drain_and_restore`]: a completion its epoch check finds superseded
/// resumes the wait without renewing it, and cancellation is checked on every
/// poll regardless of the budget.
struct DrainBudget {
    remaining: Duration,
    charged_from: Instant,
    paused: bool,
}

impl DrainBudget {
    fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    fn starting_at(now: Instant) -> Self {
        Self {
            remaining: DRAIN_DEADLINE,
            charged_from: now,
            paused: false,
        }
    }

    /// Attribute a healthy observation: `pause` suspends charging; the first
    /// active state after a pause renews the budget.
    fn observe(&mut self, state: &str) {
        let paused = state == "pause";
        if self.paused && !paused {
            self.remaining = DRAIN_DEADLINE;
        }
        self.paused = paused;
    }

    /// Charge the interval since the previous tick unless the latest healthy
    /// observation was a pause. `true` once the active budget is spent.
    fn tick(&mut self, now: Instant) -> bool {
        if !self.paused {
            self.remaining = self
                .remaining
                .saturating_sub(now.saturating_duration_since(self.charged_from));
        }
        self.charged_from = now;
        self.remaining.is_zero()
    }
}

/// The outcome of one bounded drain wait.
enum DrainOutcome {
    /// The item completed under the control epoch its deciding observation
    /// was sampled in; restoration must re-validate that epoch.
    Completed {
        epoch: u64,
    },
    Failed(RuntimeFailure),
}

/// Poll the daemon — while `budget` has active drain time left — until the
/// item counts as completed per [`CompletionTracker`]. Every observation is bound to the control epoch it
/// was sampled under: the epoch and the autostart flag are read **before** the
/// request, and an epoch that moved while the request was in flight discards
/// the sample and the stall clock (a pause accepted meanwhile has made the
/// sampled state history; the flag it set must never apply to it). `Err(())`
/// is a cancellation observed while waiting.
fn await_daemon_completion(
    inner: &SessionInner,
    budget: &mut DrainBudget,
) -> Result<DrainOutcome, ()> {
    let cancelled = || {
        inner.cancel.is_cancelled()
            || inner.gate.is_stopped()
            || !inner.running.load(Ordering::SeqCst)
    };
    let mut tracker = CompletionTracker::new();
    #[cfg(test)]
    let mut processed = 0_u32;
    loop {
        if cancelled() {
            return Err(());
        }
        // Test-only: park here — after a processed sample, before this
        // request's epoch is captured — so a regression can accept controls
        // entirely between two polls (refinery R12).
        #[cfg(test)]
        park_between_requests(inner, processed);
        let epoch = inner.control_epoch.load(Ordering::SeqCst);
        let stall_completes = inner.autostart_lost.load(Ordering::SeqCst);
        let observation = inner.client.player_progress();
        // A bounded HTTP observation may have been in flight when Stop won.
        // Its success, error or timeout is no longer a playback outcome.
        if cancelled() {
            return Err(());
        }
        if inner.control_epoch.load(Ordering::SeqCst) != epoch {
            tracker.reset();
        } else {
            match observation {
                Ok((state, progress)) => {
                    #[cfg(test)]
                    {
                        processed += 1;
                    }
                    budget.observe(&state);
                    if tracker.observe(&state, progress, Instant::now(), stall_completes, epoch) {
                        return Ok(DrainOutcome::Completed { epoch });
                    }
                }
                Err(_) => return Ok(DrainOutcome::Failed(RuntimeFailure::CompletionUnconfirmed)),
            }
        }
        if budget.tick(Instant::now()) {
            return Ok(DrainOutcome::Failed(RuntimeFailure::CompletionTimedOut));
        }
        std::thread::sleep(DRAIN_POLL);
    }
}

/// Test-only park at a named point: while `<pipe>.<arm>` exists, announce
/// `<pipe>.<parked>`, wait for `<pipe>.<release>`, then consume all three so
/// later passes run freely.
#[cfg(test)]
fn park_at_sentinel(inner: &SessionInner, arm: &str, parked: &str, release: &str) {
    let pipe = &inner.config.pipe_path;
    if !pipe.with_extension(arm).exists() {
        return;
    }
    std::fs::write(pipe.with_extension(parked), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !pipe.with_extension(release).exists() {
        assert!(Instant::now() < deadline, "the {arm} park was not released");
        std::thread::sleep(Duration::from_millis(5));
    }
    for suffix in [arm, parked, release] {
        let _ = std::fs::remove_file(pipe.with_extension(suffix));
    }
}

/// Test-only between-request park: once the `park-between` sentinel exists
/// and at least one sample was processed, announce `between-parked`, wait for
/// `between-release`, then consume both so later polls run freely.
#[cfg(test)]
fn park_between_requests(inner: &SessionInner, processed: u32) {
    let pipe = &inner.config.pipe_path;
    if processed == 0 || !pipe.with_extension("park-between").exists() {
        return;
    }
    std::fs::write(pipe.with_extension("between-parked"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !pipe.with_extension("between-release").exists() {
        assert!(
            Instant::now() < deadline,
            "the between-request park was not released"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = std::fs::remove_file(pipe.with_extension("park-between"));
    let _ = std::fs::remove_file(pipe.with_extension("between-release"));
}

/// Wait for the item to complete and restore the daemon. `Err(())` is a
/// cancellation observed while waiting (close() owns restoration then);
/// `Ok(None)` a completed and restored item; `Ok(Some(reason))` a terminal
/// failure. A completion whose epoch restoration finds superseded is void:
/// the wait resumes on the same active-drain budget ([`DrainBudget`]).
fn drain_and_restore(inner: &SessionInner) -> Result<Option<RuntimeFailure>, ()> {
    let mut budget = DrainBudget::new();
    loop {
        match await_daemon_completion(inner, &mut budget)? {
            DrainOutcome::Failed(reason) => {
                // Restoration may block, so it must remain outside the Stop
                // gate. Failed restoration retains custody for close()'s
                // serialized recovery; the drain failure is the reported one.
                let _ = inner.restore();
                return Ok(Some(reason));
            }
            DrainOutcome::Completed { epoch } => match inner.restore_completion(epoch) {
                Ok(()) => return Ok(None),
                Err(CompletionRestore::Superseded) => {
                    // A voided completion is observed again on the poll
                    // cadence. While the session is paused and the daemon
                    // already reports `stop` (its own pause timeout, its web
                    // UI, a lost receiver), every observation completes at
                    // once and is voided at once; re-polling without a pause
                    // spun both processes on `GET /api/player` until the user
                    // resumed or stopped (pre-review audit, round 11).
                    std::thread::sleep(DRAIN_POLL);
                }
                Err(CompletionRestore::Failed(_)) => {
                    return Ok(Some(RuntimeFailure::RestorationFailed));
                }
            },
        }
    }
}

/// Natural EOS: close the write end so the daemon sees end-of-input, wait
/// (bounded) for daemon-confirmed completion, restore, then publish exactly one
/// generation-scoped `TrackEnded` (§4.3, §9 item 10). A drain deadline miss or
/// transport loss is terminal failure, never completion.
///
/// The caller has already closed the pipe write end via the owned descriptor it
/// passed in; this function only waits. Completion is the daemon's own `stop`
/// (an autostarted pipe hitting EOF) or, for a pipe the pinned daemon can no
/// longer autostop after a pause/resume cycle, a `play` whose progress has
/// stalled since the writer closed ([`CompletionTracker`]). `pause` is a
/// user-visible state, not a finished item, and treating any state other than
/// `play` as success reported a paused track as completed (review F4).
fn natural_completion(inner: &SessionInner, pipeline: &gst::Pipeline) {
    pipeline.set_state(gst::State::Null).ok();
    // The real finite-EOS path arms the fake daemon's next observation only
    // after decoder shutdown; this does not alter production ordering.
    #[cfg(test)]
    if inner.config.pipe_path.with_extension("park-drain").exists() {
        std::fs::write(inner.config.pipe_path.with_extension("drain-started"), "").unwrap();
    }
    // On cancellation, close() owns restoration/recovery after joining this
    // pump. Keep its route and instance lock until that settlement completes.
    let Ok(failure) = drain_and_restore(inner) else {
        return;
    };
    // Deterministically expose the restore-to-publication gap to real-pump
    // controller regressions; this adds no production synchronization.
    #[cfg(test)]
    if inner
        .config
        .pipe_path
        .with_extension("park-terminal")
        .exists()
    {
        std::fs::write(inner.config.pipe_path.with_extension("terminal-ready"), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !inner
            .config
            .pipe_path
            .with_extension("terminal-release")
            .exists()
        {
            assert!(
                Instant::now() < deadline,
                "terminal publication was not released"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // Lock order matches control publication: settlement boundary then gate.
    // Stop either wins first and suppresses every completion/error event, or
    // follows this entire bounded publication. No check-to-send race (AE1).
    let _boundary = inner
        .mutation_lock
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    inner.gate.publish_if_live(|| {
        if inner.cancel.is_cancelled() {
            return;
        }
        if let Some(failure) = failure {
            let _ = inner
                .event_tx
                .try_send(PlayerEvent::error(inner.generation, failure.message()));
        }
        inner.publish_state(PlayerState::Stopped);
        if failure.is_none() {
            let _ = inner
                .event_tx
                .try_send(PlayerEvent::ended(inner.generation));
        }
    });
}

impl SenderSession for OwnToneSession {
    fn write_pcm(&mut self, samples: &[u8]) -> SenderWriteOutcome {
        // The pump owns decode and writes through the pipeline, so pushed PCM
        // is unused (the same pipeline-sourced contract the GStreamer adapter
        // documents).
        SenderWriteOutcome::Accepted(samples.len())
    }

    fn set_volume(&mut self, level: f64) -> bool {
        let percent = (level.clamp(0.0, 1.0) * 100.0).round() as u8;
        self.inner
            .transmit_under_boundary(
                || {
                    self.inner
                        .client
                        .set_volume(percent)
                        .map_err(MutationOutcome::into_error)
                },
                None,
                true,
            )
            .is_ok()
    }

    fn pause(&mut self) -> bool {
        // An accepted pause is published, marks the autostart binding lost and
        // advances the control epoch as one decision under the boundary.
        self.inner
            .transmit_under_boundary(
                || {
                    // Idle the writer before the daemon resets its reader.
                    self.inner.hold_pipeline();
                    self.inner.client.player_control("pause")
                },
                Some(PlayerState::Paused),
                true,
            )
            .is_ok()
    }

    fn resume(&mut self) -> bool {
        // The first accepted resume is the activation that releases the inert
        // decode pump; the pump never starts playback on its own (review R4).
        // Acceptance, the `player/play` transmission and cancellation all share
        // a single serialized boundary, so a Stop/replacement either refuses
        // the play before it is transmitted or is compensated by the teardown's
        // restoration `player/stop` — never a stale play after a cancelled load
        // (review S4, review T3, review U3).
        self.inner.activate_and_play()
    }

    fn confirm_started(&self, publish: &mut dyn FnMut(PlayerState)) -> bool {
        // The worker's coarse `Playing` after a successful `resume` must respect
        // the terminal ordering exactly like the session's own publication.
        // `activate_and_play` already published the accepted start through the
        // settlement boundary, so the worker's own cache/event write is run
        // here, under the same boundary, instead of after this method returned
        // — the caller-side gap review Y1 rejected (review X1, review Y1).
        self.inner.publish_under_boundary(publish)
    }

    fn flush(&mut self) {
        // The daemon owns buffering; there is no local flush to perform.
    }

    fn observe(&self) -> SenderPosition {
        // The worker loop refreshes its coarse state cache immediately before
        // this call, so a regression can synchronize on the observation that
        // proves its cache is live rather than after teardown overwrites it.
        #[cfg(test)]
        self.inner.note_observe();
        *self
            .inner
            .position
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn state(&self) -> PlayerState {
        *self.inner.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn is_finished(&self) -> bool {
        // Waiting for the pump thread (rather than its early terminal latch)
        // lets EOS/error publication finish before close stops the event gate.
        // The worker polls at its bounded command interval and retains all
        // resources until its existing close/restoration/recovery completes.
        self.pump.as_ref().is_some_and(|pump| pump.is_finished())
    }

    fn close(self: Box<Self>) {
        let this = *self;
        // Win the shared Stop/start boundary and the serialized activation
        // boundary before tearing down: a close that races an accepted
        // activation must block any late `player/play` (review S4, review U3).
        // A refused control may have observed restore's terminal latch before
        // the pump publishes its outcome. Worker cleanup must join that pump
        // before closing its event gate (AJ1). Explicit Stop/replacement already
        // stop the shared gate and cancel the token on the controller path, so
        // they still suppress publication immediately. Genuine live failures
        // still stop decoding and settle here on the worker.
        if !this.inner.terminal.load(Ordering::SeqCst) {
            this.inner.gate.stop();
        }
        this.inner.cancel_activation();
        this.inner.running.store(false, Ordering::SeqCst);
        if let Some(pipeline) = this.inner.pipeline() {
            let _ = pipeline.set_state(gst::State::Null);
        }
        #[cfg(test)]
        if this
            .inner
            .config
            .pipe_path
            .with_extension("park-terminal")
            .exists()
        {
            std::fs::write(
                this.inner
                    .config
                    .pipe_path
                    .with_extension("terminal-close-joining"),
                "",
            )
            .unwrap();
        }
        if let Some(handle) = this.pump {
            let _ = handle.join();
        }
        this.inner.gate.stop();
        if this.inner.restore().is_ok() {
            // Dropping the lock file releases the advisory lock only after
            // restoration has completed (§4.3).
            drop(this.lock);
            return;
        }
        // A failed restoration is not a clean close: move the route off the
        // active lease into keyed custody *before* recovery starts, so a
        // replacement preparation in the interval can never revoke it (review
        // S5), then install the same serialized recovery a `RecoveryPending`
        // open leaves behind. This runs on the load worker, never on GTK.
        let ticket = this.inner.media_ticket.as_ref().map(Arc::clone);
        if let Some(ticket) = ticket.as_ref() {
            this.inner.media_proxy.move_to_recovery_custody(ticket);
        }
        let Some(lock) = this.lock else {
            // No lock to hold; the durable takeover record still makes the
            // next opener refuse rather than adopt a half-taken-over daemon.
            return;
        };
        let route = ticket.map(|ticket| (Arc::clone(&this.inner.media_proxy), ticket));
        let completion = spawn_serialized_recovery(
            Arc::clone(&this.inner.client),
            this.inner.config.clone(),
            this.inner.recorded.clone(),
            lock,
            route,
        );
        let _ = completion.wait();
    }
}

/// The OwnTone 29.x transmission path.
pub(super) struct OwnToneSender {
    /// The resolved configuration, or the specific reason it was refused.
    config: Result<OwnToneConfig, ConfigRefusal>,
}

impl OwnToneSender {
    /// Resolve the adapter from explicit configuration. A missing or invalid
    /// configuration yields a sender whose `probe` fails closed.
    pub(super) fn from_env() -> Self {
        let config = OwnToneConfig::from_env();
        match &config {
            Ok(config) => info!(api = %config.api_base, "OwnTone AirPlay sender configured"),
            Err(refusal) => debug!(reason = refusal.0, "OwnTone AirPlay sender not configured"),
        }
        Self { config }
    }

    /// `true` when the process is configured to select this adapter.
    pub(super) fn selected() -> bool {
        let value = std::env::var(ENV_SELECT).ok();
        let selected = selection_is_owntone(value.as_deref());
        if let Some(value) = value.as_deref().map(str::trim) {
            if !selected && !value.is_empty() && !value.eq_ignore_ascii_case("raopsink") {
                // A typo must not look like a deliberate choice of the default.
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    warn!(
                        value,
                        "unrecognized TRIBUTARY_AIRPLAY_SENDER value; the raopsink adapter is used"
                    );
                });
            }
        }
        selected
    }
}

impl AirplaySender for OwnToneSender {
    fn name(&self) -> &'static str {
        "owntone"
    }

    /// Non-blocking availability gate (review R1).
    ///
    /// `probe` runs synchronously on the GTK caller, so it must never touch
    /// the network: the daemon reachability/version check is network I/O with
    /// a documented timeout, and running it here froze the UI until the
    /// dedicated daemon answered (or the timeout expired) before the load
    /// worker even started. The blocking daemon handshake moves to
    /// [`open`], which the load path already runs on its own worker
    /// thread — off GTK — before any receiver state is read or mutated.
    ///
    /// What remains here is the local, fail-closed configuration check: the
    /// platform path, the ownership record, the binary and the pipe. A missing
    /// or foreign dedicated instance is still refused before any per-track
    /// media work, and the refusal is identical whether it is observed here
    /// or by the worker's first health step.
    fn probe(&self) -> Result<(), SenderError> {
        if !platform_available() {
            return Err(unavailable(
                "this_platform_has_no_supported_owntone_acquisition_path",
            ));
        }
        // A refused configuration reports its own reason (a malformed,
        // non-loopback or ambiguous API URL), never a generic "not configured".
        let config = self.config.as_ref().map_err(|refusal| refusal.error())?;
        // Record-only here: the kernel-verified process binding (review R5)
        // walks `/proc`, so it stays on the worker with the rest of the
        // non-local gate.
        config.verify_owned_record()?;
        if !config.binary.is_file() {
            return Err(unavailable("owntone_binary_was_not_found"));
        }
        ensure_pipe(&config.pipe_path)?;
        Ok(())
    }

    fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome {
        if ctx.cancel.is_cancelled() {
            return OpenOutcome::Cancelled;
        }
        match self.config.clone() {
            Ok(config) => open(config, ctx),
            Err(refusal) => OpenOutcome::Failed(refusal.error()),
        }
    }
}

/// Confirm the dedicated daemon answers and runs a verified release. This is the
/// blocking half of the old availability gate, deliberately executed on the
/// load worker's [`open`] rather than in the synchronous, GTK-thread `probe`
/// (review R1). It is the first daemon RPC the worker performs and it never
/// reads or mutates receiver state.
fn check_daemon_health(client: &OwnToneClient) -> Result<(), SenderError> {
    check_daemon_version(client.version()?)
}

/// The version window the adapter accepts: 29.3 or newer inside the verified
/// 29.x series. The pinned behaviour (pipe autostart, autostop and completion
/// semantics) was read from the 29.3 sources, so neither an older 29.x release
/// nor an unverified later major may proceed into daemon mutations (PR #270
/// review, round 10).
fn check_daemon_version(version: (u32, u32)) -> Result<(), SenderError> {
    if version < OWNTONE_MIN_VERSION {
        return Err(unavailable("dedicated_daemon_is_older_than_29_3"));
    }
    if version.0 > OWNTONE_VERIFIED_MAJOR {
        return Err(unavailable(
            "dedicated_daemon_is_newer_than_the_verified_29_x_series",
        ));
    }
    Ok(())
}

/// A failure observed before the first mutating RPC. A load the controller
/// already stopped or replaced has nothing to unwind and, per §4.1, nothing
/// to report: the observation that failed may be the very request its Stop
/// interrupted, and a cancelled generation publishes no event.
fn pre_mutation_failure(ctx: &SenderOpenContext, error: SenderError) -> OpenOutcome {
    if ctx.cancel.is_cancelled() {
        OpenOutcome::Cancelled
    } else {
        OpenOutcome::Failed(error)
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
    // The route hand-off a recovery-pending outcome carries: the seat moves
    // this load's ticket into custody before constructing that outcome (review
    // S5).
    let custody = CustodyHandoff::from_ctx(ctx);
    let Preflight {
        client,
        lock,
        pipe_identity,
        outputs,
        selected,
    } = match open_preflight(&config, ctx, deadline) {
        Ok(preflight) => preflight,
        Err(outcome) => return outcome,
    };

    // Mutation phase: every failure or cancellation from here unwinds.
    let recorded = TakeoverRecord {
        enabled_outputs: outputs
            .iter()
            .filter(|output| output.selected)
            .map(|output| output.id)
            .collect(),
        selected_output: selected,
    };
    // Nothing is recorded or sent yet: a listener that stopped being the
    // dedicated instance since the preflight is a pre-mutation refusal with its
    // own reason, not a takeover to recover from.
    if let Err(error) = client.authorize_mutation() {
        return pre_mutation_failure(ctx, error);
    }
    if let Err(error) = recorded.write(&config.takeover_record()) {
        return OpenOutcome::Failed(error);
    }
    if let Err((unwind, unsettled)) = take_over(&client, ctx, selected) {
        return unwind_takeover(unwind, unsettled, client, config, recorded, lock, &custody);
    }

    let write_fd = match open_pipe_write(&config.pipe_path, pipe_identity, deadline, &ctx.cancel) {
        Ok(fd) => fd,
        Err(CancelOrError::Cancelled) => {
            return unwind_takeover(
                Unwind::Cancelled,
                false,
                client,
                config,
                recorded,
                lock,
                &custody,
            );
        }
        Err(CancelOrError::Failed(error)) => {
            return unwind_takeover(
                Unwind::Failed(error),
                false,
                client,
                config,
                recorded,
                lock,
                &custody,
            );
        }
    };
    if ctx.cancel.is_cancelled() {
        drop(write_fd);
        return unwind_takeover(
            Unwind::Cancelled,
            false,
            client,
            config,
            recorded,
            lock,
            &custody,
        );
    }
    let pipeline = match build_pipeline(&ctx.prepared_uri, &write_fd) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            return unwind_takeover(
                Unwind::Failed(error),
                false,
                client,
                config,
                recorded,
                lock,
                &custody,
            );
        }
    };
    start_session(
        client, config, recorded, lock, ctx, pipeline, write_fd, &custody,
    )
}

/// A cancellation checkpoint between two pre-mutation steps: a cancelled load
/// is unwound silently, because nothing has been mutated yet.
fn cancel_point(ctx: &SenderOpenContext) -> Result<(), OpenOutcome> {
    if ctx.cancel.is_cancelled() {
        Err(OpenOutcome::Cancelled)
    } else {
        Ok(())
    }
}

/// Everything [`open`] establishes before the first mutating RPC. A failure
/// or cancellation on the way here has no takeover to unwind.
struct Preflight {
    client: Arc<OwnToneClient>,
    lock: std::fs::File,
    pipe_identity: PipeIdentity,
    outputs: Vec<OwnToneOutput>,
    selected: u64,
}

/// The pre-mutation phase of [`open`], in the fail-closed order the design
/// requires: ownership and FIFO identity, the blocking daemon handshake, the
/// instance lock, crash-record recovery, then the read phase.
fn open_preflight(
    config: &OwnToneConfig,
    ctx: &SenderOpenContext,
    deadline: Instant,
) -> Result<Preflight, OpenOutcome> {
    let client = OwnToneClient::for_owned_instance(config)
        .map(Arc::new)
        .map_err(|error| pre_mutation_failure(ctx, error))?;
    config
        .verify_owned()
        .map_err(|error| pre_mutation_failure(ctx, error))?;
    // Bind the FIFO's identity at the same verification step that proved the
    // dedicated daemon scans this pathname. The writer acquired after the
    // takeover mutations must resolve to this exact object (AM1).
    let pipe_identity = verify_pipe_identity(&config.pipe_path)
        .map_err(|error| pre_mutation_failure(ctx, error))?;
    cancel_point(ctx)?;

    // The blocking daemon handshake runs here, on the load worker, never on
    // the GTK caller (review R1). Reachability and version are the network I/O
    // the synchronous `probe` must not perform; running them before the lock
    // and before any receiver state read preserves the fail-closed ordering.
    check_daemon_health(&client).map_err(|error| pre_mutation_failure(ctx, error))?;
    cancel_point(ctx)?;

    // Exclusivity is locked before the first state read.
    let lock = acquire_instance_lock(config, ctx, deadline)?;
    recover_stale_takeover(&client, config, ctx)?;
    cancel_point(ctx)?;

    let (outputs, selected) = observe_takeover_target(&client, ctx)?;
    cancel_point(ctx)?;
    Ok(Preflight {
        client,
        lock,
        pipe_identity,
        outputs,
        selected,
    })
}

/// Take the instance lock. The previous session on this same output releases
/// its lock only after its own worker has restored the daemon, and the
/// controller does not serialize that worker behind this open, so a
/// sequential hand-over is waited for — bounded by the open deadline and raced
/// against cancellation — while a genuinely concurrent holder is still refused
/// at the deadline (§9 item 5: a replacement opens cleanly).
fn acquire_instance_lock(
    config: &OwnToneConfig,
    ctx: &SenderOpenContext,
    deadline: Instant,
) -> Result<std::fs::File, OpenOutcome> {
    let lock = open_lock(&config.lock_path()).map_err(|error| pre_mutation_failure(ctx, error))?;
    while rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
        cancel_point(ctx)?;
        if Instant::now() >= deadline {
            return Err(pre_mutation_failure(
                ctx,
                unavailable("another_tributary_session_is_already_using_the_dedicated_daemon"),
            ));
        }
        std::thread::sleep(FIFO_OPEN_POLL);
    }
    Ok(lock)
}

/// A record left by a crashed holder means the daemon may be half-taken over.
/// Under the freshly taken lock, recover it the way §4.3 requires of the next
/// opener: quiesce the instance (dropping anything the dead holder left in
/// flight), restore its recorded output set — which removes the record — and
/// only then proceed. A recovery that cannot be completed refuses the open and
/// leaves the record for the next attempt; the record is never adopted
/// silently.
fn recover_stale_takeover(
    client: &OwnToneClient,
    config: &OwnToneConfig,
    ctx: &SenderOpenContext,
) -> Result<(), OpenOutcome> {
    let record_path = config.takeover_record();
    if !record_path.exists() {
        return Ok(());
    }
    let Some(stale) = TakeoverRecord::read(&record_path) else {
        return Err(pre_mutation_failure(
            ctx,
            unavailable("previous_takeover_record_unreadable"),
        ));
    };
    cancel_point(ctx)?;
    if quiesce_daemon(config).is_err() || restore_daemon(client, config, &stale).is_err() {
        return Err(pre_mutation_failure(
            ctx,
            unavailable("previous_takeover_incomplete"),
        ));
    }
    cancel_point(ctx)
}

/// Read phase: the daemon's outputs, the receiver's mapping by its retained
/// identifier, and the no-preemption check. Nothing has been mutated, so a
/// failure here has no takeover to unwind.
fn observe_takeover_target(
    client: &OwnToneClient,
    ctx: &SenderOpenContext,
) -> Result<(Vec<OwnToneOutput>, u64), OpenOutcome> {
    let outputs = client
        .outputs()
        .map_err(|error| pre_mutation_failure(ctx, error))?;
    let selected = map_receiver_to_output(&outputs, ctx.target.device_id.as_deref())
        .map_err(|failure| pre_mutation_failure(ctx, failure.refusal()))?;
    // Never preempt audible playback on the dedicated instance.
    match client.player_state() {
        Ok(state) if state == "play" => Err(pre_mutation_failure(
            ctx,
            unavailable("dedicated_daemon_is_already_playing"),
        )),
        Ok(_) => Ok((outputs, selected)),
        Err(error) => Err(pre_mutation_failure(ctx, error)),
    }
}

/// How a takeover that could not complete unwinds.
enum Unwind {
    Cancelled,
    Failed(SenderError),
}

/// Mutation phase (§4.3): select the output, clear the queue and apply the
/// user's current volume **before any activation** — so a switch to OwnTone
/// starts at the slider's level instead of the daemon's prior value (review
/// S7) — observing cancellation between the steps. A mutating RPC that
/// returned an error (or timed out) may still have been applied server-side,
/// so its unwind is not clean: `unsettled` is reported `true` (review F3,
/// review T1). A refusal the process guard raises before transmission —
/// outer or inner — settles instead, because nothing was sent (PR #270
/// review, round 12). A cancellation between two successful steps unwinds
/// cleanly.
fn take_over(
    client: &OwnToneClient,
    ctx: &SenderOpenContext,
    selected: u64,
) -> Result<(), (Unwind, bool)> {
    let cancelled = || (Unwind::Cancelled, false);
    if ctx.cancel.is_cancelled() {
        return Err(cancelled());
    }
    let failed = |(error, unsettled)| (Unwind::Failed(error), unsettled);
    client
        .takeover_step(|client| client.set_outputs(&[selected]))
        .map_err(failed)?;
    if ctx.cancel.is_cancelled() {
        return Err(cancelled());
    }
    client
        .takeover_step(OwnToneClient::clear_queue)
        .map_err(failed)?;
    if ctx.cancel.is_cancelled() {
        return Err(cancelled());
    }
    client
        .takeover_step(|client| client.set_volume(volume_percent(ctx.volume)))
        .map_err(failed)?;
    if ctx.cancel.is_cancelled() {
        return Err(cancelled());
    }
    Ok(())
}

/// Unwind an open that failed or was cancelled after the takeover record was
/// written: restore cleanly, or hand off to serialized recovery when a
/// mutation is unsettled or restoration fails (review F2, review F3).
#[allow(clippy::too_many_arguments)]
fn unwind_takeover(
    unwind: Unwind,
    unsettled: bool,
    client: Arc<OwnToneClient>,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    custody: &CustodyHandoff,
) -> OpenOutcome {
    match unwind {
        Unwind::Cancelled => cancel_outcome(client, config, recorded, lock, unsettled, custody),
        Unwind::Failed(error) => {
            fail_outcome(client, config, recorded, lock, unsettled, error, custody)
        }
    }
}

/// Build the session and start its inert decode pump. A worker-spawn failure
/// *after* takeover reclaims the session state and unwinds, so a
/// half-taken-over daemon is never leaked (review F3).
#[allow(clippy::too_many_arguments)]
fn start_session(
    client: Arc<OwnToneClient>,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    ctx: &SenderOpenContext,
    pipeline: gst::Pipeline,
    write_fd: OwnedFd,
    custody: &CustodyHandoff,
) -> OpenOutcome {
    let inner = Arc::new(SessionInner {
        client,
        config,
        generation: ctx.generation,
        event_tx: ctx.event_tx.clone(),
        recorded,
        media_proxy: Arc::clone(&ctx.media_proxy),
        media_ticket: ctx.media_ticket.clone(),
        running: AtomicBool::new(true),
        activation: Mutex::new(ActivationState::default()),
        gate: Arc::clone(&ctx.session_gate),
        cancel: ctx.cancel.clone(),
        restored: AtomicBool::new(false),
        autostart_lost: AtomicBool::new(false),
        control_epoch: AtomicU64::new(0),
        terminal: AtomicBool::new(false),
        mutation_lock: Mutex::new(()),
        unsettled: AtomicUsize::new(0),
        position: Mutex::new(SenderPosition::unknown(ctx.generation)),
        state: Mutex::new(PlayerState::Buffering),
        pipeline: Mutex::new(Some(pipeline.clone())),
        #[cfg(test)]
        probe: SessionProbe::default(),
    });

    let pump_inner = Arc::clone(&inner);
    let pump = std::thread::Builder::new()
        .name("airplay-owntone-pump".to_string())
        .spawn(move || run_pump(pump_inner, pipeline, write_fd));
    let Ok(pump) = pump else {
        let error = unavailable("decode_pump_could_not_be_started");
        return match Arc::try_unwrap(inner) {
            Ok(session) => fail_outcome(
                session.client,
                session.config,
                session.recorded,
                lock,
                false,
                error,
                custody,
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

/// The linear `[0.0, 1.0]` volume mapped to the daemon's integer percent, so
/// the initial-volume application is a pure, testable computation (review S7).
fn volume_percent(level: f64) -> u8 {
    (level.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// The route hand-off a recovery-pending outcome needs: the load's proxy and
/// its own ticket. The seat moves the ticket into keyed custody **before** it
/// constructs the outcome, so a replacement load's supersession revocation can
/// never observe it on the active lease in the interval before the load path
/// receives the outcome (review S5).
struct CustodyHandoff {
    proxy: Arc<GstreamerMediaProxy>,
    ticket: Option<Arc<GstreamerMediaTicket>>,
}

impl CustodyHandoff {
    fn from_ctx(ctx: &SenderOpenContext) -> Self {
        Self {
            proxy: Arc::clone(&ctx.media_proxy),
            ticket: ctx.media_ticket.clone(),
        }
    }

    /// Move this load's ticket off the active lease into recovery custody.
    /// Idempotent.
    fn move_to_custody(&self) {
        if let Some(ticket) = self.ticket.as_ref() {
            self.proxy.move_to_recovery_custody(ticket);
        }
    }

    /// The route handle the serialized recovery releases at its terminal
    /// disposition.
    fn route(&self) -> Option<(Arc<GstreamerMediaProxy>, Arc<GstreamerMediaTicket>)> {
        self.ticket
            .as_ref()
            .map(|ticket| (Arc::clone(&self.proxy), Arc::clone(ticket)))
    }
}

/// Handle a failure after the first mutating RPC. A clean restoration inside
/// the cleanup deadline returns the original failure; otherwise the record is
/// preserved and recovery is serialized behind a `RecoveryPending` failure
/// (review F3).
#[allow(clippy::too_many_arguments)]
fn fail_outcome(
    client: Arc<OwnToneClient>,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    unsettled: bool,
    original: SenderError,
    custody: &CustodyHandoff,
) -> OpenOutcome {
    if !unsettled {
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        if settle_restore(&client, &config, &recorded, deadline) {
            drop(lock);
            return OpenOutcome::Failed(original);
        }
    }
    // The pending-recovery refusal replaces the failure that caused it; keep
    // the cause in the log.
    warn!(
        unsettled,
        reason = %original.message(),
        "OwnTone takeover failed and could not be unwound; recovery is serialized"
    );
    OpenOutcome::Failed(recovery_pending(client, config, recorded, lock, custody))
}

/// Handle a cancellation after the first mutating RPC. A clean restoration
/// returns `Cancelled` silently; an unsettled mutation or failed restoration
/// returns the non-clean `RecoveryPending` failure (review F2, review F3).
fn cancel_outcome(
    client: Arc<OwnToneClient>,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    unsettled: bool,
    custody: &CustodyHandoff,
) -> OpenOutcome {
    if !unsettled {
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        if settle_restore(&client, &config, &recorded, deadline) {
            drop(lock);
            return OpenOutcome::Cancelled;
        }
    }
    OpenOutcome::Failed(recovery_pending(client, config, recorded, lock, custody))
}

/// Attempt restoration until `deadline`. Used as the "settle" half of
/// settle-or-restart: a mutating RPC that settled on its own is compensated by
/// re-running restoration, after which ownership can be released cleanly.
///
/// A restoration attempt that itself fails may have transmitted a `PUT` that
/// is still outstanding — retrying compensation cannot retract it (review S3).
/// After the first failure the daemon is therefore quiesced (terminated and
/// restarted, dropping every in-flight request) before the next attempt, so a
/// late restoring mutation cannot land after restoration. If quiescence itself
/// cannot be established, a later successful compensation is **not** accepted
/// as proof of settlement: the caller must retain ownership (review T1).
fn settle_restore(
    client: &OwnToneClient,
    config: &OwnToneConfig,
    recorded: &TakeoverRecord,
    deadline: Instant,
) -> bool {
    // A clean restoration first: the caller had nothing outstanding.
    if restore_daemon(client, config, recorded).is_ok() {
        return true;
    }
    loop {
        // The failed restoration step transmitted a `PUT` that may still be
        // outstanding. Quiesce before compensating; if quiescence cannot be
        // established, no later compensation is trustworthy.
        if quiesce_daemon(config).is_err() {
            return false;
        }
        if restore_daemon(client, config, recorded).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(RECOVERY_POLL);
    }
}

/// One retained recovery that a live owner must drive to settlement. Abstracted
/// behind a trait so the supervisor's liveness, retry and fairness contracts
/// are deterministically testable without a live daemon (review U2).
trait RecoveryJob: Send {
    /// When this job may next be attempted.
    fn retry_at(&self) -> Instant;
    fn set_retry_at(&mut self, at: Instant);
    /// One settlement attempt. Returns `true` once the job's resources (the
    /// advisory lock and any custodied route) have been released; `false` to
    /// requeue for a later attempt.
    fn attempt(&self) -> bool;
}

/// A recovery that could not establish quiescence inside the inline recovery
/// deadline. Rather than leaking the advisory lock descriptor with no owner
/// (review T2), the lock, the route and everything needed to retry are handed
/// to a process-global supervisor with a reachable retry/release path.
struct RetainedRecovery {
    client: Arc<OwnToneClient>,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    /// Held — and therefore keeping the advisory lock — until this job is
    /// dropped after a successful attempt.
    lock: std::fs::File,
    route: Option<(Arc<GstreamerMediaProxy>, Arc<GstreamerMediaTicket>)>,
    retry_at: Instant,
}

impl RecoveryJob for RetainedRecovery {
    fn retry_at(&self) -> Instant {
        self.retry_at
    }

    fn set_retry_at(&mut self, at: Instant) {
        self.retry_at = at;
    }

    fn attempt(&self) -> bool {
        if quiesce_daemon(&self.config).is_ok()
            && restore_daemon(&self.client, &self.config, &self.recorded).is_ok()
        {
            if let Some((proxy, ticket)) = self.route.as_ref() {
                proxy.take_and_release(ticket);
            }
            return true;
        }
        false
    }
}

/// Backoff between supervisor-worker spawn attempts while a live owner is
/// being proven, and its cap (review U2, review V2).
const SUPERVISOR_SPAWN_BACKOFF: Duration = Duration::from_millis(10);
const SUPERVISOR_SPAWN_BACKOFF_MAX: Duration = Duration::from_secs(1);

struct SupervisorQueue {
    jobs: Vec<Box<dyn RecoveryJob>>,
    /// Proven liveness of the servicing thread. Cleared by its exit guard, so
    /// a thread that died is restarted on the next registration (review U2).
    worker_live: bool,
}

/// Process-global registry and worker for retained recoveries (review T2).
/// Lazily started only when the inline recovery cannot establish quiescence, so
/// the common path pays nothing. The supervisor keeps the advisory lock held
/// and retries quiescence + restoration until they succeed, then releases the
/// route by identity and drops the lock — a live recovery owner until proven
/// settlement.
///
/// **A live owner is proven, not assumed (review U2).** The worker is started
/// only when its spawn actually succeeds; a failed spawn leaves the job queued
/// (and its lock held) and the next registration — or the next call to
/// [`Self::global`] — retries creation. The queue is serviced in retry order
/// rather than blocking on one job forever, so one unavailable instance cannot
/// starve another.
struct RecoverySupervisor {
    queue: Mutex<SupervisorQueue>,
    signal: Condvar,
    /// Test-only: force the next N worker spawns to fail, so the
    /// spawn-failure/retry path is exercised deterministically.
    #[cfg(test)]
    fail_spawns: AtomicUsize,
}

impl RecoverySupervisor {
    fn new() -> Self {
        Self {
            queue: Mutex::new(SupervisorQueue {
                jobs: Vec::new(),
                worker_live: false,
            }),
            signal: Condvar::new(),
            #[cfg(test)]
            fail_spawns: AtomicUsize::new(0),
        }
    }

    fn instance() -> Arc<Self> {
        static SUPERVISOR: OnceLock<Arc<RecoverySupervisor>> = OnceLock::new();
        Arc::clone(SUPERVISOR.get_or_init(|| Arc::new(Self::new())))
    }

    /// The process-global supervisor, with a live worker ensured whenever work
    /// is pending. A previously failed spawn is retried here, so a later
    /// recovery never finds a permanently unserviced registry (review U2).
    fn global() -> Arc<Self> {
        let supervisor = Self::instance();
        supervisor.ensure_worker_if_pending();
        supervisor
    }

    /// Hand a retained recovery to the supervisor.
    fn register(self: &Arc<Self>, job: RetainedRecovery) {
        self.enqueue(Box::new(job));
    }

    fn enqueue(self: &Arc<Self>, mut job: Box<dyn RecoveryJob>) {
        job.set_retry_at(Instant::now());
        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            queue.jobs.push(job);
        }
        self.signal.notify_all();
        // Prove a live owner before completing the handoff. A spawn that keeps
        // failing is retried **on this recovery thread** until a worker exists,
        // so a temporary spawn failure can never strand the retained lock and
        // route with no owner — the retry path does not depend on a future,
        // unrelated registration (review V2). The job stays queued (with its
        // lock held) for the whole retry, so no false clean handoff is
        // reported. This runs on a dedicated recovery/load worker, never on the
        // UI Stop path.
        self.ensure_worker_until_live();
    }

    /// Retry worker creation with bounded backoff until a worker is live. This
    /// is the autonomous retry owner that review V2 requires: it keeps going
    /// after the inline attempt budget is exhausted, so a recovered spawn
    /// facility picks up the queued job without another registration.
    fn ensure_worker_until_live(self: &Arc<Self>) {
        let mut backoff = SUPERVISOR_SPAWN_BACKOFF;
        while !self.ensure_worker() {
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(SUPERVISOR_SPAWN_BACKOFF_MAX);
        }
    }

    fn ensure_worker_if_pending(self: &Arc<Self>) {
        let pending = {
            let queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            !queue.jobs.is_empty() && !queue.worker_live
        };
        if pending {
            let _ = self.ensure_worker();
        }
    }

    /// Ensure exactly one live worker. Returns `true` when a worker is (now)
    /// live and `false` when no live owner could be started.
    fn ensure_worker(self: &Arc<Self>) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        if queue.worker_live {
            return true;
        }
        #[cfg(test)]
        if self.fail_spawns.load(Ordering::SeqCst) > 0 {
            self.fail_spawns.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        let supervisor = Arc::clone(self);
        match std::thread::Builder::new()
            .name("airplay-owntone-supervisor".to_string())
            .spawn(move || supervisor.run_worker())
        {
            Ok(_) => {
                queue.worker_live = true;
                true
            }
            Err(_) => false,
        }
    }

    /// The supervisor thread. Repeatedly takes the next due job, attempts it
    /// once, and requeues a failure — never blocking on one instance, so two
    /// independent retained recoveries are both serviced (review U2).
    fn run_worker(self: Arc<Self>) {
        let _alive = WorkerAlive {
            supervisor: Arc::clone(&self),
        };
        loop {
            let job = self.take_due();
            if job.attempt() {
                // Drop releases the job's advisory lock and any custodied
                // route now that settlement is proven.
                drop(job);
            } else {
                self.requeue(job);
            }
        }
    }

    /// Take the next job whose retry deadline has passed, waiting (bounded)
    /// for the earliest deadline when none is currently due.
    fn take_due(&self) -> Box<dyn RecoveryJob> {
        let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let now = Instant::now();
            if let Some(position) = queue.jobs.iter().position(|job| job.retry_at() <= now) {
                return queue.jobs.remove(position);
            }
            let wait = queue
                .jobs
                .iter()
                .map(|job| job.retry_at().saturating_duration_since(now))
                .min()
                .unwrap_or(RECOVERY_SUPERVISOR_POLL)
                .min(RECOVERY_SUPERVISOR_POLL);
            let (next, _) = self
                .signal
                .wait_timeout(queue, wait)
                .unwrap_or_else(|p| p.into_inner());
            queue = next;
        }
    }

    fn requeue(&self, mut job: Box<dyn RecoveryJob>) {
        job.set_retry_at(Instant::now() + RECOVERY_POLL);
        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            queue.jobs.push(job);
        }
        self.signal.notify_all();
    }

    /// Test-only: force the next `count` worker spawns to fail.
    #[cfg(test)]
    fn fail_next_spawns(&self, count: usize) {
        self.fail_spawns.store(count, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn pending_jobs(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .jobs
            .len()
    }

    #[cfg(test)]
    fn worker_is_live(&self) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .worker_live
    }
}

/// Clears the supervisor's proven-liveness flag when its worker exits, so a
/// replacement worker is started on the next registration (review U2).
struct WorkerAlive {
    supervisor: Arc<RecoverySupervisor>,
}

impl Drop for WorkerAlive {
    fn drop(&mut self) {
        let mut queue = self
            .supervisor
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        queue.worker_live = false;
        self.supervisor.signal.notify_all();
    }
}

/// Start the serialized recovery shared by the `RecoveryPending` open path and
/// the live-session teardown path (review F3, review R3). It holds the advisory
/// lock until its terminal outcome, quiesces the dedicated daemon before any
/// restoration attempt (review R2), retries restoration until the recovery
/// deadline, and — when a route is supplied — releases that route by identity
/// only after a terminal disposition.
///
/// Quiescence must be **confirmed** before either terminal outcome: a missing
/// recovery owner, or a recovery that never established quiescence, retains the
/// lock and the route for the supervisor rather than reporting a false clean
/// failure (review S3).
#[cfg(test)]
static INJECTED_RECOVERY_SPAWN_FAILURES: AtomicUsize = AtomicUsize::new(0);

/// Test-only: force the next `count` inline serialized-recovery thread spawns to
/// fail, so the fallback that hands the retained lock and route to the
/// process-global supervisor is exercised deterministically (review X2).
#[cfg(test)]
fn fail_next_recovery_spawns(count: usize) {
    INJECTED_RECOVERY_SPAWN_FAILURES.store(count, Ordering::SeqCst);
}

/// Test-only: consume one injected inline spawn failure, if any.
#[cfg(test)]
fn take_injected_recovery_spawn_failure() -> bool {
    INJECTED_RECOVERY_SPAWN_FAILURES
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
            if remaining > 0 {
                Some(remaining - 1)
            } else {
                None
            }
        })
        .is_ok()
}

/// Test-only: the endpoint whose next serialized recovery should be captured
/// instead of started inline. Keyed by `api_base` so a concurrent recovery for
/// another fixture can never steal the capture, and vice versa (review Z1).
#[cfg(test)]
static CAPTURE_RETAINED_API_BASE: Mutex<Option<String>> = Mutex::new(None);

/// Test-only: the captured retained recovery.
#[cfg(test)]
static CAPTURED_RETAINED_RECOVERY: Mutex<Option<RetainedRecovery>> = Mutex::new(None);

/// Test-only: capture the next serialized recovery for `api_base` instead of
/// starting it inline, so a controller-level regression owns the retained
/// lock/route and drives settlement at a chosen boundary.
#[cfg(test)]
fn arm_retained_recovery_capture(api_base: &str) {
    CAPTURED_RETAINED_RECOVERY
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    *CAPTURE_RETAINED_API_BASE
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(api_base.to_string());
}

/// Test-only: consume the capture arm if it targets this endpoint.
#[cfg(test)]
fn take_retained_recovery_capture_for(api_base: &str) -> bool {
    let mut slot = CAPTURE_RETAINED_API_BASE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if slot.as_deref() == Some(api_base) {
        *slot = None;
        true
    } else {
        false
    }
}

/// Test-only: take the captured retained recovery, if the recovery already ran.
#[cfg(test)]
fn take_captured_retained_recovery() -> Option<RetainedRecovery> {
    CAPTURED_RETAINED_RECOVERY
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
}

fn spawn_serialized_recovery(
    client: Arc<OwnToneClient>,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    route: Option<(Arc<GstreamerMediaProxy>, Arc<GstreamerMediaTicket>)>,
) -> RecoveryCompletion {
    let completion = RecoveryCompletion::default();
    let worker = completion.clone();
    // The lock lives in a lease the spawned worker takes ownership of. If the
    // inline worker cannot be started (or cannot establish quiescence), the
    // lease, the route and the retry state are handed to the process-global
    // supervisor rather than leaked with no owner (review T2).
    let lease = Arc::new(Mutex::new(Some(lock)));
    let worker_lease = Arc::clone(&lease);
    // Fallback copies so a spawn failure can still register a live owner.
    let fallback_client = Arc::clone(&client);
    let fallback_config = config.clone();
    let fallback_recorded = recorded.clone();
    let fallback_route = route.clone();
    let builder = std::thread::Builder::new().name("airplay-owntone-recovery".to_string());
    // Test-only: hand the retained recovery to the regression instead of
    // starting an inline owner, so settlement is driven at a chosen boundary.
    #[cfg(test)]
    if take_retained_recovery_capture_for(&config.api_base) {
        let retained_lock = lease.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(lock) = retained_lock {
            *CAPTURED_RETAINED_RECOVERY
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(RetainedRecovery {
                client: Arc::clone(&client),
                config: config.clone(),
                recorded: recorded.clone(),
                lock,
                route: route.clone(),
                retry_at: Instant::now(),
            });
        }
        completion.resolve(RecoveryOutcome::Retained {
            message: "test-controlled retained recovery".to_string(),
        });
        return completion;
    }
    let worker_body = move || {
        // Hold the advisory lock until recovery is terminal so ownership is
        // never released early.
        let lock_guard = worker_lease
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let deadline = Instant::now() + RECOVERY_DEADLINE;
        // `settled` is true only after a successful quiescence with no
        // subsequent failed restoration attempt: a restoration step that
        // fails may itself have transmitted a `PUT` that is still
        // outstanding (review T1).
        let mut settled = quiesce_daemon(&config).is_ok();
        loop {
            if settled {
                if restore_daemon(&client, &config, &recorded).is_ok() {
                    if let Some((proxy, ticket)) = route.as_ref() {
                        proxy.take_and_release(ticket);
                    }
                    worker.resolve(RecoveryOutcome::Restored);
                    return;
                }
                // The attempt may have transmitted an outstanding `PUT`: it
                // is not settled until the instance is quiesced again.
                settled = false;
            }
            if Instant::now() >= deadline {
                if settled {
                    // Quiescence was established with nothing outstanding
                    // since, so no old-generation mutation can survive; the
                    // record stays for the supervisor and the route is
                    // released by the load path.
                    worker.resolve(RecoveryOutcome::RestorationFailed {
                        message: "restoration did not complete before the recovery deadline"
                            .to_string(),
                    });
                } else {
                    // Quiescence was never established: an outstanding
                    // mutation may still land. Hand the retained recovery to
                    // the process-global supervisor, which keeps the lock and
                    // the route and retries until settlement (review T2).
                    if let Some(lock) = lock_guard {
                        RecoverySupervisor::global().register(RetainedRecovery {
                            client: Arc::clone(&client),
                            config: config.clone(),
                            recorded: recorded.clone(),
                            lock,
                            route: route.clone(),
                            retry_at: Instant::now(),
                        });
                    }
                    worker.resolve(RecoveryOutcome::Retained {
                        message: "quiescence was not established before the recovery deadline"
                            .to_string(),
                    });
                }
                return;
            }
            std::thread::sleep(RECOVERY_POLL);
            settled = quiesce_daemon(&config).is_ok();
        }
    };
    #[cfg(test)]
    let spawned = if take_injected_recovery_spawn_failure() {
        Err(std::io::Error::other(
            "injected serialized-recovery spawn failure",
        ))
    } else {
        builder.spawn(worker_body)
    };
    #[cfg(not(test))]
    let spawned = builder.spawn(worker_body);
    if spawned.is_err() {
        // No inline recovery owner could start. Hand the retained recovery to
        // the process-global supervisor rather than leaking the descriptor
        // (review T2), and report the terminal retained outcome.
        let retained_lock = lease.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(lock) = retained_lock {
            RecoverySupervisor::global().register(RetainedRecovery {
                client: fallback_client,
                config: fallback_config,
                recorded: fallback_recorded,
                lock,
                route: fallback_route,
                retry_at: Instant::now(),
            });
        }
        completion.resolve(RecoveryOutcome::Retained {
            message: "the serialized recovery could not be started".to_string(),
        });
    }
    completion
}

/// Build the non-clean recovery-pending failure and start the serialized
/// recovery (review F3). The custody move happens here, before the outcome is
/// constructed, so the route is off the active lease by the time the load path
/// sees `RecoveryPending` (review S5).
fn recovery_pending(
    client: Arc<OwnToneClient>,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    custody: &CustodyHandoff,
) -> SenderError {
    let message = unavailable("recovery_is_pending_for_the_dedicated_daemon")
        .message()
        .to_string();
    custody.move_to_custody();
    let completion = spawn_serialized_recovery(client, config, recorded, lock, custody.route());
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
        .map_err(|_| unavailable("decode_pipeline_could_not_be_constructed"))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| unavailable("decode_pipeline_is_not_a_pipeline"))?;
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
            .map_err(|_| unavailable("instance_state_directory_could_not_be_created"))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|_| unavailable("instance_lock_file_could_not_be_opened"))
}

/// Build a fully-owned [`OwnToneSender`] for controller-path regressions: a
/// temp state directory carrying a matching ownership record, a created FIFO
/// and a real (dummy) binary file, pointed at `api_base`.
#[cfg(all(test, owntone_host))]
pub(super) fn test_owned_sender(api_base: &str, state_dir: &Path, binary: &Path) -> OwnToneSender {
    let config = OwnToneConfig {
        api_base: api_base.trim_end_matches('/').to_string(),
        pipe_path: state_dir.join("airplay.pcm"),
        state_dir: state_dir.to_path_buf(),
        binary: binary.to_path_buf(),
    };
    let record = OwnershipRecord {
        token: OWNER_TOKEN.to_string(),
        api_base: config.api_base.clone(),
        pipe_path: config.pipe_path.to_string_lossy().into_owned(),
        state_dir: config.state_dir.to_string_lossy().into_owned(),
        binary: config.binary.to_string_lossy().into_owned(),
        restart_command: None,
    };
    std::fs::write(
        config.owner_marker(),
        serde_json::to_vec(&record).expect("serialize record"),
    )
    .expect("write record");
    ensure_pipe(&config.pipe_path).expect("create pipe");
    OwnToneSender { config: Ok(config) }
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

    /// R4/S4: the decode pump's activation barrier is deterministic and reads
    /// the same serialized decision the load path's acceptance and teardown's
    /// cancellation use. It releases only on an accepted activation; an
    /// already-cancelled, torn-down, or never-activated load leaves the pump
    /// inert, so no pipeline start, no PCM and no daemon play can follow a
    /// cancelled load.
    #[test]
    fn activation_gate_releases_only_on_accepted_activation() {
        let future = Instant::now() + Duration::from_millis(50);
        let open = || Mutex::new(ActivationState::default());

        // Never activated inside the bound: inert.
        let running = AtomicBool::new(true);
        let activation = open();
        let cancel = OpenCancel::new();
        assert!(!activation_gate(
            &activation,
            &running,
            &cancel,
            Instant::now(),
            Duration::ZERO
        ));

        // Cancelled before activation: inert even when a stale acceptance
        // races in.
        let activation = open();
        activation.lock().unwrap().cancelled = true;
        assert!(!activation_gate(
            &activation,
            &running,
            &cancel,
            future,
            Duration::ZERO
        ));

        // Torn down before activation: inert.
        let running = AtomicBool::new(false);
        let activation = open();
        let cancel = OpenCancel::new();
        assert!(!activation_gate(
            &activation,
            &running,
            &cancel,
            future,
            Duration::ZERO
        ));

        // Accepted activation: released.
        let running = AtomicBool::new(true);
        let activation = open();
        activation.lock().unwrap().accepted = true;
        let cancel = OpenCancel::new();
        assert!(activation_gate(
            &activation,
            &running,
            &cancel,
            future,
            Duration::ZERO
        ));
    }

    /// S4: acceptance and cancellation share one serialized decision. A
    /// cancellation that wins refuses a late acceptance, and a torn-down
    /// session refuses activation too.
    #[test]
    fn activation_and_cancellation_share_a_serialized_boundary() {
        // Cancellation first: a late acceptance is refused.
        let mut state = ActivationState {
            accepted: false,
            cancelled: true,
        };
        assert!(
            !activation_decide(&mut state, true),
            "a cancelled session must refuse a late activation"
        );
        assert!(!state.accepted);

        // Torn down first: activation is refused.
        let mut state = ActivationState::default();
        assert!(!activation_decide(&mut state, false));
        assert!(!state.accepted);

        // Acceptance first: accepted, and cancellation then wins.
        let mut state = ActivationState::default();
        assert!(activation_decide(&mut state, true));
        assert!(state.accepted && !state.cancelled);
        state.cancelled = true;
        assert!(!activation_decide(&mut state, true));
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
        let english = unavailable_in("en", "not_configured").message().to_string();
        assert!(english.contains("not configured"), "{english}");
        assert!(english.contains("OwnTone"), "{english}");

        for locale in rust_i18n::available_locales!() {
            let message = unavailable_in(&locale, "not_configured");
            let message = message.message();
            let reason = rust_i18n::t!(
                "errors.playback.airplay_owntone_reason.not_configured",
                locale = locale
            );
            assert!(!message.is_empty(), "{locale} is empty");
            // The reason comes from the selected catalog — never the key path,
            // never an English clause inside a translated sentence.
            assert!(
                !reason.contains("airplay_owntone_reason"),
                "{locale}: {reason}"
            );
            assert!(message.contains(reason.as_ref()), "{locale}: {message}");
            if locale != "en" {
                assert_ne!(message, english, "{locale} must not fall back to English");
                assert!(
                    !message.contains("not configured"),
                    "{locale} carries English: {message}"
                );
            }
        }
    }

    /// The version window is 29.3 or newer inside the verified 29.x series:
    /// an older 29.x release lacks the pinned pipe semantics and a later major
    /// is unverified, so both are refused before any daemon mutation (PR #270
    /// review, round 10).
    #[test]
    fn daemon_version_gate_accepts_only_the_verified_series() {
        for accepted in [(29, 3), (29, 4), (29, 10)] {
            assert!(check_daemon_version(accepted).is_ok(), "{accepted:?}");
        }
        let older = unavailable_in("en", "dedicated_daemon_is_older_than_29_3");
        for refused in [(0, 0), (28, 9), (29, 0), (29, 2)] {
            let error = check_daemon_version(refused).expect_err("an older daemon is refused");
            assert_eq!(error.message(), older.message(), "{refused:?}");
        }
        let newer = unavailable_in(
            "en",
            "dedicated_daemon_is_newer_than_the_verified_29_x_series",
        );
        for refused in [(30, 0), (31, 2)] {
            let error = check_daemon_version(refused).expect_err("a later major is refused");
            assert_eq!(error.message(), newer.message(), "{refused:?}");
        }
    }

    /// Every terminal failure a live session publishes renders from the
    /// selected catalog — no English literal reaches `PlayerEvent::error`
    /// (PR #270 review, round 10).
    #[test]
    fn runtime_failures_are_localized_in_every_catalog() {
        let failures = [
            RuntimeFailure::PlaybackFailed,
            RuntimeFailure::CompletionUnconfirmed,
            RuntimeFailure::CompletionTimedOut,
            RuntimeFailure::RestorationFailed,
        ];
        for locale in rust_i18n::available_locales!() {
            for failure in failures {
                let message = failure.message_in(&locale);
                assert!(
                    !message.contains(RUNTIME_CATALOG),
                    "{locale} lacks {RUNTIME_CATALOG}.{}",
                    failure.key()
                );
                assert!(!message.trim().is_empty(), "{locale}: {failure:?}");
                if locale != "en" {
                    assert_ne!(
                        message,
                        failure.message_in("en"),
                        "{locale} carries the English text for {failure:?}"
                    );
                }
            }
        }
    }

    /// The receiver-mapping refusals render from the selected catalog with the
    /// output identifier as a parameter — no English clause in a translated
    /// sentence (PR #270 review, round 9).
    #[test]
    fn mapping_failures_are_localized_in_every_catalog() {
        let english_no_match = MappingFailure::NoMatch(0x8EE5)
            .refusal()
            .message()
            .to_string();
        assert!(english_no_match.contains("36581"), "{english_no_match}");
        for locale in rust_i18n::available_locales!() {
            for (failure, key) in [
                (
                    MappingFailure::MissingIdentifier,
                    "receiver_published_no_retained_identifier",
                ),
                (
                    MappingFailure::NoMatch(0x8EE5),
                    "receiver_not_in_daemon_output_list",
                ),
                (
                    MappingFailure::Ambiguous(0x8EE5),
                    "receiver_maps_to_multiple_outputs",
                ),
            ] {
                let message = match failure {
                    MappingFailure::MissingIdentifier => unavailable_in(&locale, key),
                    MappingFailure::NoMatch(id) | MappingFailure::Ambiguous(id) => {
                        unavailable_with_id_in(&locale, key, id)
                    }
                };
                let message = message.message().to_string();
                assert!(
                    !message.contains("airplay_owntone_reason"),
                    "{locale}: {message}"
                );
                assert!(
                    !message.contains("%{"),
                    "{locale}: unrendered id in {message}"
                );
                if !matches!(failure, MappingFailure::MissingIdentifier) {
                    assert!(message.contains("36581"), "{locale}: {message}");
                }
                if locale != "en" {
                    assert!(
                        !message.contains(&failure.to_string()),
                        "{locale} carries the English mapping text: {message}"
                    );
                }
            }
        }
    }

    /// Every reason key this module refuses with exists in every catalog, so
    /// no locale can fall back to English for a runtime refusal.
    #[test]
    fn every_refusal_reason_exists_in_every_catalog() {
        // rustfmt may break `unavailable(` and its key across lines, so skip
        // whitespace between the paren and the opening quote.
        let source = include_str!("airplay_owntone.rs");
        let mut keys: Vec<&str> = source
            .match_indices("unavailable")
            .filter_map(|(at, _)| {
                // A refusal constructor call whose first argument is the key literal.
                let call = &source[at..];
                let open = call.find('(')?;
                // Only the two refusal constructors; `unavailable_in(locale, …)`
                // and friends take a locale first.
                if !matches!(
                    &call[..open],
                    "unavailable" | "unavailable_with_id" | "unavailable_config"
                ) {
                    return None;
                }
                let rest = call[open + 1..].trim_start();
                let body = rest.strip_prefix('"')?;
                Some(&body[..body.find('"')?])
            })
            // Only real keys: the scan also sees this test's own string literals.
            .filter(|key| {
                !key.is_empty()
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            .collect();
        keys.sort_unstable();
        keys.dedup();
        assert!(
            keys.len() > 50,
            "expected the refusal reasons, found {keys:?}"
        );
        for locale in rust_i18n::available_locales!() {
            let path = format!("{}/locales/{locale}.yml", env!("CARGO_MANIFEST_DIR"));
            let catalog = std::fs::read_to_string(&path).expect("catalog readable");
            for key in &keys {
                assert!(
                    catalog.contains(&format!("\n      {key}: ")),
                    "{locale}.yml lacks airplay_owntone_reason.{key}"
                );
            }
        }
    }

    #[test]
    fn selection_is_explicit_and_exact() {
        assert!(selection_is_owntone(Some("owntone")));
        assert!(selection_is_owntone(Some("OwnTone")));
        assert!(selection_is_owntone(Some("OWNTONE")));
        assert!(selection_is_owntone(Some(" owntone ")));
        assert!(!selection_is_owntone(Some("raopsink")));
        assert!(!selection_is_owntone(Some("")));
        assert!(!selection_is_owntone(None));
    }

    #[test]
    fn unconfigured_sender_probe_fails_closed() {
        let sender = OwnToneSender {
            config: Err(unavailable_config("not_configured")),
        };
        let error = sender.probe().expect_err("unconfigured sender must refuse");
        let expected = if platform_available() {
            "not configured"
        } else {
            "no supported OwnTone acquisition path"
        };
        assert!(error.message().contains(expected), "{}", error.message());
    }

    /// The takeover record is staged and renamed, so a failed write can never
    /// leave the unreadable record that refuses every later load.
    #[test]
    fn takeover_record_write_is_atomic_and_leaves_no_stage_behind() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join(".tributary-takeover.json");
        let stage = path.with_extension("json.partial");
        let record = TakeoverRecord {
            enabled_outputs: vec![3, 5],
            selected_output: 5,
        };
        // A damaged record from an earlier crash is replaced, not appended to.
        std::fs::write(&path, b"{\"enabled_out").unwrap();
        record.write(&path).expect("the record is persisted");
        assert_eq!(TakeoverRecord::read(&path), Some(record.clone()));
        assert!(!stage.exists(), "the stage is renamed away");

        let missing = directory
            .path()
            .join("absent")
            .join(".tributary-takeover.json");
        assert!(record.write(&missing).is_err());
        assert!(!missing.exists());
        assert!(!missing.with_extension("json.partial").exists());
    }

    /// A failure after the stage was renamed into place retracts the final
    /// record: a reported failure must never leave a record a later load
    /// would misread as crashed-takeover evidence and quiesce a daemon over
    /// (PR #270 review, round 12). A parent directory without read permission
    /// accepts the staging and the rename but refuses the open for the
    /// directory sync, so the post-rename failure is reached deterministically
    /// (as non-root; root bypasses the permission check).
    #[test]
    fn a_directory_sync_failure_after_the_rename_leaves_no_record() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join(".tributary-takeover.json");
        let record = TakeoverRecord {
            enabled_outputs: vec![7],
            selected_output: 7,
        };
        std::fs::set_permissions(
            directory.path(),
            std::fs::Permissions::from_mode(0o300), // write+execute, no read
        )
        .expect("the directory mode is restricted");
        let outcome = record.write(&path);
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755))
            .expect("the directory mode is restored");
        outcome.expect_err("the failed directory sync is reported");
        assert!(
            !path.exists(),
            "the final record must not survive a reported failure"
        );
        assert!(!path.with_extension("json.partial").exists());
    }

    /// The ownership record binds the endpoint as the adapter normalizes it:
    /// a record written with the exported URL's trailing slash still matches.
    #[test]
    fn ownership_record_matches_an_endpoint_written_with_a_trailing_slash() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = OwnToneConfig::from_values(
            Some("http://127.0.0.1:3689/".to_string()),
            Some(
                directory
                    .path()
                    .join("airplay.pcm")
                    .to_string_lossy()
                    .into_owned(),
            ),
            Some(directory.path().to_string_lossy().into_owned()),
            None,
        )
        .expect("a loopback endpoint");
        for api_base in ["http://127.0.0.1:3689/", "http://127.0.0.1:3689"] {
            let record = OwnershipRecord {
                token: OWNER_TOKEN.to_string(),
                api_base: api_base.to_string(),
                pipe_path: config.pipe_path.to_string_lossy().into_owned(),
                state_dir: config.state_dir.to_string_lossy().into_owned(),
                binary: config.binary.to_string_lossy().into_owned(),
                restart_command: None,
            };
            std::fs::write(config.owner_marker(), serde_json::to_vec(&record).unwrap()).unwrap();
            config
                .verify_owned_record()
                .expect("the record binds this endpoint");
        }
    }

    /// A refused configuration reports the operator's actual mistake. The
    /// sender used to drop the parser's error and answer "not configured" for
    /// a malformed, non-loopback or ambiguous API URL, so those catalog
    /// entries were unreachable from the UI (PR #270 review, round 11).
    #[test]
    fn a_refused_configuration_reports_its_own_reason() {
        let resolve = |api: Option<&str>| {
            OwnToneConfig::from_values(
                api.map(str::to_string),
                Some("/run/tributary/airplay.pcm".to_string()),
                Some("/run/tributary".to_string()),
                None,
            )
        };
        assert!(resolve(Some("http://127.0.0.1:3689")).is_ok());
        for (api, reason) in [
            (None, "not_configured"),
            (Some("not a url"), "configured_api_url_is_invalid"),
            (
                Some("http://192.0.2.10:3689"),
                "json_api_must_be_bound_to_loopback",
            ),
            (
                Some("http://localhost:3689"),
                "json_api_loopback_host_must_be_a_literal_address",
            ),
        ] {
            let refusal = resolve(api).expect_err("the configuration is refused");
            assert_eq!(refusal, ConfigRefusal(reason), "{api:?}");
            let sender = OwnToneSender {
                config: Err(refusal),
            };
            let error = sender.probe().expect_err("a refused sender never probes");
            if platform_available() {
                assert_eq!(
                    error.message(),
                    unavailable_in("en", reason).message(),
                    "{api:?}"
                );
            }
        }
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
        assert!(config("http://[::1]:3689").verify_loopback().is_ok());
        // T5: an ambiguous name resolves to both loopback families, so the
        // process the kernel matches and the endpoint HTTP dials can differ.
        // It is refused; only a literal address is accepted.
        assert!(config("http://localhost:3689").verify_loopback().is_err());
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

    /// AM1: the writer is bound to the FIFO verified on the load worker, never
    /// to whatever the pathname resolves to at acquisition time. A foreign
    /// object planted at the pathname is refused and left untouched.
    #[test]
    fn pipe_writer_is_bound_to_the_verified_fifo() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pipe = directory.path().join("airplay.pcm");
        ensure_pipe(&pipe).expect("create fifo");
        let identity = verify_pipe_identity(&pipe).expect("fifo identity");
        let deadline = || Instant::now() + Duration::from_millis(200);
        let cancel = OpenCancel::new();
        let read_end = || {
            rustix::fs::open(
                &pipe,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("open reader")
        };
        // Holding the original FIFO open keeps its inode from being reused by
        // the replacement FIFO below.
        let reader = read_end();
        assert!(open_pipe_write(&pipe, identity, deadline(), &cancel).is_ok());

        // A regular file planted at the pathname.
        std::fs::remove_file(&pipe).unwrap();
        std::fs::write(&pipe, b"foreign bytes").unwrap();
        assert!(verify_pipe_identity(&pipe).is_err());
        assert!(matches!(
            open_pipe_write(&pipe, identity, deadline(), &cancel),
            Err(CancelOrError::Failed(_))
        ));
        assert_eq!(std::fs::read(&pipe).unwrap(), b"foreign bytes");

        // A symlink to a foreign file is refused without being followed.
        std::fs::remove_file(&pipe).unwrap();
        let foreign = directory.path().join("foreign");
        std::fs::write(&foreign, b"foreign bytes").unwrap();
        std::os::unix::fs::symlink(&foreign, &pipe).unwrap();
        assert!(verify_pipe_identity(&pipe).is_err());
        assert!(matches!(
            open_pipe_write(&pipe, identity, deadline(), &cancel),
            Err(CancelOrError::Failed(_))
        ));
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign bytes");
        assert!(std::fs::symlink_metadata(&pipe)
            .unwrap()
            .file_type()
            .is_symlink());

        // Another FIFO at the pathname, even one with a reader, is not the
        // verified object; it is accepted only once it is verified itself.
        std::fs::remove_file(&pipe).unwrap();
        ensure_pipe(&pipe).unwrap();
        let other_reader = read_end();
        assert!(matches!(
            open_pipe_write(&pipe, identity, deadline(), &cancel),
            Err(CancelOrError::Failed(_))
        ));
        let replacement = verify_pipe_identity(&pipe).unwrap();
        assert_ne!(replacement, identity);
        assert!(open_pipe_write(&pipe, replacement, deadline(), &cancel).is_ok());
        drop(other_reader);
        drop(reader);
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
            restart_command: None,
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
        assert!(config.verify_owned_record().is_ok());

        let foreign_endpoint = OwnToneConfig {
            api_base: "http://127.0.0.1:9999".to_string(),
            ..config.clone()
        };
        assert!(foreign_endpoint.verify_owned_record().is_err());

        let foreign_pipe = OwnToneConfig {
            pipe_path: directory.path().join("other.pcm"),
            ..config.clone()
        };
        assert!(foreign_pipe.verify_owned_record().is_err());

        let foreign_binary = OwnToneConfig {
            binary: PathBuf::from("/usr/bin/not-owntone"),
            ..config.clone()
        };
        assert!(foreign_binary.verify_owned_record().is_err());
    }

    #[test]
    fn ownership_record_rejects_a_foreign_token_and_a_missing_record() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = owned_config(directory.path());
        assert!(config.verify_owned_record().is_err());

        std::fs::write(config.owner_marker(), "not a tributary token").expect("write marker");
        assert!(config.verify_owned_record().is_err());
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

    /// F4: a paused item is not a completed item; only `stop` completes, so a
    /// paused track is never reported as `TrackEnded`.
    #[test]
    fn drain_budget_charges_only_active_time_and_renews_on_resume() {
        let t0 = Instant::now();
        let mut budget = DrainBudget::starting_at(t0);
        budget.observe("play");
        assert!(!budget.tick(t0 + Duration::from_secs(4)));
        // A healthy pause suspends the budget for as long as it lasts.
        budget.observe("pause");
        assert!(!budget.tick(t0 + Duration::from_secs(4) + DRAIN_DEADLINE * 6));
        assert!(!budget.tick(t0 + Duration::from_secs(4) + DRAIN_DEADLINE * 60));
        // The resume starts a fresh budget: the 4 s spent draining before the
        // pause are not held against the resumed tail.
        let resumed = t0 + Duration::from_secs(4) + DRAIN_DEADLINE * 60;
        budget.observe("play");
        assert!(!budget.tick(resumed + DRAIN_DEADLINE.saturating_sub(Duration::from_secs(1))));
        assert!(budget.tick(resumed + DRAIN_DEADLINE + Duration::from_millis(100)));
    }

    #[test]
    fn drain_budget_active_time_alone_spends_the_deadline() {
        let t0 = Instant::now();
        let mut budget = DrainBudget::starting_at(t0);
        budget.observe("play");
        assert!(!budget.tick(t0 + DRAIN_DEADLINE / 2));
        // Repeated play samples never renew: only a pause→active edge does.
        budget.observe("play");
        assert!(!budget.tick(t0 + DRAIN_DEADLINE.saturating_sub(Duration::from_millis(100))));
        assert!(budget.tick(t0 + DRAIN_DEADLINE));
        // Time before the first observation counts as active drain too.
        let mut fresh = DrainBudget::starting_at(t0);
        assert!(fresh.tick(t0 + DRAIN_DEADLINE));
    }

    #[test]
    fn drain_budget_spent_before_a_pause_is_not_charged_while_paused() {
        let t0 = Instant::now();
        let mut budget = DrainBudget::starting_at(t0);
        budget.observe("play");
        // Paused just short of the deadline: the pause must hold, and the
        // resume that follows gets a full budget rather than the leftover.
        assert!(!budget.tick(t0 + DRAIN_DEADLINE.saturating_sub(Duration::from_millis(500))));
        budget.observe("pause");
        assert!(!budget.tick(t0 + DRAIN_DEADLINE * 3));
        budget.observe("play");
        let resumed = t0 + DRAIN_DEADLINE * 3;
        assert!(!budget.tick(resumed + DRAIN_DEADLINE / 2));
        assert!(budget.tick(resumed + DRAIN_DEADLINE));
    }

    #[test]
    fn completion_tracker_completes_on_stop_or_stalled_play_never_pause() {
        let t0 = Instant::now();
        let stall = COMPLETION_PROGRESS_STALL;
        let mut tracker = CompletionTracker::new();
        assert!(
            tracker.observe("stop", None, t0, false, 0),
            "autostop completes at once"
        );

        // An autostarted pipe never completes on stalled progress alone.
        let mut tracker = CompletionTracker::new();
        assert!(!tracker.observe("play", Some(100), t0, false, 0));
        assert!(!tracker.observe("play", Some(100), t0 + stall * 3, false, 0));

        let mut tracker = CompletionTracker::new();
        assert!(!tracker.observe("play", Some(100), t0, true, 0));
        assert!(
            !tracker.observe("play", Some(200), t0 + stall, true, 0),
            "advancing"
        );
        assert!(!tracker.observe("play", Some(200), t0 + stall + stall / 2, true, 0));
        assert!(
            tracker.observe("play", Some(200), t0 + stall * 2, true, 0),
            "drained"
        );

        let mut tracker = CompletionTracker::new();
        assert!(!tracker.observe("play", None, t0, true, 0));
        assert!(
            !tracker.observe("play", None, t0 + stall * 3, true, 0),
            "no progress evidence never counts as drained"
        );

        let mut tracker = CompletionTracker::new();
        assert!(!tracker.observe("pause", Some(300), t0, true, 0));
        assert!(
            !tracker.observe("pause", Some(300), t0 + stall * 3, true, 0),
            "a paused item is never complete"
        );
        assert!(
            !tracker.observe("play", Some(300), t0 + stall * 3, true, 0),
            "a resume restarts the stall clock"
        );
        assert!(!tracker.observe("play", Some(300), t0 + stall * 3 + stall / 2, true, 0));
        assert!(tracker.observe("play", Some(300), t0 + stall * 4, true, 0));
    }

    /// A reset forgets the stall clock: the next stalled sample starts over.
    #[test]
    fn completion_tracker_reset_forgets_the_stall_clock() {
        let t0 = Instant::now();
        let stall = COMPLETION_PROGRESS_STALL;
        let mut tracker = CompletionTracker::new();
        assert!(!tracker.observe("play", Some(500), t0, true, 0));
        tracker.reset();
        assert!(
            !tracker.observe("play", Some(500), t0 + stall * 2, true, 0),
            "a pre-reset sample must not count towards the stall"
        );
        assert!(tracker.observe("play", Some(500), t0 + stall * 3, true, 0));
    }

    /// R12: the history is bound to the epoch it was built under. Samples
    /// taken under a new epoch start over even when nothing was observed
    /// in between (a pause/resume pair between two polls).
    #[test]
    fn completion_tracker_starts_over_when_the_epoch_moves_between_samples() {
        let t0 = Instant::now();
        let stall = COMPLETION_PROGRESS_STALL;
        let mut tracker = CompletionTracker::new();
        assert!(!tracker.observe("play", Some(500), t0, false, 1));
        // 1.1 s later, same progress, but two controls were accepted meanwhile
        // (pause, resume): epoch 3, now eligible. Not completion.
        assert!(
            !tracker.observe("play", Some(500), t0 + stall + stall / 10, true, 3),
            "the paused interval must not count as continuous play"
        );
        assert!(!tracker.observe("play", Some(500), t0 + stall + stall / 2, true, 3));
        assert!(
            tracker.observe("play", Some(500), t0 + stall * 2 + stall / 10, true, 3),
            "a fresh continuous-playing interval under epoch 3 completes"
        );
        // Same epoch throughout: the stall accumulates as before.
        let mut tracker = CompletionTracker::new();
        assert!(!tracker.observe("play", Some(500), t0, true, 7));
        assert!(tracker.observe("play", Some(500), t0 + stall, true, 7));
    }

    /// R10: the autostart binding can be lost while the drain wait is already
    /// running (a pause accepted after the writer closed). The tracker must
    /// act on the flag as it is now, not as it was when the wait began: the
    /// same stalled `play` that was a fault under an autostarted item is the
    /// rendered item once the pause/resume cycle has happened.
    #[test]
    fn completion_tracker_honours_an_autostart_loss_during_the_wait() {
        let t0 = Instant::now();
        let stall = COMPLETION_PROGRESS_STALL;
        let mut tracker = CompletionTracker::new();
        // Autostarted: stalled progress is not completion.
        assert!(!tracker.observe("play", Some(500), t0, false, 0));
        assert!(!tracker.observe("play", Some(500), t0 + stall * 2, false, 0));
        // A pause is accepted mid-wait, then a resume.
        assert!(!tracker.observe("pause", Some(500), t0 + stall * 3, true, 0));
        assert!(!tracker.observe("play", Some(500), t0 + stall * 4, true, 0));
        assert!(
            !tracker.observe("play", Some(500), t0 + stall * 4 + stall / 2, true, 0),
            "the resume restarted the stall clock"
        );
        assert!(
            tracker.observe("play", Some(500), t0 + stall * 5, true, 0),
            "the drained, no-longer-autostarted item completes"
        );
        // The flag is consulted per observation: without it the same stall is
        // still a fault.
        let mut tracker = CompletionTracker::new();
        assert!(!tracker.observe("play", Some(500), t0, false, 0));
        assert!(!tracker.observe("play", Some(500), t0 + stall * 2, false, 0));
        assert!(tracker.observe("play", Some(500), t0 + stall * 2, true, 0));
    }

    #[test]
    fn completion_requires_stop_and_never_accepts_pause() {
        assert!(daemon_completion_reached("stop"));
        assert!(!daemon_completion_reached("pause"));
        assert!(!daemon_completion_reached("play"));
        assert!(!daemon_completion_reached(""));
    }

    /// S7: the initial-volume application maps the UI level to the daemon's
    /// integer percent (clamped), so a low/muted slider starts low/muted.
    #[test]
    fn volume_percent_maps_and_clamps_the_slider() {
        assert_eq!(volume_percent(1.0), 100);
        assert_eq!(volume_percent(0.5), 50);
        assert_eq!(volume_percent(0.0), 0);
        assert_eq!(volume_percent(-1.0), 0);
        assert_eq!(volume_percent(2.0), 100);
    }

    /// R5/S2: `/proc/net/tcp` is parsed to the socket inodes LISTENing on the
    /// configured **address and** port — the kernel-side identity the JSON API
    /// cannot provide. A listener sharing only the port (a different loopback
    /// address) is not the dedicated endpoint.
    #[test]
    fn listening_inodes_parses_the_proc_net_tcp_table() {
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:0DA5 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0\n\
   1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 99 1 0000000000000000 100 0 0 10 0\n\
   2: 0100007F:0DA5 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 555 1 0000000000000000 100 0 0 10 0\n\
   3: 0B00007F:0DA5 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1 0000000000000000 100 0 0 10 0\n";
        let loopback = vec!["0100007F".to_string()];
        assert_eq!(listening_inodes(table, 0x0DA5, &loopback), vec![12345]);
        assert_eq!(listening_inodes(table, 0x1F90, &loopback), vec![99]);
        // A non-LISTEN row is not an owner.
        assert!(listening_inodes(table, 0x0DA5, &loopback)
            .iter()
            .all(|inode| *inode != 555));
        // The same port on a different bound address is not this endpoint.
        assert!(listening_inodes(table, 0x0DA5, &loopback)
            .iter()
            .all(|inode| *inode != 777));
        assert!(listening_inodes(table, 4242, &loopback).is_empty());
    }

    /// S2: the configured loopback host maps to the kernel's little-endian
    /// address rendering, so the endpoint binding is address- and port-exact.
    #[test]
    fn expected_local_addrs_use_the_kernel_byte_order() {
        assert_eq!(
            expected_local_addrs("127.0.0.1"),
            vec!["0100007F".to_string()]
        );
        assert_eq!(
            expected_local_addrs("::1"),
            vec!["00000000000000000000000001000000".to_string()]
        );
        assert_eq!(expected_local_addrs("localhost").len(), 2);
        assert!(expected_local_addrs("192.168.1.10").is_empty());
    }

    /// R5: the listener bound to a port is resolved to its owning process out
    /// of band. The test binds its own loopback listener, so the owner must be
    /// this test process. Linux-only: resolution reads `/proc` (review T6).
    #[cfg(owntone_host)]
    #[test]
    fn listener_process_resolves_the_process_bound_to_a_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let port = listener.local_addr().expect("addr").port();
        let process = listener_process(&format!("http://127.0.0.1:{port}"))
            .expect("a listener must resolve to its owner");
        assert_eq!(process.pid, std::process::id());
        assert!(!process.exe.as_os_str().is_empty());
        // A port nobody holds resolves to no process.
        let released = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let free_port = released.local_addr().expect("addr").port();
        drop(released);
        assert!(listener_process(&format!("http://127.0.0.1:{free_port}")).is_none());
    }

    /// U5: the launch binding is the **effective** configuration option — the
    /// value of `-c`/`--config` — resolved through symlinks, restricted to the
    /// canonical state-directory `owntone.conf`, and required to bind the
    /// adapter's FIFO. A bare path argument, a foreign or non-canonical config,
    /// a symlinked name, ambiguity, and a config that names another pipe are
    /// all refused.
    #[test]
    fn cmdline_binding_requires_the_effective_configuration_and_pipe() {
        fn argv(args: &[&str]) -> Vec<String> {
            args.iter().map(|value| value.to_string()).collect()
        }

        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path();
        let pipe = state_dir.join("airplay.pcm");
        // The binding accepts only a real FIFO at the scanned pathname.
        ensure_pipe(&pipe).expect("create scanned FIFO");
        let config = state_dir.join(OWNTONE_CONFIG_FILE);
        std::fs::write(&config, dedicated_config::fixture(&pipe)).expect("write config");
        std::fs::create_dir_all(state_dir.join("sub")).expect("sub dir");

        // The effective option names the canonical file and binds the FIFO.
        assert!(cmdline_binds_instance(
            &argv(&["/usr/bin/owntone", "-c", &config.to_string_lossy()]),
            state_dir,
            &pipe
        ));
        assert!(cmdline_binds_instance(
            &argv(&[
                "/usr/bin/owntone",
                &format!("--config={}", config.display())
            ]),
            state_dir,
            &pipe
        ));
        // A `..` that stays inside the state directory still binds exactly.
        assert!(cmdline_binds_instance(
            &argv(&[
                "/usr/bin/owntone",
                "-c",
                &format!("{}/sub/../owntone.conf", state_dir.display()),
            ]),
            state_dir,
            &pipe
        ));

        // A bare path argument is not an effective configuration.
        assert!(!cmdline_binds_instance(
            &argv(&[
                "/usr/bin/owntone",
                &state_dir.to_string_lossy(),
                &config.to_string_lossy(),
            ]),
            state_dir,
            &pipe
        ));
        assert!(!cmdline_binds_instance(
            &argv(&["/usr/bin/owntone"]),
            state_dir,
            &pipe
        ));

        // A foreign configuration is refused.
        assert!(!cmdline_binds_instance(
            &argv(&["/usr/bin/owntone", "-c", "/etc/owntone.conf"]),
            state_dir,
            &pipe
        ));
        // A sibling directory sharing the name as a bare string prefix is not
        // the configured one (the S2 substring collision).
        let sibling = format!("{}-other/owntone.conf", state_dir.display());
        assert!(!cmdline_binds_instance(
            &argv(&["/usr/bin/owntone", "-c", &sibling]),
            state_dir,
            &pipe
        ));
        // A `..` traversal out of the state directory is refused.
        let traversal = format!("{}/../foreign.conf", state_dir.display());
        assert!(!cmdline_binds_instance(
            &argv(&["/usr/bin/owntone", "-c", &traversal]),
            state_dir,
            &pipe
        ));
        // A non-canonical configuration beneath the state directory is refused.
        let unrelated = state_dir.join("other.conf");
        std::fs::write(
            &unrelated,
            "library { directories = { \"/var/tmp/nope\" } }\n",
        )
        .expect("write unrelated");
        assert!(!cmdline_binds_instance(
            &argv(&["/usr/bin/owntone", "-c", &unrelated.to_string_lossy()]),
            state_dir,
            &pipe
        ));
        // More than one effective configuration is ambiguous and refused.
        assert!(!cmdline_binds_instance(
            &argv(&[
                "/usr/bin/owntone",
                "-c",
                &config.to_string_lossy(),
                "--config",
                &config.to_string_lossy(),
            ]),
            state_dir,
            &pipe
        ));

        // A symlinked launch file is refused even when it resolves elsewhere.
        let foreign = directory.path().join("foreign.conf");
        std::fs::write(&foreign, dedicated_config::fixture(&pipe)).expect("foreign config");
        let symlinked_dir = tempfile::tempdir().expect("tempdir");
        let symlink_state = symlinked_dir.path().join("state");
        std::fs::create_dir_all(&symlink_state).expect("state dir");
        let symlink_config = symlink_state.join(OWNTONE_CONFIG_FILE);
        std::os::unix::fs::symlink(&foreign, &symlink_config).expect("symlink config");
        assert!(!cmdline_binds_instance(
            &argv(&["/usr/bin/owntone", "-c", &symlink_config.to_string_lossy()]),
            &symlink_state,
            &pipe
        ));

        // A configuration that binds a different FIFO is refused.
        let other_dir = tempfile::tempdir().expect("tempdir");
        let other_pipe = other_dir.path().join("other.pcm");
        ensure_pipe(&other_pipe).expect("create other FIFO");
        let other_config = other_dir.path().join(OWNTONE_CONFIG_FILE);
        std::fs::write(&other_config, dedicated_config::fixture(&other_pipe))
            .expect("other config");
        assert!(!cmdline_binds_instance(
            &argv(&["/usr/bin/owntone", "-c", &other_config.to_string_lossy()]),
            other_dir.path(),
            &pipe
        ));
    }

    /// R5: a matching ownership record is not enough — a foreign process bound
    /// to the configured endpoint is refused. Linux-only: resolution reads
    /// `/proc` (review T6).
    #[cfg(owntone_host)]
    #[test]
    fn verify_daemon_process_refuses_a_foreign_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let port = listener.local_addr().expect("addr").port();
        let directory = tempfile::tempdir().expect("tempdir");
        let binary = directory.path().join("owntone");
        std::fs::write(&binary, b"#!/bin/true\n").expect("write dummy binary");
        let config = OwnToneConfig {
            api_base: format!("http://127.0.0.1:{port}"),
            pipe_path: directory.path().join("airplay.pcm"),
            state_dir: directory.path().to_path_buf(),
            binary,
        };
        assert!(verify_daemon_process(&config).is_err());
    }

    /// R2/S3/T5: quiescence terminates the owned process (escalating to
    /// `SIGKILL`) and **confirms** exit before returning. Linux-only: the
    /// observation primitive reads `/proc` (review T6).
    #[cfg(owntone_host)]
    #[test]
    fn signal_and_wait_stops_a_child_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let identity = read_process(pid).expect("read child").identity();
        assert!(signal_and_wait(identity, Duration::from_secs(5)).is_ok());
        // The child is gone or a zombie awaiting our reap — never still
        // running.
        assert!(matches!(
            observe_process(pid),
            ProcessObservation::Gone | ProcessObservation::Live('Z')
        ));
        let _ = child.wait();
    }

    /// S3/T5: a pid that no longer exists is already quiesced (ESRCH), not an
    /// error that could mask a live process. Linux-only (review T6).
    #[cfg(owntone_host)]
    #[test]
    fn signal_and_wait_treats_a_gone_process_as_quiesced() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let identity = read_process(pid).expect("read child").identity();
        let _ = child.kill();
        let _ = child.wait();
        assert!(signal_and_wait(identity, Duration::from_secs(1)).is_ok());
    }

    /// T5: a replaced identity (a pid whose start time no longer matches) is
    /// refused rather than signalled.
    #[cfg(owntone_host)]
    #[test]
    fn signal_and_wait_refuses_a_replaced_identity() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        // A forged start time that cannot belong to the live child.
        let forged = ProcessIdentity {
            pid,
            start_time: u64::MAX,
        };
        assert!(identity_still_ours(forged).is_err());
        let _ = child.kill();
        let _ = child.wait();
    }

    /// S3: the recovery outcome retains custody rather than reporting a clean
    /// failure when quiescence was never established.
    #[test]
    fn retained_recovery_outcome_is_terminal_and_distinct() {
        let completion = RecoveryCompletion::default();
        completion.resolve(RecoveryOutcome::Retained {
            message: "quiescence was not established".to_string(),
        });
        assert_eq!(
            completion.wait(),
            RecoveryOutcome::Retained {
                message: "quiescence was not established".to_string()
            }
        );
    }

    /// R3: `restore` reports its outcome instead of silently swallowing a
    /// failed restoration, so callers can install serialized recovery.
    #[test]
    fn restore_reports_a_failed_restoration() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = owned_config(directory.path());
        let recorded = TakeoverRecord {
            enabled_outputs: vec![1],
            selected_output: 2,
        };
        let client = Arc::new(OwnToneClient::new("http://127.0.0.1:1").expect("client"));
        let inner = SessionInner {
            client,
            config,
            generation: PlayerEventGeneration::from_raw(1),
            event_tx: async_channel::unbounded().0,
            recorded,
            media_proxy: Arc::new(GstreamerMediaProxy::new(None)),
            media_ticket: None,
            running: AtomicBool::new(true),
            activation: Mutex::new(ActivationState::default()),
            gate: Arc::new(SessionGate::new()),
            cancel: OpenCancel::new(),
            restored: AtomicBool::new(false),
            autostart_lost: AtomicBool::new(false),
            control_epoch: AtomicU64::new(0),
            terminal: AtomicBool::new(false),
            mutation_lock: Mutex::new(()),
            unsettled: AtomicUsize::new(0),
            position: Mutex::new(SenderPosition::unknown(PlayerEventGeneration::from_raw(1))),
            state: Mutex::new(PlayerState::Buffering),
            pipeline: Mutex::new(None),
            #[cfg(test)]
            probe: SessionProbe::default(),
        };
        assert!(inner.restore().is_err());
        assert!(!inner.restored.load(Ordering::SeqCst));
    }

    /// Build a `SessionInner` whose daemon endpoint is unreachable, for the
    /// fail-closed terminal paths.
    fn test_session_inner(state_dir: &Path) -> SessionInner {
        test_session_inner_with_events(state_dir, async_channel::unbounded().0)
    }

    /// As [`test_session_inner`], with an event channel the regression observes.
    fn test_session_inner_with_events(
        state_dir: &Path,
        event_tx: async_channel::Sender<PlayerEvent>,
    ) -> SessionInner {
        SessionInner {
            client: Arc::new(OwnToneClient::new("http://127.0.0.1:1").expect("client")),
            config: owned_config(state_dir),
            generation: PlayerEventGeneration::from_raw(1),
            event_tx,
            recorded: TakeoverRecord {
                enabled_outputs: vec![1],
                selected_output: 2,
            },
            media_proxy: Arc::new(GstreamerMediaProxy::new(None)),
            media_ticket: None,
            running: AtomicBool::new(true),
            activation: Mutex::new(ActivationState::default()),
            gate: Arc::new(SessionGate::new()),
            cancel: OpenCancel::new(),
            restored: AtomicBool::new(false),
            autostart_lost: AtomicBool::new(false),
            control_epoch: AtomicU64::new(0),
            terminal: AtomicBool::new(false),
            mutation_lock: Mutex::new(()),
            unsettled: AtomicUsize::new(0),
            position: Mutex::new(SenderPosition::unknown(PlayerEventGeneration::from_raw(1))),
            state: Mutex::new(PlayerState::Buffering),
            pipeline: Mutex::new(None),
            #[cfg(test)]
            probe: SessionProbe::default(),
        }
    }

    /// T3: acceptance consults the load's cancellation currency inside the
    /// serialized boundary, so a Stop that raced the worker's currentness check
    /// refuses activation instead of transmitting a stale `player/play`.
    #[test]
    fn activation_refuses_once_the_load_is_cancelled() {
        let directory = tempfile::tempdir().expect("tempdir");
        let inner = test_session_inner(directory.path());
        inner.cancel.cancel();
        assert!(!inner.activate_and_play());
        assert!(!inner.activation_live());
    }

    /// T1: a transmitted control that failed leaves the session unsettled, so
    /// restoration must quiesce first; when the daemon cannot be quiesced the
    /// restoration fails closed instead of releasing ownership.
    #[test]
    fn restore_fails_closed_while_a_mutation_is_outstanding() {
        let directory = tempfile::tempdir().expect("tempdir");
        let inner = test_session_inner(directory.path());
        inner.mark_unsettled();
        assert!(inner.restore().is_err());
        assert!(!inner.restored.load(Ordering::SeqCst));
        assert!(inner.unsettled_count() > 0);
    }

    /// U1: a failed restoration registers uncertainty and keeps it. A second
    /// restore (the close path re-invokes one after joining the pump) must not
    /// be able to clear the marker and release ownership without a confirmed
    /// quiescence: it stays failed closed with the count raised.
    #[test]
    fn a_failed_restoration_keeps_its_uncertainty_across_a_second_restore() {
        let directory = tempfile::tempdir().expect("tempdir");
        let inner = test_session_inner(directory.path());
        assert!(inner.restore().is_err());
        assert!(inner.unsettled_count() > 0);
        assert!(!inner.restored.load(Ordering::SeqCst));
        assert!(inner.restore().is_err());
        assert!(inner.unsettled_count() > 0);
        assert!(!inner.restored.load(Ordering::SeqCst));
    }

    /// A `SessionInner` pointed at an arbitrary API base, for fixtures that
    /// need a live (or stalling) endpoint rather than the unreachable default.
    fn test_session_inner_at_base(
        api_base: &str,
        state_dir: &Path,
        event_tx: async_channel::Sender<PlayerEvent>,
    ) -> SessionInner {
        SessionInner {
            client: Arc::new(OwnToneClient::new(api_base).expect("client")),
            config: owned_config(state_dir),
            generation: PlayerEventGeneration::from_raw(1),
            event_tx,
            recorded: TakeoverRecord {
                enabled_outputs: vec![1],
                selected_output: 2,
            },
            media_proxy: Arc::new(GstreamerMediaProxy::new(None)),
            media_ticket: None,
            running: AtomicBool::new(true),
            activation: Mutex::new(ActivationState::default()),
            gate: Arc::new(SessionGate::new()),
            cancel: OpenCancel::new(),
            restored: AtomicBool::new(false),
            autostart_lost: AtomicBool::new(false),
            control_epoch: AtomicU64::new(0),
            terminal: AtomicBool::new(false),
            mutation_lock: Mutex::new(()),
            unsettled: AtomicUsize::new(0),
            position: Mutex::new(SenderPosition::unknown(PlayerEventGeneration::from_raw(1))),
            state: Mutex::new(PlayerState::Buffering),
            pipeline: Mutex::new(None),
            #[cfg(test)]
            probe: SessionProbe::default(),
        }
    }

    /// V1: a Stop must return promptly even while a `player/play` RPC is
    /// genuinely stalled at the server. This is a production-path barrier: the
    /// stalling loopback endpoint signals the test when the play request has
    /// reached it, so the Stop is measured against a real in-flight effect
    /// (not a cancelled-before-transmit shortcut).
    #[test]
    fn a_stop_returns_promptly_while_the_own_tone_play_is_stalled() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalling endpoint");
        let api_base = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("addr").port()
        );
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(stream) = incoming else { continue };
                let Ok(reader_stream) = stream.try_clone() else {
                    continue;
                };
                let mut reader = std::io::BufReader::new(reader_stream);
                let mut request_line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut request_line);
                let _ = accepted_tx.send(request_line.trim().to_string());
                std::thread::spawn(move || {
                    // Hold the connection without responding: the play RPC
                    // blocks until its own client timeout.
                    let _held = stream;
                    std::thread::sleep(Duration::from_secs(30));
                });
            }
        });

        let directory = tempfile::tempdir().expect("tempdir");
        let inner = Arc::new(test_session_inner_at_base(
            &api_base,
            directory.path(),
            async_channel::unbounded().0,
        ));

        let play_inner = Arc::clone(&inner);
        let play = std::thread::spawn(move || play_inner.activate_and_play());

        // Barrier: the actual `player/play` request has reached the stalling
        // server — the Stop is measured against a genuine in-flight effect,
        // not a cancelled-before-transmit shortcut.
        let request = accepted_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the play request must reach the server");
        assert!(
            request.contains("/api/player/play"),
            "the observed request must be the play RPC: {request:?}"
        );

        let stopped = Instant::now();
        inner.gate.stop();
        assert!(
            stopped.elapsed() < Duration::from_millis(500),
            "Stop blocked behind a stalled player/play RPC: {:?}",
            stopped.elapsed()
        );

        // The transmitted play is suppressed once the request times out; the
        // session's own teardown settles the receiver state.
        assert!(
            !play.join().expect("play thread"),
            "a Stop that wins during the play must suppress the accepted start"
        );
        assert!(!inner.activation_live());
        // The stalled play transmitted before the Stop and never settled: it
        // stays recorded as outstanding, so the session's own teardown must
        // quiesce before it can release ownership.
        assert!(
            inner.unsettled_count() > 0,
            "a transmitted-but-unsettled play must remain outstanding for teardown"
        );
    }

    /// U3: a Stop that wins the shared boundary refuses the start effect before
    /// it is transmitted — the session publishes at most a truthful `Stopped`
    /// (never `Playing`) and no error event, because no play RPC ever ran.
    #[test]
    fn a_stop_before_start_refuses_the_effect() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = test_session_inner_with_events(directory.path(), tx);
        inner.gate.stop();
        assert!(!inner.activate_and_play(), "a stopped load must not start");
        assert!(!inner.activation_live());
        match rx.try_recv() {
            Ok(PlayerEvent::StateChanged {
                state: PlayerState::Stopped,
                ..
            }) => {}
            other => panic!("expected only a Stopped state, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "no further events (no error, no Playing) may follow a refused start"
        );
    }

    /// U1: a control RPC that fails after transmission is counted as
    /// outstanding **before** it is transmitted and retained on failure — a
    /// later restoration must quiesce before it can settle.
    #[test]
    fn a_failed_control_is_retained_as_outstanding() {
        let directory = tempfile::tempdir().expect("tempdir");
        let inner = test_session_inner(directory.path());
        assert!(inner
            .transmit_mutation(|| {
                inner
                    .client
                    .set_volume(50)
                    .map_err(MutationOutcome::into_error)
            })
            .is_err());
        assert!(inner.unsettled_count() > 0);
    }

    /// U3: a failed initial play never publishes `Playing`; it surfaces the
    /// error, keeps the pump inert, and records the transmitted play as
    /// outstanding for settlement.
    #[test]
    fn a_failed_initial_play_publishes_no_playing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = test_session_inner_with_events(directory.path(), tx);
        assert!(
            !inner.activate_and_play(),
            "a failed play is not activation"
        );
        assert!(!inner.activation_live());
        assert!(inner.unsettled_count() > 0);

        let mut saw_error = false;
        let mut saw_playing = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                PlayerEvent::Error { .. } => saw_error = true,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing,
                    ..
                } => saw_playing = true,
                _ => {}
            }
        }
        assert!(saw_error, "the failed play must be surfaced");
        assert!(
            !saw_playing,
            "a failed play must never be reported as Playing"
        );
    }

    // ----- U2: retained-recovery supervisor liveness, retry and fairness -----

    /// The supervisor's regression seam: a job that settles after
    /// `succeed_on` attempts and records that its resources were released.
    struct TestRecoveryJob {
        attempts: AtomicUsize,
        succeed_on: usize,
        released: Arc<AtomicBool>,
        retry_at: Instant,
    }

    impl RecoveryJob for TestRecoveryJob {
        fn retry_at(&self) -> Instant {
            self.retry_at
        }

        fn set_retry_at(&mut self, at: Instant) {
            self.retry_at = at;
        }

        fn attempt(&self) -> bool {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt >= self.succeed_on {
                self.released.store(true, Ordering::SeqCst);
                true
            } else {
                false
            }
        }
    }

    /// Drain events until a `Stopped` arrives, then assert the failed-load
    /// shape: every event for `generation`, exactly one Error, no TrackEnded
    /// and no Playing. Returns the drained events.
    #[cfg(owntone_host)]
    fn assert_failed_load_events(
        rx: &async_channel::Receiver<PlayerEvent>,
        generation: PlayerEventGeneration,
    ) -> Vec<PlayerEvent> {
        let mut events = Vec::new();
        wait_until(|| {
            events.extend(std::iter::from_fn(|| rx.try_recv().ok()));
            events.iter().any(|event| {
                matches!(
                    event,
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
                )
            })
        });
        assert!(
            events.iter().all(|event| event.generation() == generation),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::Error { .. }))
                .count(),
            1,
            "{events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. }
                    | PlayerEvent::StateChanged {
                        state: PlayerState::Playing,
                        ..
                    }
            )),
            "{events:?}"
        );
        events
    }

    /// The daemon's outputs equal a prior snapshot, id and selection alike.
    #[cfg(owntone_host)]
    fn assert_outputs_match(client: &OwnToneClient, baseline: &[OwnToneOutput]) {
        let restored = client.outputs().unwrap();
        assert_eq!(
            restored
                .iter()
                .map(|o| (o.id, o.selected))
                .collect::<Vec<_>>(),
            baseline
                .iter()
                .map(|o| (o.id, o.selected))
                .collect::<Vec<_>>()
        );
    }

    /// A fresh load on the same controller plays, stops and releases cleanly:
    /// the daemon is usable again after whatever the test did to it.
    #[cfg(owntone_host)]
    fn assert_next_load_plays(
        controller: &crate::audio::airplay_output::ControllerHarness,
        daemon: &RecordingOwnedDaemon,
        prepared: crate::audio::gstreamer_media::PreparedGstreamerMedia,
        generation: PlayerEventGeneration,
    ) {
        let next_ticket = prepared.ticket().unwrap();
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        assert_eq!(next_ticket.route_count(), 1);
        controller.stop();
        wait_until(|| next_ticket.route_count() == 0);
        wait_until(|| !daemon.config.takeover_record().exists());
    }

    /// Like [`assert_next_load_plays`], with a pause/resume cycle before the
    /// stop, for fixtures whose earlier phase exercised live controls.
    #[cfg(owntone_host)]
    fn assert_next_load_plays_with_pause(
        controller: &crate::audio::airplay_output::ControllerHarness,
        prepared: crate::audio::gstreamer_media::PreparedGstreamerMedia,
        generation: PlayerEventGeneration,
    ) {
        let next_ticket = prepared.ticket().unwrap();
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        assert_eq!(next_ticket.route_count(), 1);
        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        controller.stop();
        wait_until(|| next_ticket.route_count() == 0);
    }

    /// Bounded wait that names the waiting call site when it expires, so an
    /// intermittent expiry under suite concurrency identifies which predicate
    /// stalled (tr-9utsm) instead of an anonymous timeout.
    #[track_caller]
    fn wait_until(predicate: impl FnMut() -> bool) {
        wait_until_within(Duration::from_secs(10), predicate);
    }

    /// Drain the event channel through the publication of `state`. The
    /// controller's state cache is written before its event is sent
    /// (`publish_state`), so a fixture that waits on the cache and then drains
    /// can leave that very event behind and trip a later "no play/pause"
    /// assertion (seen once in release mode under a full-suite load).
    fn drain_through(rx: &async_channel::Receiver<PlayerEvent>, state: PlayerState) {
        let mut seen = false;
        wait_until(|| {
            while let Ok(event) = rx.try_recv() {
                if matches!(event, PlayerEvent::StateChanged { state: s, .. } if s == state) {
                    seen = true;
                }
            }
            seen
        });
    }

    /// [`wait_until`] with an explicit bound, for phases that legitimately
    /// span the adapter's own bounded quiescence/recovery deadlines.
    #[track_caller]
    fn wait_until_within(limit: Duration, mut predicate: impl FnMut() -> bool) {
        let caller = std::panic::Location::caller();
        let started = Instant::now();
        let deadline = started + limit;
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out after {:?} waiting for the condition at {}:{}",
                started.elapsed(),
                caller.file(),
                caller.line()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Explicit bound for waits whose predicate can only turn true after a
    /// failed mutation's full settle-or-restart unwind. That unwind spans the
    /// adapter's own bounded deadlines — the 2s `API_TIMEOUT` of the failed
    /// request, quiescence (`SIGTERM` grace 5s, endpoint release 5s, restart
    /// 15s — the `QUIESCE_*` deadlines) and restoration inside the 30s
    /// serialized-recovery deadline — so the generic 10s [`wait_until`] bound
    /// is smaller than the phase it waits on (tr-9utsm: its one intermittent
    /// suite-concurrency expiry identified this predicate by deadline-budget
    /// analysis). These waits use this bound instead; the adapter deadlines
    /// themselves are never raised, and every behavioral assertion is kept.
    /// The subsequent lock-free wait resolves in the same unwind tail (route
    /// release and lock drop are adjacent), so it keeps the default bound,
    /// exactly like the retained-recovery fixture's 90s route wait above.
    const SETTLE_UNWIND_BOUND: Duration = Duration::from_secs(90);

    /// V2: after the spawn budget is exhausted the production path keeps an
    /// executable retry owner on its own. Once the spawn facility recovers, the
    /// queued job is serviced and its resources are released — with no manual
    /// retry-helper call and no second job registration.
    #[test]
    fn supervisor_retains_a_retry_owner_until_the_spawn_facility_recovers() {
        let supervisor = Arc::new(RecoverySupervisor::new());
        // Far more failures than any fixed inline budget: enough to prove the
        // production path retries past them rather than only trying a few times.
        supervisor.fail_next_spawns(usize::MAX / 2);
        let released = Arc::new(AtomicBool::new(false));
        let enqueue_supervisor = Arc::clone(&supervisor);
        let enqueue_released = Arc::clone(&released);
        let handle = std::thread::spawn(move || {
            enqueue_supervisor.enqueue(Box::new(TestRecoveryJob {
                attempts: AtomicUsize::new(0),
                succeed_on: 1,
                released: enqueue_released,
                retry_at: Instant::now(),
            }));
        });

        // The owner is retrying spawns and the job stays queued (with its lock
        // held). A failed handoff is never reported as clean: while no live
        // owner exists the job must remain queued and its resources unreleased.
        wait_until(|| supervisor.pending_jobs() >= 1);
        assert!(
            !released.load(Ordering::SeqCst),
            "the job must not be released before a live owner exists"
        );
        assert!(
            supervisor.pending_jobs() >= 1,
            "the job must stay queued (with its lock held) while every spawn fails"
        );
        // Recover the spawn facility and let the same production retry path
        // start a worker.
        std::thread::sleep(Duration::from_millis(50));
        supervisor.fail_next_spawns(0);
        wait_until(|| released.load(Ordering::SeqCst));
        wait_until(|| supervisor.pending_jobs() == 0);
        assert!(
            supervisor.worker_is_live(),
            "the recovered spawn must leave a live owner, not a static queue"
        );
        handle.join().expect("enqueue owner");
    }

    /// U2: two independent retained recoveries are both serviced. A job that is
    /// not yet settleable must not starve the other, and both must eventually
    /// reach release.
    #[test]
    fn supervisor_services_two_independent_jobs_fairly() {
        let supervisor = Arc::new(RecoverySupervisor::new());
        let released_a = Arc::new(AtomicBool::new(false));
        let released_b = Arc::new(AtomicBool::new(false));
        supervisor.enqueue(Box::new(TestRecoveryJob {
            attempts: AtomicUsize::new(0),
            succeed_on: 2,
            released: Arc::clone(&released_a),
            retry_at: Instant::now(),
        }));
        supervisor.enqueue(Box::new(TestRecoveryJob {
            attempts: AtomicUsize::new(0),
            succeed_on: 2,
            released: Arc::clone(&released_b),
            retry_at: Instant::now(),
        }));

        wait_until(|| released_a.load(Ordering::SeqCst) && released_b.load(Ordering::SeqCst));
        wait_until(|| supervisor.pending_jobs() == 0);
    }

    /// W2: the **real** [`RetainedRecovery`] path, with a real advisory lock,
    /// holds that lock while it cannot establish settlement. The supervisor
    /// starts a live owner after an injected spawn failure, the job is
    /// attempted against an unreachable daemon, and it stays queued — the lock
    /// is never released on a failed attempt, so no false clean handoff is
    /// reported.
    #[cfg(unix)]
    #[test]
    fn a_retained_recovery_holds_its_real_advisory_lock_until_settlement() {
        let directory = tempfile::tempdir().expect("tempdir");
        let lock_path = directory.path().join("instance.lock");
        let lock = open_lock(&lock_path).expect("open lock");
        assert!(
            rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_ok(),
            "the retained recovery must hold the instance lock"
        );
        // A competing opener cannot take the same advisory lock while the
        // retained job owns it.
        let competing = open_lock(&lock_path).expect("open competing lock");
        assert!(
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err(),
            "the advisory lock must be held by the retained recovery"
        );

        let config = owned_config(directory.path());
        let client = Arc::new(OwnToneClient::new("http://127.0.0.1:1").expect("client"));
        let job = RetainedRecovery {
            client,
            config,
            recorded: TakeoverRecord {
                enabled_outputs: vec![1],
                selected_output: 0,
            },
            lock,
            route: None,
            retry_at: Instant::now(),
        };

        let supervisor = Arc::new(RecoverySupervisor::new());
        // The first owner spawn fails; the production retry path must still
        // prove a live owner before completing the handoff.
        supervisor.fail_next_spawns(1);
        let enqueue_supervisor = Arc::clone(&supervisor);
        let handle = std::thread::spawn(move || enqueue_supervisor.register(job));
        handle.join().expect("enqueue owner");

        // The job is serviced by a live owner; its attempt against the
        // unreachable daemon fails and it stays queued with the lock held.
        wait_until(|| supervisor.worker_is_live());
        // The job leaves the queue for the length of its attempt, and an
        // attempt against an absent daemon now waits out the restart window
        // before it fails (an exited instance is restarted, not refused), so
        // the requeue is observed on that bound rather than the default one.
        wait_until_within(QUIESCE_RESTART_DEADLINE + Duration::from_secs(10), || {
            supervisor.pending_jobs() >= 1
        });
        assert!(
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err(),
            "a failed settlement attempt must not release the advisory lock"
        );
    }

    // ----- W1/W2: HTTP-faithful terminal-settlement regressions -----

    /// A hermetic fake OwnTone daemon speaking the adapter's HTTP contract.
    ///
    /// It records the request line of every request it serves (so a
    /// regression can assert the *actual* request/effect boundary rather than
    /// a scheduling opportunity) and answers the adapter's endpoints
    /// truthfully: `/api/config`, `/api/outputs`, `/api/player` return JSON and
    /// the mutating `PUT`s return `200`. An optional single path can be
    /// *parked*: the server records it and holds the response until the test
    /// releases it, so a concurrent operation can be deterministically placed
    /// behind an in-flight request (review W1, review W2).
    #[cfg(owntone_host)]
    struct FakeOwnToneServer {
        api_base: String,
        requests: Arc<Mutex<Vec<String>>>,
        park: Arc<ParkGate>,
    }

    #[cfg(owntone_host)]
    struct ParkGate {
        marker: String,
        seen: Mutex<bool>,
        released: Mutex<bool>,
        cv: Condvar,
    }

    #[cfg(owntone_host)]
    impl ParkGate {
        fn new(marker: &str) -> Arc<Self> {
            Arc::new(Self {
                marker: marker.to_string(),
                seen: Mutex::new(false),
                released: Mutex::new(false),
                cv: Condvar::new(),
            })
        }

        /// Block until the parked request has reached the server.
        fn wait_seen(&self) {
            let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            while !*seen {
                seen = self.cv.wait(seen).unwrap_or_else(|p| p.into_inner());
            }
        }

        /// Let the parked request complete.
        fn release(&self) {
            *self.released.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.cv.notify_all();
        }

        /// Called on the server's request thread: mark the request observed and
        /// hold it until released.
        fn hold(&self) {
            *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.cv.notify_all();
            let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
            while !*released {
                released = self.cv.wait(released).unwrap_or_else(|p| p.into_inner());
            }
        }
    }

    #[cfg(owntone_host)]
    impl FakeOwnToneServer {
        /// A server that answers every request immediately and reports a
        /// finished item (`stop`) for `/api/player`.
        fn start() -> Self {
            Self::with_park_and_state("/__never_park__", "stop")
        }

        /// A server whose `/api/player` never reports completion, so
        /// [`natural_completion`] runs to its drain deadline.
        fn start_playing_forever() -> Self {
            Self::with_park_and_state("/__never_park__", "play")
        }

        /// A server that parks the restoring `player/stop` request until
        /// [`ParkGate::release`], so a concurrent operation can be placed
        /// deterministically behind an in-flight restoration.
        fn start_parking_stop() -> Self {
            Self::with_park_and_state("/api/player/stop", "stop")
        }

        /// A server that parks the `player/play` request until
        /// [`ParkGate::release`], so a terminal restoration can be placed
        /// deterministically behind an in-flight accepted start (review X1).
        fn start_parking_play() -> Self {
            Self::with_park_and_state("/api/player/play", "stop")
        }

        fn with_park_and_state(marker: &str, player_state: &str) -> Self {
            use std::net::TcpListener;
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
            let port = listener.local_addr().expect("addr").port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let park = ParkGate::new(marker);
            let requests_worker = Arc::clone(&requests);
            let park_worker = Arc::clone(&park);
            let state = Arc::new(player_state.to_string());
            let state_worker = Arc::clone(&state);
            std::thread::spawn(move || {
                for incoming in listener.incoming() {
                    let Ok(stream) = incoming else { continue };
                    let requests = Arc::clone(&requests_worker);
                    let park = Arc::clone(&park_worker);
                    let state = Arc::clone(&state_worker);
                    std::thread::spawn(move || {
                        serve_fake_own_tone(stream, &requests, &park, &state);
                    });
                }
            });
            Self {
                api_base: format!("http://127.0.0.1:{port}"),
                requests,
                park,
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }

        fn requests_matching(&self, needle: &str) -> Vec<String> {
            self.requests()
                .into_iter()
                .filter(|request| request.contains(needle))
                .collect()
        }
    }

    /// Serve one request on its own thread: record the request line, park the
    /// designated path, then answer.
    #[cfg(owntone_host)]
    fn serve_fake_own_tone(
        stream: std::net::TcpStream,
        requests: &Mutex<Vec<String>>,
        park: &ParkGate,
        player_state: &str,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        let Ok(reader_stream) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reader_stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let trimmed = request_line.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                break;
            }
            if let Some(value) = header
                .to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .map(str::to_string)
            {
                content_length = value.parse().unwrap_or(0);
            }
        }
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
        }
        requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(trimmed.clone());
        let path = trimmed
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string();
        if path == park.marker {
            park.hold();
        }
        let body = fake_own_tone_body(&path, player_state);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let mut stream = stream;
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    /// The JSON the adapter expects from each read endpoint.
    #[cfg(owntone_host)]
    fn fake_own_tone_body(path: &str, player_state: &str) -> String {
        match path {
            "/api/config" => r#"{"version":"29.3"}"#.to_string(),
            "/api/outputs" => r#"{"outputs":[]}"#.to_string(),
            "/api/player" => format!(r#"{{"state":"{player_state}"}}"#),
            _ => "{}".to_string(),
        }
    }

    /// W1: a control that is queued behind the terminal restoration can never
    /// transmit. The restoration owns the settlement boundary and has set the
    /// terminal flag (and sent its first restoring RPC) before the control is
    /// even scheduled; when the control finally acquires the boundary it is
    /// refused, so no late request reaches the daemon and no post-restoration
    /// effect is left outstanding.
    #[cfg(owntone_host)]
    #[test]
    fn a_control_parked_behind_terminal_restoration_transmits_nothing() {
        let server = FakeOwnToneServer::start_parking_stop();
        let directory = tempfile::tempdir().expect("tempdir");
        let inner = Arc::new(test_session_inner_at_base(
            &server.api_base,
            directory.path(),
            async_channel::unbounded().0,
        ));

        let restore_inner = Arc::clone(&inner);
        let restore = std::thread::spawn(move || restore_inner.restore());

        // Barrier: the restoration has acquired the settlement boundary and is
        // genuinely in flight at the daemon.
        server.park.wait_seen();

        // A control queued behind the terminal restoration. It can only acquire
        // the boundary after the terminal transition, so `transmit_mutation`
        // must refuse it.
        let control_inner = Arc::clone(&inner);
        let control = std::thread::spawn(move || {
            control_inner.transmit_mutation(|| {
                control_inner
                    .client
                    .set_volume(50)
                    .map_err(MutationOutcome::into_error)
            })
        });
        std::thread::sleep(Duration::from_millis(100));
        server.park.release();

        assert!(
            restore.join().expect("restore thread").is_ok(),
            "the terminal restoration must succeed"
        );
        assert!(
            control.join().expect("control thread").is_err(),
            "a control parked behind the terminal transition must be refused"
        );
        assert!(inner.terminal.load(Ordering::SeqCst));
        assert!(inner.restored.load(Ordering::SeqCst));
        assert!(
            server.requests_matching("/api/player/volume").is_empty(),
            "no late control request may reach the restored daemon: {:?}",
            server.requests()
        );
    }

    /// W1 (error path): once the decode-error restoration is terminal, a
    /// `resume` the worker may still service transmits no `player/play` and
    /// reports no `Playing` — the restored output selection is never re-driven.
    #[cfg(owntone_host)]
    #[test]
    fn a_terminal_restoration_refuses_a_late_resume_and_sends_no_play() {
        let server = FakeOwnToneServer::start();
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = test_session_inner_at_base(&server.api_base, directory.path(), tx);

        // The decode-error terminal sequence: the pump has already restored.
        assert!(inner.restore().is_ok());
        assert!(inner.terminal.load(Ordering::SeqCst));

        assert!(
            !inner.activate_and_play(),
            "a resume after terminal restoration must not activate"
        );
        assert!(!inner.activation_live());
        assert!(
            server.requests_matching("/api/player/play").is_empty(),
            "no late player/play may reach the restored daemon: {:?}",
            server.requests()
        );

        let mut saw_playing = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing,
                    ..
                }
            ) {
                saw_playing = true;
            }
        }
        assert!(!saw_playing, "a late resume must never publish Playing");
    }

    /// W1: the close path's second restoration cannot short-circuit over an
    /// outstanding mutation. After a successful terminal restoration, a
    /// mutation whose outcome never settled must still force a quiescence
    /// before ownership may be released; when that cannot be established the
    /// restore fails closed and the count is retained.
    #[cfg(owntone_host)]
    #[test]
    fn a_terminal_restore_does_not_release_over_an_outstanding_mutation() {
        let server = FakeOwnToneServer::start();
        let directory = tempfile::tempdir().expect("tempdir");
        let inner = test_session_inner_at_base(
            &server.api_base,
            directory.path(),
            async_channel::unbounded().0,
        );
        assert!(inner.restore().is_ok());
        assert!(inner.restored.load(Ordering::SeqCst));

        // The outstanding mutation the terminal transition could not observe.
        inner.mark_unsettled();
        assert!(
            inner.restore().is_err(),
            "a second restore must not treat `restored` as proof that nothing is outstanding"
        );
        assert!(
            inner.unsettled_count() > 0,
            "the outstanding mutation must be retained until a confirmed quiescence"
        );
    }

    // ----- X1: control/start publication is serialized with the terminal transition -----

    /// X1: the start-state publication (the worker's `confirm_started`, run
    /// through [`SessionInner::publish_under_boundary`]) is taken under the
    /// settlement boundary. Before the terminal transition it is live and
    /// publishes `Playing`; after the transition it publishes nothing, because
    /// no `Playing` may follow the terminal state. A bare `terminal` check
    /// outside the boundary would leave a fresh check-to-effect race between
    /// the check and the publication. (The pump's own republication of the
    /// start is gone — refinery R13 — so this is the only start publication
    /// after activation.)
    #[test]
    fn a_start_publication_after_the_terminal_transition_publishes_nothing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = test_session_inner_with_events(directory.path(), tx);
        let mut publish = |state: PlayerState| inner.publish_state(state);

        assert!(inner.publish_under_boundary(&mut publish));
        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::StateChanged {
                state: PlayerState::Playing,
                ..
            })
        ));

        // The terminal transition latches even though the unreachable daemon
        // makes restoration fail.
        assert!(inner.restore().is_err());
        assert!(inner.terminal.load(Ordering::SeqCst));

        assert!(!inner.publish_under_boundary(&mut publish));
        assert!(
            rx.try_recv().is_err(),
            "no start state may follow the terminal transition"
        );
    }

    /// X1: a `pause` the control loop may still service after the terminal
    /// transition transmits nothing and publishes no `Paused`; the terminal
    /// state is the final word.
    #[test]
    fn a_pause_after_the_terminal_transition_publishes_no_paused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(test_session_inner_with_events(directory.path(), tx));
        assert!(inner.restore().is_err());
        assert!(inner.terminal.load(Ordering::SeqCst));

        let mut session = OwnToneSession {
            inner,
            pump: None,
            lock: None,
        };
        session.pause();
        assert!(
            rx.try_recv().is_err(),
            "a terminal session must not publish Paused"
        );
        assert_ne!(
            *session
                .inner
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
            PlayerState::Paused
        );
    }

    /// X1: an accepted start and a concurrent terminal restoration cannot be
    /// reordered. The play RPC is parked at the daemon while the control holds
    /// the settlement boundary; the real [`natural_completion`] terminal path
    /// runs concurrently and must acquire the boundary only after the control
    /// publishes `Playing`. The final cached state is terminal and no
    /// `Playing`/`Paused` may follow the single `TrackEnded`.
    #[cfg(owntone_host)]
    #[test]
    fn a_start_publication_cannot_follow_a_concurrent_terminal_restoration() {
        gst::init().expect("GStreamer init");
        let server = FakeOwnToneServer::start_parking_play();
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(test_session_inner_at_base(
            &server.api_base,
            directory.path(),
            tx,
        ));

        let play_inner = Arc::clone(&inner);
        let play = std::thread::spawn(move || play_inner.activate_and_play());
        // Barrier: the accepted start genuinely reached the daemon and holds the
        // settlement boundary while parked.
        server.park.wait_seen();

        let completion_inner = Arc::clone(&inner);
        let pipeline = gst::Pipeline::new();
        let completion =
            std::thread::spawn(move || natural_completion(&completion_inner, &pipeline));
        // Let the terminal path block on the settlement boundary the parked
        // start holds.
        std::thread::sleep(Duration::from_millis(100));
        server.park.release();

        assert!(
            play.join().expect("play thread"),
            "the accepted start must report a live play"
        );
        completion.join().expect("completion thread");

        assert_eq!(
            *inner.state.lock().unwrap_or_else(|p| p.into_inner()),
            PlayerState::Stopped,
            "the terminal transition must be the final cached state"
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let terminal_at = events
            .iter()
            .position(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
            .unwrap_or_else(|| panic!("one TrackEnded must be published: {events:?}"));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
                .count(),
            1,
            "exactly one TrackEnded must be published: {events:?}"
        );
        assert!(
            !events.iter().skip(terminal_at).any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing | PlayerState::Paused,
                    ..
                }
            )),
            "no Playing/Paused may follow the terminal TrackEnded: {events:?}"
        );
    }

    /// Y1: the worker's coarse start publication is run *through* the session
    /// (`confirm_started`), under the session's terminal-ordering boundary. A
    /// live session runs the caller's publication; a session that has already
    /// gone terminal runs nothing, so no `Playing` may follow the terminal
    /// state. The previous enum-returning contract let the caller publish
    /// *after* the boundary was released — the gap this exercises.
    #[test]
    fn the_worker_start_publication_respects_the_terminal_transition() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(test_session_inner_with_events(directory.path(), tx.clone()));
        let session = OwnToneSession {
            inner: Arc::clone(&inner),
            pump: None,
            lock: None,
        };

        // Live session: the caller's publication runs and is observed.
        let mut live = None;
        assert!(session.confirm_started(&mut |state| {
            live = Some(state);
            let _ = tx.try_send(PlayerEvent::state(inner.generation, state));
        }));
        assert_eq!(live, Some(PlayerState::Playing));
        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::StateChanged {
                state: PlayerState::Playing,
                ..
            })
        ));

        // Terminal session: the caller's publication must not run at all.
        assert!(inner.restore().is_err());
        let mut terminal = None;
        assert!(!session.confirm_started(&mut |state| terminal = Some(state)));
        assert_eq!(terminal, None);
        assert!(rx.try_recv().is_err());
    }

    /// A test sender that hands the real [`run_session_worker`] a single
    /// prepared session, so the worker's own cache/event publication can be
    /// interposed while it drives a real `OwnToneSession` boundary (review Z2).
    struct FixedSessionSender {
        session: Mutex<Option<Box<dyn SenderSession>>>,
    }

    impl AirplaySender for FixedSessionSender {
        fn name(&self) -> &'static str {
            "test-fixed-session"
        }

        fn probe(&self) -> Result<(), SenderError> {
            Ok(())
        }

        fn open_session(&self, _ctx: &SenderOpenContext) -> OpenOutcome {
            let session = self
                .session
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            match session {
                Some(session) => OpenOutcome::Opened(session),
                None => OpenOutcome::Failed(unavailable("no_prepared_test_session")),
            }
        }
    }

    /// Y1/Z2: the worker's start publication is **atomic** with the terminal
    /// transition, driven through the **real** `run_session_worker` and a real
    /// `OwnToneSession`. The worker's own cache/event publication is parked
    /// while it holds the settlement boundary, the terminal contender's arrival
    /// at that boundary is observed explicitly (no sleep as arrival evidence),
    /// and only then is the publication released. The terminal path acquires the
    /// boundary after the publication drains, so `Playing` precedes the single
    /// `TrackEnded` and the terminal state is final. The worker's own cache is
    /// asserted terminal after settlement — the state the UI reads.
    #[cfg(owntone_host)]
    #[test]
    fn the_worker_start_publication_is_atomic_with_the_terminal_transition() {
        use crate::architecture::media::ResolvedHttpRequest;
        use crate::audio::airplay_output::{spawn_test_session_worker, TestSessionWorker};
        use crate::audio::airplay_sender::SenderTarget;
        use std::sync::atomic::{AtomicU64, AtomicU8};

        gst::init().expect("GStreamer init");
        let server = FakeOwnToneServer::start();
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("url"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");

        let generation = PlayerEventGeneration::from_raw(41);
        let inner = Arc::new({
            let mut inner =
                test_session_inner_at_base(&server.api_base, directory.path(), tx.clone());
            inner.generation = generation;
            inner.media_proxy = Arc::clone(&proxy);
            inner.media_ticket = prepared.ticket();
            inner
        });

        // Park the worker's own publication inside the boundary, and observe the
        // terminal contender's arrival at it explicitly.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        inner.arm_publication_probe(entered_tx, release_rx);
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel::<()>();
        inner.arm_restore_attempt_probe(attempt_tx);

        let cancel = OpenCancel::new();
        let registration = proxy.register_in_flight_cancel(41, prepared.generation(), &cancel);
        let ctx = SenderOpenContext {
            target: SenderTarget::new("Test", "127.0.0.1", 7000, None),
            prepared_uri: prepared.uri().to_string(),
            event_tx: tx,
            generation,
            media_proxy: Arc::clone(&proxy),
            media_ticket: prepared.ticket(),
            volume: 1.0,
            cancel: cancel.clone(),
            session_gate: Arc::new(SessionGate::new()),
            open_id: 41,
        };
        let sender = Arc::new(FixedSessionSender {
            session: Mutex::new(Some(Box::new(OwnToneSession {
                inner: Arc::clone(&inner),
                pump: None,
                lock: None,
            }))),
        });

        let state_cache = Arc::new(AtomicU8::new(PlayerState::Buffering as u8));
        let position_cache = Arc::new(Mutex::new(SenderPosition::unknown(generation)));
        let event_generation = Arc::new(AtomicU64::new(generation.as_raw()));
        let mut worker: TestSessionWorker = spawn_test_session_worker(
            sender,
            ctx,
            registration,
            Arc::clone(&state_cache),
            Arc::clone(&position_cache),
            event_generation,
        );

        // The worker has entered its own publication and is parked inside the
        // boundary *before* its cache/event effects run.
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the worker publication must enter the boundary");
        assert_eq!(
            state_cache.load(Ordering::SeqCst),
            PlayerState::Buffering as u8,
            "the publication must be parked before it writes the worker cache"
        );

        // The real terminal path runs concurrently. Its arrival at the
        // settlement boundary is observed explicitly, so the assertion below is
        // not vacuous (no sleep-as-arrival-evidence).
        let completion_inner = Arc::clone(&inner);
        let completion = std::thread::spawn(move || {
            natural_completion(&completion_inner, &gst::Pipeline::new());
        });
        attempt_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the terminal contender must reach the settlement boundary");
        assert!(
            !inner.terminal.load(Ordering::SeqCst),
            "the terminal transition must not latch while the worker publication holds the boundary"
        );
        release_tx.send(()).expect("release the worker publication");

        completion.join().expect("completion thread");

        // The worker's own loop observes the session and refreshes its cache
        // after the terminal transition. Synchronize on that real observation
        // (signalled from the live session's `observe`), then assert the cache
        // **while the worker is still running** — before any Stop/teardown can
        // write the same `Stopped` value and mask a missing refresh. If the
        // loop's cache write were removed, this loop times out instead of
        // passing on the teardown store.
        let (observed_tx, observed_rx) = std::sync::mpsc::channel::<()>();
        inner.arm_observe_probe(observed_tx);
        let observe_deadline = Instant::now() + Duration::from_secs(5);
        while worker.cached_state() != PlayerState::Stopped {
            let remaining = observe_deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the worker's own cache never observed the terminal state"
            );
            observed_rx
                .recv_timeout(remaining)
                .expect("the worker must keep observing the live session");
        }
        assert_eq!(
            worker.cached_state(),
            PlayerState::Stopped,
            "the worker's own cache must be terminal while the worker is still live"
        );

        // Only now tear the worker down; its unconditional `Stopped` store can
        // no longer be mistaken for the observation above.
        worker.stop_and_join();

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let playing_at = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    PlayerEvent::StateChanged {
                        state: PlayerState::Playing,
                        ..
                    }
                )
            })
            .unwrap_or_else(|| panic!("the worker publication must be observed: {events:?}"));
        let terminal_at = events
            .iter()
            .position(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
            .unwrap_or_else(|| panic!("one TrackEnded must be published: {events:?}"));
        assert!(
            playing_at < terminal_at,
            "Playing must precede the terminal event: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
                .count(),
            1,
            "exactly one TrackEnded must be published: {events:?}"
        );
        assert!(
            !events.iter().skip(terminal_at).any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing | PlayerState::Paused,
                    ..
                }
            )),
            "no Playing/Paused may follow the terminal TrackEnded: {events:?}"
        );
    }

    // ----- X2: natural-completion terminal-path regressions -----

    /// W2: a finite item whose daemon reports `stop` publishes exactly one
    /// generation-scoped `TrackEnded` **after** `Stopped`, restores the daemon,
    /// and leaves the cached state terminal. A duplicate completion advances the
    /// queue twice; a missing one never advances it.
    #[cfg(owntone_host)]
    #[test]
    fn natural_completion_publishes_exactly_one_track_ended() {
        gst::init().expect("GStreamer init");
        let server = FakeOwnToneServer::start();
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = test_session_inner_at_base(&server.api_base, directory.path(), tx);
        let pipeline = gst::Pipeline::new();

        natural_completion(&inner, &pipeline);

        assert!(
            inner.restored.load(Ordering::SeqCst),
            "a completed item restores the daemon"
        );
        assert_eq!(
            *inner.state.lock().unwrap_or_else(|p| p.into_inner()),
            PlayerState::Stopped
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
                .count(),
            1,
            "exactly one TrackEnded: {events:?}"
        );
        let stopped_at = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
                )
            })
            .unwrap_or_else(|| panic!("Stopped must be published: {events:?}"));
        let ended_at = events
            .iter()
            .position(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
            .expect("TrackEnded");
        assert!(
            stopped_at < ended_at,
            "the terminal Stopped must precede TrackEnded: {events:?}"
        );
    }

    /// AE1: even a confirmed EOS must not publish if Stop wins while the
    /// restoring RPC is in flight, after the last drain cancellation check.
    #[cfg(owntone_host)]
    #[test]
    fn natural_completion_stop_during_restore_suppresses_publication() {
        gst::init().unwrap();
        let server = FakeOwnToneServer::start_parking_stop();
        let directory = tempfile::tempdir().unwrap();
        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(test_session_inner_at_base(
            &server.api_base,
            directory.path(),
            tx,
        ));
        let completing = Arc::clone(&inner);
        let handle =
            std::thread::spawn(move || natural_completion(&completing, &gst::Pipeline::new()));
        server.park.wait_seen();
        let started = Instant::now();
        inner.gate.stop();
        inner.cancel.cancel();
        assert!(started.elapsed() < Duration::from_millis(100));
        server.park.release();
        handle.join().unwrap();
        assert!(inner.restored.load(Ordering::SeqCst));
        assert!(
            rx.try_recv().is_err(),
            "Stop must suppress all late terminal publications"
        );
    }

    /// W2: a transport loss while waiting for completion is a terminal failure
    /// that restores and publishes no `TrackEnded` — completion is never
    /// inferred from a failed observation.
    #[test]
    fn natural_completion_failure_is_terminal_and_never_track_ended() {
        gst::init().expect("GStreamer init");
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = test_session_inner_at_base("http://127.0.0.1:1", directory.path(), tx);
        let pipeline = gst::Pipeline::new();

        natural_completion(&inner, &pipeline);

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PlayerEvent::Error { .. })),
            "a failed completion must surface an error: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })),
            "transport loss is terminal failure, never completion: {events:?}"
        );
        assert_eq!(
            *inner.state.lock().unwrap_or_else(|p| p.into_inner()),
            PlayerState::Stopped
        );
    }

    /// W2: a daemon that never reports completion is a bounded drain-deadline
    /// miss — a terminal failure, never a false `TrackEnded`.
    #[cfg(owntone_host)]
    #[test]
    fn natural_completion_deadline_miss_is_terminal_and_never_track_ended() {
        gst::init().expect("GStreamer init");
        let server = FakeOwnToneServer::start_playing_forever();
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = test_session_inner_at_base(&server.api_base, directory.path(), tx);
        let pipeline = gst::Pipeline::new();

        let started = Instant::now();
        natural_completion(&inner, &pipeline);
        assert!(
            started.elapsed() >= DRAIN_DEADLINE,
            "the drain must be bounded by the deadline, waited {:?}",
            started.elapsed()
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayerEvent::Error { message, .. } if message.contains("timed out")
            )),
            "a deadline miss must surface as a timeout error: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })),
            "a deadline miss is not a completion: {events:?}"
        );
    }

    #[cfg(owntone_host)]
    #[test]
    fn cancelled_pump_stops_an_already_activated_pipeline() {
        gst::init().expect("GStreamer init");
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(test_session_inner_with_events(directory.path(), tx));
        let pipeline = gst::parse::launch("audiotestsrc is-live=true ! fakesink")
            .expect("build pipeline")
            .downcast::<gst::Pipeline>()
            .expect("pipeline");
        pipeline.set_state(gst::State::Playing).expect("start");
        assert_eq!(
            pipeline.state(gst::ClockTime::from_seconds(5)).1,
            gst::State::Playing
        );
        // Activation finished, but cancellation arrives before the observer
        // consumes that acceptance. The observer must shut down the decoder
        // itself before returning its descriptor, without waiting for close.
        inner.activation.lock().unwrap().accepted = true;
        inner.cancel.cancel();
        let descriptor = std::fs::File::open("/dev/null").expect("descriptor");
        run_pump(inner, pipeline.clone(), descriptor.into());
        assert_eq!(pipeline.current_state(), gst::State::Null);
        assert!(
            rx.try_recv().is_err(),
            "cancelled observation published an event"
        );
    }

    #[cfg(owntone_host)]
    fn exercise_oversized_fifo_write(cancel_write: bool) {
        use std::io::Read;

        gst::init().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audio.pcm");
        ensure_pipe(&path).unwrap();
        let reader = rustix::fs::open(
            &path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let measuring_writer = rustix::fs::open(
            &path,
            OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let chunk = [0_u8; 4096];
        let mut capacity = 0;
        loop {
            match rustix::io::write(&measuring_writer, &chunk) {
                Ok(n) => capacity += n,
                Err(rustix::io::Errno::AGAIN) => break,
                other => panic!("measure FIFO capacity: {other:?}"),
            }
        }
        assert!(capacity > 0);
        let mut seed = vec![0; capacity];
        let mut reader = std::fs::File::from(reader);
        reader.read_exact(&mut seed).unwrap();
        drop(measuring_writer);
        // A single buffer exceeds the *measured* empty pipe capacity. Waiting
        // for a full pipe proves the write has begun, before cancelling or
        // allowing consumption. The reader stays open throughout teardown.
        let pcm: Vec<u8> = (0..capacity * 4).map(|i| (i % 251) as u8).collect();
        let media = directory.path().join("pcm.raw");
        std::fs::write(&media, &pcm).unwrap();
        let writer = open_pipe_write(
            &path,
            verify_pipe_identity(&path).unwrap(),
            Instant::now() + OPEN_DEADLINE,
            &OpenCancel::new(),
        )
        .unwrap_or_else(|_| panic!("open writer"));
        let pipeline = gst::parse::launch(&format!(
            "filesrc location=\"{}\" blocksize={} ! fdsink fd={} sync=false",
            media.display(),
            pcm.len(),
            writer.as_raw_fd(),
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rustix::io::ioctl_fionread(&reader).unwrap() < capacity as u64 {
            assert!(
                Instant::now() < deadline,
                "oversized write never filled FIFO"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let bus = pipeline.bus().unwrap();
        assert!(
            bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(100),
                &[gst::MessageType::Error, gst::MessageType::Eos]
            )
            .is_none(),
            "backpressure must neither fail nor prematurely complete the buffer"
        );
        if !cancel_write {
            let mut received = vec![0; pcm.len()];
            let mut offset = 0;
            while offset < received.len() {
                match reader.read(&mut received[offset..]) {
                    Ok(n) => offset += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    other => panic!("read PCM: {other:?}"),
                }
                assert!(Instant::now() < deadline, "partial write did not resume");
            }
            assert_eq!(
                received, pcm,
                "partial writes must preserve every byte in order"
            );
            let terminal = bus
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(2),
                    &[gst::MessageType::Error, gst::MessageType::Eos],
                )
                .expect("EOS");
            assert_eq!(terminal.type_(), gst::MessageType::Eos);
        }
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Keep descriptor custody until the streaming task has joined.
            let stopped = pipeline.set_state(gst::State::Null);
            stopped_tx.send((stopped, writer)).ok();
        });
        let (stopped, writer) = stopped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("pipeline Null hung in an oversized FIFO write");
        stopped.unwrap();
        assert!(rustix::fs::fcntl_getfl(&writer)
            .unwrap()
            .contains(OFlags::NONBLOCK));
        if cancel_write {
            assert_eq!(
                rustix::io::ioctl_fionread(&reader).unwrap(),
                capacity as u64
            );
        }
    }

    #[cfg(owntone_host)]
    #[test]
    fn oversized_fifo_write_is_interruptible_after_partial_progress() {
        exercise_oversized_fifo_write(true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn oversized_fifo_write_resumes_without_losing_pcm() {
        exercise_oversized_fifo_write(false);
    }

    /// X2/W2: a **failed start** leaves the production pump inert. The play RPC
    /// is refused, the pump's activation gate stays closed, and the real decode
    /// pipeline is never started: the FIFO reader observes EOF with zero bytes
    /// and no `Playing`/`TrackEnded` is ever published.
    #[cfg(owntone_host)]
    #[test]
    fn a_failed_start_leaves_the_pump_inert_and_writes_no_pcm() {
        use std::io::Read;

        gst::init().expect("GStreamer init");
        let directory = tempfile::tempdir().expect("tempdir");
        let pipe_path = directory.path().join("airplay.pcm");
        ensure_pipe(&pipe_path).expect("create fifo");

        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(test_session_inner_with_events(directory.path(), tx));
        assert!(
            !inner.activate_and_play(),
            "an unreachable daemon must refuse the start"
        );
        assert!(!inner.activation_live());

        let reader_path = pipe_path.clone();
        let reader = std::thread::spawn(move || {
            let mut fifo = std::fs::OpenOptions::new()
                .read(true)
                .open(&reader_path)
                .expect("open fifo reader");
            let mut total = 0usize;
            let mut buffer = [0u8; 4096];
            loop {
                match fifo.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => total += read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            total
        });

        let write_fd = open_pipe_write(
            &pipe_path,
            verify_pipe_identity(&pipe_path).unwrap(),
            Instant::now() + Duration::from_secs(5),
            &inner.cancel,
        )
        .unwrap_or_else(|_| panic!("open fifo write end"));
        let pipeline = gst::parse::launch(&format!(
            "audiotestsrc num-buffers=8 ! audioconvert ! audio/x-raw,format=S16LE,rate=44100,channels=2 ! fdsink fd={}",
            write_fd.as_raw_fd(),
        ))
        .expect("build pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline");

        run_pump(Arc::clone(&inner), pipeline, write_fd);

        let written = reader.join().expect("reader");
        assert_eq!(
            written, 0,
            "a refused start must write no PCM into the FIFO"
        );
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing,
                    ..
                }
            )),
            "a refused start must not publish Playing: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })),
            "an inert pump must not complete: {events:?}"
        );
    }

    /// A daemon that reports `play` until the FIFO reader observes EOF, then
    /// `stop` — the production completion contract.
    #[cfg(owntone_host)]
    fn serve_drain_aware_daemon(
        listener: std::net::TcpListener,
        server_drained: Arc<AtomicBool>,
        server_received: Arc<AtomicBool>,
    ) {
        use std::io::Read;
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else { continue };
            let drained = Arc::clone(&server_drained);
            let received = Arc::clone(&server_received);
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader, Write};
                let Ok(reader_stream) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(reader_stream);
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                let mut content_length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = header
                        .to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                    {
                        content_length = value.parse().unwrap_or(0);
                    }
                }
                if content_length > 0 {
                    let mut body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body);
                }
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let body = match path.as_str() {
                    "/api/config" => r#"{"version":"29.3"}"#.to_string(),
                    "/api/outputs" => r#"{"outputs":[]}"#.to_string(),
                    "/api/player" => {
                        let state =
                            if drained.load(Ordering::SeqCst) || !received.load(Ordering::SeqCst) {
                                "stop"
                            } else {
                                "play"
                            };
                        format!(r#"{{"state":"{state}"}}"#)
                    }
                    _ => "{}".to_string(),
                };
                let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                let mut stream = stream;
                let _ = stream.write_all(response.as_bytes());
            });
        }
    }

    /// Read the FIFO until the writer's EOF and only then flip the daemon's
    /// completion state — a real drain barrier, not a sleep. Returns the
    /// bytes read.
    #[cfg(owntone_host)]
    fn drain_fifo_until_eof(
        reader_path: PathBuf,
        received: Arc<AtomicBool>,
        reader_drained: Arc<AtomicBool>,
    ) -> usize {
        use std::io::Read;

        let mut fifo = std::fs::OpenOptions::new()
            .read(true)
            .open(&reader_path)
            .expect("open fifo reader");
        let mut total = 0usize;
        let mut buffer = [0u8; 4096];
        loop {
            match fifo.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    total += read;
                    received.store(true, Ordering::SeqCst);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        reader_drained.store(true, Ordering::SeqCst);
        total
    }

    /// X2/W2: the **production** [`run_pump`] path drives a real decode
    /// pipeline into a real FIFO; the daemon observes the writer's EOF and
    /// reports completion, and exactly one `TrackEnded` is published after
    /// `Stopped`, with the daemon restored and the cached state terminal. The
    /// earlier fixtures called [`natural_completion`] directly with a pipeline
    /// already gone; this drives the real pump, the real pipe write end and the
    /// daemon drain.
    #[cfg(owntone_host)]
    #[test]
    fn the_pump_publishes_completion_after_a_real_fifo_drain() {
        use std::net::TcpListener;

        gst::init().expect("GStreamer init");
        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path();
        let pipe_path = state_dir.join("airplay.pcm");
        ensure_pipe(&pipe_path).expect("create fifo");

        // A daemon that reports `play` until the FIFO reader observes EOF, then
        // `stop` — the production completion contract.
        let drained = Arc::new(AtomicBool::new(false));
        let server_drained = Arc::clone(&drained);
        let received = Arc::new(AtomicBool::new(false));
        let server_received = Arc::clone(&received);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let api_base = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("addr").port()
        );
        std::thread::spawn(move || {
            serve_drain_aware_daemon(listener, server_drained, server_received);
        });

        // The FIFO reader observes the drained writer and only then flips the
        // daemon's completion state — a real drain barrier, not a sleep.
        let reader_path = pipe_path.clone();
        let reader_drained = Arc::clone(&drained);
        let reader =
            std::thread::spawn(move || drain_fifo_until_eof(reader_path, received, reader_drained));

        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(test_session_inner_at_base(&api_base, state_dir, tx));
        let write_fd = open_pipe_write(
            &pipe_path,
            verify_pipe_identity(&pipe_path).unwrap(),
            Instant::now() + Duration::from_secs(5),
            &inner.cancel,
        )
        .unwrap_or_else(|_| panic!("open fifo write end"));
        let pipeline = gst::parse::launch(&format!(
            "audiotestsrc num-buffers=8 ! audioconvert ! audio/x-raw,format=S16LE,rate=44100,channels=2 ! fdsink fd={}",
            write_fd.as_raw_fd(),
        ))
        .expect("build pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline");
        *inner.pipeline.lock().unwrap_or_else(|p| p.into_inner()) = Some(pipeline.clone());

        let pump_inner = Arc::clone(&inner);
        let pump = std::thread::spawn(move || run_pump(pump_inner, pipeline, write_fd));
        assert!(
            inner.activate_and_play(),
            "activation must start the real pipeline"
        );

        let written = reader.join().expect("reader");
        pump.join().expect("pump");

        assert!(
            written > 0,
            "the pipeline must have written PCM into the FIFO before EOF"
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
                .count(),
            1,
            "exactly one TrackEnded: {events:?}"
        );
        let stopped_at = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
                )
            })
            .unwrap_or_else(|| panic!("Stopped must be published: {events:?}"));
        let ended_at = events
            .iter()
            .position(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
            .expect("TrackEnded");
        assert!(
            stopped_at < ended_at,
            "Stopped must precede TrackEnded: {events:?}"
        );
        assert!(
            inner.restored.load(Ordering::SeqCst),
            "completion must restore the daemon"
        );
        assert_eq!(
            *inner.state.lock().unwrap_or_else(|p| p.into_inner()),
            PlayerState::Stopped,
            "the terminal state must be the final cached state"
        );
    }

    /// W2: the real [`OwnToneSession::close`] owns teardown. A play in flight
    /// when the close begins is settled by the close's own restoration, and both
    /// the session's media route and the advisory instance lock are released
    /// only after that settlement. This drives the production close path rather
    /// than calling the gate directly.
    #[cfg(owntone_host)]
    #[test]
    fn a_session_close_settles_a_stalled_play_and_releases_route_and_lock() {
        use crate::architecture::media::ResolvedHttpRequest;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let server = FakeOwnToneServer::start_parking_play();
        let directory = tempfile::tempdir().expect("tempdir");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("endpoint"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");
        let ticket = prepared.ticket().expect("protected media ticket");
        assert!(proxy.has_active_lease());
        assert_eq!(ticket.route_count(), 1);

        let lock_path = directory.path().join("instance.lock");
        let lock = open_lock(&lock_path).expect("open lock");
        assert!(rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_ok());
        let competing = open_lock(&lock_path).expect("open competing lock");
        assert!(
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err(),
            "the live session must hold the advisory instance lock"
        );

        let inner = Arc::new({
            let mut inner = test_session_inner_at_base(
                &server.api_base,
                directory.path(),
                async_channel::unbounded().0,
            );
            inner.media_proxy = Arc::clone(&proxy);
            inner.media_ticket = Some(Arc::clone(&ticket));
            inner
        });

        let session = OwnToneSession {
            inner: Arc::clone(&inner),
            pump: None,
            lock: Some(lock),
        };

        // A play is accepted and transmitted; the daemon parks it, so the close
        // must wait on the settlement boundary and settle the in-flight effect.
        let play_inner = Arc::clone(&inner);
        let play = std::thread::spawn(move || play_inner.activate_and_play());
        server.park.wait_seen();

        let closer = std::thread::spawn(move || {
            let boxed: Box<dyn SenderSession> = Box::new(session);
            boxed.close();
        });
        std::thread::sleep(Duration::from_millis(100));
        server.park.release();

        let _ = play.join().expect("play thread");
        closer.join().expect("close thread");

        assert!(
            !proxy.has_active_lease() && !proxy.has_custody_entries(),
            "close must release the media route by identity"
        );
        assert_eq!(
            ticket.route_count(),
            0,
            "the released route must be shut down"
        );
        assert!(
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok(),
            "close must release the advisory lock after settlement"
        );
    }

    // ----- X2: hermetic fake owned daemon + retained-recovery settlement -----

    /// Source for a hermetic fake OwnTone daemon that satisfies the adapter's
    /// full ownership binding: it binds the loopback endpoint and answers the
    /// HTTP contract. Because it is a real process launched with
    /// `-c <state>/owntone.conf`, `process_is_owned` accepts it and
    /// `quiesce_daemon` can genuinely terminate and restart it.
    #[cfg(owntone_host)]
    const FAKE_OWNED_DAEMON_SOURCE: &str = r##"
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

fn main() {
    let addr = std::env::var("TRIBUTARY_FAKE_LISTEN").expect("listen address");
    let listener = TcpListener::bind(&addr).expect("bind");
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        std::thread::spawn(move || serve(stream));
    }
}

fn serve(stream: std::net::TcpStream) {
    let Ok(reader_stream) = stream.try_clone() else { return };
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() { return; }
    let trimmed = request_line.trim().to_string();
    if trimmed.is_empty() { return; }
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() { break; }
        if let Some(value) = header
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(str::trim)
        {
            content_length = value.parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }
    let path = trimmed.split_whitespace().nth(1).unwrap_or_default().to_string();
    let body = match path.as_str() {
        "/api/config" => r#"{"version":"29.3"}"#.to_string(),
        "/api/outputs" => r#"{"outputs":[]}"#.to_string(),
        "/api/player" => r#"{"state":"stop"}"#.to_string(),
        _ => "{}".to_string(),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let mut stream = stream;
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
"##;

    /// A real, owned, restartable fake daemon plus the configuration that binds
    /// it. Kept alive for the duration of the test; `Drop` stops whichever
    /// instance currently holds the endpoint and reaps the initial child.
    #[cfg(owntone_host)]
    struct FakeOwnedDaemon {
        _directory: tempfile::TempDir,
        config: OwnToneConfig,
        child: Option<std::process::Child>,
    }

    #[cfg(owntone_host)]
    impl Drop for FakeOwnedDaemon {
        fn drop(&mut self) {
            if let Some(process) = listener_process(&self.config.api_base) {
                let _ = terminate_process(&process, &self.config, Duration::from_secs(5));
            }
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait();
            }
        }
    }

    #[cfg(owntone_host)]
    impl FakeOwnedDaemon {
        fn start() -> Self {
            use std::net::TcpListener;
            let directory = tempfile::tempdir().expect("tempdir");
            let state_dir = directory.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("state dir");
            let pipe = state_dir.join("airplay.pcm");
            // The dedicated-config authority check binds only a real FIFO at
            // the scanned pathname, so the fixture provisions one.
            ensure_pipe(&pipe).expect("create scanned FIFO");
            let config_path = state_dir.join(OWNTONE_CONFIG_FILE);
            std::fs::write(&config_path, dedicated_config::fixture(&pipe)).expect("write config");

            let source = directory.path().join("fake_owned.rs");
            std::fs::write(&source, FAKE_OWNED_DAEMON_SOURCE).expect("write source");
            let binary = directory.path().join("fake_owntone");
            let compiled = std::process::Command::new("rustc")
                .arg("-O")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .expect("invoke rustc");
            assert!(compiled.success(), "the fake owned daemon must compile");

            let probe = TcpListener::bind("127.0.0.1:0").expect("reserve port");
            let port = probe.local_addr().expect("addr").port();
            drop(probe);

            let api_base = format!("http://127.0.0.1:{port}");
            let config = OwnToneConfig {
                api_base: api_base.clone(),
                pipe_path: pipe.clone(),
                state_dir: state_dir.clone(),
                binary: binary.clone(),
            };
            let restart = format!(
                "TRIBUTARY_FAKE_LISTEN=127.0.0.1:{port} {} -c {}",
                binary.display(),
                config_path.display()
            );
            let record = OwnershipRecord {
                token: OWNER_TOKEN.to_string(),
                api_base,
                pipe_path: pipe.to_string_lossy().into_owned(),
                state_dir: state_dir.to_string_lossy().into_owned(),
                binary: binary.to_string_lossy().into_owned(),
                restart_command: Some(restart),
            };
            std::fs::write(
                config.owner_marker(),
                serde_json::to_vec(&record).expect("serialize record"),
            )
            .expect("write ownership record");

            let child = launch_fake_owned(&binary, &config_path, port);
            Self {
                _directory: directory,
                config,
                child: Some(child),
            }
        }
    }

    /// Launch the fake owned daemon bound to `port`, wait until it is listening,
    /// and return the child so it can be reaped on drop.
    #[cfg(owntone_host)]
    fn launch_fake_owned(binary: &Path, config_path: &Path, port: u16) -> std::process::Child {
        let child = std::process::Command::new(binary)
            .arg("-c")
            .arg(config_path)
            .env("TRIBUTARY_FAKE_LISTEN", format!("127.0.0.1:{port}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn fake owned daemon");
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "the fake daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        child
    }

    /// A real custodied media route for the retained-recovery fixtures.
    #[cfg(owntone_host)]
    fn custodied_route() -> (Arc<GstreamerMediaProxy>, Arc<GstreamerMediaTicket>) {
        use crate::architecture::media::ResolvedHttpRequest;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("endpoint"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");
        let ticket = prepared.ticket().expect("protected ticket");
        proxy.move_to_recovery_custody(&ticket);
        (proxy, ticket)
    }

    /// X2: the **real** [`RetainedRecovery`] settles against a real owned daemon.
    /// Quiescence terminates and restarts the instance, restoration succeeds,
    /// and only then are the custodied route (by identity) and the advisory
    /// lock released. This is the success path the previous leg never
    /// exercised.
    #[cfg(owntone_host)]
    #[test]
    fn a_retained_recovery_releases_lock_and_custodied_route_on_settlement() {
        let daemon = FakeOwnedDaemon::start();
        let (proxy, ticket) = custodied_route();
        assert!(proxy.is_custodied(&ticket));
        assert_eq!(ticket.route_count(), 1);

        let lock_path = daemon.config.state_dir.join("instance.lock");
        let lock = open_lock(&lock_path).expect("open lock");
        assert!(rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_ok());
        let competing = open_lock(&lock_path).expect("competing lock");
        assert!(
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err(),
            "the retained recovery must hold the advisory lock"
        );

        let job = RetainedRecovery {
            client: Arc::new(OwnToneClient::new(&daemon.config.api_base).expect("client")),
            config: daemon.config.clone(),
            recorded: TakeoverRecord {
                enabled_outputs: vec![1],
                selected_output: 0,
            },
            lock,
            route: Some((Arc::clone(&proxy), Arc::clone(&ticket))),
            retry_at: Instant::now(),
        };
        let supervisor = Arc::new(RecoverySupervisor::new());
        supervisor.register(job);

        wait_until(|| !proxy.is_custodied(&ticket) && !proxy.has_custody_entries());
        assert_eq!(
            ticket.route_count(),
            0,
            "the custodied route must be shut down"
        );
        wait_until(|| {
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok()
        });
    }

    /// X2: an **injected inline** `spawn_serialized_recovery` thread-spawn
    /// failure is not leaked. The lock is handed to the process-global
    /// supervisor, which starts a live owner, settles against the real daemon
    /// and releases the lock; the completion reports the terminal retained
    /// outcome.
    #[cfg(owntone_host)]
    #[test]
    fn an_injected_inline_recovery_spawn_failure_hands_off_to_the_supervisor() {
        let daemon = FakeOwnedDaemon::start();
        let lock_path = daemon.config.state_dir.join("instance.lock");
        let lock = open_lock(&lock_path).expect("open lock");
        assert!(rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_ok());
        let competing = open_lock(&lock_path).expect("competing lock");
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());

        fail_next_recovery_spawns(1);
        let completion = spawn_serialized_recovery(
            Arc::new(OwnToneClient::new(&daemon.config.api_base).expect("client")),
            daemon.config.clone(),
            TakeoverRecord {
                enabled_outputs: vec![1],
                selected_output: 0,
            },
            lock,
            None,
        );
        assert!(
            matches!(completion.wait(), RecoveryOutcome::Retained { .. }),
            "an inline spawn failure reports the terminal retained outcome"
        );

        // The fallback registered a live owner with the process-global
        // supervisor, which settles and releases the lock.
        wait_until(|| {
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok()
        });
    }

    // ----- X2/Y2: full open() production-path ordering -----

    /// Hermetic fake OwnTone daemon for the full `open()` fixture: it satisfies
    /// the ownership binding, reports one output matching device id 255, answers
    /// the control endpoints, and records every request line to the file named
    /// by `TRIBUTARY_FAKE_REQUESTS`, so the adapter's real request ordering is
    /// observable.
    #[cfg(owntone_host)]
    const RECORDING_DAEMON_SOURCE: &str = r##"
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::os::unix::fs::OpenOptionsExt;

struct State { queued: bool, playing: bool, paused: bool, selected: bool, bytes: usize, autostarted: bool }
static STATE: Mutex<State> = Mutex::new(State { queued: false, playing: false, paused: false, selected: false, bytes: 0, autostarted: false });
static FIFO: Mutex<Option<std::fs::File>> = Mutex::new(None);
static TERM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    fn syscall(num: i64, ...) -> i64;
}
extern "C" fn on_term(_sig: i32) { TERM.store(true, std::sync::atomic::Ordering::SeqCst); }
fn pipe() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::args().nth(2).unwrap()).parent().unwrap().join("airplay.pcm")
}
fn open_fifo_reader() -> Option<std::fs::File> {
    use std::os::unix::fs::FileTypeExt;
    // The pinned pipe watcher only ever holds a FIFO. A foreign object planted
    // at the pathname (the AM1 fixture's sentinel file or symlink) is never
    // opened here, so PCM observations can only come from the real FIFO.
    let is_fifo = std::fs::symlink_metadata(pipe())
        .map(|m| m.file_type().is_fifo())
        .unwrap_or(false);
    is_fifo.then(|| std::fs::OpenOptions::new().read(true)
        .custom_flags(0x800).open(pipe()).expect("FIFO reader"))
}
fn main() {
    *FIFO.lock().unwrap() = open_fifo_reader();
    std::thread::spawn(move || loop {
        // Keep the reader open but stop consuming after the first PCM. This
        // models failed autostart and an already-playing receiver that stalls.
        let mut state = STATE.lock().unwrap();
        if pipe().with_extension("stall").exists() && state.bytes > 0 {
            drop(state);
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        // Pinned pause stopped the pipe input: nothing reads the FIFO until
        // the item is resumed (the re-armed watcher sees "already playing").
        if state.paused {
            drop(state);
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        if pipe().with_extension("break-pipe").exists() {
            std::fs::remove_file(pipe().with_extension("break-pipe")).unwrap();
            drop(FIFO.lock().unwrap().take());
            std::fs::write(pipe().with_extension("pipe-broken"), "").unwrap();
        }
        let mut buffer = [0; 4096];
        let read = FIFO.lock().unwrap().as_mut().map(|fifo| fifo.read(&mut buffer));
        if let Some(Ok(n)) = read {
            if n > 0 {
                if state.bytes == 0 {
                    let record = std::env::var("TRIBUTARY_FAKE_REQUESTS").unwrap();
                    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(record).unwrap();
                    writeln!(file, "PCM received").unwrap();
                }
                state.bytes += n;
                // Pinned pipe_read_cb can autostart only on readable PCM.
                if !state.paused && !pipe().with_extension("fail-play").exists() {
                    state.queued = true;
                    state.playing = true;
                    state.autostarted = true;
                }
            } else {
                if state.autostarted { state.playing = false; state.autostarted = false; }
            }
        }
        drop(state);
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
    let addr = std::env::var("TRIBUTARY_FAKE_LISTEN").expect("listen address");
    let listener = TcpListener::bind(&addr).expect("bind");
    unsafe { signal(15, on_term); }
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else { continue };
            std::thread::spawn(move || serve(stream));
        }
    });
    while !TERM.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if pipe().with_extension("linger-on-term").exists() {
        // The kernel window a real daemon shows on SIGTERM: the thread-group
        // leader is already a zombie while a sibling thread still holds the
        // listening socket. Exit only this thread; the process follows later.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(400));
            std::process::exit(0);
        });
        unsafe { syscall(60, 0i64); }
    }
    std::process::exit(0);
}
fn serve(stream: std::net::TcpStream) {
    let Ok(reader_stream) = stream.try_clone() else { return };
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() { return; }
    let trimmed = request_line.trim().to_string();
    if trimmed.is_empty() { return; }
    if let Ok(record) = std::env::var("TRIBUTARY_FAKE_REQUESTS") {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(record) {
            let _ = writeln!(file, "{trimmed}");
            let _ = file.flush();
        }
    }
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() { break; }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:").map(str::trim) {
            content_length = value.parse().unwrap_or(0);
        }
    }
    let mut request_body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut request_body);
    let path = trimmed.split_whitespace().nth(1).unwrap_or_default();
    let method = trimmed.split_whitespace().next().unwrap_or_default();
    let volume = path.strip_prefix("/api/player/volume?volume=")
        .and_then(|v| v.parse::<u8>().ok()).filter(|v| *v <= 100);
    let pipe = pipe();
    // An on-disk response override survives owned-process recovery restarts.
    // Never apply the mutation when returning this non-success response.
    if let Ok(override_response) = std::fs::read_to_string(pipe.with_extension("http-response")) {
        let mut lines = override_response.lines();
        let target = lines.next().unwrap_or_default();
        let status = lines.next().unwrap_or("307 Temporary Redirect");
        let location = lines.next().unwrap_or_default();
        if target == path || (target == "mutations" && method == "PUT") {
            let location = if location.is_empty() { String::new() }
                else { format!("Location: {location}\r\n") };
            let response = format!("HTTP/1.1 {status}\r\n{location}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}");
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            return;
        }
    }
    if method == "GET" && path == "/api/player" && pipe.with_extension("drain-started").exists()
        && !pipe.with_extension("drain-replied").exists() {
        std::fs::write(pipe.with_extension("drain-seen"), "").unwrap();
        while !pipe.with_extension("drain-release").exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let answer = std::fs::read_to_string(pipe.with_extension("drain-release")).unwrap();
        std::fs::write(pipe.with_extension("drain-replied"), "").unwrap();
        // A JSON object is served verbatim (a parked sample with progress).
        let body = if answer.trim_start().starts_with('{') { answer.trim().to_string() }
            else { format!(r#"{{"state":"{}"}}"#, answer) };
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        let mut stream = stream;
        let _ = stream.write_all(response.as_bytes());
        return;
    }
    // Park actual startup observation/resume requests without blocking FIFO
    // consumption or other daemon requests. Release only after controller Stop.
    let activation = pipe.with_extension("park-activation");
    let park_start = method == "GET" && path == "/api/player"
        && STATE.lock().unwrap().bytes > 0;
    let park_resume = method == "PUT" && path == "/api/player/play";
    let activation_mode = std::fs::read_to_string(&activation).unwrap_or_default();
    if ((activation_mode == "start" && park_start) || (activation_mode == "resume" && park_resume))
        && !pipe.with_extension("activation-replied").exists() {
        std::fs::write(pipe.with_extension("activation-seen"), "").unwrap();
        while !pipe.with_extension("activation-release").exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::fs::write(pipe.with_extension("activation-replied"), "").unwrap();
    }
    let control_mode = std::fs::read_to_string(pipe.with_extension("park-control")).unwrap_or_default();
    let parked_control = method == "PUT" && ((control_mode == "pause" && path == "/api/player/pause")
        || (control_mode == "volume" && volume == Some(20)));
    if parked_control {
        std::fs::write(pipe.with_extension("control-seen"), "").unwrap();
        while !pipe.with_extension("control-release").exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if pipe.with_extension("fail-control").exists() {
            let mut stream = stream;
            let _ = stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            return;
        }
    }
    // One-shot restoring mutation fault, consumed before restart so recovery
    // can restore normally. A parked request really outlives the HTTP timeout.
    if method == "PUT" && path == "/api/player/stop"
        && std::fs::rename(pipe.with_extension("park-restore"), pipe.with_extension("restore-seen")).is_ok() {
        let mode = std::fs::read_to_string(pipe.with_extension("restore-seen")).unwrap();
        while !pipe.with_extension("restore-release").exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if mode == "fail" {
            let mut stream = stream;
            let _ = stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            return;
        }
    }
    // AM1 fixture: substitute the scanned pipe pathname while serving the
    // adapter's last pre-acquisition mutation (the initial volume PUT), i.e.
    // after every probe and ownership check and immediately before the writer
    // descriptor is acquired. One-shot; the planted object is never opened
    // by this daemon (see open_fifo_reader).
    if method == "PUT" && path.starts_with("/api/player/volume") {
        if let Ok(mode) = std::fs::read_to_string(pipe.with_extension("substitute")) {
            std::fs::remove_file(pipe.with_extension("substitute")).unwrap();
            let sentinel = pipe.with_extension("sentinel");
            match mode.trim() {
                "regular" => std::fs::rename(&sentinel, &pipe).unwrap(),
                "symlink" => {
                    let link = pipe.with_extension("substitute-link");
                    std::os::unix::fs::symlink(&sentinel, &link).unwrap();
                    std::fs::rename(&link, &pipe).unwrap();
                }
                other => panic!("unknown pipe substitution {}", other),
            }
            std::fs::write(pipe.with_extension("substituted"), "").unwrap();
        }
    }
    let mut state = STATE.lock().unwrap();
    // item_progress_ms models pinned pos_ms: it advances only for PCM the
    // input actually read and stands still once the pipe is dry.
    let dynamic = format!(r#"{{"state":"{}","pcm_bytes":{},"queued":{},"item_progress_ms":{}}}"#, if state.playing { "play" } else if state.paused { "pause" } else { "stop" }, state.bytes, state.queued, state.bytes as u64 * 1000 / 176_400);
    let outputs = format!(r#"{{"outputs":[{{"id":"11189196","name":"Test","selected":{}}},{{"id":"42","name":"Prior","selected":{}}}]}}"#, state.selected, !state.selected);
    let overridden = match (method, path) {
        ("GET", "/api/player") => std::fs::read_to_string(pipe.with_extension("player-response")).ok(),
        ("GET", "/api/outputs") => std::fs::read_to_string(pipe.with_extension("outputs-response")).ok(),
        _ => None,
    };
    let (status, body) = match (method, path) {
        ("GET", _) if overridden.is_some() => ("200 OK", overridden.as_deref().unwrap()),
        ("GET", "/api/config") => ("200 OK", r#"{"version":"29.3"}"#),
        ("GET", "/api/outputs") => ("200 OK", outputs.as_str()),
        ("GET", "/api/player") => ("200 OK", dynamic.as_str()),
        ("PUT", "/api/queue/clear") => {
            state.queued = false; state.playing = false; state.paused = false; state.autostarted = false;
            ("204 No Content", "")
        }
        ("PUT", "/api/player/play") => {
            if !state.queued || pipe.with_extension("fail-play").exists() {
                ("500 Internal Server Error", "{}")
            } else { state.playing = true; state.paused = false; ("204 No Content", "") }
        }
        ("PUT", "/api/player/stop") => {
            // Pinned input stop closes its reader and resets the pipe watcher.
            // After producer shutdown this discards buffered old PCM instead
            // of spuriously treating it as a new autostart on the next read.
            let mut fifo = FIFO.lock().unwrap();
            drop(fifo.take());
            *fifo = open_fifo_reader();
            state.playing = false; state.paused = false; state.autostarted = false;
            ("204 No Content", "")
        }
        ("PUT", "/api/player/pause") => {
            // Pinned pause stops the pipe input (inputs/pipe.c: stop): its
            // reader closes, discarding buffered PCM, the watcher is re-armed
            // and pipe_autostart_id is cleared, so the later resume restarts
            // the item as a plain, never-autostopping source.
            let mut fifo = FIFO.lock().unwrap();
            drop(fifo.take());
            *fifo = open_fifo_reader();
            state.playing = false; state.paused = true; state.autostarted = false;
            ("204 No Content", "")
        }
        ("PUT", _) if path.starts_with("/api/player/volume") => {
            if volume.is_some() && content_length == 0 { ("204 No Content", "") }
            else { ("400 Bad Request", "{}") }
        }
        ("PUT", "/api/outputs/set") => {
            state.selected = String::from_utf8_lossy(&request_body).contains("11189196");
            ("204 No Content", "")
        }
        _ => ("404 Not Found", "{}"),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let mut stream = stream;
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
"##;

    /// A real, owned, request-recording fake daemon for the full `open()` path.
    #[cfg(owntone_host)]
    struct RecordingOwnedDaemon {
        _directory: tempfile::TempDir,
        config: OwnToneConfig,
        requests: PathBuf,
        child: Option<std::process::Child>,
    }

    /// The endpoint check follows the listening socket itself, not a process
    /// that can be named as holding it.
    #[cfg(owntone_host)]
    #[test]
    fn endpoint_is_bound_follows_the_listening_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        assert!(endpoint_is_bound(&api_base));
        drop(listener);
        wait_until(|| !endpoint_is_bound(&api_base));
        assert!(listener_process(&api_base).is_none());
    }

    /// A restart must never race the previous instance's socket release. The
    /// recording daemon models the kernel window a real multithreaded daemon
    /// shows on SIGTERM — its thread-group leader is reported as a zombie
    /// while a sibling thread still holds the listening socket — and the
    /// quiescence must still come back with a fresh owned instance well
    /// inside the restart deadline instead of spawning a restart that cannot
    /// bind.
    #[cfg(owntone_host)]
    #[test]
    fn quiesce_waits_for_the_endpoint_before_restarting() {
        let daemon = RecordingOwnedDaemon::start();
        std::fs::write(
            daemon.config.pipe_path.with_extension("linger-on-term"),
            b"",
        )
        .unwrap();
        let before = listener_process(&daemon.config.api_base)
            .expect("the daemon listens")
            .identity();
        let started = Instant::now();
        quiesce_daemon(&daemon.config).expect("quiescence restarts the instance");
        let after =
            listener_process(&daemon.config.api_base).expect("the restarted instance listens");
        assert!(process_is_owned(&after, &daemon.config));
        assert_ne!(after.identity(), before);
        assert!(
            started.elapsed() < QUIESCE_RESTART_DEADLINE,
            "the restart waited for the release instead of timing out: {:?}",
            started.elapsed()
        );
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        assert!(client.outputs().is_ok());
    }

    /// Terminate the fixture's daemon the way a crash leaves it: no listener,
    /// nothing bound, and nobody but the recorded restart command to bring it
    /// back.
    #[cfg(owntone_host)]
    fn crash(daemon: &RecordingOwnedDaemon) {
        let process = listener_process(&daemon.config.api_base).expect("the daemon listens");
        terminate_process(&process, &daemon.config, Duration::from_secs(5))
            .expect("the daemon terminates");
        wait_until(|| !endpoint_is_bound(&daemon.config.api_base));
    }

    /// An instance that already exited is quiesced by definition; recovery
    /// must still run the recorded restart command so restoration can
    /// proceed. Refusing with "not running" retained the takeover record, the
    /// instance lock and the media route forever (PR #270 review, round 11).
    #[cfg(owntone_host)]
    #[test]
    fn quiesce_restarts_an_instance_that_already_exited() {
        let daemon = RecordingOwnedDaemon::start();
        let before = listener_process(&daemon.config.api_base)
            .expect("the daemon listens")
            .identity();
        crash(&daemon);
        assert!(listener_process(&daemon.config.api_base).is_none());

        quiesce_daemon(&daemon.config).expect("recovery restarts the exited instance");
        let after =
            listener_process(&daemon.config.api_base).expect("the restarted instance listens");
        assert!(process_is_owned(&after, &daemon.config));
        assert_ne!(after.identity(), before);
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        assert!(client.outputs().is_ok());
    }

    /// A listener that is not the dedicated instance never receives a
    /// mutation: the session client proves the endpoint's process before it
    /// transmits, not only once at open (PR #270 review, round 11).
    #[cfg(owntone_host)]
    #[test]
    fn session_client_refuses_a_foreign_listener_before_sending() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let config = OwnToneConfig {
            api_base: format!("http://{}", listener.local_addr().unwrap()),
            pipe_path: directory.path().join("airplay.pcm"),
            state_dir: directory.path().to_path_buf(),
            binary: PathBuf::from("/usr/bin/owntone"),
        };
        let client = OwnToneClient::for_owned_instance(&config).unwrap();
        for result in [client.clear_queue(), client.set_outputs(&[1])] {
            let outcome = result.expect_err("a foreign listener is refused");
            // The refusal is the primitive's typed pre-send outcome, so a
            // takeover step can settle it instead of reporting a request
            // that was never transmitted as unsettled (round 12).
            assert!(
                matches!(outcome, MutationOutcome::Refused(_)),
                "a guard refusal must be typed as pre-send"
            );
            assert_eq!(
                outcome.message(),
                unavailable_in("en", "endpoint_process_is_not_the_owntone_binary").message()
            );
        }
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "no connection may reach the foreign listener"
        );
    }

    /// A takeover mutation the process guard refuses was never transmitted, so
    /// it is a settled failure; one that fails in flight is not.
    #[cfg(owntone_host)]
    #[test]
    fn a_guard_refusal_is_a_settled_takeover_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let config = OwnToneConfig {
            api_base: format!("http://{}", listener.local_addr().unwrap()),
            pipe_path: directory.path().join("airplay.pcm"),
            state_dir: directory.path().to_path_buf(),
            binary: PathBuf::from("/usr/bin/owntone"),
        };
        let guarded = OwnToneClient::for_owned_instance(&config).unwrap();
        let (_, unsettled) = guarded
            .takeover_step(OwnToneClient::clear_queue)
            .expect_err("a foreign listener is refused");
        assert!(!unsettled, "nothing was transmitted");

        let unreachable = OwnToneClient::new("http://127.0.0.1:1").unwrap();
        let (_, unsettled) = unreachable
            .takeover_step(OwnToneClient::clear_queue)
            .expect_err("an unreachable daemon fails in flight");
        assert!(unsettled, "a request that failed in flight may still apply");
    }

    /// An inner guard refusal — the primitive's own proof at the transmission
    /// boundary, which can flip after the outer authorization passed — is
    /// settled like the outer one: the request was never sent, so it keeps
    /// its own reason and does not enter the recovery path; a failure at or
    /// after transmission stays unsettled (PR #270 review, round 12).
    #[test]
    fn an_inner_guard_refusal_is_a_settled_takeover_failure() {
        let client = OwnToneClient::new("http://127.0.0.1:1").unwrap();
        let (error, unsettled) = client
            .takeover_step(|_| {
                Err(MutationOutcome::Refused(unavailable_in(
                    "en",
                    "endpoint_process_is_not_the_owntone_binary",
                )))
            })
            .expect_err("an inner pre-send refusal fails the step");
        assert!(!unsettled, "nothing was transmitted");
        assert_eq!(
            error.message(),
            unavailable_in("en", "endpoint_process_is_not_the_owntone_binary").message(),
            "the refusal keeps its own reason"
        );

        let (error, unsettled) = client
            .takeover_step(|_| {
                Err(MutationOutcome::Unsettled(unavailable_in(
                    "en",
                    "dedicated_daemon_is_unreachable",
                )))
            })
            .expect_err("a transmitted failure fails the step");
        assert!(unsettled, "a transmitted failure may still have applied");
        assert_eq!(
            error.message(),
            unavailable_in("en", "dedicated_daemon_is_unreachable").message()
        );
    }

    /// A daemon that stops on its own while the session is paused makes every
    /// drain observation complete and be voided at once. The wait must keep
    /// its poll cadence instead of spinning on `GET /api/player`.
    #[cfg(owntone_host)]
    #[test]
    fn a_daemon_side_stop_while_paused_does_not_spin_the_drain_wait() {
        let generation = PlayerEventGeneration::from_raw(55);
        let (fixture, ticket, _baseline) = exercise_healthy_pause_beyond_the_drain_deadline(
            Duration::from_millis(500),
            generation,
        );
        let polls = |fixture: &DaemonControllerFixture| {
            fixture
                .daemon
                .recorded()
                .lines()
                .filter(|line| line.starts_with("GET /api/player"))
                .count()
        };
        fixture
            .client
            .player_control("stop")
            .expect("daemon-side stop");
        wait_until(|| {
            fixture
                .client
                .player_state()
                .is_ok_and(|state| state == "stop")
        });
        let before = polls(&fixture);
        std::thread::sleep(Duration::from_secs(1));
        let observed = polls(&fixture) - before;
        assert!(
            observed <= 30,
            "{observed} player polls in one second: the voided completion is re-polled without a pause"
        );
        assert_eq!(fixture.controller.state(), PlayerState::Paused);

        fixture.controller.stop();
        wait_until(|| ticket.route_count() == 0);
        wait_until(|| fixture.controller.state() == PlayerState::Stopped);
        wait_until(|| fixture.lock_is_free());
    }

    /// The session client follows the owned instance across a legitimate
    /// restart and refuses once nothing owned serves the endpoint.
    #[cfg(owntone_host)]
    #[test]
    fn session_client_follows_the_owned_instance_and_refuses_once_it_is_gone() {
        let daemon = RecordingOwnedDaemon::start();
        let client = OwnToneClient::for_owned_instance(&daemon.config).unwrap();
        client.clear_queue().expect("the owned instance accepts");
        quiesce_daemon(&daemon.config).expect("quiescence restarts the instance");
        client
            .clear_queue()
            .expect("the restarted owned instance is proven again and accepts");
        crash(&daemon);
        let error = client
            .clear_queue()
            .expect_err("nothing owned serves the endpoint");
        assert_eq!(
            error.message(),
            unavailable_in(
                "en",
                "no_process_is_bound_to_the_configured_dedicated_instance_endpoint"
            )
            .message()
        );
    }

    #[cfg(owntone_host)]
    impl Drop for RecordingOwnedDaemon {
        fn drop(&mut self) {
            if let Some(process) = listener_process(&self.config.api_base) {
                let _ = terminate_process(&process, &self.config, Duration::from_secs(5));
            }
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait();
            }
        }
    }

    #[cfg(owntone_host)]
    impl RecordingOwnedDaemon {
        fn start() -> Self {
            use std::net::TcpListener;
            let directory = tempfile::tempdir().expect("tempdir");
            let state_dir = directory.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("state dir");
            let pipe = state_dir.join("airplay.pcm");
            ensure_pipe(&pipe).expect("create scanned FIFO");
            let config_path = state_dir.join(OWNTONE_CONFIG_FILE);
            std::fs::write(&config_path, dedicated_config::fixture(&pipe)).expect("write config");

            let source = directory.path().join("fake_recording.rs");
            std::fs::write(&source, RECORDING_DAEMON_SOURCE).expect("write source");
            let binary = directory.path().join("fake_owntone");
            let compiled = std::process::Command::new("rustc")
                .arg("-O")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .expect("invoke rustc");
            assert!(compiled.success(), "the recording daemon must compile");

            let probe = TcpListener::bind("127.0.0.1:0").expect("reserve port");
            let port = probe.local_addr().expect("addr").port();
            drop(probe);

            let api_base = format!("http://127.0.0.1:{port}");
            let requests = state_dir.join("requests.txt");
            let config = OwnToneConfig {
                api_base: api_base.clone(),
                pipe_path: pipe.clone(),
                state_dir: state_dir.clone(),
                binary: binary.clone(),
            };
            let restart = format!(
                "TRIBUTARY_FAKE_LISTEN=127.0.0.1:{port} TRIBUTARY_FAKE_REQUESTS={} {} -c {}",
                requests.display(),
                binary.display(),
                config_path.display()
            );
            let record = OwnershipRecord {
                token: OWNER_TOKEN.to_string(),
                api_base,
                pipe_path: pipe.to_string_lossy().into_owned(),
                state_dir: state_dir.to_string_lossy().into_owned(),
                binary: binary.to_string_lossy().into_owned(),
                restart_command: Some(restart),
            };
            std::fs::write(
                config.owner_marker(),
                serde_json::to_vec(&record).expect("serialize record"),
            )
            .expect("write ownership record");

            let child = std::process::Command::new(&binary)
                .arg("-c")
                .arg(&config_path)
                .env("TRIBUTARY_FAKE_LISTEN", format!("127.0.0.1:{port}"))
                .env("TRIBUTARY_FAKE_REQUESTS", &requests)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn recording daemon");
            let deadline = Instant::now() + Duration::from_secs(5);
            while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                assert!(
                    Instant::now() < deadline,
                    "the recording daemon never listened"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Self {
                _directory: directory,
                config,
                requests,
                child: Some(child),
            }
        }

        fn recorded(&self) -> String {
            std::fs::read_to_string(&self.requests).unwrap_or_default()
        }
    }

    /// Run proxy configuration and the recovery capture in a private process;
    /// neither environment nor capture state is changed in the test runner.
    #[cfg(owntone_host)]
    #[test]
    fn control_transport_stays_on_verified_daemon() {
        const CHILD: &str = "TRIBUTARY_CONTROL_TRANSPORT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let foreign = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            foreign.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}", foreign.local_addr().unwrap());
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "audio::airplay_owntone::tests::control_transport_stays_on_verified_daemon",
                    "--nocapture",
                ])
                .env(CHILD, &endpoint)
                .env("HTTP_PROXY", &endpoint)
                .env("http_proxy", &endpoint)
                .env("ALL_PROXY", &endpoint)
                .env("all_proxy", &endpoint)
                .env("NO_PROXY", "")
                .env("no_proxy", "")
                .status()
                .unwrap();
            assert!(status.success(), "isolated transport regressions failed");
            assert!(
                matches!(foreign.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "the foreign redirect target / configured proxy received a connection"
            );
            return;
        }
        let foreign = std::env::var(CHILD).unwrap();
        // Existing full controller assertions also prove valid 200 observations
        // and 204 mutations work with an explicitly configured foreign proxy.
        exercise_takeover_observation(r#"{"state":"stop"}"#, false, true);
        for status in [
            "302 Found",
            "307 Temporary Redirect",
            "308 Permanent Redirect",
        ] {
            for location in ["", foreign.as_str()] {
                for path in ["/api/config", "/api/outputs/set", "/api/queue/clear"] {
                    exercise_transport_refusal(path, status, location, false);
                }
                for path in ["/api/player/stop", "/api/outputs/set"] {
                    exercise_transport_refusal(path, status, location, true);
                }
            }
        }
    }

    #[cfg(owntone_host)]
    fn exercise_transport_refusal(path: &str, status: &str, location: &str, live: bool) {
        use crate::audio::airplay_output::ControllerHarness;
        use crate::local::resolver::ResolvedLocalMedia;

        gst::init().unwrap();
        let daemon = RecordingOwnedDaemon::start();
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        let baseline = client.outputs().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = async_channel::unbounded();
        let controller = ControllerHarness::new(
            runtime.handle().clone(),
            Arc::new(OwnToneSender {
                config: Ok(daemon.config.clone()),
            }),
            tx,
        )
        .with_device_id("aabbcc");
        let root = tempfile::tempdir().unwrap();
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let wav = root.path().join("transport.wav");
        write_startup_wav(&wav, 30);
        let proxy = controller.proxy();
        let prepare = || {
            proxy
                .prepare_local(
                    ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &wav)
                        .unwrap(),
                )
                .unwrap()
        };
        let override_path = daemon.config.pipe_path.with_extension("http-response");
        let refuse =
            || std::fs::write(&override_path, format!("{path}\n{status}\n{location}\n")).unwrap();
        let step = format!("{path} {status} location={location:?} live={live}");
        let started = Instant::now();
        let prepared = prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(81);
        controller.set_generation(generation);
        let mutation = path != "/api/config";
        if mutation {
            arm_retained_recovery_capture(&daemon.config.api_base);
        }
        if !live {
            refuse();
            if !mutation {
                assert!(
                    client.get_json(path).is_err(),
                    "non-2xx JSON is not an observation"
                );
            }
        }
        controller.load(generation, prepared);
        if live {
            wait_until(|| controller.state() == PlayerState::Playing);
            drain_through(&rx, PlayerState::Playing);
            refuse();
            controller.stop();
        }
        if mutation {
            let mut recovery = None;
            wait_until(|| {
                recovery = recovery.take().or_else(take_captured_retained_recovery);
                recovery.is_some()
            });
            let recovery = recovery.unwrap();
            wait_until(|| proxy.is_custodied(&ticket));
            assert_eq!(ticket.route_count(), 1);
            assert!(flock_is_held(&daemon.config.lock_path()));
            assert!(daemon.config.takeover_record().exists());
            // The queue-clear refusal only affects takeover. Make restoration
            // itself refuse too, before attempting the retained job.
            if path == "/api/queue/clear" {
                std::fs::write(&override_path, format!("mutations\n{status}\n{location}\n"))
                    .unwrap();
            }
            assert!(
                !recovery.attempt(),
                "a redirect is not confirmed restoration ({})",
                transport_refusal_context(&step, &daemon, &override_path)
            );
            assert!(daemon.config.takeover_record().exists());
            assert!(proxy.is_custodied(&ticket));
            assert_eq!(ticket.route_count(), 1);
            assert!(flock_is_held(&daemon.config.lock_path()));
            std::fs::remove_file(&override_path).unwrap();
            assert!(
                recovery.attempt(),
                "valid responses must settle recovery ({})",
                transport_refusal_context(&step, &daemon, &override_path)
            );
            drop(recovery);
        } else {
            wait_until(|| ticket.route_count() == 0);
            assert!(!daemon
                .recorded()
                .lines()
                .any(|line| line.starts_with("PUT ")));
            std::fs::remove_file(&override_path).unwrap();
        }
        wait_until(|| ticket.route_count() == 0);
        assert!(!daemon.config.takeover_record().exists());
        assert!(!proxy.has_custody_entries());
        assert!(flock_is_acquirable(&daemon.config.lock_path()));
        if !live {
            assert_failed_load_events(&rx, generation);
        }
        assert_outputs_match(&client, &baseline);
        assert_next_load_plays(&controller, &daemon, prepare(), generation.next());
        // The isolated child runs with --nocapture, so this lands in CI logs.
        eprintln!(
            "transport refusal {step}: settled in {:?}",
            started.elapsed()
        );
    }

    /// What the daemon side looked like when a transport-refusal step did not
    /// settle: the listener bound to the endpoint (and whether it is the owned
    /// instance), the response override, the takeover record and the last
    /// requests the daemon recorded.
    #[cfg(owntone_host)]
    fn transport_refusal_context(
        step: &str,
        daemon: &RecordingOwnedDaemon,
        override_path: &Path,
    ) -> String {
        let listener = listener_process(&daemon.config.api_base).map(|process| {
            format!(
                "pid {} owned={} exe={:?}",
                process.pid,
                process_is_owned(&process, &daemon.config),
                process.exe
            )
        });
        let recorded = daemon.recorded();
        let recent: Vec<&str> = recorded.lines().rev().take(8).collect();
        format!(
            "{step}: listener={listener:?} override_present={} takeover_record={} recent requests={recent:?}",
            override_path.exists(),
            daemon.config.takeover_record().exists()
        )
    }

    /// AK1: malformed observations must fail before any takeover effect.
    #[cfg(owntone_host)]
    fn exercise_takeover_observation(response: &str, outputs: bool, accepted: bool) {
        use crate::audio::airplay_output::ControllerHarness;
        use crate::local::resolver::ResolvedLocalMedia;

        gst::init().unwrap();
        let daemon = RecordingOwnedDaemon::start();
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = async_channel::unbounded();
        let controller = ControllerHarness::new(
            runtime.handle().clone(),
            Arc::new(OwnToneSender {
                config: Ok(daemon.config.clone()),
            }),
            tx,
        )
        .with_device_id("aabbcc");
        let root = tempfile::tempdir().unwrap();
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let path = root.path().join("observation.wav");
        write_startup_wav(&path, 30);
        let proxy = controller.proxy();
        let prepare = || {
            proxy
                .prepare_local(
                    ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
                        .unwrap(),
                )
                .unwrap()
        };
        let baseline_outputs = client.outputs().unwrap();
        let baseline_player = client.get_json("/api/player").unwrap();
        let override_path = daemon.config.pipe_path.with_extension(if outputs {
            "outputs-response"
        } else {
            "player-response"
        });
        std::fs::write(&override_path, response).unwrap();
        if !outputs && !accepted && response != r#"{"state":"play"}"# {
            assert!(client.player_state().is_err());
            assert!(
                client.player_progress().is_err(),
                "drain must reject malformed state too"
            );
        }
        let prepared = prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(41);
        controller.set_generation(generation);
        let started = Instant::now();
        controller.load(generation, prepared);
        assert!(started.elapsed() < Duration::from_millis(500));
        if accepted {
            // Remove the read override once open has accepted it, so actual PCM
            // autostart can be observed normally by the production activation.
            wait_until(|| daemon.recorded().contains("PUT /api/outputs/set "));
            std::fs::remove_file(&override_path).unwrap();
            wait_until(|| controller.state() == PlayerState::Playing);
            controller.stop();
        }
        wait_until(|| ticket.route_count() == 0);
        let competing = open_lock(&daemon.config.lock_path()).unwrap();
        wait_until(|| {
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok()
        });
        assert!(!daemon.config.takeover_record().exists());
        assert!(!proxy.is_custodied(&ticket));
        assert!(!proxy.has_custody_entries());
        if !accepted {
            assert_failed_load_events(&rx, generation);
            assert_eq!(controller.state(), PlayerState::Stopped);
            assert!(
                !daemon
                    .recorded()
                    .lines()
                    .any(|line| line.starts_with("PUT ")
                        || line.starts_with("POST ")
                        || line.starts_with("DELETE ")),
                "{}",
                daemon.recorded()
            );
            std::fs::remove_file(&override_path).unwrap();
            assert_eq!(
                client.get_json("/api/player").unwrap(),
                baseline_player,
                "queue and player unchanged"
            );
        }
        assert_outputs_match(&client, &baseline_outputs);
        drop(competing);
        // A failure must release the prepared route and lock without a UI Stop,
        // and leave this same controller/daemon usable for the next generation.
        while rx.try_recv().is_ok() {}
        assert_next_load_plays(&controller, &daemon, prepare(), generation.next());
        assert_outputs_match(&client, &baseline_outputs);
    }

    #[cfg(owntone_host)]
    #[test]
    fn takeover_observation_rejects_malformed_player_states() {
        for response in [
            "{}",
            r#"{"state":null}"#,
            r#"{"state":17}"#,
            r#"{"state":"unexpected"}"#,
        ] {
            exercise_takeover_observation(response, false, false);
        }
    }

    #[cfg(owntone_host)]
    #[test]
    fn takeover_observation_rejects_incomplete_output_snapshots() {
        for second in [
            serde_json::json!({"selected": true}),
            serde_json::json!({"id": "invalid", "selected": true}),
            serde_json::json!({"id": null, "selected": true}),
            serde_json::json!({"id": 42, "selected": true}),
            serde_json::json!({"id": "42"}),
            serde_json::json!({"id": "42", "selected": "true"}),
            serde_json::json!({"id": "42", "selected": null}),
            serde_json::json!({"id": "11189196", "selected": true}),
        ] {
            let response = serde_json::json!({"outputs": [
                {"id": "11189196", "name": "Target", "selected": false}, second
            ]});
            exercise_takeover_observation(&response.to_string(), true, false);
        }
    }

    #[cfg(owntone_host)]
    #[test]
    fn takeover_observation_preserves_recognized_state_policy() {
        for state in ["stop", "pause", "play"] {
            exercise_takeover_observation(
                &format!(r#"{{"state":"{state}"}}"#),
                false,
                state != "play",
            );
        }
    }

    /// AM1: which foreign object is planted at the scanned pipe pathname
    /// between the adapter's ownership checks and its writer acquisition.
    #[cfg(owntone_host)]
    #[derive(Clone, Copy)]
    enum PipeSubstitution {
        RegularFile,
        Symlink,
    }

    /// The planted object is still exactly what the test put at the pathname.
    #[cfg(owntone_host)]
    fn assert_planted(pipe: &Path, sentinel: &Path, substitution: PipeSubstitution) {
        let planted = std::fs::symlink_metadata(pipe).unwrap().file_type();
        match substitution {
            PipeSubstitution::RegularFile => assert!(planted.is_file()),
            PipeSubstitution::Symlink => {
                assert!(planted.is_symlink());
                assert_eq!(std::fs::read_link(pipe).unwrap(), sentinel);
            }
        }
    }

    /// AM1: the recording daemon substitutes the pipe pathname while it serves
    /// the initial volume PUT — the production adapter's last mutation before
    /// `open_pipe_write` — so the substitution lands after every probe and
    /// ownership check and immediately before the descriptor is acquired. The
    /// production controller must refuse without delivering a byte to the
    /// foreign object, fail this generation truthfully (Error + Stopped, no
    /// Playing, no TrackEnded), restore the daemon or retain every piece of
    /// ownership evidence until a refused restoration settles, leave the
    /// planted object exactly as it found it, and stay usable afterwards.
    #[cfg(owntone_host)]
    fn exercise_pipe_substitution(substitution: PipeSubstitution, restore_fault: bool) {
        use crate::audio::airplay_output::ControllerHarness;
        use crate::local::resolver::ResolvedLocalMedia;

        gst::init().unwrap();
        let daemon = RecordingOwnedDaemon::start();
        let original_process = listener_process(&daemon.config.api_base)
            .unwrap()
            .identity();
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = async_channel::unbounded();
        let controller = ControllerHarness::new(
            runtime.handle().clone(),
            Arc::new(OwnToneSender {
                config: Ok(daemon.config.clone()),
            }),
            tx,
        )
        .with_device_id("aabbcc");
        let root = tempfile::tempdir().unwrap();
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let path = root.path().join("substitution.wav");
        write_startup_wav(&path, 30);
        let proxy = controller.proxy();
        let prepare = || {
            proxy
                .prepare_local(
                    ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
                        .unwrap(),
                )
                .unwrap()
        };
        let baseline_outputs = client.outputs().unwrap();
        let pipe = daemon.config.pipe_path.clone();

        // The foreign object: larger than one decoded PCM buffer, so a writer
        // that reached it would have changed bytes from offset zero.
        let sentinel = pipe.with_extension("sentinel");
        let sentinel_bytes = vec![0xA5u8; 256 * 1024];
        std::fs::write(&sentinel, &sentinel_bytes).unwrap();
        let foreign = match substitution {
            PipeSubstitution::RegularFile => pipe.clone(),
            PipeSubstitution::Symlink => sentinel.clone(),
        };
        let planted_as_expected = || assert_planted(&pipe, &sentinel, substitution);
        std::fs::write(
            pipe.with_extension("substitute"),
            match substitution {
                PipeSubstitution::RegularFile => "regular",
                PipeSubstitution::Symlink => "symlink",
            },
        )
        .unwrap();
        if restore_fault {
            std::fs::write(pipe.with_extension("park-restore"), "fail").unwrap();
        }

        let prepared = prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(43);
        controller.set_generation(generation);
        let started = Instant::now();
        controller.load(generation, prepared);
        assert!(started.elapsed() < Duration::from_millis(500));

        // The daemon applied the substitution while serving the initial volume
        // PUT: every takeover mutation has succeeded and the very next
        // production step is descriptor acquisition.
        wait_until(|| pipe.with_extension("substituted").exists());
        let recorded = daemon.recorded();
        assert!(recorded.contains("PUT /api/outputs/set "), "{recorded}");
        assert!(recorded.contains("PUT /api/queue/clear "), "{recorded}");
        assert!(
            recorded.contains("/api/player/volume?volume="),
            "{recorded}"
        );
        planted_as_expected();
        let competing = open_lock(&daemon.config.lock_path()).unwrap();
        let lock_is_free =
            || rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok();
        if restore_fault {
            // The refused writer unwinds through the takeover restoration
            // path. While the restoring stop is parked, every piece of
            // ownership evidence is retained.
            wait_until(|| pipe.with_extension("restore-seen").exists());
            assert_eq!(ticket.route_count(), 1, "restoring mutation retains media");
            assert!(daemon.config.takeover_record().exists());
            assert!(!lock_is_free());
            assert!(client.outputs().unwrap()[0].selected);
            std::fs::write(pipe.with_extension("restore-release"), "").unwrap();
            // The refused restoration forces quiescence, and the restarted
            // daemon cannot be verified as the dedicated instance while a
            // foreign object sits where its scanned FIFO must be. Recovery is
            // therefore retained — route custody, takeover record and
            // instance lock all held by a live recovery owner — and the load
            // fails truthfully instead of releasing ownership.
            wait_until_within(Duration::from_secs(90), || proxy.is_custodied(&ticket));
            wait_until(|| controller.state() == PlayerState::Stopped);
            assert!(daemon.config.takeover_record().exists());
            assert!(!lock_is_free());
            assert_eq!(ticket.route_count(), 1, "custodied route is retained");
        } else {
            wait_until(|| ticket.route_count() == 0);
            wait_until(lock_is_free);
            assert!(!daemon.config.takeover_record().exists());
            assert!(!proxy.is_custodied(&ticket));
            assert!(!proxy.has_custody_entries());
        }
        assert_failed_load_events(&rx, generation);
        assert_eq!(controller.state(), PlayerState::Stopped);

        // Not one byte reached the foreign object, the daemon never observed
        // PCM, and the adapter left the planted object exactly where it was.
        assert_eq!(std::fs::read(&foreign).unwrap(), sentinel_bytes);
        assert!(
            !daemon.recorded().contains("PCM received"),
            "{}",
            daemon.recorded()
        );
        planted_as_expected();

        // Only the operator (here, the test) removes the foreign object and
        // provisions the scanned FIFO again.
        std::fs::remove_file(&pipe).unwrap();
        ensure_pipe(&pipe).unwrap();
        if restore_fault {
            // The retained recovery settles on its own once the dedicated
            // instance can be verified again: quiescence restarts the daemon,
            // restoration succeeds, and only then are custody, route, record
            // and lock released. The original process was replaced.
            wait_until_within(Duration::from_secs(90), || ticket.route_count() == 0);
            wait_until(lock_is_free);
            assert!(!daemon.config.takeover_record().exists());
            assert!(!proxy.is_custodied(&ticket));
            assert!(!proxy.has_custody_entries());
            assert_ne!(
                listener_process(&daemon.config.api_base)
                    .unwrap()
                    .identity(),
                original_process
            );
        }
        assert_outputs_match(&client, &baseline_outputs);
        assert_eq!(client.player_state().unwrap(), "stop");
        drop(competing);

        // The daemon re-arms its FIFO reader on stop; then a fresh load must
        // play normally against the restored FIFO.
        client.player_control("stop").unwrap();
        while rx.try_recv().is_ok() {}
        assert_next_load_plays(&controller, &daemon, prepare(), generation.next());
        assert!(daemon.recorded().contains("PCM received"));
        assert_outputs_match(&client, &baseline_outputs);
        if matches!(substitution, PipeSubstitution::Symlink) {
            assert_eq!(std::fs::read(&sentinel).unwrap(), sentinel_bytes);
        }
    }

    #[cfg(owntone_host)]
    #[test]
    fn substituted_regular_file_at_the_pipe_path_is_refused_and_restored() {
        exercise_pipe_substitution(PipeSubstitution::RegularFile, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn substituted_symlink_at_the_pipe_path_is_refused_and_restored() {
        exercise_pipe_substitution(PipeSubstitution::Symlink, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn substituted_regular_file_retains_ownership_until_a_refused_restore_settles() {
        exercise_pipe_substitution(PipeSubstitution::RegularFile, true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn substituted_symlink_retains_ownership_until_a_refused_restore_settles() {
        exercise_pipe_substitution(PipeSubstitution::Symlink, true);
    }

    /// Shared production-controller setup against one owned recording daemon:
    /// a controller wired to the real OwnTone sender, an event channel, and a
    /// prepared protected local WAV of `seconds` seconds.
    #[cfg(owntone_host)]
    struct DaemonControllerFixture {
        daemon: RecordingOwnedDaemon,
        client: OwnToneClient,
        _runtime: tokio::runtime::Runtime,
        controller: crate::audio::airplay_output::ControllerHarness,
        rx: async_channel::Receiver<PlayerEvent>,
        root: tempfile::TempDir,
        marker: String,
        media: PathBuf,
    }

    #[cfg(owntone_host)]
    impl DaemonControllerFixture {
        fn start(seconds: u32) -> Self {
            use crate::audio::airplay_output::ControllerHarness;
            gst::init().unwrap();
            let daemon = RecordingOwnedDaemon::start();
            let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let (tx, rx) = async_channel::unbounded();
            let controller = ControllerHarness::new(
                runtime.handle().clone(),
                Arc::new(OwnToneSender {
                    config: Ok(daemon.config.clone()),
                }),
                tx,
            )
            .with_device_id("aabbcc");
            let root = tempfile::tempdir().unwrap();
            let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
            std::fs::write(
                root.path().join(".tributary-root-id"),
                format!("{marker}\n"),
            )
            .unwrap();
            let media = root.path().join("fixture.wav");
            write_startup_wav(&media, seconds);
            Self::with_media(daemon, client, runtime, controller, rx, root, marker, media)
        }

        /// A fixture whose media is `pcm_bytes` of PCM — small enough to sit
        /// entirely in the FIFO's buffer, so the pump reaches EOS and closes
        /// the writer whether or not the daemon is reading.
        fn start_brief(pcm_bytes: u32) -> Self {
            let fixture = Self::start(1);
            write_pcm_wav(&fixture.media, pcm_bytes);
            fixture
        }

        #[allow(clippy::too_many_arguments)]
        fn with_media(
            daemon: RecordingOwnedDaemon,
            client: OwnToneClient,
            runtime: tokio::runtime::Runtime,
            controller: crate::audio::airplay_output::ControllerHarness,
            rx: async_channel::Receiver<PlayerEvent>,
            root: tempfile::TempDir,
            marker: String,
            media: PathBuf,
        ) -> Self {
            Self {
                daemon,
                client,
                _runtime: runtime,
                controller,
                rx,
                root,
                marker,
                media,
            }
        }

        fn prepare(&self) -> crate::audio::gstreamer_media::PreparedGstreamerMedia {
            use crate::local::resolver::ResolvedLocalMedia;
            self.controller
                .proxy()
                .prepare_local(
                    ResolvedLocalMedia::from_authorized_path_for_test(
                        self.root.path(),
                        &self.marker,
                        &self.media,
                    )
                    .unwrap(),
                )
                .unwrap()
        }

        fn drain_events(&self) -> Vec<PlayerEvent> {
            std::iter::from_fn(|| self.rx.try_recv().ok()).collect()
        }

        fn lock_is_free(&self) -> bool {
            let competing = open_lock(&self.daemon.config.lock_path()).unwrap();
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok()
        }
    }

    /// R10: a pause accepted after the writer already closed. The item was
    /// autostarted when the drain wait began, so a stalled `play` was a
    /// fault; the pause/resume cycle loses the autostart binding while that
    /// wait is running, and the pinned daemon then never reports `stop`. The
    /// wait must notice the lost binding as it happens and complete the
    /// rendered item instead of timing out into an Error.
    #[cfg(owntone_host)]
    #[test]
    fn a_pause_after_the_writer_closed_still_completes_naturally() {
        // 40 KB of PCM fits the FIFO's buffer: the pump reaches EOS and closes
        // the writer even though the daemon is only reading its first chunk.
        let fixture = DaemonControllerFixture::start_brief(40 * 1024);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let stall = fixture.daemon.config.pipe_path.with_extension("stall");
        let baseline = client.outputs().unwrap();
        // The recording daemon reads one chunk (it must observe PCM to report
        // `play`), then holds off reading while the sentinel exists.
        std::fs::write(&stall, b"").unwrap();
        let prepared = fixture.prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(48);
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        wait_until(
            || matches!(client.player_progress(), Ok((ref s, Some(p))) if s == "play" && p > 0),
        );
        // Everything the pump had is now buffered in the FIFO and the writer
        // is closed; the drain wait is running against an autostarted item.
        std::thread::sleep(Duration::from_millis(500));

        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        assert_eq!(client.player_state().unwrap(), "pause");
        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        assert_eq!(client.player_state().unwrap(), "play");
        // Let the daemon read again: the pipe is dry and no longer
        // autostarted, so its progress stalls under `play`.
        std::fs::remove_file(&stall).unwrap();
        assert_single_natural_completion(&fixture, &ticket, generation, &baseline);
    }

    /// The item completes exactly once after a resume: one generation-correct
    /// `TrackEnded`, no `Error`, no play/pause after the terminal event, the
    /// outputs restored, no takeover record, a released route and a free lock.
    #[cfg(owntone_host)]
    fn assert_single_natural_completion(
        fixture: &DaemonControllerFixture,
        ticket: &GstreamerMediaTicket,
        generation: PlayerEventGeneration,
        baseline: &[OwnToneOutput],
    ) {
        let controller = &fixture.controller;
        let mut events = Vec::new();
        wait_until_within(Duration::from_secs(20), || {
            events.extend(fixture.drain_events());
            events
                .iter()
                .any(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
        });
        wait_until(|| ticket.route_count() == 0);
        wait_until(|| controller.state() == PlayerState::Stopped);
        events.extend(fixture.drain_events());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PlayerEvent::Error { .. })),
            "the item must complete, not fail: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
                .count(),
            1,
            "{events:?}"
        );
        assert!(
            events.iter().all(|event| event.generation() == generation),
            "{events:?}"
        );
        let ended = events
            .iter()
            .position(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
            .unwrap();
        assert!(
            !events[ended..].iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing | PlayerState::Paused,
                    ..
                }
            )),
            "no play/pause may follow the terminal TrackEnded: {events:?}"
        );
        assert_outputs_match(&fixture.client, baseline);
        assert!(!fixture.daemon.config.takeover_record().exists());
        assert!(fixture.lock_is_free());
    }

    /// R12: a pause/resume pair accepted entirely between two drain polls. The
    /// drain worker is parked after it processed one playing sample and before
    /// it captures the next request's epoch; the controller pauses (longer than
    /// the stall interval) and resumes while it is parked, with the daemon's
    /// progress unchanged; the park is released. The first fresh sample must
    /// not complete the item (no pre-pause or paused interval may count), and
    /// a fresh continuous-playing interval must then complete it exactly once.
    #[cfg(owntone_host)]
    fn exercise_control_transition_between_polls(already_eligible: bool) {
        let fixture = DaemonControllerFixture::start_brief(40 * 1024);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let pipe = &fixture.daemon.config.pipe_path;
        let baseline = client.outputs().unwrap();
        std::fs::write(pipe.with_extension("stall"), b"").unwrap();
        let prepared = fixture.prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(50);
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        wait_until(
            || matches!(client.player_progress(), Ok((ref s, Some(p))) if s == "play" && p > 0),
        );
        if already_eligible {
            // A prior pause/resume: the item is already no longer autostarted.
            controller.pause();
            wait_until(|| controller.state() == PlayerState::Paused);
            controller.play();
            wait_until(|| controller.state() == PlayerState::Playing);
        }
        // The writer closes once the pump reaches EOS; the drain wait then
        // processes at least one playing sample before parking.
        std::fs::write(pipe.with_extension("park-between"), b"").unwrap();
        wait_until(|| pipe.with_extension("between-parked").exists());

        // Two controls entirely between polls; the pause outlives the stall.
        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        std::thread::sleep(COMPLETION_PROGRESS_STALL + Duration::from_millis(200));
        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        assert_eq!(client.player_state().unwrap(), "play");
        let _ = fixture.drain_events();

        std::fs::write(pipe.with_extension("between-release"), b"").unwrap();
        std::thread::sleep(COMPLETION_PROGRESS_STALL / 2);
        let events = fixture.drain_events();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. }
                    | PlayerEvent::Error { .. }
                    | PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
            )),
            "the first sample after a between-poll pause/resume completed the item: {events:?}"
        );
        assert_eq!(controller.state(), PlayerState::Playing);
        assert!(fixture.daemon.config.takeover_record().exists());
        assert_eq!(ticket.route_count(), 1);
        // A fresh continuous-playing interval at unchanged progress completes.
        assert_single_natural_completion(&fixture, &ticket, generation, &baseline);
    }

    #[cfg(owntone_host)]
    #[test]
    fn a_first_pause_between_polls_never_borrows_the_pre_pause_stall() {
        exercise_control_transition_between_polls(false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn a_later_pause_between_polls_never_borrows_the_pre_pause_stall() {
        exercise_control_transition_between_polls(true);
    }

    /// R13 (pump path): activation confirmed daemon playback and published
    /// `Playing`; the pump is parked at the point where it used to republish
    /// that start; the command worker accepts a pause meanwhile. Releasing
    /// the pump must publish nothing over the accepted `Paused` (the
    /// republication is gone; this guards its return): the daemon stays paused,
    /// the controller cache stays paused, resume still works, and Stop settles
    /// with generation-correct events and no duplicate terminal event.
    #[cfg(owntone_host)]
    #[test]
    fn a_pause_accepted_before_the_pump_start_publication_wins() {
        let fixture = DaemonControllerFixture::start(30);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let pipe = &fixture.daemon.config.pipe_path;
        let baseline = client.outputs().unwrap();
        std::fs::write(pipe.with_extension("park-startup"), b"").unwrap();
        let prepared = fixture.prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(51);
        controller.set_generation(generation);
        controller.load(generation, prepared);
        // Activation published Playing; the pump is now parked before its
        // startup republication.
        wait_until(|| controller.state() == PlayerState::Playing);
        wait_until(|| pipe.with_extension("startup-parked").exists());
        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        assert_eq!(client.player_state().unwrap(), "pause");
        let _ = fixture.drain_events();

        std::fs::write(pipe.with_extension("startup-release"), b"").unwrap();
        wait_until(|| !pipe.with_extension("startup-parked").exists());
        std::thread::sleep(Duration::from_millis(400));
        let events = fixture.drain_events();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing,
                    ..
                }
            )),
            "the pump's late startup publication overwrote an accepted pause: {events:?}"
        );
        assert_eq!(controller.state(), PlayerState::Paused);
        assert_eq!(client.player_state().unwrap(), "pause");

        // Resume is still the user's, and the pipe resumes.
        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        assert_eq!(client.player_state().unwrap(), "play");
        controller.stop();
        wait_until(|| ticket.route_count() == 0);
        wait_until(|| controller.state() == PlayerState::Stopped);
        wait_until(|| !fixture.daemon.config.takeover_record().exists());
        wait_until(|| fixture.lock_is_free());
        let events = fixture.drain_events();
        assert!(
            events.iter().all(|event| event.generation() == generation),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
                ))
                .count(),
            1,
            "{events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. } | PlayerEvent::Error { .. }
            )),
            "{events:?}"
        );
        assert_outputs_match(client, &baseline);
    }

    /// R13 (sampler path): a position observation is parked in the daemon,
    /// a control is accepted while it is parked, and the stale reply is then
    /// released. The controller cache must never regress to the sampled
    /// state, so the next control the UI derives from the cache is right.
    /// `pause_then_stale_play`: parked under play, pause accepted, stale
    /// `play` released → still Paused, and a play then really resumes.
    /// Otherwise: parked under pause, resume accepted, stale `pause`
    /// released → still Playing, and a pause then really pauses.
    #[cfg(owntone_host)]
    fn exercise_stale_position_observation(pause_then_stale_play: bool) {
        let fixture = DaemonControllerFixture::start(30);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let pipe = &fixture.daemon.config.pipe_path;
        let prepared = fixture.prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(52);
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        if !pause_then_stale_play {
            controller.pause();
            wait_until(|| controller.state() == PlayerState::Paused);
            assert_eq!(client.player_state().unwrap(), "pause");
        }
        // Park the pump's next position sample inside the daemon. No test
        // request may touch /api/player while the park is armed.
        std::fs::write(pipe.with_extension("drain-started"), b"").unwrap();
        wait_until(|| pipe.with_extension("drain-seen").exists());
        if pause_then_stale_play {
            controller.pause();
            wait_until(|| controller.state() == PlayerState::Paused);
        } else {
            controller.play();
            wait_until(|| controller.state() == PlayerState::Playing);
        }
        let _ = fixture.drain_events();
        let stale = if pause_then_stale_play {
            "play"
        } else {
            "pause"
        };
        std::fs::write(
            pipe.with_extension("drain-release"),
            format!(r#"{{"state":"{stale}","item_progress_ms":1200}}"#),
        )
        .unwrap();
        wait_until(|| pipe.with_extension("drain-replied").exists());
        std::thread::sleep(Duration::from_millis(400));
        for suffix in [
            "drain-started",
            "drain-seen",
            "drain-release",
            "drain-replied",
        ] {
            let _ = std::fs::remove_file(pipe.with_extension(suffix));
        }
        let expected = if pause_then_stale_play {
            PlayerState::Paused
        } else {
            PlayerState::Playing
        };
        assert_eq!(
            controller.state(),
            expected,
            "a stale position observation overwrote the accepted control"
        );
        let events = fixture.drain_events();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PlayerEvent::StateChanged { .. })),
            "no state event may follow the stale observation: {events:?}"
        );
        // The next control the UI derives from the cache is the right one.
        let before = fixture.daemon.recorded();
        if pause_then_stale_play {
            controller.play();
            wait_until(|| controller.state() == PlayerState::Playing);
            assert_eq!(client.player_state().unwrap(), "play");
            let after = fixture.daemon.recorded();
            assert!(
                after.matches("PUT /api/player/play ").count()
                    > before.matches("PUT /api/player/play ").count(),
                "resume must transmit play"
            );
        } else {
            controller.pause();
            wait_until(|| controller.state() == PlayerState::Paused);
            assert_eq!(client.player_state().unwrap(), "pause");
        }
        controller.stop();
        wait_until(|| ticket.route_count() == 0);
        wait_until(|| controller.state() == PlayerState::Stopped);
        wait_until(|| fixture.lock_is_free());
    }

    #[cfg(owntone_host)]
    #[test]
    fn a_stale_playing_observation_never_undoes_an_accepted_pause() {
        exercise_stale_position_observation(true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn a_stale_paused_observation_never_undoes_an_accepted_resume() {
        exercise_stale_position_observation(false);
    }

    /// R11: a playing sample taken before a pause is history once the pause
    /// is accepted. The recording daemon parks the drain wait's next
    /// observation; the controller pauses while it is parked; the parked
    /// reply is then released as the stale `play` sample it is (same stalled
    /// progress as the samples before it). The session must stay paused with
    /// no completion and no restoration; after a resume and drain the item
    /// completes exactly once.
    #[cfg(owntone_host)]
    #[test]
    fn a_stale_playing_observation_never_completes_a_paused_item() {
        let fixture = DaemonControllerFixture::start_brief(40 * 1024);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let pipe = &fixture.daemon.config.pipe_path;
        let stall = pipe.with_extension("stall");
        let baseline = client.outputs().unwrap();
        std::fs::write(&stall, b"").unwrap();
        let prepared = fixture.prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(49);
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        wait_until(
            || matches!(client.player_progress(), Ok((ref s, Some(p))) if s == "play" && p > 0),
        );
        // The writer is closed and the drain wait has sampled `play` at the
        // stalled progress at least once.
        std::thread::sleep(Duration::from_millis(500));
        let (_, progress) = client.player_progress().unwrap();
        let progress = progress.expect("stalled progress");

        // Park the next observation, pause while it is parked, then release it
        // as the pre-pause sample it is.
        std::fs::write(pipe.with_extension("drain-started"), b"").unwrap();
        wait_until(|| pipe.with_extension("drain-seen").exists());
        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        // Release it only once a full stall interval has passed since the
        // fresh samples before it: combined with the eligibility the pause
        // created, this is exactly the sample that used to complete the item.
        std::thread::sleep(COMPLETION_PROGRESS_STALL + Duration::from_millis(300));
        std::fs::write(
            pipe.with_extension("drain-release"),
            format!(r#"{{"state":"play","item_progress_ms":{progress}}}"#),
        )
        .unwrap();
        wait_until(|| pipe.with_extension("drain-replied").exists());
        std::thread::sleep(COMPLETION_PROGRESS_STALL * 2);
        let events = fixture.drain_events();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. }
                    | PlayerEvent::Error { .. }
                    | PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
            )),
            "a stale playing sample completed a paused item: {events:?}"
        );
        assert_eq!(controller.state(), PlayerState::Paused);
        assert_eq!(client.player_state().unwrap(), "pause");
        assert!(fixture.daemon.config.takeover_record().exists());
        assert_eq!(ticket.route_count(), 1);

        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        std::fs::remove_file(&stall).unwrap();
        assert_single_natural_completion(&fixture, &ticket, generation, &baseline);
    }

    /// R14: a pause accepted during the post-EOF drain suspends the drain
    /// deadline. The recording daemon holds the drain (stall sentinel) so the
    /// wait keeps observing healthy `pause` samples for longer than
    /// [`DRAIN_DEADLINE`]. The session must stay paused — no Error, no
    /// Stopped, no TrackEnded, no restoration, custody/route/lock retained —
    /// and a later resume must still drain and settle exactly once.
    #[cfg(owntone_host)]
    fn exercise_healthy_pause_beyond_the_drain_deadline(
        active_before_pause: Duration,
        generation: PlayerEventGeneration,
    ) -> (
        DaemonControllerFixture,
        Arc<GstreamerMediaTicket>,
        Vec<OwnToneOutput>,
    ) {
        let fixture = DaemonControllerFixture::start_brief(40 * 1024);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let pipe = &fixture.daemon.config.pipe_path;
        let stall = pipe.with_extension("stall");
        let baseline = client.outputs().unwrap();
        std::fs::write(&stall, b"").unwrap();
        let prepared = fixture.prepare();
        let ticket = prepared.ticket().unwrap();
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        wait_until(
            || matches!(client.player_progress(), Ok((ref s, Some(p))) if s == "play" && p > 0),
        );
        // The writer is closed; the drain wait is running against an
        // autostarted item whose progress the held daemon leaves stalled.
        std::thread::sleep(active_before_pause);

        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        assert_eq!(client.player_state().unwrap(), "pause");
        // Leave it paused well past the point where an absolute deadline
        // taken at the start of the drain would have expired.
        std::thread::sleep(DRAIN_DEADLINE + Duration::from_millis(1500));

        let events = fixture.drain_events();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. }
                    | PlayerEvent::Error { .. }
                    | PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
            )),
            "a healthy pause was cut short by the drain deadline: {events:?}"
        );
        assert_eq!(controller.state(), PlayerState::Paused);
        assert_eq!(client.player_state().unwrap(), "pause");
        assert!(
            fixture.daemon.config.takeover_record().exists(),
            "restoration must not run while the item is paused"
        );
        // The media route (the ticket's custody) and the instance lock are
        // still held: nothing has settled.
        assert_eq!(ticket.route_count(), 1);
        assert!(!fixture.lock_is_free());
        let recorded = fixture.daemon.recorded();
        let paused = recorded.rfind("PUT /api/player/pause ").unwrap();
        assert!(
            !recorded[paused..].contains("PUT /api/player/stop "),
            "the daemon was stopped under a healthy pause: {recorded}"
        );
        (fixture, ticket, baseline)
    }

    /// R14: paused longer than the drain deadline, then resumed — the item
    /// drains and settles exactly once.
    #[cfg(owntone_host)]
    #[test]
    fn a_healthy_pause_outlives_the_drain_deadline_and_stays_resumable() {
        let generation = PlayerEventGeneration::from_raw(52);
        let (fixture, ticket, baseline) = exercise_healthy_pause_beyond_the_drain_deadline(
            Duration::from_millis(500),
            generation,
        );
        let controller = &fixture.controller;
        let client = &fixture.client;
        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        assert_eq!(client.player_state().unwrap(), "play");
        std::fs::remove_file(fixture.daemon.config.pipe_path.with_extension("stall")).unwrap();
        assert_single_natural_completion(&fixture, &ticket, generation, &baseline);
    }

    /// R14: a pause accepted just before the original absolute deadline
    /// would have expired, held across it, then resumed. The resumed tail
    /// gets a fresh active budget instead of the leftover the pause consumed.
    #[cfg(owntone_host)]
    #[test]
    fn a_pause_near_the_drain_deadline_then_resume_still_completes() {
        let generation = PlayerEventGeneration::from_raw(53);
        let (fixture, ticket, baseline) = exercise_healthy_pause_beyond_the_drain_deadline(
            DRAIN_DEADLINE.saturating_sub(Duration::from_millis(1500)),
            generation,
        );
        let controller = &fixture.controller;
        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        std::fs::remove_file(fixture.daemon.config.pipe_path.with_extension("stall")).unwrap();
        assert_single_natural_completion(&fixture, &ticket, generation, &baseline);
    }

    /// R14: Stop while paused past the drain deadline settles the session
    /// exactly once — one Stopped, no TrackEnded, no Error — and restores the
    /// daemon, releases the route, the takeover record and the lock.
    #[cfg(owntone_host)]
    #[test]
    fn a_stop_while_paused_past_the_drain_deadline_settles_without_completion() {
        let generation = PlayerEventGeneration::from_raw(54);
        let (fixture, ticket, baseline) = exercise_healthy_pause_beyond_the_drain_deadline(
            Duration::from_millis(500),
            generation,
        );
        let controller = &fixture.controller;
        let client = &fixture.client;
        controller.stop();
        wait_until(|| ticket.route_count() == 0);
        wait_until(|| controller.state() == PlayerState::Stopped);
        wait_until(|| !fixture.daemon.config.takeover_record().exists());
        wait_until(|| fixture.lock_is_free());
        let events = fixture.drain_events();
        assert!(
            events.iter().all(|event| event.generation() == generation),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
                ))
                .count(),
            1,
            "{events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. } | PlayerEvent::Error { .. }
            )),
            "{events:?}"
        );
        assert_eq!(client.player_state().unwrap(), "stop");
        assert!(!controller.proxy().has_custody_entries());
        assert_outputs_match(client, &baseline);
    }

    /// Pinned OwnTone autostops a pipe only while it is *autostarted*: a
    /// `player/pause` stops the pipe input and clears that binding, and the
    /// following `player/play` resumes the same item as a plain source that
    /// never emits EOF. The adapter must still complete the item exactly once
    /// after the pipe drains — by observing the daemon's stalled progress under
    /// `play` and issuing the restoring stop itself — instead of timing out
    /// into an Error.
    #[cfg(owntone_host)]
    #[test]
    fn a_resumed_pipe_completes_naturally_after_a_pause() {
        // Long enough that the pause lands mid-stream: the recording daemon
        // drains PCM faster than real time.
        let fixture = DaemonControllerFixture::start(12);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let prepared = fixture.prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(47);
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        assert_eq!(client.player_state().unwrap(), "pause");
        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        assert_eq!(client.player_state().unwrap(), "play");

        // The resumed item is no longer autostarted: the daemon never reports
        // `stop` on its own once the writer closes.
        let mut events = Vec::new();
        wait_until_within(Duration::from_secs(40), || {
            events.extend(fixture.drain_events());
            events
                .iter()
                .any(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
        });
        wait_until(|| ticket.route_count() == 0);
        wait_until(|| controller.state() == PlayerState::Stopped);
        events.extend(fixture.drain_events());
        assert!(
            events.iter().all(|event| event.generation() == generation),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
                .count(),
            1,
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PlayerEvent::Error { .. })),
            "{events:?}"
        );
        let ended = events
            .iter()
            .position(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
            .unwrap();
        assert!(
            !events[ended..].iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing | PlayerState::Paused,
                    ..
                }
            )),
            "{events:?}"
        );
        // The adapter's own restoring stop ended the daemon's playback after
        // the resume, and the prior output set was restored.
        let recorded = fixture.daemon.recorded();
        let resumed = recorded.rfind("PUT /api/player/play ").unwrap();
        assert!(
            recorded[resumed..].contains("PUT /api/player/stop "),
            "{recorded}"
        );
        wait_until(|| !fixture.daemon.config.takeover_record().exists());
        wait_until(|| fixture.lock_is_free());
        assert!(!client.outputs().unwrap()[0].selected);
        assert!(client.outputs().unwrap()[1].selected);
        assert_eq!(client.player_state().unwrap(), "stop");
        assert!(!controller.proxy().has_custody_entries());
    }

    /// Skipping to the next item on the same output replaces the session while
    /// the previous worker is still restoring the daemon and holding the
    /// instance lock. The replacement must wait for that sequential hand-over
    /// instead of failing as if a concurrent session held the lock.
    #[cfg(owntone_host)]
    #[test]
    fn back_to_back_loads_hand_over_the_instance_lock() {
        let fixture = DaemonControllerFixture::start(30);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let first = fixture.prepare();
        let first_ticket = first.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(53);
        controller.set_generation(generation);
        controller.load(generation, first);
        wait_until(|| controller.state() == PlayerState::Playing);
        assert!(!fixture.lock_is_free());
        fixture.drain_events();

        // Replace immediately: the previous worker has neither restored the
        // daemon nor released the lock yet.
        let second = fixture.prepare();
        let second_ticket = second.ticket().unwrap();
        let next = generation.next();
        controller.set_generation(next);
        let started = Instant::now();
        controller.load(next, second);
        assert!(started.elapsed() < Duration::from_millis(500));
        wait_until_within(Duration::from_secs(20), || {
            controller.state() == PlayerState::Playing && second_ticket.route_count() == 1
        });
        let events = fixture.drain_events();
        assert!(
            !events.iter().any(|event| {
                event.generation() == next && matches!(event, PlayerEvent::Error { .. })
            }),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| {
                event.generation() == next
                    && matches!(
                        event,
                        PlayerEvent::StateChanged {
                            state: PlayerState::Playing,
                            ..
                        }
                    )
            }),
            "{events:?}"
        );
        wait_until(|| first_ticket.route_count() == 0);
        assert!(fixture.daemon.config.takeover_record().exists());
        assert!(client.outputs().unwrap()[0].selected);
        controller.stop();
        wait_until(|| second_ticket.route_count() == 0);
        wait_until(|| !fixture.daemon.config.takeover_record().exists());
        wait_until(|| fixture.lock_is_free());
        assert!(!client.outputs().unwrap()[0].selected);
        assert!(client.outputs().unwrap()[1].selected);
        assert!(!controller.proxy().has_custody_entries());
    }

    /// A takeover record left behind by a crashed holder (the OS released its
    /// lock; the daemon is still taken over) must be recovered by the next
    /// opener under its own lock — quiesce, restore the recorded output set,
    /// remove the record — and only then admit the load. A recovery that
    /// cannot be completed refuses the load and leaves the record in place.
    #[cfg(owntone_host)]
    fn exercise_stale_record_recovery(recoverable: bool) {
        let fixture = DaemonControllerFixture::start(30);
        let controller = &fixture.controller;
        let client = &fixture.client;
        let pipe = fixture.daemon.config.pipe_path.clone();
        let original_process = listener_process(&fixture.daemon.config.api_base)
            .unwrap()
            .identity();
        // The dead holder's state: target selected, prior output disabled,
        // record on disk, no lock held.
        client.set_outputs(&[11_189_196]).unwrap();
        assert!(client.outputs().unwrap()[0].selected);
        assert!(!client.outputs().unwrap()[1].selected);
        TakeoverRecord {
            enabled_outputs: vec![42],
            selected_output: 11_189_196,
        }
        .write(&fixture.daemon.config.takeover_record())
        .unwrap();
        if !recoverable {
            std::fs::write(pipe.with_extension("park-restore"), "fail").unwrap();
        }
        let prepared = fixture.prepare();
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(59);
        controller.set_generation(generation);
        let started = Instant::now();
        controller.load(generation, prepared);
        assert!(started.elapsed() < Duration::from_millis(500));
        if recoverable {
            wait_until_within(Duration::from_secs(30), || {
                controller.state() == PlayerState::Playing
            });
            // Recovery quiesced the instance and restored the recorded set
            // before this load's own takeover.
            assert_ne!(
                listener_process(&fixture.daemon.config.api_base)
                    .unwrap()
                    .identity(),
                original_process
            );
            let events = fixture.drain_events();
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, PlayerEvent::Error { .. })),
                "{events:?}"
            );
            assert_eq!(ticket.route_count(), 1);
            controller.stop();
            wait_until(|| ticket.route_count() == 0);
            wait_until(|| !fixture.daemon.config.takeover_record().exists());
        } else {
            wait_until_within(Duration::from_secs(30), || {
                pipe.with_extension("restore-seen").exists()
            });
            std::fs::write(pipe.with_extension("restore-release"), "").unwrap();
            wait_until(|| controller.state() == PlayerState::Stopped);
            wait_until(|| ticket.route_count() == 0);
            let events = fixture.drain_events();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, PlayerEvent::Error { .. }))
                    .count(),
                1,
                "{events:?}"
            );
            assert!(
                !events.iter().any(|event| matches!(
                    event,
                    PlayerEvent::TrackEnded { .. }
                        | PlayerEvent::StateChanged {
                            state: PlayerState::Playing,
                            ..
                        }
                )),
                "{events:?}"
            );
            assert!(
                fixture.daemon.config.takeover_record().exists(),
                "an unrecoverable record stays for the next attempt"
            );
            assert!(
                !fixture.daemon.recorded().contains("PUT /api/queue/clear "),
                "no takeover happened behind an unrecovered record"
            );
        }
        wait_until(|| fixture.lock_is_free());
        assert!(!controller.proxy().is_custodied(&ticket));
        assert!(!controller.proxy().has_custody_entries());
        if recoverable {
            assert!(!client.outputs().unwrap()[0].selected);
            assert!(client.outputs().unwrap()[1].selected);
            assert_eq!(client.player_state().unwrap(), "stop");
        }
    }

    #[cfg(owntone_host)]
    #[test]
    fn a_stale_takeover_record_is_recovered_by_the_next_opener() {
        exercise_stale_record_recovery(true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn an_unrecoverable_stale_takeover_record_refuses_the_load() {
        exercise_stale_record_recovery(false);
    }

    /// Park after the real open, before the real worker activates the session.
    /// This allows Stop to win the shared first-effect gate deterministically.
    #[cfg(owntone_host)]
    struct ParkedOwnedSender {
        sender: OwnToneSender,
        opened: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    #[cfg(owntone_host)]
    impl AirplaySender for ParkedOwnedSender {
        fn name(&self) -> &'static str {
            "parked-owned"
        }
        fn probe(&self) -> Result<(), SenderError> {
            self.sender.probe()
        }
        fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome {
            let outcome = self.sender.open_session(ctx);
            self.opened.send(()).expect("notify open");
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .expect("release open");
            outcome
        }
    }

    #[cfg(owntone_host)]
    fn write_startup_wav(path: &Path, seconds: u32) {
        // Thirty seconds of valid stereo 44.1kHz s16 PCM, long enough to
        // observe live playback before driving Stop through the worker.
        write_pcm_wav(path, 44_100_u32 * 4 * seconds);
    }

    /// Valid stereo 44.1kHz s16 PCM of exactly `size` data bytes.
    fn write_pcm_wav(path: &Path, size: u32) {
        let mut wav = b"RIFF".to_vec();
        wav.extend((size + 36).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16_u32.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(2_u16.to_le_bytes());
        wav.extend(44_100_u32.to_le_bytes());
        wav.extend(176_400_u32.to_le_bytes());
        wav.extend(4_u16.to_le_bytes());
        wav.extend(16_u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(size.to_le_bytes());
        wav.resize(44 + size as usize, 1);
        std::fs::write(path, wav).unwrap();
    }

    #[cfg(owntone_host)]
    enum StartupCase {
        Play,
        StopBeforePcm,
        StopDuringAutostart,
        FailAutostart,
        NaturalEos,
        StalledTimeout,
        StalledStartupStop,
        StalledPlayingStop,
    }

    /// Drive one startup case to its expected worker outcome: observe queued
    /// PCM on a stalled reader, Stop during autostart, live playback, a
    /// refused/failed start, or natural completion.
    #[cfg(owntone_host)]
    #[allow(clippy::too_many_arguments)]
    fn await_startup_outcome(
        flags: StartupFlags,
        daemon: &RecordingOwnedDaemon,
        client: &OwnToneClient,
        gate: &Arc<SessionGate>,
        worker: &crate::audio::airplay_output::TestSessionWorker,
        ticket: &Arc<GstreamerMediaTicket>,
        fifo_observer: Option<&OwnedFd>,
        deadline: Instant,
    ) {
        let StartupFlags {
            stop_first,
            stop_during,
            stalled,
            fail_play,
            natural_eos,
        } = flags;
        if stalled {
            // The fixture has consumed only its first chunk and retains the
            // reader. Observe queued PCM, then give the decoder time to fill
            // the pipe. The independent oversized-buffer test proves the
            // partial-write/cancellation behavior without relying on timing.
            while rustix::io::ioctl_fionread(fifo_observer.unwrap()).unwrap() == 0
                || !daemon.recorded().contains("PCM received")
            {
                assert!(Instant::now() < deadline, "FIFO never received PCM");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(client.get_json("/api/player").unwrap()["pcm_bytes"], 4096);
        }
        if stop_during {
            while !daemon.recorded().contains("PCM received") {
                assert!(
                    Instant::now() < deadline,
                    "first PCM never reached the daemon"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let started = Instant::now();
            gate.stop();
            assert!(
                started.elapsed() < Duration::from_millis(100),
                "Stop waited for activation"
            );
        }
        if !stop_first && !fail_play {
            loop {
                let player = client.get_json("/api/player").unwrap();
                if worker.cached_state() == PlayerState::Playing
                    && player["state"] == "play"
                    && player["pcm_bytes"].as_u64().unwrap() > 0
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "real worker never played usable PCM"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(ticket.route_count(), 1);
        } else {
            while worker.cached_state() != PlayerState::Stopped {
                assert!(Instant::now() < deadline, "failed activation never stopped");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if natural_eos {
            while ticket.route_count() != 0 || worker.cached_state() != PlayerState::Stopped {
                assert!(
                    Instant::now() < deadline,
                    "finite PCM never completed in the live worker"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// What each [`StartupCase`] arranges and expects (a flag set by nature).
    #[cfg(owntone_host)]
    #[derive(Clone, Copy)]
    #[allow(clippy::struct_excessive_bools)]
    struct StartupFlags {
        stop_first: bool,
        stop_during: bool,
        stalled: bool,
        fail_play: bool,
        natural_eos: bool,
    }

    #[cfg(owntone_host)]
    impl StartupFlags {
        fn of(case: &StartupCase) -> Self {
            let stop_during = matches!(
                case,
                StartupCase::StopDuringAutostart | StartupCase::StalledStartupStop
            );
            Self {
                stop_first: matches!(case, StartupCase::StopBeforePcm),
                stop_during,
                stalled: matches!(
                    case,
                    StartupCase::StalledTimeout
                        | StartupCase::StalledStartupStop
                        | StartupCase::StalledPlayingStop
                ),
                fail_play: matches!(
                    case,
                    StartupCase::FailAutostart | StartupCase::StalledTimeout
                ) || stop_during,
                natural_eos: matches!(case, StartupCase::NaturalEos),
            }
        }
    }

    #[cfg(owntone_host)]
    fn exercise_empty_queue_start(case: StartupCase) {
        use crate::audio::airplay_output::spawn_test_session_worker;
        use crate::audio::airplay_sender::SenderTarget;
        use crate::local::resolver::ResolvedLocalMedia;
        use std::sync::atomic::{AtomicU64, AtomicU8};

        let flags = StartupFlags::of(&case);
        let StartupFlags {
            stop_first,
            stop_during: _,
            stalled,
            fail_play,
            natural_eos,
        } = flags;
        gst::init().expect("GStreamer");
        let daemon = RecordingOwnedDaemon::start();
        let client = OwnToneClient::new(&daemon.config.api_base).expect("client");
        assert!(!client.outputs().unwrap()[0].selected);
        client.clear_queue().expect("empty queue");
        assert!(
            client.player_control("play").is_err(),
            "ordinary play needs a queue item"
        );
        assert_eq!(client.player_state().unwrap(), "stop");
        assert_eq!(client.get_json("/api/player").unwrap()["pcm_bytes"], 0);
        // Merely opening a writer is not pipe autostart.
        let writer = open_pipe_write(
            &daemon.config.pipe_path,
            verify_pipe_identity(&daemon.config.pipe_path).unwrap(),
            Instant::now() + OPEN_DEADLINE,
            &OpenCancel::new(),
        );
        assert!(writer.is_ok());
        assert_eq!(client.player_state().unwrap(), "stop");
        drop(writer);
        std::fs::write(&daemon.requests, "").expect("reset request observations");

        let media_root = tempfile::tempdir().expect("media root");
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            media_root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let media_path = media_root.path().join("tone.wav");
        write_startup_wav(&media_path, if natural_eos { 1 } else { 30 });
        let media = ResolvedLocalMedia::from_authorized_path_for_test(
            media_root.path(),
            &marker,
            &media_path,
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let prepared = proxy.prepare_local(media).expect("protected usable audio");
        let ticket = prepared.ticket().expect("route ticket");
        let cancel = OpenCancel::new();
        let gate = Arc::new(SessionGate::new());
        let generation = PlayerEventGeneration::from_raw(71);
        let (tx, rx) = async_channel::unbounded();
        let registration = proxy.register_in_flight_cancel(71, prepared.generation(), &cancel);
        let ctx = SenderOpenContext {
            target: SenderTarget::new("Test", "127.0.0.1", 7000, Some("aabbcc".to_string())),
            prepared_uri: prepared.uri().to_string(),
            event_tx: tx,
            generation,
            media_proxy: Arc::clone(&proxy),
            media_ticket: prepared.ticket(),
            volume: 0.42,
            cancel,
            session_gate: Arc::clone(&gate),
            open_id: 71,
        };
        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let sender = Arc::new(ParkedOwnedSender {
            sender: OwnToneSender {
                config: Ok(daemon.config.clone()),
            },
            opened: opened_tx,
            release: Mutex::new(release_rx),
        });
        let mut worker = spawn_test_session_worker(
            sender,
            ctx,
            registration,
            Arc::new(AtomicU8::new(PlayerState::Buffering as u8)),
            Arc::new(Mutex::new(SenderPosition::unknown(generation))),
            Arc::new(AtomicU64::new(generation.as_raw())),
        );
        opened_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("real open");
        let competing = open_lock(&daemon.config.lock_path()).unwrap();
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());
        assert!(client.outputs().unwrap()[0].selected);
        assert_eq!(ticket.route_count(), 1);
        assert_eq!(client.player_state().unwrap(), "stop");
        assert_eq!(client.get_json("/api/player").unwrap()["pcm_bytes"], 0);
        assert!(!daemon.recorded().contains("/api/player/play"));
        if stop_first {
            gate.stop();
        }
        if fail_play {
            std::fs::write(daemon.config.pipe_path.with_extension("fail-play"), "fail").unwrap();
        }
        let fifo_observer = stalled.then(|| {
            rustix::fs::open(
                &daemon.config.pipe_path,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap()
        });
        if stalled {
            std::fs::write(daemon.config.pipe_path.with_extension("stall"), "stall").unwrap();
        }
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        await_startup_outcome(
            flags,
            &daemon,
            &client,
            &gate,
            &worker,
            &ticket,
            fifo_observer.as_ref(),
            deadline,
        );
        let (joined_tx, joined_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            worker.stop_and_join();
            joined_tx.send(worker).ok();
        });
        let worker = joined_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Stop must join even when the FIFO reader does not drain");
        while ticket.route_count() != 0 {
            assert!(
                Instant::now() < deadline,
                "restoration never released the route"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok());
        assert!(!daemon.config.takeover_record().exists());
        assert!(
            !client.outputs().unwrap()[0].selected,
            "prior output selection must be restored"
        );
        assert_eq!(worker.cached_state(), PlayerState::Stopped);
        assert_eq!(client.player_state().unwrap(), "stop");
        let recorded = daemon.recorded();
        assert!(recorded.contains("/api/player/volume?volume=42"));
        assert!(recorded.contains("/api/player/stop"));
        assert!(recorded.matches("/api/outputs/set").count() >= 2);
        assert_eq!(recorded.contains("PCM received"), !stop_first);
        assert_startup_events(&rx, generation, flags, &recorded, &client);
    }

    /// The startup fixture's event contract for one case (see the harness).
    #[cfg(owntone_host)]
    fn assert_startup_events(
        rx: &async_channel::Receiver<PlayerEvent>,
        generation: PlayerEventGeneration,
        flags: StartupFlags,
        recorded: &str,
        client: &OwnToneClient,
    ) {
        let StartupFlags {
            stop_first,
            stop_during,
            stalled: _,
            fail_play,
            natural_eos,
        } = flags;
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(events.iter().all(|event| event.generation() == generation));
        let has_error = events
            .iter()
            .any(|event| matches!(event, PlayerEvent::Error { .. }));
        if stop_first || stop_during {
            assert!(
                !has_error,
                "cancelled activation published Error: {events:?}"
            );
        } else if fail_play {
            assert!(has_error, "genuine activation failure must remain visible");
        }
        let playing = |event: &PlayerEvent| {
            matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing,
                    ..
                }
            )
        };
        if stop_first || fail_play {
            assert!(
                !events.iter().any(playing),
                "failed start published Playing: {events:?}"
            );
            if stop_first {
                assert_eq!(client.get_json("/api/player").unwrap()["pcm_bytes"], 0);
            }
            if stop_first {
                assert!(!recorded.contains("/api/player/play"));
            }
        } else {
            assert!(events.iter().any(playing));
            assert!(
                !recorded.contains("/api/player/play"),
                "first startup uses PCM autostart"
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, PlayerEvent::TrackEnded { .. }))
                .count(),
            usize::from(natural_eos),
            "only natural EOF may publish TrackEnded"
        );
        if natural_eos {
            let ended = events
                .iter()
                .position(|e| matches!(e, PlayerEvent::TrackEnded { .. }))
                .unwrap();
            assert!(events.iter().position(playing).unwrap() < ended);
            assert!(!events[ended..].iter().any(playing));
        }
        if let Some(stopped) = events.iter().position(|e| {
            matches!(
                e,
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                }
            )
        }) {
            assert!(
                !events[stopped..].iter().any(playing),
                "Playing followed terminal Stop"
            );
        }
    }

    #[cfg(owntone_host)]
    #[test]
    fn empty_queue_start_plays_scanned_pipe_through_real_open_and_worker() {
        exercise_empty_queue_start(StartupCase::Play);
    }

    #[cfg(owntone_host)]
    #[test]
    fn empty_queue_start_stop_wins_first_effect_and_restores() {
        exercise_empty_queue_start(StartupCase::StopBeforePcm);
    }

    #[cfg(owntone_host)]
    #[test]
    fn empty_queue_start_autostart_failure_never_publishes_playing_and_restores() {
        exercise_empty_queue_start(StartupCase::FailAutostart);
    }

    #[cfg(owntone_host)]
    #[test]
    fn empty_queue_start_finite_pcm_completes_once_through_live_worker() {
        exercise_empty_queue_start(StartupCase::NaturalEos);
    }

    #[cfg(owntone_host)]
    #[test]
    fn empty_queue_start_stop_during_autostart_settles_pcm_without_playing() {
        exercise_empty_queue_start(StartupCase::StopDuringAutostart);
    }

    #[cfg(owntone_host)]
    #[test]
    fn stalled_fifo_startup_timeout_restores_without_hanging() {
        exercise_empty_queue_start(StartupCase::StalledTimeout);
    }

    #[cfg(owntone_host)]
    #[test]
    fn stalled_fifo_stop_during_startup_restores_without_hanging() {
        exercise_empty_queue_start(StartupCase::StalledStartupStop);
    }

    #[cfg(owntone_host)]
    #[test]
    fn stalled_fifo_stop_after_playing_restores_without_hanging() {
        exercise_empty_queue_start(StartupCase::StalledPlayingStop);
    }

    /// AE1: real finite PCM enters the pump's EOS drain and parks its HTTP
    /// observation. Drive Stop/replacement through the production controller
    /// before releasing either completion or still-playing observations.
    #[cfg(owntone_host)]
    fn exercise_cancelled_eos_drain(replace: bool, response: &str) {
        use crate::audio::airplay_output::ControllerHarness;
        use crate::local::resolver::ResolvedLocalMedia;

        gst::init().unwrap();
        let daemon = RecordingOwnedDaemon::start();
        let next_daemon = replace.then(RecordingOwnedDaemon::start);
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = async_channel::unbounded();
        let mut controller = ControllerHarness::new(
            runtime.handle().clone(),
            Arc::new(OwnToneSender {
                config: Ok(daemon.config.clone()),
            }),
            tx,
        )
        .with_device_id("aabbcc");
        let root = tempfile::tempdir().unwrap();
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let prepare = |name: &str, seconds| {
            let path = root.path().join(name);
            write_startup_wav(&path, seconds);
            let media =
                ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
                    .unwrap();
            controller.proxy().prepare_local(media).unwrap()
        };
        let prepared = prepare("finite.wav", 1);
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(81);
        controller.set_generation(generation);
        std::fs::write(daemon.config.pipe_path.with_extension("park-drain"), "").unwrap();
        controller.load(generation, prepared);
        wait_until(|| {
            daemon
                .config
                .pipe_path
                .with_extension("drain-seen")
                .exists()
        });
        assert_eq!(ticket.route_count(), 1, "in-flight drain retains media");
        let competing = open_lock(&daemon.config.lock_path()).unwrap();
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());
        assert!(client.outputs().unwrap()[0].selected);
        let started = Instant::now();
        let next_ticket = if let Some(next) = next_daemon.as_ref() {
            let prepared = prepare("replacement.wav", 30);
            let next_ticket = prepared.ticket().unwrap();
            controller.set_sender(Arc::new(OwnToneSender {
                config: Ok(next.config.clone()),
            }));
            let next_generation = PlayerEventGeneration::from_raw(82);
            controller.set_generation(next_generation);
            controller.load(next_generation, prepared);
            Some(next_ticket)
        } else {
            controller.stop();
            None
        };
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "controller blocked on drain"
        );
        assert_eq!(
            ticket.route_count(),
            1,
            "cancellation must not release unsettled media"
        );
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());
        std::fs::write(
            daemon.config.pipe_path.with_extension("drain-release"),
            response,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while ticket.route_count() != 0
            || rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err()
        {
            assert!(
                Instant::now() < deadline,
                "cancelled drain did not settle promptly"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!daemon.config.takeover_record().exists());
        assert!(!client.outputs().unwrap()[0].selected);
        assert_eq!(client.player_state().unwrap(), "stop");
        assert!(!controller.proxy().is_custodied(&ticket));
        let recorded = daemon.recorded();
        assert!(recorded.contains("PCM received"));
        assert!(recorded.contains("/api/player/stop"));
        assert!(recorded.matches("/api/outputs/set").count() >= 2);
        if let Some(next_ticket) = next_ticket {
            wait_until(|| controller.state() == PlayerState::Playing);
            assert_eq!(next_ticket.route_count(), 1);
            controller.pause();
            wait_until(|| controller.state() == PlayerState::Paused);
            controller.play();
            wait_until(|| controller.state() == PlayerState::Playing);
            controller.stop();
            wait_until(|| next_ticket.route_count() == 0);
        }
        assert_eq!(controller.state(), PlayerState::Stopped);
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. } | PlayerEvent::Error { .. }
            )),
            "cancelled EOS published an outcome: {events:?}"
        );
        assert!(events.iter().all(|event| event.generation() == generation
            || (replace && event.generation() == PlayerEventGeneration::from_raw(82))));
        assert!(events.iter().any(|event| matches!(event, PlayerEvent::StateChanged { generation: g, state: PlayerState::Playing } if *g == generation)));
    }

    #[cfg(owntone_host)]
    fn exercise_cancelled_activation(resume: bool, replace: bool, fail: bool) {
        use crate::audio::airplay_output::ControllerHarness;
        use crate::local::resolver::ResolvedLocalMedia;

        gst::init().unwrap();
        let daemon = RecordingOwnedDaemon::start();
        let next_daemon = replace.then(RecordingOwnedDaemon::start);
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = async_channel::unbounded();
        let mut controller = ControllerHarness::new(
            runtime.handle().clone(),
            Arc::new(OwnToneSender {
                config: Ok(daemon.config.clone()),
            }),
            tx,
        )
        .with_device_id("aabbcc");
        let root = tempfile::tempdir().unwrap();
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let prepare = |name: &str, seconds| {
            let path = root.path().join(name);
            write_startup_wav(&path, seconds);
            let media =
                ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
                    .unwrap();
            controller.proxy().prepare_local(media).unwrap()
        };
        let prepared = prepare("activation.wav", 30);
        let ticket = prepared.ticket().unwrap();
        let generation = PlayerEventGeneration::from_raw(81);
        controller.set_generation(generation);
        if !resume {
            std::fs::write(
                daemon.config.pipe_path.with_extension("park-activation"),
                "start",
            )
            .unwrap();
        }
        controller.load(generation, prepared);
        if resume {
            wait_until(|| controller.state() == PlayerState::Playing);
            controller.pause();
            wait_until(|| controller.state() == PlayerState::Paused);
            std::fs::write(
                daemon.config.pipe_path.with_extension("park-activation"),
                "resume",
            )
            .unwrap();
            if fail {
                std::fs::write(daemon.config.pipe_path.with_extension("fail-play"), "").unwrap();
            }
            controller.play();
        }
        wait_until(|| {
            daemon
                .config
                .pipe_path
                .with_extension("activation-seen")
                .exists()
        });
        assert_eq!(
            ticket.route_count(),
            1,
            "in-flight activation retains media"
        );
        let competing = open_lock(&daemon.config.lock_path()).unwrap();
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());
        assert!(client.outputs().unwrap()[0].selected);
        let before_stop: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(!before_stop.iter().any(|event| matches!(
            event,
            PlayerEvent::Error { .. } | PlayerEvent::TrackEnded { .. }
        )));
        let started = Instant::now();
        let next_ticket = if let Some(next) = next_daemon.as_ref() {
            let prepared = prepare("replacement.wav", 30);
            let next_ticket = prepared.ticket().unwrap();
            controller.set_sender(Arc::new(OwnToneSender {
                config: Ok(next.config.clone()),
            }));
            let next_generation = PlayerEventGeneration::from_raw(82);
            controller.set_generation(next_generation);
            controller.load(next_generation, prepared);
            Some(next_ticket)
        } else {
            controller.stop();
            None
        };
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "controller blocked on activation"
        );
        assert_eq!(
            ticket.route_count(),
            1,
            "cancellation must not release unsettled media"
        );
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());
        std::fs::write(
            daemon.config.pipe_path.with_extension("activation-release"),
            "",
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while ticket.route_count() != 0
            || rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err()
        {
            assert!(
                Instant::now() < deadline,
                "cancelled activation did not settle promptly"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!daemon.config.takeover_record().exists());
        assert!(!client.outputs().unwrap()[0].selected);
        assert_eq!(client.player_state().unwrap(), "stop");
        assert!(!controller.proxy().is_custodied(&ticket));
        let recorded = daemon.recorded();
        assert!(recorded.contains("PCM received"));
        assert!(recorded.contains("/api/player/stop"));
        assert!(recorded.matches("/api/outputs/set").count() >= 2);
        if let Some(next_ticket) = next_ticket {
            wait_until(|| controller.state() == PlayerState::Playing);
            assert_eq!(next_ticket.route_count(), 1);
            controller.pause();
            wait_until(|| controller.state() == PlayerState::Paused);
            controller.play();
            wait_until(|| controller.state() == PlayerState::Playing);
            controller.stop();
            wait_until(|| next_ticket.route_count() == 0);
        }
        assert_eq!(controller.state(), PlayerState::Stopped);
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. } | PlayerEvent::Error { .. }
            )),
            "cancelled activation published an outcome: {events:?}"
        );
        assert!(events.iter().all(|event| event.generation() == generation
            || (replace && event.generation() == PlayerEventGeneration::from_raw(82))));
        assert!(!events.iter().any(|event| matches!(event, PlayerEvent::StateChanged { generation: g, state: PlayerState::Playing } if *g == generation)), "late Playing after cancellation: {events:?}");
    }

    /// AI1: the actual decoder/pump and command worker must settle without
    /// Stop, replacement, controller drop, or UI handling of terminal events.
    #[cfg(owntone_host)]
    fn exercise_pump_settlement(decode_error: bool, restore_fault: Option<bool>) {
        exercise_pump_settlement_with_control(decode_error, restore_fault, None);
    }

    #[derive(Clone, Copy)]
    #[cfg(owntone_host)]
    enum TerminalControl {
        Volume,
        Pause,
        Resume,
        StopAfterVolume,
    }

    #[cfg(owntone_host)]
    fn exercise_pump_settlement_with_control(
        decode_error: bool,
        restore_fault: Option<bool>,
        terminal_control: Option<TerminalControl>,
    ) {
        use crate::audio::airplay_output::ControllerHarness;
        use crate::local::resolver::ResolvedLocalMedia;

        gst::init().unwrap();
        let daemon = RecordingOwnedDaemon::start();
        let original_process = listener_process(&daemon.config.api_base)
            .unwrap()
            .identity();
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = async_channel::unbounded();
        let mut controller = ControllerHarness::new(
            runtime.handle().clone(),
            Arc::new(OwnToneSender {
                config: Ok(daemon.config.clone()),
            }),
            tx,
        )
        .with_device_id("aabbcc");
        let root = tempfile::tempdir().unwrap();
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let path = root.path().join("pump.wav");
        write_startup_wav(&path, if decode_error { 30 } else { 1 });
        let proxy = controller.proxy();
        let prepare = || {
            let media =
                ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
                    .unwrap();
            proxy.prepare_local(media).unwrap()
        };
        let prepared = prepare();
        let ticket = prepared.ticket().unwrap();
        let mut playback = crate::ui::playback::PlaybackSession::default();
        let direct = crate::ui::playback::QueueItem::direct_for_test(
            "https://radio.invalid/live".into(),
            "Radio".into(),
            String::new(),
            String::new(),
        );
        assert!(playback.replace_queue(vec![direct], 0));
        let generation = playback.current_event_generation();
        controller.set_generation(generation);
        let pipe = &daemon.config.pipe_path;
        // Park EOS before restoring so setup's own stop RPC cannot consume the
        // fault. Decoder-error cases keep streaming until we close the reader.
        if !decode_error {
            std::fs::write(pipe.with_extension("park-drain"), "").unwrap();
        }
        let started = Instant::now();
        controller.load(generation, prepared);
        assert!(started.elapsed() < Duration::from_millis(500));
        if decode_error {
            wait_until(|| controller.state() == PlayerState::Playing);
        } else {
            wait_until(|| pipe.with_extension("drain-seen").exists());
        }
        assert!(daemon.recorded().contains("PCM received"));
        let competing = open_lock(&daemon.config.lock_path()).unwrap();
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());
        assert_eq!(ticket.route_count(), 1);
        assert!(daemon.config.takeover_record().exists());
        assert!(client.outputs().unwrap()[0].selected);
        while rx.try_recv().is_ok() {}
        arm_pump_fault(
            pipe,
            decode_error,
            restore_fault,
            terminal_control.is_some(),
        );
        if restore_fault.is_some() || terminal_control.is_some() {
            wait_until(|| pipe.with_extension("restore-seen").exists());
            assert_restoration_parked(&ticket, &daemon, &competing, &client);
            if let Some(control) = terminal_control {
                drive_terminal_control(&mut controller, control, pipe);
            }
            if restore_fault != Some(true) {
                std::fs::write(pipe.with_extension("restore-release"), "").unwrap();
            }
            // Timeout is never released: only automatic worker quiescence can
            // kill that request and make releasing the instance lock safe.
        }
        if let Some(control) = terminal_control {
            release_parked_terminal(&controller, &daemon, &rx, &competing, control);
        }
        wait_until(|| ticket.route_count() == 0);
        wait_until(|| {
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok()
        });
        wait_until(|| controller.state() == PlayerState::Stopped);
        assert!(!daemon.config.takeover_record().exists());
        assert!(!controller.proxy().is_custodied(&ticket));
        assert!(!controller.proxy().has_custody_entries());
        assert!(!client.outputs().unwrap()[0].selected);
        assert!(client.outputs().unwrap()[1].selected);
        assert_eq!(client.player_state().unwrap(), "stop");
        assert_quiescence(&daemon, original_process, restore_fault.is_some());
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(events.iter().all(|event| event.generation() == generation));
        assert!(
            !playback.mark_resolved_load_failed(generation),
            "direct-source UI does not send Stop"
        );
        assert!(playback.accepts_event_generation(generation));
        let stopped = matches!(terminal_control, Some(TerminalControl::StopAfterVolume));
        let failed = decode_error || restore_fault.is_some();
        assert_settlement_events(&events, failed, stopped);
        assert_late_controls_refused(&controller, &daemon, &rx);
        drop(competing);
        for suffix in ["park-drain", "drain-started", "park-terminal"] {
            let _ = std::fs::remove_file(pipe.with_extension(suffix));
        }
        write_startup_wav(&path, 30);
        assert_next_load_plays_with_pause(&controller, prepare(), generation.next());
    }

    /// Arrange the pump fault for one settlement case: park terminal
    /// publication and/or the restoring stop as requested, then end the
    /// stream (decoder error via a broken pipe, or a released finite drain).
    #[cfg(owntone_host)]
    fn arm_pump_fault(
        pipe: &Path,
        decode_error: bool,
        restore_fault: Option<bool>,
        park_terminal: bool,
    ) {
        if park_terminal {
            std::fs::write(pipe.with_extension("park-terminal"), "").unwrap();
        }
        if restore_fault.is_some() || park_terminal {
            std::fs::write(
                pipe.with_extension("park-restore"),
                match restore_fault {
                    Some(true) => "timeout",
                    Some(false) => "fail",
                    None => "success",
                },
            )
            .unwrap();
        }
        if decode_error {
            std::fs::write(pipe.with_extension("break-pipe"), "").unwrap();
            wait_until(|| pipe.with_extension("pipe-broken").exists());
        } else {
            std::fs::write(pipe.with_extension("drain-release"), "stop").unwrap();
        }
    }

    /// While the restoring mutation is parked, every piece of ownership
    /// evidence is retained: route, record, lock and the taken-over output.
    #[cfg(owntone_host)]
    fn assert_restoration_parked(
        ticket: &Arc<GstreamerMediaTicket>,
        daemon: &RecordingOwnedDaemon,
        competing: &std::fs::File,
        client: &OwnToneClient,
    ) {
        assert_eq!(ticket.route_count(), 1, "restoring mutation retains media");
        assert!(daemon.config.takeover_record().exists());
        assert!(rustix::fs::flock(competing, FlockOperation::NonBlockingLockExclusive).is_err());
        assert!(client.outputs().unwrap()[0].selected);
    }

    /// An uncertain restore must have quiesced (restarted) the daemon; a clean
    /// one must not have.
    #[cfg(owntone_host)]
    fn assert_quiescence(
        daemon: &RecordingOwnedDaemon,
        original_process: ProcessIdentity,
        expect_restart: bool,
    ) {
        let final_process = listener_process(&daemon.config.api_base)
            .unwrap()
            .identity();
        if expect_restart {
            assert_ne!(
                final_process, original_process,
                "uncertain restore requires quiescence"
            );
        } else {
            assert_eq!(
                final_process, original_process,
                "clean restore needs no restart"
            );
        }
    }

    /// A finished worker refuses late controls: nothing is transmitted and
    /// nothing is published.
    #[cfg(owntone_host)]
    fn assert_late_controls_refused(
        controller: &crate::audio::airplay_output::ControllerHarness,
        daemon: &RecordingOwnedDaemon,
        rx: &async_channel::Receiver<PlayerEvent>,
    ) {
        let before_controls = daemon.recorded();
        controller.play();
        controller.pause();
        controller.play();
        assert_eq!(controller.state(), PlayerState::Stopped);
        assert_eq!(
            daemon.recorded(),
            before_controls,
            "finished worker refuses late controls"
        );
        assert!(rx.try_recv().is_err());
    }

    /// Issue one live control while restoration is parked and, for controls
    /// that transmit, wait until it is queued behind the settlement boundary.
    #[cfg(owntone_host)]
    fn drive_terminal_control(
        controller: &mut crate::audio::airplay_output::ControllerHarness,
        control: TerminalControl,
        pipe: &Path,
    ) {
        let started = Instant::now();
        match control {
            TerminalControl::Volume | TerminalControl::StopAfterVolume => {
                controller.set_volume(0.37);
            }
            TerminalControl::Pause => controller.pause(),
            TerminalControl::Resume => controller.play(),
        }
        assert!(started.elapsed() < Duration::from_millis(500));
        // Pause/volume block on restore's mutation boundary. Resume observes
        // running=false and is refused without an RPC.
        if !matches!(control, TerminalControl::Resume) {
            wait_until(|| pipe.with_extension("terminal-control-waiting").exists());
        }
    }

    /// With terminal publication parked: the refused control transmitted
    /// nothing, the lock is still held, nothing was published; then release.
    #[cfg(owntone_host)]
    fn release_parked_terminal(
        controller: &crate::audio::airplay_output::ControllerHarness,
        daemon: &RecordingOwnedDaemon,
        rx: &async_channel::Receiver<PlayerEvent>,
        competing: &std::fs::File,
        control: TerminalControl,
    ) {
        let pipe = &daemon.config.pipe_path;
        wait_until(|| pipe.with_extension("terminal-ready").exists());
        wait_until(|| pipe.with_extension("terminal-close-joining").exists());
        assert!(rx.try_recv().is_err(), "publication remains parked");
        assert!(rustix::fs::flock(competing, FlockOperation::NonBlockingLockExclusive).is_err());
        let requests = daemon.recorded();
        assert!(!requests.contains("PUT /api/player/pause"));
        assert!(!requests.contains("PUT /api/player/play"));
        assert!(!requests.contains("volume=37"));
        if matches!(control, TerminalControl::StopAfterVolume) {
            let started = Instant::now();
            controller.stop();
            assert!(started.elapsed() < Duration::from_millis(500));
        }
        std::fs::write(pipe.with_extension("terminal-release"), "").unwrap();
    }

    /// The settled session's events: an Error only for a genuine failure that
    /// no Stop suppressed, exactly one TrackEnded only for a clean completion,
    /// a terminal Stopped, and never a Playing/Paused.
    #[cfg(owntone_host)]
    fn assert_settlement_events(events: &[PlayerEvent], failed: bool, stopped: bool) {
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, PlayerEvent::Error { .. })),
            failed && !stopped,
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PlayerEvent::TrackEnded { .. }))
                .count(),
            usize::from(!failed && !stopped),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                }
            )),
            "{events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing | PlayerState::Paused,
                    ..
                }
            )),
            "{events:?}"
        );
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_decoder_error_settles_without_stop() {
        exercise_pump_settlement(true, None);
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_decoder_error_failed_restore_settles_without_stop() {
        exercise_pump_settlement(true, Some(false));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_decoder_error_timed_out_restore_settles_without_stop() {
        exercise_pump_settlement(true, Some(true));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_finite_eos_releases_lock_without_stop() {
        exercise_pump_settlement(false, None);
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_finite_eos_failed_restore_settles_without_stop() {
        exercise_pump_settlement(false, Some(false));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_finite_eos_timed_out_restore_settles_without_stop() {
        exercise_pump_settlement(false, Some(true));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_terminal_control_volume_completion() {
        exercise_pump_settlement_with_control(false, None, Some(TerminalControl::Volume));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_terminal_control_pause_completion() {
        exercise_pump_settlement_with_control(false, None, Some(TerminalControl::Pause));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_terminal_control_resume_completion() {
        exercise_pump_settlement_with_control(false, None, Some(TerminalControl::Resume));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_terminal_control_volume_restore_failure() {
        exercise_pump_settlement_with_control(false, Some(false), Some(TerminalControl::Volume));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_terminal_control_pause_restore_failure() {
        exercise_pump_settlement_with_control(false, Some(false), Some(TerminalControl::Pause));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_terminal_control_volume_stop_first() {
        exercise_pump_settlement_with_control(false, None, Some(TerminalControl::StopAfterVolume));
    }

    #[cfg(owntone_host)]
    #[test]
    fn pump_terminal_control_volume_failed_restore_stop_first() {
        exercise_pump_settlement_with_control(
            false,
            Some(false),
            Some(TerminalControl::StopAfterVolume),
        );
    }

    /// AG1: events are observed without any UI-generated Stop, matching direct
    /// radio's mark_resolved_load_failed=false semantics. Protected local PCM
    /// also lets us verify route custody through the actual controller.
    #[cfg(owntone_host)]
    fn exercise_live_resume_failure(timeout: bool) {
        use crate::audio::airplay_output::ControllerHarness;
        use crate::local::resolver::ResolvedLocalMedia;

        gst::init().unwrap();
        let daemon = RecordingOwnedDaemon::start();
        let original_process = listener_process(&daemon.config.api_base)
            .unwrap()
            .identity();
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = async_channel::unbounded();
        let controller = ControllerHarness::new(
            runtime.handle().clone(),
            Arc::new(OwnToneSender {
                config: Ok(daemon.config.clone()),
            }),
            tx,
        )
        .with_device_id("aabbcc");
        let root = tempfile::tempdir().unwrap();
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let path = root.path().join("resume.wav");
        write_startup_wav(&path, 30);
        let prepare = || {
            let media =
                ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
                    .unwrap();
            controller.proxy().prepare_local(media).unwrap()
        };
        let prepared = prepare();
        let ticket = prepared.ticket().unwrap();
        let mut playback = crate::ui::playback::PlaybackSession::default();
        let direct = crate::ui::playback::QueueItem::direct_for_test(
            "https://radio.invalid/live".into(),
            "Radio".into(),
            String::new(),
            String::new(),
        );
        assert!(playback.replace_queue(vec![direct], 0));
        let generation = playback.current_event_generation();
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        drain_through(&rx, PlayerState::Paused);
        let pipe = &daemon.config.pipe_path;
        std::fs::write(pipe.with_extension("park-activation"), "resume").unwrap();
        if !timeout {
            std::fs::write(pipe.with_extension("fail-play"), "").unwrap();
        }
        let started = Instant::now();
        controller.play();
        assert!(started.elapsed() < Duration::from_millis(500));
        wait_until(|| pipe.with_extension("activation-seen").exists());
        let competing = open_lock(&daemon.config.lock_path()).unwrap();
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());
        assert_eq!(ticket.route_count(), 1);
        assert!(daemon.config.takeover_record().exists());
        assert!(client.outputs().unwrap()[0].selected);
        if !timeout {
            std::fs::write(pipe.with_extension("activation-release"), "").unwrap();
        }
        // No Stop, replacement, controller drop, or event-driven UI cleanup.
        // The timeout case never releases the request: quiescence must kill it.
        // The route is released only at the end of the unwind (quiescence,
        // restart and restoration), so it waits within SETTLE_UNWIND_BOUND.
        wait_until_within(SETTLE_UNWIND_BOUND, || ticket.route_count() == 0);
        wait_until(|| {
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok()
        });
        wait_until(|| controller.state() == PlayerState::Stopped);
        assert!(!daemon.config.takeover_record().exists());
        assert!(!controller.proxy().is_custodied(&ticket));
        assert!(!controller.proxy().has_custody_entries());
        assert!(!client.outputs().unwrap()[0].selected);
        assert!(client.outputs().unwrap()[1].selected);
        assert_eq!(client.player_state().unwrap(), "stop");
        assert_ne!(
            listener_process(&daemon.config.api_base)
                .unwrap()
                .identity(),
            original_process,
            "failed mutation must be quiesced before releasing ownership"
        );
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(events.iter().all(|event| event.generation() == generation));
        assert!(
            !playback.mark_resolved_load_failed(generation),
            "direct-source Error does not ask the UI to stop the output"
        );
        assert!(playback.accepts_event_generation(generation));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PlayerEvent::Error { .. })),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Stopped,
                    ..
                }
            )),
            "{events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. }
                    | PlayerEvent::StateChanged {
                        state: PlayerState::Playing,
                        ..
                    }
            )),
            "{events:?}"
        );
        let plays = daemon.recorded().matches("PUT /api/player/play ").count();
        controller.play();
        controller.pause();
        controller.play();
        assert_eq!(controller.state(), PlayerState::Stopped);
        assert_eq!(
            daemon.recorded().matches("PUT /api/player/play ").count(),
            plays
        );
        assert!(
            rx.try_recv().is_err(),
            "terminal controls cannot publish events"
        );
        drop(competing);
        for suffix in ["fail-play", "park-activation"] {
            let _ = std::fs::remove_file(pipe.with_extension(suffix));
        }
        let prepared = prepare();
        let next_ticket = prepared.ticket().unwrap();
        controller.set_generation(generation.next());
        controller.load(generation.next(), prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        assert_eq!(next_ticket.route_count(), 1);
        controller.pause();
        wait_until(|| controller.state() == PlayerState::Paused);
        controller.play();
        wait_until(|| controller.state() == PlayerState::Playing);
        controller.stop();
        wait_until(|| next_ticket.route_count() == 0);
    }

    #[cfg(owntone_host)]
    fn exercise_live_control_failure(volume: bool, timeout: bool, cancel: bool) {
        use crate::audio::airplay_output::ControllerHarness;
        use crate::local::resolver::ResolvedLocalMedia;

        gst::init().unwrap();
        let daemon = RecordingOwnedDaemon::start();
        let original_process = listener_process(&daemon.config.api_base)
            .unwrap()
            .identity();
        let client = OwnToneClient::new(&daemon.config.api_base).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = async_channel::unbounded();
        let mut controller = ControllerHarness::new(
            runtime.handle().clone(),
            Arc::new(OwnToneSender {
                config: Ok(daemon.config.clone()),
            }),
            tx,
        )
        .with_device_id("aabbcc");
        let root = tempfile::tempdir().unwrap();
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .unwrap();
        let path = root.path().join("resume.wav");
        write_startup_wav(&path, 30);
        let prepare = |controller: &ControllerHarness| {
            let media =
                ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
                    .unwrap();
            controller.proxy().prepare_local(media).unwrap()
        };
        let prepared = prepare(&controller);
        let ticket = prepared.ticket().unwrap();
        let mut playback = crate::ui::playback::PlaybackSession::default();
        let direct = crate::ui::playback::QueueItem::direct_for_test(
            "https://radio.invalid/live".into(),
            "Radio".into(),
            String::new(),
            String::new(),
        );
        assert!(playback.replace_queue(vec![direct], 0));
        let generation = playback.current_event_generation();
        controller.set_generation(generation);
        controller.load(generation, prepared);
        wait_until(|| controller.state() == PlayerState::Playing);
        drain_through(&rx, PlayerState::Playing);
        let pipe = &daemon.config.pipe_path;
        std::fs::write(
            pipe.with_extension("park-control"),
            if volume { "volume" } else { "pause" },
        )
        .unwrap();
        if !timeout {
            std::fs::write(pipe.with_extension("fail-control"), "").unwrap();
        }
        let started = Instant::now();
        if volume {
            controller.set_volume(0.2);
        } else {
            controller.pause();
        }
        assert!(started.elapsed() < Duration::from_millis(500));
        wait_until(|| pipe.with_extension("control-seen").exists());
        let competing = open_lock(&daemon.config.lock_path()).unwrap();
        assert!(rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_err());
        assert_eq!(ticket.route_count(), 1);
        assert!(daemon.config.takeover_record().exists());
        assert!(client.outputs().unwrap()[0].selected);
        // Queue controls while the failing request owns the worker. Neither
        // may transmit, even after the real HTTP timeout releases the lock.
        let before = daemon.recorded();
        let plays_before = before.matches("PUT /api/player/play ").count();
        let pauses_before = before.matches("PUT /api/player/pause ").count();
        let volumes_before = before.matches("PUT /api/player/volume?").count();
        controller.play();
        controller.pause();
        controller.set_volume(0.8);
        if cancel {
            let started = Instant::now();
            controller.stop();
            assert!(started.elapsed() < Duration::from_millis(500));
        }
        if !timeout {
            std::fs::write(pipe.with_extension("control-release"), "").unwrap();
        }
        // Failure cases have no Stop, replacement, drop or UI cleanup.
        // Cancellation cases separately verify Stop-first silence.
        // The timeout case never releases the request: quiescence must kill it.
        // The route is released only at the end of the unwind (quiescence,
        // restart and restoration), so it waits within SETTLE_UNWIND_BOUND.
        wait_until_within(SETTLE_UNWIND_BOUND, || ticket.route_count() == 0);
        wait_until(|| {
            rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).is_ok()
        });
        wait_until(|| controller.state() == PlayerState::Stopped);
        assert!(!daemon.config.takeover_record().exists());
        assert!(!controller.proxy().is_custodied(&ticket));
        assert!(!controller.proxy().has_custody_entries());
        assert!(!client.outputs().unwrap()[0].selected);
        assert!(client.outputs().unwrap()[1].selected);
        assert_eq!(client.player_state().unwrap(), "stop");
        assert_ne!(
            listener_process(&daemon.config.api_base)
                .unwrap()
                .identity(),
            original_process,
            "failed mutation must be quiesced before releasing ownership"
        );
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(events.iter().all(|event| event.generation() == generation));
        assert!(
            !playback.mark_resolved_load_failed(generation),
            "direct-source Error does not ask the UI to stop the output"
        );
        assert!(playback.accepts_event_generation(generation));
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, PlayerEvent::Error { .. })),
            !cancel,
            "{events:?}"
        );
        if !cancel {
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    PlayerEvent::StateChanged {
                        state: PlayerState::Stopped,
                        ..
                    }
                )),
                "{events:?}"
            );
        }
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::TrackEnded { .. }
                    | PlayerEvent::StateChanged {
                        state: PlayerState::Playing | PlayerState::Paused,
                        ..
                    }
            )),
            "{events:?}"
        );
        let after = daemon.recorded();
        assert_eq!(after.matches("PUT /api/player/play ").count(), plays_before);
        assert_eq!(
            after.matches("PUT /api/player/pause ").count(),
            pauses_before
        );
        assert_eq!(
            after.matches("PUT /api/player/volume?").count(),
            volumes_before
        );
        let plays = daemon.recorded().matches("PUT /api/player/play ").count();
        controller.play();
        controller.pause();
        controller.play();
        assert_eq!(controller.state(), PlayerState::Stopped);
        assert_eq!(
            daemon.recorded().matches("PUT /api/player/play ").count(),
            plays
        );
        assert!(
            rx.try_recv().is_err(),
            "terminal controls cannot publish events"
        );
        drop(competing);
        for suffix in ["fail-control", "park-control"] {
            let _ = std::fs::remove_file(pipe.with_extension(suffix));
        }
        assert_next_load_plays_with_pause(&controller, prepare(&controller), generation.next());
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_pause_http_failure_settles() {
        exercise_live_control_failure(false, false, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_pause_timeout_settles() {
        exercise_live_control_failure(false, true, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_pause_stop_before_failure_settles() {
        exercise_live_control_failure(false, false, true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_pause_stop_before_timeout_settles() {
        exercise_live_control_failure(false, true, true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_volume_http_failure_settles() {
        exercise_live_control_failure(true, false, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_volume_timeout_settles() {
        exercise_live_control_failure(true, true, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_volume_stop_before_failure_settles() {
        exercise_live_control_failure(true, false, true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_volume_stop_before_timeout_settles() {
        exercise_live_control_failure(true, true, true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_resume_http_failure_settles_without_ui_stop() {
        exercise_live_resume_failure(false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn live_resume_timeout_settles_without_ui_stop() {
        exercise_live_resume_failure(true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn cancelled_activation_autostart_stop_is_silent() {
        exercise_cancelled_activation(false, false, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn cancelled_activation_autostart_replacement_is_silent() {
        exercise_cancelled_activation(false, true, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn cancelled_activation_resume_stop_is_silent() {
        exercise_cancelled_activation(true, false, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn cancelled_activation_resume_replacement_is_silent() {
        exercise_cancelled_activation(true, true, false);
    }

    #[cfg(owntone_host)]
    #[test]
    fn cancelled_activation_resume_failure_stop_is_silent() {
        exercise_cancelled_activation(true, false, true);
    }

    #[cfg(owntone_host)]
    #[test]
    fn eos_drain_stop_suppresses_completed_observation() {
        exercise_cancelled_eos_drain(false, "stop");
    }

    #[cfg(owntone_host)]
    #[test]
    fn eos_drain_stop_suppresses_playing_observation() {
        exercise_cancelled_eos_drain(false, "play");
    }

    #[cfg(owntone_host)]
    #[test]
    fn eos_drain_replacement_suppresses_completed_observation() {
        exercise_cancelled_eos_drain(true, "stop");
    }

    #[cfg(owntone_host)]
    #[test]
    fn eos_drain_replacement_suppresses_playing_observation() {
        exercise_cancelled_eos_drain(true, "play");
    }

    /// Y2: the initial volume is applied **before any activation** through the
    /// real `open()`, so a switch to OwnTone starts at the slider's level and
    /// the daemon never sees a `player/play` before the `player/volume` PUT.
    #[cfg(owntone_host)]
    #[test]
    fn the_initial_volume_is_applied_before_the_first_play_through_open() {
        gst::init().expect("GStreamer init");
        let daemon = RecordingOwnedDaemon::start();
        // `open()` assumes the availability gate created the FIFO; the direct
        // call must set it up the way `probe` would.
        ensure_pipe(&daemon.config.pipe_path).expect("create fifo");

        let media_path = daemon.config.state_dir.join("test.wav");
        write_startup_wav(&media_path, 30);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let (tx, _rx) = async_channel::unbounded();
        let generation = PlayerEventGeneration::from_raw(3);
        let cancel = OpenCancel::new();
        let ctx = SenderOpenContext {
            target: crate::audio::airplay_sender::SenderTarget::new(
                "Test",
                "127.0.0.1",
                7000,
                Some("aabbcc".to_string()),
            ),
            prepared_uri: url::Url::from_file_path(&media_path).unwrap().to_string(),
            event_tx: tx,
            generation,
            media_proxy: Arc::clone(&proxy),
            media_ticket: None,
            volume: 0.42,
            cancel: cancel.clone(),
            session_gate: Arc::new(SessionGate::new()),
            open_id: 1,
        };

        let outcome = open(daemon.config.clone(), &ctx);
        let session = match outcome {
            OpenOutcome::Opened(session) => session,
            OpenOutcome::Failed(error) => {
                panic!(
                    "open() failed against the recording daemon: {}",
                    error.message()
                )
            }
            OpenOutcome::Cancelled => panic!("open() was cancelled unexpectedly"),
        };

        let recorded = daemon.recorded();
        assert!(
            recorded.contains("PUT /api/player/volume?volume=42 HTTP/1.1"),
            "open() must apply the initial volume: {recorded:?}"
        );
        assert!(
            !recorded.contains("/api/player/play"),
            "no play may precede the initial volume: {recorded:?}"
        );

        let mut session = session;
        session.set_volume(0.73);
        assert!(daemon
            .recorded()
            .contains("PUT /api/player/volume?volume=73 HTTP/1.1"));

        assert!(session.resume(), "the accepted start must transmit");
        assert!(
            !daemon.recorded().contains("/api/player/play"),
            "initial start uses actual PCM"
        );
        assert!(
            OwnToneClient::new(&daemon.config.api_base)
                .unwrap()
                .get_json("/api/player")
                .unwrap()["pcm_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        session.pause();
        assert!(session.resume(), "live resume uses the populated queue");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !daemon.recorded().contains("/api/player/play") {
            assert!(
                Instant::now() < deadline,
                "the play must reach the daemon: {:?}",
                daemon.recorded()
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let recorded = daemon.recorded();
        let volume_at = recorded
            .find("/api/player/volume")
            .expect("volume request recorded");
        let play_at = recorded
            .find("/api/player/play")
            .expect("play request recorded");
        assert!(
            volume_at < play_at,
            "the initial volume must precede the first play: {recorded:?}"
        );

        session.close();
    }

    #[cfg(owntone_host)]
    #[test]
    fn volume_contract_rejects_body_only_and_propagates_http_failure() {
        let daemon = RecordingOwnedDaemon::start();
        let client = OwnToneClient::new(&daemon.config.api_base).expect("client");
        assert!(client
            .put_json("/api/player/volume", &serde_json::json!({"volume": 42}))
            .is_err());
        assert!(client.put("/api/player/volume?volume=invalid").is_err());
        assert!(client.set_volume(101).is_err());
        for percent in [0, 42, 73, 100] {
            client.set_volume(percent).expect("supported query volume");
            assert!(daemon
                .recorded()
                .contains(&format!("PUT /api/player/volume?volume={percent} HTTP/1.1")));
        }
    }

    /// Z1: a live session whose close fails restoration hands its route to real
    /// recovery custody. The recovery retains the **production** instance lock
    /// (`.tributary-lock`), so a replacement on the same instance fails closed
    /// exactly as production does; a replacement on a **separate** legitimate
    /// instance opens, plays, and stays usable across the old instance's
    /// settlement. Settlement releases only the old route and the old lock, and
    /// UI Stop never blocks on the failing restoration.
    #[cfg(owntone_host)]
    #[test]
    fn a_failed_live_close_is_replaced_on_a_separate_instance_without_losing_the_recovery_route() {
        use super::controller_regression::ControllerReplacementFixture;
        use crate::architecture::media::ResolvedHttpRequest;
        use crate::audio::airplay_output::ControllerHarness;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let fixture = ControllerReplacementFixture::start();
        // A second, independent legitimate instance (its own state dir, binary
        // binding, ownership record and instance lock) for the replacement.
        let separate = ControllerReplacementFixture::start();
        let (tx, rx) = async_channel::unbounded();
        let mut harness = ControllerHarness::new(runtime.handle().clone(), fixture.sender(), tx);

        // --- Live load #1, with a real protected-media route. ---
        let old_generation = PlayerEventGeneration::from_raw(1);
        harness.set_generation(old_generation);
        let old_prepared = harness.prepare(
            ResolvedHttpRequest::new(
                url::Url::parse("https://music.test/stream-a.flac").expect("url"),
            )
            .expect("resolved request"),
        );
        let old_ticket = old_prepared.ticket().expect("old protected ticket");
        harness.load(old_generation, old_prepared);
        wait_until(|| harness.state() == PlayerState::Playing);
        let proxy = harness.proxy();
        assert!(proxy.has_active_lease());
        assert_eq!(old_ticket.route_count(), 1);

        // --- UI Stop must not block on the failing restoration; the retained
        // recovery is handed to the regression at its boundary. ---
        fixture.arm_capture();
        let stopped = Instant::now();
        harness.stop();
        assert!(
            stopped.elapsed() < Duration::from_millis(500),
            "UI Stop must not block on the failing restoration: {:?}",
            stopped.elapsed()
        );

        wait_until(|| proxy.is_custodied(&old_ticket));
        assert_eq!(
            old_ticket.route_count(),
            1,
            "the old route must survive while recovery owns it"
        );
        let recovery = wait_for_captured_recovery();
        let old_locks = fixture.lock_paths();
        assert_eq!(
            old_locks,
            vec![fixture.lock_path()],
            "the live session must hold the production instance lock"
        );
        assert!(
            flock_is_held(&old_locks[0]),
            "the retained recovery must hold the old instance lock"
        );

        // --- Same-instance replacement must fail closed while recovery owns the
        // lock: this is the authority boundary production enforces. ---
        let same_generation = PlayerEventGeneration::from_raw(2);
        harness.set_generation(same_generation);
        let same_prepared = harness.prepare(
            ResolvedHttpRequest::new(
                url::Url::parse("https://music.test/stream-a-replacement.flac").expect("url"),
            )
            .expect("resolved request"),
        );
        let same_ticket = same_prepared
            .ticket()
            .expect("same-instance protected ticket");
        harness.load(same_generation, same_prepared);
        let refusal = wait_for_error_message(&rx, same_generation);
        assert!(
            refusal.contains("already using"),
            "the same-instance replacement must be refused for exclusivity: {refusal}"
        );
        assert_ne!(
            harness.state(),
            PlayerState::Playing,
            "a replacement production must reject must not become live"
        );
        assert!(
            !proxy.is_custodied(&same_ticket),
            "the refused load's route must not be retained in custody"
        );
        assert_eq!(
            same_ticket.route_count(),
            0,
            "the refused load's route must be released, not left live"
        );
        assert_eq!(
            old_ticket.route_count(),
            1,
            "the refused attempt must not disturb the recovery route"
        );
        assert!(
            flock_is_held(&old_locks[0]),
            "the refused attempt must not disturb the recovery's instance lock"
        );

        // --- Replacement load on a separate legitimate instance succeeds. ---
        harness.set_sender(separate.sender());
        let new_generation = PlayerEventGeneration::from_raw(3);
        harness.set_generation(new_generation);
        let new_prepared = harness.prepare(
            ResolvedHttpRequest::new(
                url::Url::parse("https://music.test/stream-b.flac").expect("url"),
            )
            .expect("resolved request"),
        );
        let new_ticket = new_prepared.ticket().expect("new protected ticket");
        harness.load(new_generation, new_prepared);
        wait_until(|| harness.state() == PlayerState::Playing);
        assert!(
            proxy.has_active_lease(),
            "the replacement route must be the active lease"
        );
        assert_eq!(
            new_ticket.route_count(),
            1,
            "the replacement route must be live"
        );
        assert!(!proxy.is_custodied(&new_ticket));
        assert!(
            proxy.is_custodied(&old_ticket),
            "the old route stays in recovery custody"
        );
        assert_eq!(old_ticket.route_count(), 1);
        let new_locks = separate.lock_paths();
        assert_eq!(
            new_locks,
            vec![separate.lock_path()],
            "the separate instance's session must hold its own instance lock"
        );
        assert_ne!(
            new_locks[0], old_locks[0],
            "the replacement must run on a distinct instance lock"
        );
        assert!(flock_is_held(&new_locks[0]));

        // --- Settlement releases only the old route and the old lock. ---
        fixture.allow_settlement();
        assert!(
            recovery.attempt(),
            "the retained recovery must settle once restoration can succeed"
        );
        drop(recovery);
        wait_until(|| !proxy.has_custody_entries());
        assert_eq!(
            old_ticket.route_count(),
            0,
            "settlement must shut down the old route"
        );
        assert!(
            flock_is_acquirable(&old_locks[0]),
            "settlement must release the old instance lock"
        );
        assert!(
            flock_is_held(&new_locks[0]),
            "settlement must not touch the replacement instance lock"
        );
        assert_eq!(
            new_ticket.route_count(),
            1,
            "settlement must not touch the replacement route"
        );

        // Observable usability across the old instance's settlement: the
        // replacement is still playing and accepts a real control.
        assert_eq!(
            harness.state(),
            PlayerState::Playing,
            "the replacement instance must remain usable after old-instance settlement"
        );
        harness.pause();
        wait_until(|| harness.state() == PlayerState::Paused);
        harness.play();
        wait_until(|| harness.state() == PlayerState::Playing);

        // Cleanup: let the replacement instance settle cleanly as well.
        separate.allow_settlement();
        harness.stop();
        wait_until(|| !proxy.has_active_lease());
    }

    /// Poll for the error event a failed load published for `generation`.
    fn wait_for_error_message(
        rx: &async_channel::Receiver<PlayerEvent>,
        generation: PlayerEventGeneration,
    ) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut observed = Vec::new();
        loop {
            while let Ok(event) = rx.try_recv() {
                if let PlayerEvent::Error {
                    generation: event_generation,
                    message,
                } = &event
                {
                    if *event_generation == generation {
                        return message.clone();
                    }
                }
                observed.push(event);
            }
            assert!(
                Instant::now() < deadline,
                "no error event for generation {generation:?}: {observed:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Poll for the retained recovery the failing close handed off.
    fn wait_for_captured_recovery() -> super::controller_regression::CapturedRetainedRecovery {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(recovery) =
                super::controller_regression::ControllerReplacementFixture::take_captured()
            {
                return recovery;
            }
            assert!(
                Instant::now() < deadline,
                "the failing close never handed off its retained recovery"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn flock_is_held(path: &Path) -> bool {
        let file = open_lock(path).expect("open competing lock");
        rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).is_err()
    }

    fn flock_is_acquirable(path: &Path) -> bool {
        let file = open_lock(path).expect("open competing lock");
        rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).is_ok()
    }
}

/// Test-only support for the controller-level failed-live-close/replacement
/// regression (review Z1). It owns a hermetic, Tributary-owned fake OwnTone
/// daemon whose restoring RPCs fail while a control file exists, a real
/// [`AirplaySender`] that builds one real [`OwnToneSession`] per open (with a
/// real advisory lock), and the hand-off of the retained recovery to the test
/// so settlement is driven at a chosen boundary.
#[cfg(test)]
pub(super) mod controller_regression {
    use super::*;

    /// A hermetic fake daemon that answers the adapter's HTTP contract, but
    /// returns `500` for the restoring `player/stop` and `outputs/set` while the
    /// path named by `TRIBUTARY_FAKE_FAIL_RESTORE` exists. Launched with
    /// `-c <state>/owntone.conf`, it satisfies the adapter's ownership binding so
    /// `quiesce_daemon` can genuinely terminate and restart it.
    const RESTORE_SWITCH_DAEMON_SOURCE: &str = r##"
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

fn main() {
    let addr = std::env::var("TRIBUTARY_FAKE_LISTEN").expect("listen address");
    let fail = std::env::var("TRIBUTARY_FAKE_FAIL_RESTORE").ok();
    let listener = TcpListener::bind(&addr).expect("bind");
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let fail = fail.clone();
        std::thread::spawn(move || serve(stream, fail));
    }
}

fn serve(stream: std::net::TcpStream, fail: Option<String>) {
    let Ok(reader_stream) = stream.try_clone() else { return };
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() { return; }
    let trimmed = request_line.trim().to_string();
    if trimmed.is_empty() { return; }
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() { break; }
        if let Some(value) = header
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(str::trim)
        {
            content_length = value.parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }
    let path = trimmed.split_whitespace().nth(1).unwrap_or_default().to_string();
    let failing = fail
        .as_deref()
        .is_some_and(|file| std::path::Path::new(file).exists())
        && matches!(path.as_str(), "/api/player/stop" | "/api/outputs/set");
    let (status, body) = if failing {
        ("500 Internal Server Error", "{\"error\":\"restoration refused\"}".to_string())
    } else {
        let body = match path.as_str() {
            "/api/config" => r#"{"version":"29.3"}"#.to_string(),
            "/api/outputs" => r#"{"outputs":[]}"#.to_string(),
            "/api/player" => r#"{"state":"stop"}"#.to_string(),
            _ => "{}".to_string(),
        };
        ("200 OK", body)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut stream = stream;
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
"##;

    /// Owns the fake daemon and the config that binds it for the duration of the
    /// regression. `Drop` stops whichever instance currently holds the endpoint
    /// (the recovery may have restarted it) and reaps the initial child.
    pub(super) struct ControllerReplacementFixture {
        api_base: String,
        state_dir: PathBuf,
        binary: PathBuf,
        fail_restore: PathBuf,
        lock_paths: Arc<Mutex<Vec<PathBuf>>>,
        child: Option<std::process::Child>,
        _directory: tempfile::TempDir,
    }

    impl Drop for ControllerReplacementFixture {
        fn drop(&mut self) {
            let config = OwnToneConfig {
                api_base: self.api_base.clone(),
                pipe_path: self.state_dir.join("airplay.pcm"),
                state_dir: self.state_dir.clone(),
                binary: self.binary.clone(),
            };
            if let Some(process) = listener_process(&self.api_base) {
                let _ = terminate_process(&process, &config, Duration::from_secs(5));
            }
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait();
            }
        }
    }

    impl ControllerReplacementFixture {
        /// Compile and start the daemon, bind it as the owned instance (record +
        /// restart command carrying the failing-restoration control file), and
        /// leave restoration failing until [`Self::allow_settlement`].
        pub(super) fn start() -> Self {
            use std::net::TcpListener;

            let directory = tempfile::tempdir().expect("tempdir");
            let state_dir = directory.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("state dir");
            let pipe = state_dir.join("airplay.pcm");
            // The dedicated-config authority check binds only a real FIFO at
            // the scanned pathname, so the fixture provisions one.
            ensure_pipe(&pipe).expect("create scanned FIFO");
            let config_path = state_dir.join(OWNTONE_CONFIG_FILE);
            std::fs::write(&config_path, dedicated_config::fixture(&pipe)).expect("write config");

            let source = directory.path().join("fake_restore_switch.rs");
            std::fs::write(&source, RESTORE_SWITCH_DAEMON_SOURCE).expect("write source");
            let binary = directory.path().join("fake_owntone");
            let compiled = std::process::Command::new("rustc")
                .arg("-O")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .expect("invoke rustc");
            assert!(compiled.success(), "the restore-switch daemon must compile");

            let probe = TcpListener::bind("127.0.0.1:0").expect("reserve port");
            let port = probe.local_addr().expect("addr").port();
            drop(probe);
            let api_base = format!("http://127.0.0.1:{port}");

            let fail_restore = state_dir.join("fail-restore");
            // Present from the start: the live close's restoration must fail so
            // the route is retained for recovery.
            std::fs::write(&fail_restore, b"fail").expect("write fail flag");

            let config = OwnToneConfig {
                api_base: api_base.clone(),
                pipe_path: pipe.clone(),
                state_dir: state_dir.clone(),
                binary: binary.clone(),
            };
            let restart = format!(
                "TRIBUTARY_FAKE_LISTEN=127.0.0.1:{port} TRIBUTARY_FAKE_FAIL_RESTORE={} {} -c {}",
                fail_restore.display(),
                binary.display(),
                config_path.display()
            );
            let record = OwnershipRecord {
                token: OWNER_TOKEN.to_string(),
                api_base: api_base.clone(),
                pipe_path: pipe.to_string_lossy().into_owned(),
                state_dir: state_dir.to_string_lossy().into_owned(),
                binary: binary.to_string_lossy().into_owned(),
                restart_command: Some(restart),
            };
            std::fs::write(
                config.owner_marker(),
                serde_json::to_vec(&record).expect("serialize record"),
            )
            .expect("write ownership record");

            let child = std::process::Command::new(&binary)
                .arg("-c")
                .arg(&config_path)
                .env("TRIBUTARY_FAKE_LISTEN", format!("127.0.0.1:{port}"))
                .env("TRIBUTARY_FAKE_FAIL_RESTORE", &fail_restore)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn restore-switch daemon");
            let deadline = Instant::now() + Duration::from_secs(5);
            while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                assert!(
                    Instant::now() < deadline,
                    "the restore-switch daemon never listened"
                );
                std::thread::sleep(Duration::from_millis(20));
            }

            Self {
                api_base,
                state_dir,
                binary,
                fail_restore,
                lock_paths: Arc::new(Mutex::new(Vec::new())),
                child: Some(child),
                _directory: directory,
            }
        }

        /// A real sender that builds one real `OwnToneSession` per open, each
        /// holding the **production** single-instance advisory lock
        /// ([`OwnToneConfig::lock_path`], the same `.tributary-lock` production
        /// `open` takes), recorded for the regression. A second session on this
        /// instance therefore fails closed exactly as production does.
        pub(super) fn sender(&self) -> Arc<dyn AirplaySender> {
            let config = OwnToneConfig {
                api_base: self.api_base.clone(),
                pipe_path: self.state_dir.join("airplay.pcm"),
                state_dir: self.state_dir.clone(),
                binary: self.binary.clone(),
            };
            Arc::new(ControllerSessionSender {
                config,
                lock_paths: Arc::clone(&self.lock_paths),
            })
        }

        /// Let restoration succeed from now on (the settlement boundary).
        pub(super) fn allow_settlement(&self) {
            match std::fs::remove_file(&self.fail_restore) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("could not clear the fail-restore flag: {error}"),
            }
        }

        /// The advisory lock files handed out so far, in open order.
        pub(super) fn lock_paths(&self) -> Vec<PathBuf> {
            self.lock_paths
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }

        /// The **production** single-instance lock path for this fixture
        /// (`state_dir/.tributary-lock`, exactly what production `open` takes).
        pub(super) fn lock_path(&self) -> PathBuf {
            OwnToneConfig {
                api_base: self.api_base.clone(),
                pipe_path: self.state_dir.join("airplay.pcm"),
                state_dir: self.state_dir.clone(),
                binary: self.binary.clone(),
            }
            .lock_path()
        }

        /// Capture the next serialized recovery instead of letting it run inline.
        pub(super) fn arm_capture(&self) {
            arm_retained_recovery_capture(&self.api_base);
        }

        /// Take the captured retained recovery, if the failing close has run.
        pub(super) fn take_captured() -> Option<CapturedRetainedRecovery> {
            take_captured_retained_recovery().map(|job| CapturedRetainedRecovery { job })
        }
    }

    /// A real sender that builds real sessions bound to the fixture daemon,
    /// taking the **production** instance lock (the single
    /// `state_dir/.tributary-lock` that production `open` uses) rather than a
    /// per-session test lock. The exclusivity semantics the regression relies on
    /// are therefore production's, not an artificial arrangement.
    struct ControllerSessionSender {
        config: OwnToneConfig,
        lock_paths: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl AirplaySender for ControllerSessionSender {
        fn name(&self) -> &'static str {
            "test-controller-session"
        }

        fn probe(&self) -> Result<(), SenderError> {
            Ok(())
        }

        fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome {
            let client = match OwnToneClient::new(&self.config.api_base) {
                Ok(client) => Arc::new(client),
                Err(error) => return OpenOutcome::Failed(error),
            };
            // Production instance exclusivity: the single `.tributary-lock` for
            // this instance, refused while another session (or the recovery that
            // retains it) owns it. This is the authority boundary a same-instance
            // replacement must respect, so the fixture cannot permit a
            // replacement production would reject.
            let lock_path = self.config.lock_path();
            let lock = match open_lock(&lock_path) {
                Ok(lock) => lock,
                Err(error) => return OpenOutcome::Failed(error),
            };
            if rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
                return OpenOutcome::Failed(unavailable(
                    "another_tributary_session_is_already_using_the_dedicated_daemon",
                ));
            }
            self.lock_paths
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(lock_path);

            let inner = SessionInner {
                client,
                config: self.config.clone(),
                generation: ctx.generation,
                event_tx: ctx.event_tx.clone(),
                recorded: TakeoverRecord {
                    enabled_outputs: vec![1],
                    selected_output: 0,
                },
                media_proxy: Arc::clone(&ctx.media_proxy),
                media_ticket: ctx.media_ticket.clone(),
                running: AtomicBool::new(true),
                activation: Mutex::new(ActivationState::default()),
                gate: Arc::clone(&ctx.session_gate),
                cancel: ctx.cancel.clone(),
                restored: AtomicBool::new(false),
                autostart_lost: AtomicBool::new(false),
                control_epoch: AtomicU64::new(0),
                terminal: AtomicBool::new(false),
                mutation_lock: Mutex::new(()),
                unsettled: AtomicUsize::new(0),
                position: Mutex::new(SenderPosition::unknown(ctx.generation)),
                state: Mutex::new(PlayerState::Buffering),
                pipeline: Mutex::new(None),
                probe: SessionProbe::default(),
            };
            OpenOutcome::Opened(Box::new(OwnToneSession {
                inner: Arc::new(inner),
                pump: None,
                lock: Some(lock),
            }))
        }
    }

    /// The real retained recovery the controller's failing close handed off,
    /// driven by the regression at its chosen settlement boundary.
    pub(super) struct CapturedRetainedRecovery {
        job: RetainedRecovery,
    }

    impl CapturedRetainedRecovery {
        /// One real settlement attempt: quiesce the owned daemon, restore it,
        /// and release the retained route by identity on success.
        pub(super) fn attempt(&self) -> bool {
            self.job.attempt()
        }
    }
}
