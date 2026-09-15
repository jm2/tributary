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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
        if is_loopback_host(host) && !is_literal_loopback_host(host) {
            // "localhost" resolves to *both* loopback families, so the process
            // the kernel match finds and the address the HTTP client actually
            // dials can be different listeners (review T5). Require the literal
            // address the client will use, so the observed family is the
            // endpoint's family by construction.
            return Err(unavailable(
                "the JSON API loopback host must be a literal address (127.0.0.1 or ::1), not an ambiguous name",
            ));
        }
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

    /// Read and validate the on-disk ownership record only: the token must be
    /// ours and the record must bind the *configured* endpoint, pipe, state
    /// directory and binary (review F5). This is the local, non-blocking half
    /// of the ownership gate, safe to run synchronously on the GTK caller.
    fn verify_owned_record(&self) -> Result<(), SenderError> {
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
fn listener_process(api_base: &str) -> Option<ListenerProcess> {
    let port = api_port(api_base)?;
    let addrs = api_host(api_base)
        .map(|host| expected_local_addrs(&host))
        .unwrap_or_default();
    if addrs.is_empty() {
        return None;
    }
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(table) {
            inodes.extend(listening_inodes(&text, port, &addrs));
        }
    }
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

/// The config directives that bind OwnTone's named-pipe input to a path. The
/// dedicated instance must read the same FIFO the adapter writes, so the
/// effective configuration is read and its pipe path compared to
/// [`OwnToneConfig::pipe_path`] (review U5).
const OWNTONE_PIPE_KEYS: &[&str] = &["pipe_path"];

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
    let Ok(text) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let expected = std::fs::canonicalize(pipe_path).unwrap_or_else(|_| pipe_path.to_path_buf());
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !OWNTONE_PIPE_KEYS
            .iter()
            .any(|candidate| key.trim().eq_ignore_ascii_case(candidate))
        {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if value.is_empty() {
            continue;
        }
        let candidate = Path::new(value);
        let candidate =
            std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
        if candidate == expected {
            return true;
        }
    }
    false
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
    let Some(process) = listener_process(&config.api_base) else {
        return Err(unavailable(
            "no process is bound to the configured dedicated-instance endpoint",
        ));
    };
    if !same_binary(&process.exe, &config.binary) {
        return Err(unavailable(
            "the process bound to the configured endpoint is not the dedicated owntone binary",
        ));
    }
    if !cmdline_binds_instance(&process.argv, &config.state_dir, &config.pipe_path) {
        return Err(unavailable(
            "the process bound to the configured endpoint is not the dedicated Tributary-owned instance",
        ));
    }
    Ok(())
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
        return Err(unavailable("the dedicated daemon is no longer running"));
    };
    if current.identity() != process.identity() {
        return Err(unavailable(
            "the dedicated daemon process identity changed before it could be signalled",
        ));
    }
    if !process_is_owned(&current, config) {
        return Err(unavailable(
            "the process bound to the configured endpoint is not the dedicated Tributary-owned instance",
        ));
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
            "the dedicated daemon process state could not be observed",
        )),
        ProcessObservation::Live(_) => {
            if process_start_time(identity.pid) == Some(identity.start_time) {
                Ok(true)
            } else {
                Err(unavailable(
                    "the dedicated daemon process identity changed before it could be signalled",
                ))
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
                return Err(unavailable("the dedicated daemon process id is invalid"));
            };
            match pidfd_open(pid, PidfdFlags::empty()) {
                Ok(pidfd) => {
                    // Prove the handle names the observed process, not a
                    // successor that reused the pid in the interval.
                    if process_start_time(identity.pid) != Some(identity.start_time) {
                        return Err(unavailable(
                            "the dedicated daemon process identity changed before it could be signalled",
                        ));
                    }
                    Ok(Some(Self { identity, pidfd }))
                }
                Err(rustix::io::Errno::SRCH) => Ok(None),
                Err(_) => Err(unavailable(
                    "a stable handle to the dedicated daemon could not be opened",
                )),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            match identity_still_ours(identity)? {
                false => Ok(None),
                true => Ok(Some(Self { identity })),
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
                Err(_) => Err(unavailable("the dedicated daemon could not be signalled")),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            use rustix::process::{kill_process, Pid};
            if !identity_still_ours(self.identity)? {
                return Ok(false);
            }
            let Some(pid) = Pid::from_raw(self.identity.pid as i32) else {
                return Err(unavailable("the dedicated daemon process id is invalid"));
            };
            match kill_process(pid, signal) {
                Ok(()) => Ok(true),
                Err(rustix::io::Errno::SRCH) => Ok(false),
                Err(_) => Err(unavailable("the dedicated daemon could not be signalled")),
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
                    "the dedicated daemon process state could not be observed",
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
                    "the dedicated daemon process state could not be observed",
                ));
            }
        }
        if Instant::now() >= kill_end {
            return Err(unavailable(
                "the dedicated daemon did not terminate after SIGKILL",
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
        .map(|_| ())
        .map_err(|_| unavailable("the dedicated daemon could not be restarted"))
}

/// Wait (bounded) until the configured endpoint is again served by an owned
/// instance after quiescence. The **same** still-live process that was just
/// terminated never satisfies the wait: its stable identity is excluded, so a
/// failed termination cannot be mistaken for a restart (review S3).
fn wait_for_owned_listener(
    config: &OwnToneConfig,
    deadline: Instant,
    previous: ProcessIdentity,
) -> Result<(), SenderError> {
    loop {
        if let Some(process) = listener_process(&config.api_base) {
            if process_is_owned(&process, config) && process.identity() != previous {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(unavailable(
                "the dedicated daemon did not come back after quiescence",
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
        return Err(unavailable(
            "the dedicated daemon is not running to quiesce",
        ));
    };
    // Full authority, not just the binary: a same-binary shared instance that
    // happens to hold the endpoint must never be terminated (review S2).
    if !process_is_owned(&process, config) {
        return Err(unavailable(
            "the process bound to the configured endpoint is not the dedicated Tributary-owned instance",
        ));
    }
    let previous = process.identity();
    terminate_process(&process, config, QUIESCE_TERMINATE_DEADLINE)?;
    if let Some(command) = config.restart_command() {
        spawn_restart_command(&command)?;
    }
    wait_for_owned_listener(config, Instant::now() + QUIESCE_RESTART_DEADLINE, previous)
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

/// The serialized activation/cancellation decision shared by the load path
/// (which accepts a current load) and teardown (which cancels the load). A
/// single mutex makes the two a defined boundary: an activation that loses the
/// race to a cancellation is refused *before* it transmits `player/play`, and
/// a cancellation that follows an accepted activation knows a play may have
/// been transmitted and must be covered by restoration (review S4).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ActivationState {
    /// The load path accepted this session; the decode pump may start.
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
    /// inert — no pipeline start, no PCM, no daemon play — until acceptance,
    /// and a cancellation that wins the boundary refuses a late acceptance
    /// (review R4, review S4).
    activation: Mutex<ActivationState>,
    /// The load path's Stop/start boundary (review U3). `activate_and_play`
    /// authorizes the `player/play` effect through this gate, and the load path
    /// stops it on teardown, so a Stop can never land between an acceptance
    /// check and the transmitted play.
    gate: Arc<SessionGate>,
    /// The load's cancellation currency, so the pump can abort its activation
    /// wait the moment the load is cancelled or replaced (review R4).
    cancel: OpenCancel,
    restored: AtomicBool,
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
    fn transmit_mutation<F>(&self, effect: F) -> Result<(), SenderError>
    where
        F: FnOnce() -> Result<(), SenderError>,
    {
        self.transmit_under_boundary(effect, None)
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
        self.transmit_under_boundary(effect, Some(on_success))
    }

    /// Shared body of [`Self::transmit_mutation`] and
    /// [`Self::transmit_mutation_publishing`]: raise the outstanding count
    /// **before** transmission, lower it only on confirmed success, and publish
    /// an optional control state before the boundary is released.
    fn transmit_under_boundary<F>(
        &self,
        effect: F,
        on_success: Option<PlayerState>,
    ) -> Result<(), SenderError>
    where
        F: FnOnce() -> Result<(), SenderError>,
    {
        let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.terminal.load(Ordering::SeqCst) {
            return Err(unavailable(
                "the AirPlay session is no longer accepting control",
            ));
        }
        self.unsettled.fetch_add(1, Ordering::SeqCst);
        match effect() {
            Ok(()) => {
                self.unsettled.fetch_sub(1, Ordering::SeqCst);
                if let Some(state) = on_success {
                    // Still unterminated: `restore` latches `terminal` only
                    // under this same lock, so it cannot have interleaved
                    // between the transmission and here (review X1).
                    self.publish_state(state);
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Publish the start state for a successful [`Self::activate_and_play`]
    /// under the settlement boundary, so a terminal transition can never
    /// interleave between the accepted start and its `Playing` publication
    /// (review X1). Returns `None` — publishing nothing — once the session is
    /// terminal, because no `Playing` may follow a terminal
    /// `Stopped`/`TrackEnded`.
    fn publish_start_if_live(&self) -> Option<PlayerState> {
        let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.terminal.load(Ordering::SeqCst) {
            return None;
        }
        self.publish_state(PlayerState::Playing);
        Some(PlayerState::Playing)
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
        publish(PlayerState::Playing);
        true
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
    fn publish_terminal(&self) {
        let _boundary = self.mutation_lock.lock().unwrap_or_else(|p| p.into_inner());
        self.terminal.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        self.publish_state(PlayerState::Stopped);
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

    /// Accept activation and transmit `player/play` as **one** serialized
    /// decision (review S4, review T3, review U3).
    ///
    /// The acceptance/cancellation mutex is held across the decision *and* the
    /// transmission. A Stop/replacement either wins the mutex first — setting
    /// `cancelled`, so no play is ever sent — or loses it, in which case the
    /// play completes and the teardown's restoration `player/stop` compensates
    /// it. There is no check-to-effect window: cancellation cannot land between
    /// a currency check and a separate play RPC.
    ///
    /// `accepted` is set only after the play RPC succeeds, so the inert decode
    /// pump is released only after a truthful, accepted start. A failed or
    /// cancelled play clears `accepted`, marks the boundary cancelled, and
    /// returns `false` — the caller never publishes `Playing`, and the pump
    /// returns without starting PCM (review U3).
    fn activate_and_play(&self) -> bool {
        let mut state = self.activation.lock().unwrap_or_else(|p| p.into_inner());
        // Re-check the load's cancellation currency inside the same lock a Stop
        // cancels through: a Stop that raced the worker's currentness check has
        // already cancelled this token, so the play is refused before it is
        // transmitted (review T3).
        if self.cancel.is_cancelled() {
            state.cancelled = true;
        }
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
                || self.client.player_control("play"),
                PlayerState::Playing,
            ) {
                Ok(()) => true,
                Err(error) => {
                    let _ = self.event_tx.try_send(PlayerEvent::error(
                        self.generation,
                        error.message().to_string(),
                    ));
                    false
                }
            }
        });
        if played {
            return true;
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
        return;
    }
    // Final serialized check before any pipeline start, PCM or daemon drive: a
    // cancellation that won the boundary after the wait must still leave the
    // pump inert, so an accepted-then-cancelled load never starts playback
    // (review S4).
    if !inner.activation_live() || inner.cancel.is_cancelled() {
        return;
    }

    let Some(bus) = pipeline.bus() else {
        // A pump that cannot run is terminal for the session: latch terminal
        // and publish `Stopped` together, so no control can publish after it
        // (review Y1).
        inner.publish_terminal();
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
        inner.publish_terminal();
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }
    // Publish under the settlement boundary and suppress it once the session is
    // terminal: a decode start must not report `Playing` after a terminal
    // `Stopped`/`TrackEnded` (review X1).
    let _ = inner.publish_start_if_live();

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
                    // Terminal failure: latch and publish together (review Y1).
                    inner.publish_terminal();
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

/// The daemon must report `stop` for a finite item to count as completed:
/// `pause` is a user-visible state, not a finished item (review F4). Kept as a
/// predicate so the pause-is-not-completion regression is unit-testable.
fn daemon_completion_reached(state: &str) -> bool {
    state == "stop"
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
            Ok(state) if daemon_completion_reached(&state) => break,
            Ok(_) => {}
            Err(_) => {
                let _ = inner.event_tx.try_send(PlayerEvent::error(
                    inner.generation,
                    "AirPlay completion could not be confirmed".to_string(),
                ));
                // Latch terminal and publish `Stopped` together, so no control
                // can successfully publish after this terminal state (review
                // Y1).
                inner.publish_terminal();
                let _ = inner.restore();
                return;
            }
        }
        if Instant::now() >= deadline {
            let _ = inner.event_tx.try_send(PlayerEvent::error(
                inner.generation,
                "AirPlay completion timed out".to_string(),
            ));
            // Latched with the publication: no control may follow it (review
            // Y1).
            inner.publish_terminal();
            let _ = inner.restore();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Do not publish completion after a failed restore: the daemon may still be
    // half-taken-over, and a clean `TrackEnded` would misreport it (review R3).
    if inner.restore().is_err() {
        let _ = inner.event_tx.try_send(PlayerEvent::error(
            inner.generation,
            "AirPlay restoration failed".to_string(),
        ));
        inner.publish_terminal();
        return;
    }
    // `restore` already latched terminal under the boundary; publish the
    // terminal `Stopped` through the same latch so the ordering holds even if
    // a later restore path re-enters (review Y1).
    inner.publish_terminal();
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
        // Transmit under the settlement boundary so a failed (or timed-out)
        // PUT is recorded as outstanding for terminal settlement, and a
        // concurrent restore cannot observe "settled" while this RPC is in
        // flight (review T1, review U1).
        if let Err(error) = self
            .inner
            .transmit_mutation(|| self.inner.client.set_volume(percent))
        {
            debug!(reason = %error.message(), "OwnTone volume change failed");
        }
    }

    fn pause(&mut self) {
        // A successful pause publishes `Paused` inside the settlement boundary,
        // so a terminal `Stopped`/`TrackEnded` can never be followed by a late
        // `Paused` (review X1).
        match self.inner.transmit_mutation_publishing(
            || self.inner.client.player_control("pause"),
            PlayerState::Paused,
        ) {
            Ok(()) => {}
            Err(error) => {
                debug!(reason = %error.message(), "OwnTone pause failed");
            }
        }
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
        // Win the shared Stop/start boundary and the serialized activation
        // boundary before tearing down: a close that races an accepted
        // activation must block any late `player/play` (review S4, review U3).
        this.inner.gate.stop();
        this.inner.cancel_activation();
        this.inner.running.store(false, Ordering::SeqCst);
        if let Some(pipeline) = this.inner.pipeline() {
            let _ = pipeline.set_state(gst::State::Null);
        }
        if let Some(handle) = this.pump {
            let _ = handle.join();
        }
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
                "this platform has no supported OwnTone acquisition path",
            ));
        }
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| unavailable("not configured"))?;
        // Record-only here: the kernel-verified process binding (review R5)
        // walks `/proc`, so it stays on the worker with the rest of the
        // non-local gate.
        config.verify_owned_record()?;
        if !config.binary.is_file() {
            return Err(unavailable("the owntone binary was not found"));
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

/// Confirm the dedicated daemon answers and is new enough. This is the
/// blocking half of the old availability gate, deliberately executed on the
/// load worker's [`open`] rather than in the synchronous, GTK-thread `probe`
/// (review R1). It is the first daemon RPC the worker performs and it never
/// reads or mutates receiver state.
fn check_daemon_health(client: &OwnToneClient) -> Result<(), SenderError> {
    let (major, _minor) = client.version()?;
    if major < OWNTONE_MIN_MAJOR {
        return Err(unavailable("the dedicated daemon is older than 29.x"));
    }
    Ok(())
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
        Ok(client) => Arc::new(client),
        Err(error) => return OpenOutcome::Failed(error),
    };
    // The route hand-off a recovery-pending outcome carries: the seat moves
    // this load's ticket into custody before constructing that outcome (review
    // S5).
    let custody = CustodyHandoff::from_ctx(ctx);
    if let Err(error) = config.verify_owned() {
        return OpenOutcome::Failed(error);
    }
    if ctx.cancel.is_cancelled() {
        return OpenOutcome::Cancelled;
    }

    // The blocking daemon handshake runs here, on the load worker, never on
    // the GTK caller (review R1). Reachability and version are the network I/O
    // the synchronous `probe` must not perform; running them before the lock
    // and before any receiver state read preserves the fail-closed ordering.
    if let Err(error) = check_daemon_health(&client) {
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
        return cancel_outcome(client, config, recorded, lock, unsettled, &custody);
    }
    if let Err(error) = client.set_outputs(&[selected]) {
        // A mutating RPC that returned an error may still have been applied
        // server-side, so this is not a clean unwind (review F3).
        unsettled = true;
        return fail_outcome(client, config, recorded, lock, unsettled, error, &custody);
    }
    if ctx.cancel.is_cancelled() {
        return cancel_outcome(client, config, recorded, lock, unsettled, &custody);
    }
    if let Err(error) = client.clear_queue() {
        unsettled = true;
        return fail_outcome(client, config, recorded, lock, unsettled, error, &custody);
    }
    if ctx.cancel.is_cancelled() {
        return cancel_outcome(client, config, recorded, lock, unsettled, &custody);
    }

    // Apply the user's current volume **before any activation**, so a switch
    // to OwnTone starts at the slider's level instead of the daemon's prior
    // value until the user moves it again (review S7). A failed volume RPC is
    // surfaced and unwound, never swallowed.
    if let Err(error) = client.set_volume(volume_percent(ctx.volume)) {
        // A failed (or timed-out) volume PUT may still be applied server-side,
        // so this is not a clean unwind (review T1).
        unsettled = true;
        return fail_outcome(client, config, recorded, lock, unsettled, error, &custody);
    }
    if ctx.cancel.is_cancelled() {
        return cancel_outcome(client, config, recorded, lock, unsettled, &custody);
    }

    let write_fd = match open_pipe_write(&config.pipe_path, deadline, &ctx.cancel) {
        Ok(fd) => fd,
        Err(CancelOrError::Cancelled) => {
            return cancel_outcome(client, config, recorded, lock, unsettled, &custody);
        }
        Err(CancelOrError::Failed(error)) => {
            return fail_outcome(client, config, recorded, lock, unsettled, error, &custody);
        }
    };
    if ctx.cancel.is_cancelled() {
        drop(write_fd);
        return cancel_outcome(client, config, recorded, lock, unsettled, &custody);
    }

    let pipeline = match build_pipeline(&ctx.prepared_uri, &write_fd) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            return fail_outcome(client, config, recorded, lock, unsettled, error, &custody);
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
        activation: Mutex::new(ActivationState::default()),
        gate: Arc::clone(&ctx.session_gate),
        cancel: ctx.cancel.clone(),
        restored: AtomicBool::new(false),
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
                &custody,
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
    let message = unavailable("recovery is pending for the dedicated daemon")
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

/// Build a fully-owned [`OwnToneSender`] for controller-path regressions: a
/// temp state directory carrying a matching ownership record, a created FIFO
/// and a real (dummy) binary file, pointed at `api_base`.
#[cfg(test)]
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
    OwnToneSender {
        config: Some(config),
    }
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
    #[cfg(target_os = "linux")]
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
        std::fs::write(&pipe, b"fifo").expect("write pipe placeholder");
        let config = state_dir.join(OWNTONE_CONFIG_FILE);
        std::fs::write(&config, format!("pipe_path = \"{}\"\n", pipe.display()))
            .expect("write config");
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
        std::fs::write(&unrelated, "pipe_path = \"/tmp/nope\"\n").expect("write unrelated");
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
        std::fs::write(&foreign, format!("pipe_path = \"{}\"\n", pipe.display()))
            .expect("foreign config");
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
        std::fs::write(&other_pipe, b"other").expect("other pipe");
        let other_config = other_dir.path().join(OWNTONE_CONFIG_FILE);
        std::fs::write(
            &other_config,
            format!("pipe_path = \"{}\"\n", other_pipe.display()),
        )
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
    #[cfg(target_os = "linux")]
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

    /// T6: on platforms without a Linux `/proc`, the kernel-side process
    /// binding is explicitly unsupported and fails closed — it never silently
    /// accepts a listener as the owned instance.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn process_resolution_is_unsupported_off_linux() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let port = listener.local_addr().expect("addr").port();
        assert!(
            listener_process(&format!("http://127.0.0.1:{port}")).is_none(),
            "no /proc enumeration exists off Linux, so no listener may resolve"
        );
    }

    /// R2/S3/T5: quiescence terminates the owned process (escalating to
    /// `SIGKILL`) and **confirms** exit before returning. Linux-only: the
    /// observation primitive reads `/proc` (review T6).
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
            .transmit_mutation(|| inner.client.set_volume(50))
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

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for condition");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

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
        wait_until(|| supervisor.pending_jobs() >= 1);
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
    #[cfg(target_os = "linux")]
    struct FakeOwnToneServer {
        api_base: String,
        requests: Arc<Mutex<Vec<String>>>,
        park: Arc<ParkGate>,
    }

    #[cfg(target_os = "linux")]
    struct ParkGate {
        marker: String,
        seen: Mutex<bool>,
        released: Mutex<bool>,
        cv: Condvar,
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
            control_inner.transmit_mutation(|| control_inner.client.set_volume(50))
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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

    /// X1: the start-state publication is taken under the settlement boundary.
    /// Before the terminal transition it is live and publishes `Playing`; after
    /// the transition it publishes nothing, because no `Playing` may follow the
    /// terminal state. A bare `terminal` check outside the boundary would leave
    /// a fresh check-to-effect race between the check and the publication.
    #[test]
    fn a_start_publication_after_the_terminal_transition_publishes_nothing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = async_channel::unbounded();
        let inner = test_session_inner_with_events(directory.path(), tx);

        assert_eq!(inner.publish_start_if_live(), Some(PlayerState::Playing));
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

        assert_eq!(inner.publish_start_if_live(), None);
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
    #[cfg(target_os = "linux")]
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
                None => OpenOutcome::Failed(unavailable("no prepared test session")),
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
    #[cfg(target_os = "linux")]
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
        worker.stop_and_join();

        assert_eq!(
            worker.cached_state(),
            PlayerState::Stopped,
            "the worker's own cache must be terminal after settlement"
        );

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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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

    /// X2/W2: a **failed start** leaves the production pump inert. The play RPC
    /// is refused, the pump's activation gate stays closed, and the real decode
    /// pipeline is never started: the FIFO reader observes EOF with zero bytes
    /// and no `Playing`/`TrackEnded` is ever published.
    #[cfg(target_os = "linux")]
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

    /// X2/W2: the **production** [`run_pump`] path drives a real decode
    /// pipeline into a real FIFO; the daemon observes the writer's EOF and
    /// reports completion, and exactly one `TrackEnded` is published after
    /// `Stopped`, with the daemon restored and the cached state terminal. The
    /// earlier fixtures called [`natural_completion`] directly with a pipeline
    /// already gone; this drives the real pump, the real pipe write end and the
    /// daemon drain.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_pump_publishes_completion_after_a_real_fifo_drain() {
        use std::io::Read;
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
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let api_base = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("addr").port()
        );
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(stream) = incoming else { continue };
                let drained = Arc::clone(&server_drained);
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
                            let state = if drained.load(Ordering::SeqCst) {
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
        });

        // The FIFO reader observes the drained writer and only then flips the
        // daemon's completion state — a real drain barrier, not a sleep.
        let reader_path = pipe_path.clone();
        let reader_drained = Arc::clone(&drained);
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
            reader_drained.store(true, Ordering::SeqCst);
            total
        });

        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(test_session_inner_at_base(&api_base, state_dir, tx));
        assert!(
            inner.activate_and_play(),
            "the accepted start must transmit"
        );

        let write_fd = open_pipe_write(
            &pipe_path,
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    struct FakeOwnedDaemon {
        _directory: tempfile::TempDir,
        config: OwnToneConfig,
        child: Option<std::process::Child>,
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    impl FakeOwnedDaemon {
        fn start() -> Self {
            use std::net::TcpListener;
            let directory = tempfile::tempdir().expect("tempdir");
            let state_dir = directory.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("state dir");
            let pipe = state_dir.join("airplay.pcm");
            let config_path = state_dir.join(OWNTONE_CONFIG_FILE);
            std::fs::write(
                &config_path,
                format!("pipe_path = \"{}\"\n", pipe.display()),
            )
            .expect("write config");

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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    const RECORDING_DAEMON_SOURCE: &str = r##"
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
        "/api/outputs" => r#"{"outputs":[{"id":"11189196","name":"Test","selected":true}]}"#.to_string(),
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

    /// A real, owned, request-recording fake daemon for the full `open()` path.
    #[cfg(target_os = "linux")]
    struct RecordingOwnedDaemon {
        _directory: tempfile::TempDir,
        config: OwnToneConfig,
        requests: PathBuf,
        child: Option<std::process::Child>,
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    impl RecordingOwnedDaemon {
        fn start() -> Self {
            use std::net::TcpListener;
            let directory = tempfile::tempdir().expect("tempdir");
            let state_dir = directory.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("state dir");
            let pipe = state_dir.join("airplay.pcm");
            let config_path = state_dir.join(OWNTONE_CONFIG_FILE);
            std::fs::write(
                &config_path,
                format!("pipe_path = \"{}\"\n", pipe.display()),
            )
            .expect("write config");

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

    /// Y2: the initial volume is applied **before any activation** through the
    /// real `open()`, so a switch to OwnTone starts at the slider's level and
    /// the daemon never sees a `player/play` before the `player/volume` PUT.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_initial_volume_is_applied_before_the_first_play_through_open() {
        use std::io::Read;

        gst::init().expect("GStreamer init");
        let daemon = RecordingOwnedDaemon::start();
        // `open()` assumes the availability gate created the FIFO; the direct
        // call must set it up the way `probe` would.
        ensure_pipe(&daemon.config.pipe_path).expect("create fifo");

        // A reader must hold the FIFO open for `open_pipe_write` to succeed.
        let pipe_path = daemon.config.pipe_path.clone();
        let reader = std::thread::spawn(move || {
            let mut fifo = std::fs::OpenOptions::new()
                .read(true)
                .open(&pipe_path)
                .expect("open fifo reader");
            let mut buffer = [0u8; 4096];
            loop {
                match fifo.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });

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
            prepared_uri: "file:///nonexistent/dummy.wav".to_string(),
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
            recorded.contains("/api/player/volume"),
            "open() must apply the initial volume: {recorded:?}"
        );
        assert!(
            !recorded.contains("/api/player/play"),
            "no play may precede the initial volume: {recorded:?}"
        );

        let mut session = session;
        assert!(session.resume(), "the accepted start must transmit");
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
        reader.join().expect("reader");
    }

    /// Z1: a live session whose close fails restoration must be replaced
    /// through the **real controller** without losing either route. The close
    /// hands the old route to real recovery custody; the old exact ticket
    /// survives only while recovery owns it; a replacement load through the
    /// same controller installs a usable route; and settlement releases only the
    /// old route and the old advisory lock. UI Stop never blocks on the failing
    /// restoration.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_live_close_is_replaced_without_losing_the_recovery_route() {
        use super::controller_regression::ControllerReplacementFixture;
        use crate::architecture::media::ResolvedHttpRequest;
        use crate::audio::airplay_output::ControllerHarness;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let fixture = ControllerReplacementFixture::start();
        let (tx, _rx) = async_channel::unbounded();
        let harness = ControllerHarness::new(runtime.handle().clone(), fixture.sender(), tx);

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
        assert_eq!(old_locks.len(), 1, "the live session held one real lock");
        assert!(
            flock_is_held(&old_locks[0]),
            "the retained recovery must hold the old advisory lock"
        );

        // --- Replacement load #2 while the retained recovery is outstanding. ---
        let new_generation = PlayerEventGeneration::from_raw(2);
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
            "the replacement route must be usable"
        );
        assert!(!proxy.is_custodied(&new_ticket));
        assert!(
            proxy.is_custodied(&old_ticket),
            "the old route stays in recovery custody"
        );
        assert_eq!(old_ticket.route_count(), 1);

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
            "settlement must release the old advisory lock"
        );
        assert!(
            proxy.has_active_lease(),
            "the replacement route must remain usable"
        );
        assert_eq!(
            new_ticket.route_count(),
            1,
            "settlement must not touch the replacement route"
        );

        harness.stop();
        wait_until(|| !proxy.has_active_lease());
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
            let config_path = state_dir.join(OWNTONE_CONFIG_FILE);
            std::fs::write(
                &config_path,
                format!("pipe_path = \"{}\"\n", pipe.display()),
            )
            .expect("write config");

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
        /// holding a real advisory lock recorded for the regression.
        pub(super) fn sender(&self) -> Arc<dyn AirplaySender> {
            let config = OwnToneConfig {
                api_base: self.api_base.clone(),
                pipe_path: self.state_dir.join("airplay.pcm"),
                state_dir: self.state_dir.clone(),
                binary: self.binary.clone(),
            };
            Arc::new(ControllerSessionSender {
                config,
                state_dir: self.state_dir.clone(),
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

        /// Capture the next serialized recovery instead of letting it run inline.
        pub(super) fn arm_capture(&self) {
            arm_retained_recovery_capture(&self.api_base);
        }

        /// Take the captured retained recovery, if the failing close has run.
        pub(super) fn take_captured() -> Option<CapturedRetainedRecovery> {
            take_captured_retained_recovery().map(|job| CapturedRetainedRecovery { job })
        }
    }

    /// A real sender that builds real sessions bound to the fixture daemon.
    struct ControllerSessionSender {
        config: OwnToneConfig,
        state_dir: PathBuf,
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
            let index = self
                .lock_paths
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len()
                + 1;
            let lock_path = self.state_dir.join(format!("session-{index}.lock"));
            let lock = match open_lock(&lock_path) {
                Ok(lock) => lock,
                Err(error) => return OpenOutcome::Failed(error),
            };
            if rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
                return OpenOutcome::Failed(unavailable("the test session lock is contested"));
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
