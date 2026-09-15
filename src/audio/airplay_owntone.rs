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
    cmdline: String,
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

/// `/proc/<pid>/cmdline` is NUL-separated; render it for substring binding.
fn render_cmdline(raw: &[u8]) -> String {
    raw.split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `stat` state letter of a live process (`Z` for a zombie), or `None`
/// when it no longer exists.
fn process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Skip the parenthesised `comm` (it can contain spaces and ')').
    let close = stat.rfind(')')?;
    stat[close + 1..].split_whitespace().next()?.chars().next()
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
    let cmdline = render_cmdline(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?);
    let start_time = process_start_time(pid)?;
    Some(ListenerProcess {
        pid,
        start_time,
        exe,
        cmdline,
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

/// `true` when an argument in `cmdline` names the dedicated instance's state
/// directory — the out-of-band configuration binding that distinguishes the
/// owned daemon from a foreign listener that merely holds the same port
/// (review R5, strengthened by S2).
///
/// Binding is **path-component exact**: an argument is accepted when it equals
/// the state directory or has it as a leading path prefix (`/state/owntone.conf`
/// under `/state`). A bare substring is rejected because a sibling directory
/// such as `/run/tributary-other` would otherwise match `/run/tributary`, which
/// is exactly the S2 collision.
fn cmdline_binds_state_dir(cmdline: &str, state_dir: &Path) -> bool {
    if state_dir.as_os_str().is_empty() {
        return false;
    }
    cmdline.split_whitespace().any(|arg| {
        let arg = Path::new(arg);
        arg == state_dir || arg.starts_with(state_dir)
    })
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
        && cmdline_binds_state_dir(&process.cmdline, &config.state_dir)
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
    if !cmdline_binds_state_dir(&process.cmdline, &config.state_dir) {
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

/// Signal `pid`, escalating to `SIGKILL` at `deadline`, and **confirm** the
/// process is gone (or a zombie). A signal error is reported rather than
/// ignored, and `SIGKILL` must be followed by observed exit: quiescence is
/// only established once the old process is actually gone (review S3).
fn signal_and_wait(pid: u32, deadline: Duration) -> Result<(), SenderError> {
    use rustix::io::Errno;
    use rustix::process::{kill_process, Pid, Signal};
    let Some(signal_pid) = Pid::from_raw(pid as i32) else {
        return Err(unavailable("the dedicated daemon process id is invalid"));
    };
    match kill_process(signal_pid, Signal::TERM) {
        Ok(()) => {}
        // Already gone: quiescence is satisfied.
        Err(Errno::SRCH) => return Ok(()),
        Err(_) => return Err(unavailable("the dedicated daemon could not be signalled")),
    }
    let end = Instant::now() + deadline;
    loop {
        match process_state(pid) {
            None | Some('Z') => return Ok(()),
            Some(_) => {}
        }
        if Instant::now() >= end {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    match kill_process(signal_pid, Signal::KILL) {
        Ok(()) => {}
        Err(Errno::SRCH) => return Ok(()),
        Err(_) => return Err(unavailable("the dedicated daemon could not be terminated")),
    }
    let kill_end = Instant::now() + QUIESCE_KILL_DEADLINE;
    loop {
        match process_state(pid) {
            None | Some('Z') => return Ok(()),
            Some(_) => {}
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
    signal_and_wait(process.pid, deadline)
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
    /// The load's cancellation currency, so the pump can abort its activation
    /// wait the moment the load is cancelled or replaced (review R4).
    cancel: OpenCancel,
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
    ///
    /// Returns the restoration outcome so callers cannot mistake a failed
    /// restore for a clean teardown: `Ok(())` once the record is cleared and
    /// the route revoked by identity, `Err` when the record and route are
    /// retained for serialized recovery (review R3).
    fn restore(&self) -> Result<(), SenderError> {
        if self.restored.load(Ordering::SeqCst) {
            return Ok(());
        }
        restore_daemon(&self.client, &self.config, &self.recorded)?;
        // Release this load's loopback route by identity only after the daemon
        // has been restored: the route stays valid for every request the
        // daemon might still be applying (§4.1, §4.3). The identity-bound
        // `take_and_release` is the single release primitive, so a ticket that
        // was moved into recovery custody during a superseded open is also
        // removed from custody here rather than stranded (review S5).
        if let Some(ticket) = self.media_ticket.as_ref() {
            self.media_proxy.take_and_release(ticket);
        }
        self.restored.store(true, Ordering::SeqCst);
        Ok(())
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

    /// Accept activation: release the decode pump to start playback. Returns
    /// `false` when a cancellation already won the serialized boundary, in
    /// which case the caller must not transmit `player/play` (review S4).
    fn activate(&self) -> bool {
        let mut state = self.activation.lock().unwrap_or_else(|p| p.into_inner());
        activation_decide(&mut state, self.running.load(Ordering::SeqCst))
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

/// Pure acceptance decision shared by [`SessionInner::activate`]: accept
/// activation only while the session is running and no cancellation has won
/// the serialized boundary (review S4). Split out so the boundary is
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
/// gate observes the same serialized boundary as [`SessionInner::activate`]
/// and [`SessionInner::cancel_activation`].
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
                inner.publish_state(PlayerState::Stopped);
                let _ = inner.restore();
                return;
            }
        }
        if Instant::now() >= deadline {
            let _ = inner.event_tx.try_send(PlayerEvent::error(
                inner.generation,
                "AirPlay completion timed out".to_string(),
            ));
            inner.publish_state(PlayerState::Stopped);
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
        inner.publish_state(PlayerState::Stopped);
        return;
    }
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
        // The first accepted resume is the activation that releases the inert
        // decode pump; the pump never starts playback on its own (review R4).
        // Acceptance and cancellation share one serialized boundary: if a
        // Stop/replacement won it, activation is refused and no `player/play`
        // is transmitted, so a stale daemon play can never follow a cancelled
        // load (review S4).
        if !self.inner.activate() {
            return;
        }
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
        // Win the serialized activation boundary before tearing down: a close
        // that races an accepted activation must block any late `player/play`
        // (review S4).
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
        cancel: ctx.cancel.clone(),
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
/// late restoring mutation cannot land after restoration.
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
        // A failed restoration step may itself have transmitted a `PUT` that is
        // still outstanding; retrying compensation cannot retract it, so
        // quiesce the daemon before the next attempt (review S3).
        let _ = quiesce_daemon(config);
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(RECOVERY_POLL);
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
fn spawn_serialized_recovery(
    client: Arc<OwnToneClient>,
    config: OwnToneConfig,
    recorded: TakeoverRecord,
    lock: std::fs::File,
    route: Option<(Arc<GstreamerMediaProxy>, Arc<GstreamerMediaTicket>)>,
) -> RecoveryCompletion {
    let completion = RecoveryCompletion::default();
    let worker = completion.clone();
    // The lock lives in a lease the spawned worker takes ownership of. On a
    // spawn failure (or an unquiescible recovery) the lease is retained by
    // leaking the descriptor, so the advisory lock stays held and no other
    // opener adopts a daemon whose recovery never ran (review S3).
    let lease = Arc::new(Mutex::new(Some(lock)));
    let worker_lease = Arc::clone(&lease);
    let spawned = std::thread::Builder::new()
        .name("airplay-owntone-recovery".to_string())
        .spawn(move || {
            // Hold the advisory lock until recovery is terminal so ownership is
            // never released early.
            let lock_guard = worker_lease
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            let deadline = Instant::now() + RECOVERY_DEADLINE;
            // Bounded quiescence precedes every restoration attempt: a
            // transmitted mutating RPC cannot be retracted by releasing the OS
            // lock, so terminate/restart the owned instance so no old-generation
            // mutation can land after restoration (review R2).
            let mut quiesced = quiesce_daemon(&config).is_ok();
            loop {
                if quiesced && restore_daemon(&client, &config, &recorded).is_ok() {
                    if let Some((proxy, ticket)) = route.as_ref() {
                        proxy.take_and_release(ticket);
                    }
                    worker.resolve(RecoveryOutcome::Restored);
                    return;
                }
                if Instant::now() >= deadline {
                    if quiesced {
                        // Quiescence was established, so no old-generation
                        // mutation can survive; the record stays for the
                        // supervisor and the route is released by the load path.
                        worker.resolve(RecoveryOutcome::RestorationFailed {
                            message: "restoration did not complete before the recovery deadline"
                                .to_string(),
                        });
                    } else {
                        // Quiescence was never established: an outstanding
                        // mutation may still land. Retain the lock and the route
                        // rather than claiming a failed-but-released outcome.
                        if let Some(lock) = lock_guard {
                            std::mem::forget(lock);
                        }
                        worker.resolve(RecoveryOutcome::Retained {
                            message: "quiescence was not established before the recovery deadline"
                                .to_string(),
                        });
                    }
                    return;
                }
                std::thread::sleep(RECOVERY_POLL);
                quiesced = quiesce_daemon(&config).is_ok();
            }
        });
    if spawned.is_err() {
        // No executable recovery owner exists. Retain the lock (leak its
        // descriptor so the flock stays held for the process) and the route for
        // the supervisor, and report a terminal retained outcome rather than
        // pretending the failed recovery released ownership (review S3).
        let retained_lock = lease.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(lock) = retained_lock {
            std::mem::forget(lock);
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
    /// this test process.
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

    /// S2: the command-line binding is path-component exact, not a substring.
    /// A sibling state directory that merely has the configured one as a
    /// string prefix must not match.
    #[test]
    fn cmdline_binding_requires_the_exact_state_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path();
        let cmdline = format!("/usr/bin/owntone -c {}/owntone.conf", state_dir.display());
        assert!(cmdline_binds_state_dir(&cmdline, state_dir));
        assert!(!cmdline_binds_state_dir(
            "/usr/bin/owntone -c /etc/owntone.conf",
            state_dir
        ));
        assert!(!cmdline_binds_state_dir("", state_dir));

        // A sibling directory sharing the name as a bare string prefix is not
        // the configured one (the S2 substring collision).
        let sibling = format!("{}-other", state_dir.display());
        let cmdline = format!("/usr/bin/owntone -c {sibling}/owntone.conf");
        assert!(!cmdline_binds_state_dir(&cmdline, state_dir));
    }

    /// R5: a matching ownership record is not enough — a foreign process bound
    /// to the configured endpoint is refused.
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

    /// R2/S3: quiescence terminates the owned process (escalating to
    /// `SIGKILL`) and **confirms** exit before returning.
    #[test]
    fn signal_and_wait_stops_a_child_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(signal_and_wait(pid, Duration::from_secs(5)).is_ok());
        // The child is gone or a zombie awaiting our reap — never still
        // running.
        assert!(matches!(process_state(pid), None | Some('Z')));
        let _ = child.wait();
    }

    /// S3: a pid that no longer exists is already quiesced (ESRCH), not an
    /// error that could mask a live process.
    #[test]
    fn signal_and_wait_treats_a_gone_process_as_quiesced() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        assert!(signal_and_wait(pid, Duration::from_secs(1)).is_ok());
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
            cancel: OpenCancel::new(),
            restored: AtomicBool::new(false),
            position: Mutex::new(SenderPosition::unknown(PlayerEventGeneration::from_raw(1))),
            state: Mutex::new(PlayerState::Buffering),
            pipeline: Mutex::new(None),
        };
        assert!(inner.restore().is_err());
        assert!(!inner.restored.load(Ordering::SeqCst));
    }
}
