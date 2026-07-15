//! Library scanning engine — initial scan + real-time filesystem watching.
//!
//! Runs entirely on the tokio runtime. Sends `LibraryEvent` messages
//! to the GTK main thread via `async_channel`.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    Set, TransactionTrait,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

use super::root_authority::{AbsenceProof, BoundDirectory, BoundFile, RootAuthorityLease};
use super::tag_parser::{self, ParsedTrack};
use crate::architecture::models::Track;
use crate::db::entities::{library_root, track};

// ---------------------------------------------------------------------------
// LibraryEvent — messages sent to GTK main thread
// ---------------------------------------------------------------------------

/// Events sent from the background engine to the GTK main thread.
#[derive(Debug, Clone)]
pub enum LibraryEvent {
    /// Complete library snapshot after initial scan.
    FullSync(Vec<Track>),
    /// Tracks from a remote backend, keyed by source (e.g. server URL).
    RemoteSync {
        source_key: String,
        /// Connection generation validated at the GTK publication boundary.
        generation: u64,
        /// Opaque registry lease used to synthesize credential-free media refs.
        lease_key: Uuid,
        tracks: Vec<Track>,
    },
    /// Tracks from a generation-scoped DAAP session. The GTK receiver
    /// validates this ownership token before publishing the tracks.
    DaapSync {
        source_key: String,
        generation: u64,
        session_key: Uuid,
        tracks: Vec<Track>,
    },
    /// A single track was added or updated.
    TrackUpserted(Box<Track>),
    /// A track was removed (by file_path).
    TrackRemoved(String),
    /// Scan progress: (files_scanned, total_files).
    ScanProgress(u64, u64),
    /// Initial scan complete.
    ScanComplete,
    /// Playlists loaded from the database.
    /// Vec of (id, name, is_smart).
    PlaylistsLoaded(Vec<(String, String, bool)>),
    /// Persisted track changes and any resulting playlist reconciliation have
    /// settled, so active playlist projections should be loaded again.
    PlaylistProjectionsInvalidated,
    /// Complete, exact-configured roots which require an explicit user trust
    /// decision before their observed storage may become authoritative.
    RootTrustRequired(Vec<RootTrustRequest>),
    /// Result of one engine-validated root-trust command.
    RootTrustFinished {
        request_id: Uuid,
        path: PathBuf,
        reason: RootTrustReason,
        outcome: RootTrustOutcome,
    },
    /// An error occurred.
    Error(String),
}

/// Why an exact configured library root requires explicit trust.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootTrustReason {
    /// Persisted tracks predate durable library-root identity.
    LegacyEnrollment,
    /// A previously confirmed root now exposes a different identity.
    Replacement,
    /// A complete empty root cannot be distinguished from an unmounted
    /// removable-volume mountpoint without an explicit decision.
    EmptyRoot,
}

/// Public result of an explicit root-trust request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootTrustOutcome {
    /// The enrollment scan and its separate authoritative follow-up completed.
    Active,
    /// The marker was staged, but a complete conversion could not finish.
    TrustedButUnavailable,
    /// Filesystem or persisted evidence no longer matched the prompt.
    Stale,
    /// A marker, database, task, or scan operation failed.
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RootTrustExpectedState {
    device_id: Option<String>,
    identity_confirmed: bool,
    is_available: bool,
    last_scan_complete: bool,
}

impl RootTrustExpectedState {
    fn from_model(state: &library_root::Model) -> Self {
        Self {
            device_id: state.device_id.clone(),
            identity_confirmed: state.identity_confirmed,
            is_available: state.is_available,
            last_scan_complete: state.last_scan_complete,
        }
    }

    fn matches(&self, state: &library_root::Model) -> bool {
        self.device_id == state.device_id
            && self.identity_confirmed == state.identity_confirmed
            && self.is_available == state.is_available
            && self.last_scan_complete == state.last_scan_complete
    }
}

/// Immutable evidence for one explicit root-trust decision.
///
/// GTK may display the path, reason, and remembered-row count, but the
/// filesystem evidence and expected database state remain private. The only
/// supported mutation path is cloning this value back into
/// [`LibraryCommand::ConfirmRootTrust`].
#[derive(Clone)]
pub struct RootTrustRequest {
    request_id: Uuid,
    path: PathBuf,
    reason: RootTrustReason,
    remembered_track_count: usize,
    requires_empty_acknowledgement: bool,
    observed_identity: String,
    observed_mount_generation: u64,
    expected_state: RootTrustExpectedState,
}

impl RootTrustRequest {
    pub fn request_id(&self) -> Uuid {
        self.request_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reason(&self) -> RootTrustReason {
        self.reason
    }

    pub fn remembered_track_count(&self) -> usize {
        self.remembered_track_count
    }

    /// Whether the complete observation was empty and therefore requires the
    /// stronger unmounted-volume/destructive-reconciliation acknowledgement.
    pub fn requires_empty_acknowledgement(&self) -> bool {
        self.requires_empty_acknowledgement
    }
}

impl std::fmt::Debug for RootTrustRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RootTrustRequest")
            .field("request_id", &self.request_id)
            .field("path", &self.path)
            .field("reason", &self.reason)
            .field("remembered_track_count", &self.remembered_track_count)
            .field(
                "requires_empty_acknowledgement",
                &self.requires_empty_acknowledgement,
            )
            .field("evidence", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Commands accepted by the single owner of local-library mutation state.
#[derive(Clone, Debug)]
pub enum LibraryCommand {
    ConfirmRootTrust(RootTrustRequest),
}

// ---------------------------------------------------------------------------
// LibraryEngine
// ---------------------------------------------------------------------------

/// The background scanning and watching engine.
pub struct LibraryEngine {
    db: DatabaseConnection,
    music_dirs: Vec<PathBuf>,
    tx: async_channel::Sender<LibraryEvent>,
    command_rx: async_channel::Receiver<LibraryCommand>,
}

impl LibraryEngine {
    /// Create a new engine. Does NOT start scanning yet.
    ///
    /// Accepts multiple music directories — all will be scanned and watched.
    pub fn new(
        db: DatabaseConnection,
        music_dirs: Vec<PathBuf>,
        tx: async_channel::Sender<LibraryEvent>,
        command_rx: async_channel::Receiver<LibraryCommand>,
    ) -> Self {
        Self {
            db,
            music_dirs,
            tx,
            command_rx,
        }
    }

    /// Run the engine: initial scan across all directories, then continuous
    /// FS watching on each.
    pub async fn run(self) {
        let Self {
            db,
            music_dirs,
            tx,
            command_rx,
        } = self;
        let db = Arc::new(db);

        // Install before traversing so changes observed during the initial
        // scan are retained for replay after its snapshot is published.
        // Construction remains best-effort: a watcher backend failure must
        // not suppress the useful one-shot scan.
        let (mut watcher, watcher_error) = match install_directory_watcher(&music_dirs) {
            Ok(watcher) => (Some(watcher), None),
            Err(error) => {
                error!(%error, "Filesystem watcher could not be installed");
                (None, Some(error.to_string()))
            }
        };

        // ── Initial scan (all directories) ───────────────────────────
        for dir in &music_dirs {
            info!(dir = %dir.display(), "Starting initial library scan");
        }
        if let Err(e) = initial_scan(&db, &music_dirs, &tx).await {
            error!(error = %e, "Initial scan failed");
            let _ = tx.send(LibraryEvent::Error(e.to_string())).await;
        }

        // A missing or temporarily unwatchable root can become available
        // while another root is being enumerated. Retain every successful
        // pre-scan registration, then retry only the gaps at the handoff so a
        // root the scan just indexed is never left unwatched until restart.
        if let Some(watcher) = watcher.as_mut() {
            watcher.watch_available_directories(&music_dirs);
        }

        if let Some(error) = watcher_error {
            let _ = tx.send(LibraryEvent::Error(error)).await;
        }

        // ── Filesystem watcher (all directories) ─────────────────────
        let mut completed_commands = HashMap::new();
        if let Some(watcher) = watcher {
            if let Err(e) = process_directory_events(
                &db,
                &music_dirs,
                &tx,
                &command_rx,
                &mut completed_commands,
                watcher,
            )
            .await
            {
                error!(error = %e, "Filesystem watcher failed");
                let _ = tx.send(LibraryEvent::Error(e.to_string())).await;
            }
        }

        // Keep trust decisions usable even when watcher installation failed
        // or a previously installed watcher backend shut down. Commands are
        // serialized through the same engine owner in both modes.
        process_library_commands_without_watcher(
            db.as_ref(),
            &music_dirs,
            &tx,
            &command_rx,
            &mut completed_commands,
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Initial scan
// ---------------------------------------------------------------------------

/// The result of enumerating one configured library root.
///
/// Keeping completeness separate from the discovered files is important: an
/// empty directory is a complete, authoritative view, while a directory that
/// yielded some files plus a traversal error is not. Only the former may be
/// used to remove stale database rows.
#[derive(Debug)]
struct RootScan {
    root: PathBuf,
    audio_files: Vec<PathBuf>,
    errors: Vec<String>,
    device_id: Option<String>,
    mount_generation: Option<u64>,
    authority_lease: Option<Arc<RootAuthorityLease>>,
    reconciliation_authoritative: bool,
    content_authorized: bool,
}

impl RootScan {
    fn is_complete(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Run one retained-authority filesystem probe outside Tokio's async worker
/// threads. Library roots may live on removable or network filesystems, so
/// even a small handle validation can block indefinitely at the OS boundary.
async fn spawn_authority_probe<F, T>(probe: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(probe).await
}

#[derive(Debug)]
struct AuthorityTaskJoinFailure(String);

impl std::fmt::Display for AuthorityTaskJoinFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "retained-authority task failed: {}", self.0)
    }
}

impl std::error::Error for AuthorityTaskJoinFailure {}

const ROOT_IDENTITY_FILE: &str = ".tributary-root-id";
const ROOT_IDENTITY_PREFIX: &str = "marker:v1:";

fn root_identity_path(root: &Path) -> PathBuf {
    root.join(ROOT_IDENTITY_FILE)
}

fn parse_root_marker(contents: &str) -> std::io::Result<String> {
    let value = contents.strip_suffix('\n').unwrap_or(contents);
    if value.is_empty() || value.contains(char::is_whitespace) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "library root marker has invalid whitespace",
        ));
    }
    let Some(uuid) = value.strip_prefix(ROOT_IDENTITY_PREFIX) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "library root marker has an unsupported format",
        ));
    };
    let uuid = Uuid::parse_str(uuid).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("library root marker has an invalid UUID: {error}"),
        )
    })?;
    Ok(format!("{ROOT_IDENTITY_PREFIX}{uuid}"))
}

#[cfg(unix)]
fn open_root_marker(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(File::from(descriptor))
}

#[cfg(windows)]
fn open_root_marker(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    // Open the reparse point itself instead of following it, then reject every
    // reparse-point flavor (not only ordinary symlinks) from handle metadata.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "library root marker must not be a reparse point",
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_root_marker(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

fn read_root_marker(root: &Path) -> std::io::Result<Option<String>> {
    let path = root_identity_path(root);
    let mut file = match open_root_marker(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 128 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsafe library root marker: {}", path.display()),
        ));
    }

    // Bound the read independently of metadata: a concurrent writer cannot
    // bypass the size check after the handle has been validated.
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    (&mut file).take(129).read_to_end(&mut contents)?;
    if contents.len() > 128 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "library root marker exceeds 128 bytes",
        ));
    }
    let contents = std::str::from_utf8(&contents).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("library root marker is not UTF-8: {error}"),
        )
    })?;
    parse_root_marker(contents).map(Some)
}

struct RootMarkerCreation {
    identity: String,
    created: bool,
}

fn create_root_marker_with_identity(
    root: &Path,
    requested_identity: &str,
) -> std::io::Result<RootMarkerCreation> {
    let identity = parse_root_marker(requested_identity)?;
    if let Some(identity) = read_root_marker(root)? {
        return Ok(RootMarkerCreation {
            identity,
            created: false,
        });
    }

    let path = root_identity_path(root);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let identity = read_root_marker(root)?.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "library root marker appeared but could not be read",
                )
            })?;
            return Ok(RootMarkerCreation {
                identity,
                created: false,
            });
        }
        Err(error) => return Err(error),
    };

    let write_result = file
        .write_all(format!("{identity}\n").as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    // Do not remove by path on failure: another process may have replaced the
    // entry after this handle was opened. A partial marker safely fails closed.
    write_result?;

    let observed = read_root_marker(root)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "library root marker disappeared after creation",
        )
    })?;
    if observed != identity {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "library root marker changed during creation",
        ));
    }
    Ok(RootMarkerCreation {
        identity,
        created: true,
    })
}

fn create_root_marker(root: &Path) -> std::io::Result<RootMarkerCreation> {
    create_root_marker_with_identity(root, &format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4()))
}

fn is_marker_identity(identity: &str) -> bool {
    identity.starts_with(ROOT_IDENTITY_PREFIX)
}

fn is_legacy_identity(identity: &str) -> bool {
    identity.starts_with("unix:")
        || identity.starts_with("windows:")
        || identity.starts_with("path:")
}

#[cfg(unix)]
fn legacy_filesystem_identity(path: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path)?;
    let filesystem = rustix::fs::statvfs(path).map_err(std::io::Error::from)?;
    Ok(format!(
        "unix:{}:{}:{}",
        filesystem.f_fsid,
        metadata.dev(),
        metadata.ino()
    ))
}

#[cfg(windows)]
fn legacy_filesystem_identity(path: &Path) -> std::io::Result<String> {
    use std::os::windows::fs::MetadataExt;

    let metadata = std::fs::metadata(path)?;
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Ok(format!(
        "windows:{}:{}",
        canonical.to_string_lossy(),
        metadata.creation_time()
    ))
}

#[cfg(not(any(unix, windows)))]
fn legacy_filesystem_identity(path: &Path) -> std::io::Result<String> {
    Ok(format!("path:{}", path.canonicalize()?.to_string_lossy()))
}

fn filesystem_identity(path: &Path) -> std::io::Result<String> {
    read_root_marker(path)?.map_or_else(|| legacy_filesystem_identity(path), Ok)
}

#[cfg(target_os = "linux")]
fn filesystem_boundary_id(path: &Path) -> std::io::Result<u64> {
    // A bind mount can share `st_dev` with its parent while still being an
    // independent availability scope. The per-mount ID catches that boundary
    // even if it appeared after the initial mount-table snapshot.
    root_mount_generation(path)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn filesystem_boundary_id(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    Ok(std::fs::metadata(path)?.dev())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn filesystem_boundary_id(_path: &Path) -> std::io::Result<u64> {
    // Keep the fallible signature shared with Unix so callers fail closed when
    // a platform-specific boundary probe is added here.
    Ok(0)
}

/// Return the current Linux mount instance for `path`.
///
/// Unlike the persisted filesystem identity, this value is intentionally
/// ephemeral: unmounting and remounting the same volume produces a new mount
/// ID even when its fsid, device number, and root inode are unchanged. Comparing
/// it before and after traversal closes that ABA window without making a normal
/// remount permanently invalidate the persisted library-root identity.
#[cfg(target_os = "linux")]
fn root_mount_generation(path: &Path) -> std::io::Result<u64> {
    use rustix::fs::{AtFlags, StatxFlags, CWD};

    if let Ok(stat) = rustix::fs::statx(CWD, path, AtFlags::empty(), StatxFlags::MNT_ID) {
        if stat.stx_mask & StatxFlags::MNT_ID.bits() != 0 {
            return Ok(stat.stx_mnt_id);
        }
    }

    // `STATX_MNT_ID` was added after statx itself. Fall back to mountinfo on
    // older kernels or restricted containers rather than silently dropping
    // the generation check.
    let canonical = path.canonicalize()?;
    let contents = std::fs::read_to_string("/proc/self/mountinfo")?;
    mount_generation_from_mountinfo(&contents, &canonical)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no Linux mount scope contains {}", canonical.display()),
        )
    })
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn root_mount_generation(_path: &Path) -> std::io::Result<u64> {
    // Other platforms still use the stable pre/post filesystem identity. A
    // constant generation and shared fallible signature keep the traversal
    // implementation portable without weakening Linux's generation checks.
    Ok(0)
}

#[cfg(target_os = "linux")]
fn decode_mountinfo_path(field: &str) -> std::io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let octal = &bytes[index + 1..index + 4];
            if octal.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                decoded.push((octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + (octal[2] - b'0'));
                index += 4;
                continue;
            }
        }
        if bytes[index] == b'\\' {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid mountinfo path escape in {field:?}"),
            ));
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    Ok(PathBuf::from(OsString::from_vec(decoded)))
}

#[cfg(target_os = "linux")]
fn parse_mountinfo(contents: &str) -> std::io::Result<Vec<(u64, PathBuf)>> {
    let mut records = Vec::new();
    for (line_index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let separator = fields.iter().position(|field| *field == "-");
        if fields.len() < 10
            || !separator.is_some_and(|index| index >= 6 && fields.len() >= index + 4)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("malformed mountinfo line {}", line_index + 1),
            ));
        }
        let mount_id = fields[0].parse::<u64>().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid mount ID on mountinfo line {}: {error}",
                    line_index + 1
                ),
            )
        })?;
        let mountpoint = decode_mountinfo_path(fields[4])?;
        if !mountpoint.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "non-absolute mountpoint on mountinfo line {}",
                    line_index + 1
                ),
            ));
        }
        records.push((mount_id, mountpoint));
    }
    if records.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mountinfo contained no mount records",
        ));
    }
    Ok(records)
}

#[cfg(target_os = "linux")]
fn mount_generation_from_mountinfo(contents: &str, path: &Path) -> std::io::Result<Option<u64>> {
    Ok(parse_mountinfo(contents)?
        .into_iter()
        .filter(|(_, mountpoint)| path.starts_with(mountpoint))
        .max_by_key(|(_, mountpoint)| mountpoint.components().count())
        .map(|(mount_id, _)| mount_id))
}

#[cfg(target_os = "linux")]
fn mounted_subroots_from_mountinfo(
    contents: &str,
    configured_roots: &[PathBuf],
) -> std::io::Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = parse_mountinfo(contents)?
        .into_iter()
        .map(|(_, mountpoint)| mountpoint)
        .filter(|mountpoint| {
            configured_roots
                .iter()
                .any(|root| mountpoint != root && mountpoint.starts_with(root))
        })
        .collect();
    roots.sort_unstable();
    roots.dedup();
    Ok(roots)
}

#[cfg(target_os = "linux")]
fn mounted_subroots(configured_roots: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .and_then(|contents| mounted_subroots_from_mountinfo(&contents, configured_roots))
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn mounted_subroots(_configured_roots: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    // The fallible return type is part of the cross-platform fail-closed scan
    // contract even though only Linux currently has mount-table discovery.
    Ok(Vec::new())
}

fn expanded_scan_roots(
    configured_roots: &[PathBuf],
    persisted_roots: &[library_root::Model],
) -> std::io::Result<Vec<PathBuf>> {
    expanded_scan_roots_with_mount_result(
        configured_roots,
        persisted_roots,
        mounted_subroots(configured_roots),
    )
}

fn expanded_scan_roots_with_mount_result(
    configured_roots: &[PathBuf],
    persisted_roots: &[library_root::Model],
    mounted_roots: std::io::Result<Vec<PathBuf>>,
) -> std::io::Result<Vec<PathBuf>> {
    mounted_roots.map(|mounted_roots| {
        expanded_scan_roots_with_mounts(configured_roots, persisted_roots, mounted_roots)
    })
}

fn expanded_scan_roots_with_mounts(
    configured_roots: &[PathBuf],
    persisted_roots: &[library_root::Model],
    mounted_roots: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut roots = configured_roots.to_vec();
    roots.extend(mounted_roots);
    roots.extend(persisted_roots.iter().filter_map(|state| {
        let path = PathBuf::from(&state.path);
        configured_roots
            .iter()
            .any(|configured| path.starts_with(configured))
            .then_some(path)
    }));
    roots.sort_unstable();
    roots.dedup();
    roots
}

fn scan_root(root: PathBuf) -> RootScan {
    scan_root_with_probes_and_exclusions(
        root,
        filesystem_identity,
        root_mount_generation,
        &[],
        true,
    )
}

fn scan_root_with_identity_probe<F>(root: PathBuf, identity_probe: F) -> RootScan
where
    F: FnMut(&Path) -> std::io::Result<String>,
{
    scan_root_with_probes_and_exclusions(root, identity_probe, root_mount_generation, &[], false)
}

fn scan_root_with_exclusions(root: PathBuf, all_roots: &[PathBuf]) -> RootScan {
    let exclusions: Vec<PathBuf> = all_roots
        .iter()
        .filter(|candidate| candidate.as_path() != root && candidate.starts_with(&root))
        .cloned()
        .collect();
    scan_root_with_probes_and_exclusions(
        root,
        filesystem_identity,
        root_mount_generation,
        &exclusions,
        true,
    )
}

fn scan_root_with_probes_and_exclusions<F, G>(
    root: PathBuf,
    mut identity_probe: F,
    mut mount_generation_probe: G,
    exclusions: &[PathBuf],
    retain_authority: bool,
) -> RootScan
where
    F: FnMut(&Path) -> std::io::Result<String>,
    G: FnMut(&Path) -> std::io::Result<u64>,
{
    if !root.is_dir() {
        return RootScan {
            errors: vec![format!(
                "library root does not exist or is not a directory: {}",
                root.display()
            )],
            root,
            audio_files: Vec::new(),
            device_id: None,
            mount_generation: None,
            authority_lease: None,
            reconciliation_authoritative: false,
            content_authorized: false,
        };
    }

    let device_id = match identity_probe(&root) {
        Ok(identity) => Some(identity),
        Err(error) => {
            return RootScan {
                errors: vec![format!(
                    "failed to identify library root {}: {error}",
                    root.display()
                )],
                root,
                audio_files: Vec::new(),
                device_id: None,
                mount_generation: None,
                authority_lease: None,
                reconciliation_authoritative: false,
                content_authorized: false,
            };
        }
    };

    // A marker-backed scan retains the exact root and marker objects before
    // traversal. Scalar marker text or a path-based mount probe cannot reject
    // a copied-marker replacement that appears after enumeration begins.
    let authority_lease =
        if retain_authority && device_id.as_deref().is_some_and(is_marker_identity) {
            let expected_marker = device_id.as_deref().expect("checked marker identity");
            match RootAuthorityLease::acquire(&root, expected_marker) {
                Ok(lease) => Some(Arc::new(lease)),
                Err(error) => {
                    return RootScan {
                        errors: vec![format!(
                            "failed to retain library root authority {}: {error}",
                            root.display()
                        )],
                        root,
                        audio_files: Vec::new(),
                        device_id,
                        mount_generation: None,
                        authority_lease: None,
                        reconciliation_authoritative: false,
                        content_authorized: false,
                    };
                }
            }
        } else {
            None
        };

    let mount_generation = match mount_generation_probe(&root) {
        Ok(generation) => generation,
        Err(error) => {
            return RootScan {
                errors: vec![format!(
                    "failed to identify library root mount generation {}: {error}",
                    root.display()
                )],
                root,
                audio_files: Vec::new(),
                device_id,
                mount_generation: None,
                authority_lease,
                reconciliation_authoritative: false,
                content_authorized: false,
            };
        }
    };

    if let Some(lease_generation) = authority_lease
        .as_ref()
        .and_then(|lease| lease.mount_generation())
    {
        if lease_generation != mount_generation {
            return RootScan {
                errors: vec![format!(
                    "library root mount differs from retained authority (path={mount_generation}, handle={lease_generation})"
                )],
                root,
                audio_files: Vec::new(),
                device_id,
                mount_generation: Some(mount_generation),
                authority_lease,
                reconciliation_authoritative: false,
                content_authorized: false,
            };
        }
    }

    let mut errors = Vec::new();
    let root_boundary = match filesystem_boundary_id(&root) {
        Ok(boundary) => Some(boundary),
        Err(error) => {
            errors.push(format!(
                "failed to identify library root boundary {}: {error}",
                root.display()
            ));
            None
        }
    };

    let (audio_files, traversal_errors) = enumerate_audio_files(&root, root_boundary, exclusions);
    errors.extend(traversal_errors);

    match mount_generation_probe(&root) {
        Ok(generation) if generation == mount_generation => {}
        Ok(generation) => errors.push(format!(
            "library root mount generation changed during traversal (before={mount_generation}, after={generation})"
        )),
        Err(error) => errors.push(format!(
            "failed to re-identify library root mount generation {} after traversal: {error}",
            root.display()
        )),
    }

    match identity_probe(&root) {
        Ok(identity) if Some(&identity) == device_id.as_ref() => {}
        Ok(identity) => errors.push(format!(
            "library root identity changed during traversal (before={device_id:?}, after={identity})"
        )),
        Err(error) => errors.push(format!(
            "failed to re-identify library root {} after traversal: {error}",
            root.display()
        )),
    }

    if let Some(lease) = &authority_lease {
        if let Err(error) = lease.validate() {
            errors.push(format!(
                "library root authority changed during traversal {}: {error}",
                root.display()
            ));
        }
    }

    RootScan {
        root,
        audio_files,
        errors,
        device_id,
        mount_generation: Some(mount_generation),
        authority_lease,
        reconciliation_authoritative: false,
        content_authorized: false,
    }
}

/// Enumerate the audio files under `directory` using the one indexing policy
/// every scope shares.
///
/// Symlinks are never followed: the notify watcher does not follow them either,
/// so following here would index files that are never watched for changes, and
/// could index one physical file under several paths as duplicate rows. A
/// subdirectory on another filesystem, or one that owns its own scan scope, is
/// skipped rather than absorbed. Every error is returned: a caller that cannot
/// see the whole subtree must fail closed instead of treating a partial view as
/// authoritative.
///
/// `boundary` is the filesystem the enclosing library root lives on, not the
/// one `directory` itself lives on — a scoped traversal must not accept a
/// nested filesystem simply because it is self-consistent below its own mount.
fn enumerate_audio_files(
    directory: &Path,
    boundary: Option<u64>,
    exclusions: &[PathBuf],
) -> (Vec<PathBuf>, Vec<String>) {
    enumerate_audio_files_with_observer(directory, boundary, exclusions, |_| Ok(()))
}

/// Shared traversal with an optional per-file observation performed at the
/// instant the entry is discovered. Directory-rename scans use this to retain
/// the exact filesystem object that justified a path mapping; ordinary root
/// scans use the zero-cost no-op wrapper above.
fn enumerate_audio_files_with_observer<F>(
    directory: &Path,
    boundary: Option<u64>,
    exclusions: &[PathBuf],
    mut observe: F,
) -> (Vec<PathBuf>, Vec<String>)
where
    F: FnMut(&Path) -> Result<(), String>,
{
    let mut audio_files = Vec::new();
    let mut errors = Vec::new();

    let mut entries = WalkDir::new(directory).follow_links(false).into_iter();
    while let Some(entry) = entries.next() {
        match entry {
            Ok(entry) if entry.depth() > 0 && entry.file_type().is_dir() => {
                if exclusions.iter().any(|path| path == entry.path()) {
                    entries.skip_current_dir();
                    continue;
                }
                if let Some(boundary) = boundary {
                    match filesystem_boundary_id(entry.path()) {
                        Ok(entry_id) if boundary != entry_id => {
                            errors.push(format!(
                                "nested filesystem requires its own configured root: {}",
                                entry.path().display()
                            ));
                            entries.skip_current_dir();
                        }
                        Ok(_) => {}
                        Err(error) => {
                            errors.push(format!(
                                "failed to identify filesystem boundary {}: {error}",
                                entry.path().display()
                            ));
                            entries.skip_current_dir();
                        }
                    }
                } else {
                    // A scope whose boundary could not be established is never
                    // authoritative, but avoid crossing any child scope while
                    // collecting diagnostics from the rest of the traversal.
                    errors.push(format!(
                        "skipping directory without a trusted root boundary: {}",
                        entry.path().display()
                    ));
                    entries.skip_current_dir();
                }
            }
            Ok(entry) if entry.file_type().is_file() && tag_parser::is_audio_file(entry.path()) => {
                let path = entry.into_path();
                if let Err(error) = observe(&path) {
                    errors.push(error);
                }
                audio_files.push(path);
            }
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }

    (audio_files, errors)
}

/// Traversal of the destination of a paired directory rename.
#[derive(Debug, Default)]
struct DirectoryRenameScan {
    audio_files: Vec<PathBuf>,
    observed_files: HashMap<String, BoundFile>,
    errors: Vec<String>,
}

impl DirectoryRenameScan {
    fn failed(error: String) -> Self {
        Self {
            audio_files: Vec::new(),
            observed_files: HashMap::new(),
            errors: vec![error],
        }
    }

    fn is_complete(&self) -> bool {
        self.errors.is_empty()
    }

    /// Reopen every observed path and compare it with the live handle retained
    /// by the scan. This closes the traversal-to-commit window for removals,
    /// replacements, symlinks, and directory swaps without holding a SQLite
    /// write transaction open for a second recursive traversal.
    fn observations_still_current(
        &self,
        lease: &RootAuthorityLease,
        destination: &BoundDirectory,
    ) -> bool {
        if !self.is_complete() || self.audio_files.len() != self.observed_files.len() {
            return false;
        }
        if destination.validate(lease).is_err() {
            return false;
        }

        for path in &self.audio_files {
            let key = path.to_string_lossy();
            let Some(expected) = self.observed_files.get(key.as_ref()) else {
                return false;
            };
            if expected.validate(lease).is_err() {
                return false;
            }
        }

        // The destination and root must remain authoritative across all of the
        // per-file probes.
        destination.validate(lease).is_ok() && lease.validate().is_ok()
    }
}

/// Enumerate the destination of a paired directory rename under its library
/// root's filesystem.
///
/// The destination itself is checked against the root's boundary, which the
/// shared per-entry traversal cannot do: it only compares descendants, so a
/// whole filesystem mounted exactly at `directory` would otherwise look
/// self-consistent. An incomplete traversal is never used to derive identity.
fn scan_renamed_directory(
    lease: &RootAuthorityLease,
    destination: &BoundDirectory,
    directory: &Path,
) -> DirectoryRenameScan {
    if let Err(error) = destination.validate(lease) {
        return DirectoryRenameScan::failed(format!(
            "failed to validate bound renamed directory {}: {error}",
            directory.display()
        ));
    }

    let boundary = match filesystem_boundary_id(lease.root()) {
        Ok(boundary) => boundary,
        Err(error) => {
            return DirectoryRenameScan::failed(format!(
                "failed to identify library root boundary {}: {error}",
                lease.root().display()
            ))
        }
    };
    match filesystem_boundary_id(directory) {
        Ok(destination) if destination == boundary => {}
        Ok(_) => {
            return DirectoryRenameScan::failed(format!(
                "renamed directory does not share the library root filesystem: {}",
                directory.display()
            ))
        }
        Err(error) => {
            return DirectoryRenameScan::failed(format!(
                "failed to identify renamed directory boundary {}: {error}",
                directory.display()
            ))
        }
    }

    // No exclusions: a pair whose subtree owns another scan scope is rejected
    // before it reaches this traversal (`subtree_owns_another_scope`).
    let mut observed_files = HashMap::new();
    let (audio_files, mut errors) =
        enumerate_audio_files_with_observer(directory, Some(boundary), &[], |path| {
            let bound_file = lease.open_regular_file(path).map_err(|error| {
                format!(
                    "failed to bind renamed file beneath its retained root {}: {error}",
                    path.display()
                )
            })?;
            let key = path.to_string_lossy().into_owned();
            if observed_files.insert(key.clone(), bound_file).is_some() {
                return Err(format!(
                    "multiple renamed files collapse to the persisted path key: {key}"
                ));
            }
            Ok(())
        });
    if destination.validate(lease).is_err() {
        errors.push(format!(
            "renamed directory changed during traversal: {}",
            directory.display()
        ));
    }
    DirectoryRenameScan {
        audio_files,
        observed_files,
        errors,
    }
}

/// Select traversal observations that may inherit indexed identities.
/// Descendant paths with their own event in the same batch are deliberately
/// excluded because that event may describe a replacement independent of the
/// directory move.
fn directory_identity_destinations(
    audio_files: &[PathBuf],
    source: &Path,
    destination: &Path,
    upsert_paths: &HashSet<PathBuf>,
    remove_paths: &HashSet<PathBuf>,
    deferred_paths: &HashSet<PathBuf>,
    dirty_directory_scopes: &HashSet<PathBuf>,
) -> HashSet<String> {
    let dirty_scopes: Vec<&Path> = upsert_paths
        .iter()
        .chain(remove_paths.iter())
        .chain(deferred_paths.iter())
        .chain(dirty_directory_scopes.iter())
        .map(PathBuf::as_path)
        .collect();

    audio_files
        .iter()
        .filter(|path| {
            let Ok(relative) = path.strip_prefix(destination) else {
                return false;
            };
            let source_path = source.join(relative);
            !dirty_scopes
                .iter()
                .any(|dirty| path.starts_with(dirty) || source_path.starts_with(dirty))
        })
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn collect_audio_files(root_scans: &[RootScan]) -> Vec<PathBuf> {
    let mut audio_files: Vec<PathBuf> = root_scans
        .iter()
        .filter(|scan| scan.content_authorized)
        .flat_map(|scan| scan.audio_files.iter().cloned())
        .collect();

    // The same file is visible through every configured ancestor root. Scan
    // it once even if the user's configuration contains overlapping roots.
    audio_files.sort_unstable();
    audio_files.dedup();
    audio_files
}

/// Return the most specific configured root containing `path`.
///
/// Choosing the deepest root ensures an explicitly configured unavailable
/// child protects its rows even when an available parent also contains it.
fn root_scan_for_path<'a>(path: &Path, root_scans: &'a [RootScan]) -> Option<&'a RootScan> {
    root_scans
        .iter()
        .filter(|scan| path.starts_with(&scan.root))
        .max_by_key(|scan| scan.root.components().count())
}

/// Recheck the most-specific authorized root for one pending initial-scan
/// write. A failed probe invalidates that root for every later file in this
/// scan; callers persist the returned root's unavailable state.
async fn revalidate_scan_root_for_path(
    path: &Path,
    root_scans: &mut [RootScan],
) -> Result<(bool, Option<PathBuf>), tokio::task::JoinError> {
    let Some(index) = root_scans
        .iter()
        .enumerate()
        .filter(|(_, scan)| path.starts_with(&scan.root))
        .max_by_key(|(_, scan)| scan.root.components().count())
        .map(|(index, _)| index)
    else {
        return Ok((false, None));
    };
    if !root_scans[index].content_authorized {
        return Ok((false, None));
    }
    let expected = root_scans[index]
        .device_id
        .as_deref()
        .filter(|expected| is_marker_identity(expected))
        .map(str::to_owned);
    let authority_lease = root_scans[index].authority_lease.clone();
    let matches = match (expected, authority_lease) {
        (Some(expected), Some(lease)) => {
            spawn_authority_probe(move || {
                lease.expected_marker() == expected && lease.validate().is_ok()
            })
            .await?
        }
        _ => false,
    };
    if matches {
        return Ok((true, None));
    }

    let scan = &mut root_scans[index];
    scan.content_authorized = false;
    scan.reconciliation_authoritative = false;
    Ok((false, Some(scan.root.clone())))
}

fn most_specific_root_for_path<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a Path> {
    roots
        .iter()
        .map(PathBuf::as_path)
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
}

fn should_remove_stale_track(
    path: &Path,
    on_disk_paths: &HashSet<String>,
    root_scans: &[RootScan],
) -> bool {
    if on_disk_paths.contains(path.to_string_lossy().as_ref()) {
        return false;
    }

    root_scan_for_path(path, root_scans)
        .is_some_and(|scan| scan.is_complete() && scan.reconciliation_authoritative)
}

/// Decide whether a complete scan may authoritatively remove stale rows.
///
/// An explicitly established filesystem identity must match the current root.
/// New observations never authorize deletion in the same scan: enrollment
/// and reconciliation authority are deliberately separate states.
fn reconciliation_is_authoritative(
    scan: &RootScan,
    previous: Option<&library_root::Model>,
) -> bool {
    if !scan.is_complete() {
        return false;
    }

    let Some(observed_device_id) = scan.device_id.as_deref() else {
        return false;
    };
    if !is_marker_identity(observed_device_id) {
        return false;
    }

    previous.is_some_and(|state| {
        state.identity_confirmed && state.device_id.as_deref() == Some(observed_device_id)
    })
}

/// Decide whether this scan establishes a root identity for future scans.
///
/// A brand-new root can be enrolled only when it has content and no existing
/// metadata can be harmed. A legacy root with rows is deliberately left
/// unconfirmed until an explicit-trust UX can resolve the intended volume:
/// even a complete path/size/mtime clone cannot prove physical identity. A
/// different device never silently replaces a confirmed identity.
fn scan_confirms_identity(
    scan: &RootScan,
    previous: Option<&library_root::Model>,
    existing_track_count: usize,
) -> bool {
    scan_confirms_identity_for_scope(scan, previous, existing_track_count, true)
}

fn scan_confirms_identity_for_scope(
    scan: &RootScan,
    previous: Option<&library_root::Model>,
    existing_track_count: usize,
    allow_new_enrollment: bool,
) -> bool {
    if !scan.is_complete()
        || !scan.device_id.as_deref().is_some_and(is_marker_identity)
        || scan.audio_files.is_empty()
    {
        return false;
    }

    if let Some(state) = previous {
        if state.identity_confirmed {
            return state.device_id.as_deref() == scan.device_id.as_deref();
        }
    }

    allow_new_enrollment && previous.is_none() && existing_track_count == 0
}

fn root_trust_reason(
    scan: &RootScan,
    previous: Option<&library_root::Model>,
    existing_track_count: usize,
    explicitly_configured: bool,
    confirms_identity: bool,
) -> Option<RootTrustReason> {
    let observed_identity = scan.device_id.as_deref()?;
    if !explicitly_configured
        || !scan.is_complete()
        || scan.mount_generation.is_none()
        || confirms_identity
        || scan.reconciliation_authoritative
        || !(is_marker_identity(observed_identity) || is_legacy_identity(observed_identity))
    {
        return None;
    }

    if previous.is_some_and(|state| state.identity_confirmed)
        && previous.and_then(|state| state.device_id.as_deref()) != Some(observed_identity)
    {
        return Some(RootTrustReason::Replacement);
    }

    if scan.audio_files.is_empty() {
        return Some(RootTrustReason::EmptyRoot);
    }

    (existing_track_count > 0
        || previous.is_some_and(|state| !state.identity_confirmed)
        || (is_legacy_identity(observed_identity)
            && previous.is_some_and(|state| state.identity_confirmed)))
    .then_some(RootTrustReason::LegacyEnrollment)
}

fn root_trust_request_id(
    path: &Path,
    reason: RootTrustReason,
    remembered_track_count: usize,
    requires_empty_acknowledgement: bool,
    observed_identity: &str,
    observed_mount_generation: u64,
    expected_state: &RootTrustExpectedState,
) -> Uuid {
    // This UUID is a stable correlation token, not an authorization secret.
    // Every security-relevant field is still compared directly when the
    // command returns. Delimit and tag each value so concatenation cannot make
    // two different evidence tuples hash to the same namespace input.
    let mut evidence = b"tributary:root-trust:v1\0path:".to_vec();
    let path_evidence = root_trust_path_evidence(path);
    evidence.extend_from_slice(&path_evidence.len().to_le_bytes());
    evidence.extend_from_slice(&path_evidence);
    evidence.extend_from_slice(
        format!(
        "\0reason:{reason:?}\0count:{remembered_track_count}\0empty-ack:{requires_empty_acknowledgement}\0observed:{observed_identity}\0mount:{observed_mount_generation}\0expected-device:{:?}\0expected-confirmed:{}\0expected-available:{}\0expected-complete:{}",
        expected_state.device_id,
        expected_state.identity_confirmed,
        expected_state.is_available,
        expected_state.last_scan_complete,
    )
        .as_bytes(),
    );
    Uuid::new_v5(&Uuid::NAMESPACE_URL, &evidence)
}

#[cfg(unix)]
fn root_trust_path_evidence(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn root_trust_path_evidence(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn root_trust_path_evidence(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

fn build_root_trust_request(
    scan: &RootScan,
    stored: &library_root::Model,
    reason: RootTrustReason,
    remembered_track_count: usize,
) -> Option<RootTrustRequest> {
    let observed_identity = scan.device_id.clone()?;
    let observed_mount_generation = scan.mount_generation?;
    let requires_empty_acknowledgement = scan.audio_files.is_empty();
    let expected_state = RootTrustExpectedState::from_model(stored);
    let request_id = root_trust_request_id(
        &scan.root,
        reason,
        remembered_track_count,
        requires_empty_acknowledgement,
        &observed_identity,
        observed_mount_generation,
        &expected_state,
    );
    Some(RootTrustRequest {
        request_id,
        path: scan.root.clone(),
        reason,
        remembered_track_count,
        requires_empty_acknowledgement,
        observed_identity,
        observed_mount_generation,
        expected_state,
    })
}

#[derive(Debug)]
enum RootTrustError {
    Stale(&'static str),
    Failed(anyhow::Error),
}

impl From<std::io::Error> for RootTrustError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(error.into())
    }
}

fn establish_explicit_root_marker(request: &RootTrustRequest) -> Result<String, RootTrustError> {
    if root_mount_generation(&request.path).map_err(RootTrustError::from)?
        != request.observed_mount_generation
    {
        return Err(RootTrustError::Stale(
            "library root mount generation changed after prompting",
        ));
    }

    if is_marker_identity(&request.observed_identity) {
        let observed = filesystem_identity(&request.path).map_err(RootTrustError::from)?;
        if observed != request.observed_identity {
            return Err(RootTrustError::Stale(
                "library root marker changed after prompting",
            ));
        }
        if root_mount_generation(&request.path).map_err(RootTrustError::from)?
            != request.observed_mount_generation
        {
            return Err(RootTrustError::Stale(
                "library root mount changed while adopting its marker",
            ));
        }
        return Ok(observed);
    }

    if !is_legacy_identity(&request.observed_identity) {
        return Err(RootTrustError::Stale(
            "library root identity format is no longer supported",
        ));
    }

    let observed_legacy =
        legacy_filesystem_identity(&request.path).map_err(RootTrustError::from)?;
    if observed_legacy != request.observed_identity {
        return Err(RootTrustError::Stale(
            "library root identity changed after prompting",
        ));
    }

    match read_root_marker(&request.path).map_err(RootTrustError::from)? {
        Some(_) => Err(RootTrustError::Stale(
            "an unexpected library root marker appeared after prompting",
        )),
        None => {
            let creation = create_root_marker(&request.path).map_err(RootTrustError::from)?;
            if !creation.created {
                return Err(RootTrustError::Stale(
                    "a library root marker appeared during confirmation",
                ));
            }
            let requested_marker = creation.identity;

            let legacy_stable = legacy_filesystem_identity(&request.path)
                .is_ok_and(|identity| identity == request.observed_identity);
            let generation_stable = root_mount_generation(&request.path)
                .is_ok_and(|generation| generation == request.observed_mount_generation);
            let marker_stable = read_root_marker(&request.path)
                .is_ok_and(|identity| identity.as_deref() == Some(requested_marker.as_str()));
            if !legacy_stable || !generation_stable || !marker_stable {
                return Err(RootTrustError::Stale(
                    "library root changed while creating its durable marker",
                ));
            }

            Ok(requested_marker)
        }
    }
}

/// Bind a brand-new, nonempty, explicitly configured root to a durable marker.
///
/// Roots with inherited metadata or a previously persisted legacy identity
/// are deliberately excluded: those require an explicit trust request.
#[derive(Debug, Eq, PartialEq)]
enum RootIdentityPreparation {
    Unchanged,
    MarkerCreated { identity: String },
}

fn prepare_durable_root_identity(
    scan: &mut RootScan,
    previous: Option<&library_root::Model>,
    existing_track_count: usize,
    explicitly_configured: bool,
) -> RootIdentityPreparation {
    if !scan.is_complete() {
        return RootIdentityPreparation::Unchanged;
    }

    let is_new_enrollment = explicitly_configured
        && !previous.is_some_and(|state| state.identity_confirmed)
        && existing_track_count == 0
        && !scan.audio_files.is_empty();
    let needs_marker =
        scan.device_id.as_deref().is_some_and(is_legacy_identity) && is_new_enrollment;

    if !needs_marker {
        return RootIdentityPreparation::Unchanged;
    }

    // Retain both probes across creation. A marker written to a root that was
    // replaced after traversal must never bless the replacement volume.
    let before_legacy = match legacy_filesystem_identity(&scan.root) {
        Ok(identity) => identity,
        Err(error) => {
            scan.errors.push(format!(
                "failed to re-identify library root before marker creation {}: {error}",
                scan.root.display()
            ));
            return RootIdentityPreparation::Unchanged;
        }
    };
    let before_generation = match root_mount_generation(&scan.root) {
        Ok(generation) => generation,
        Err(error) => {
            scan.errors.push(format!(
                "failed to identify library root mount before marker creation {}: {error}",
                scan.root.display()
            ));
            return RootIdentityPreparation::Unchanged;
        }
    };
    if scan.device_id.as_deref() != Some(before_legacy.as_str())
        || scan.mount_generation != Some(before_generation)
    {
        scan.errors.push(format!(
            "library root identity or mount changed before marker creation: {}",
            scan.root.display()
        ));
        return RootIdentityPreparation::Unchanged;
    }

    let creation = match create_root_marker(&scan.root) {
        Ok(creation) => creation,
        Err(error) => {
            scan.errors.push(format!(
                "failed to create durable library root identity {}: {error}",
                scan.root.display()
            ));
            return RootIdentityPreparation::Unchanged;
        }
    };
    if !creation.created {
        scan.errors.push(format!(
            "library root marker appeared during enrollment: {}",
            scan.root.display()
        ));
        return RootIdentityPreparation::Unchanged;
    }

    let legacy_stable =
        legacy_filesystem_identity(&scan.root).is_ok_and(|identity| identity == before_legacy);
    let generation_stable =
        root_mount_generation(&scan.root).is_ok_and(|generation| generation == before_generation);
    if !legacy_stable || !generation_stable {
        scan.errors.push(format!(
            "library root changed while creating its durable identity: {}",
            scan.root.display()
        ));
        return RootIdentityPreparation::Unchanged;
    }

    scan.device_id = Some(creation.identity.clone());
    RootIdentityPreparation::MarkerCreated {
        identity: creation.identity,
    }
}

fn reject_duplicate_marker_identities(root_scans: &mut [RootScan]) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for identity in root_scans
        .iter()
        .filter_map(|scan| scan.device_id.as_deref())
        .filter(|identity| is_marker_identity(identity))
    {
        *counts.entry(identity.to_string()).or_default() += 1;
    }

    for scan in root_scans {
        if scan.device_id.as_ref().is_some_and(|identity| {
            is_marker_identity(identity) && counts.get(identity).copied().unwrap_or_default() > 1
        }) {
            scan.errors.push(format!(
                "duplicate library root marker detected at {}",
                scan.root.display()
            ));
        }
    }
}

async fn persist_root_scan_status(
    db: &DatabaseConnection,
    scan: &RootScan,
    previous: Option<&library_root::Model>,
    confirms_identity: bool,
    activation_allowed: bool,
    demote_identity: bool,
) -> anyhow::Result<library_root::Model> {
    let was_confirmed = !demote_identity && previous.is_some_and(|state| state.identity_confirmed);
    let recorded_device_id = if demote_identity {
        scan.device_id
            .clone()
            .or_else(|| previous.and_then(|state| state.device_id.clone()))
    } else if confirms_identity {
        scan.device_id.clone()
    } else if was_confirmed || !scan.is_complete() {
        previous.and_then(|state| state.device_id.clone())
    } else {
        // Keep the latest untrusted observation for diagnostics, but it never
        // authorizes deletion until a later scan explicitly confirms it.
        scan.device_id.clone()
    };
    let identity_confirmed = was_confirmed || confirms_identity;
    let is_available = activation_allowed
        && scan.is_complete()
        && (scan.reconciliation_authoritative || confirms_identity);
    let last_checked_at = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;

    let stored = if let Some(state) = previous {
        let mut active: library_root::ActiveModel = state.clone().into();
        active.device_id = Set(recorded_device_id);
        active.identity_confirmed = Set(identity_confirmed);
        active.is_available = Set(is_available);
        active.last_scan_complete = Set(scan.is_complete());
        active.last_checked_at = Set(last_checked_at);
        active.update(&transaction).await?
    } else {
        library_root::ActiveModel {
            path: Set(scan.root.to_string_lossy().into_owned()),
            device_id: Set(recorded_device_id),
            identity_confirmed: Set(identity_confirmed),
            is_available: Set(is_available),
            last_scan_complete: Set(scan.is_complete()),
            last_checked_at: Set(last_checked_at),
        }
        .insert(&transaction)
        .await?
    };

    // Revocations must remain writable precisely when storage is unavailable,
    // but every confirmation or availability promotion is derived from the
    // retained root object and must keep that lease valid through commit.
    if confirms_identity || is_available {
        let expected = scan.device_id.clone();
        let lease_matches = if let Some(lease) = scan.authority_lease.clone() {
            match spawn_authority_probe(move || {
                expected.as_deref() == Some(lease.expected_marker()) && lease.validate().is_ok()
            })
            .await
            {
                Ok(matches) => matches,
                Err(error) => {
                    transaction.rollback().await?;
                    return Err(AuthorityTaskJoinFailure(error.to_string()).into());
                }
            }
        } else {
            false
        };
        if !lease_matches {
            transaction.rollback().await?;
            return Err(anyhow::anyhow!(
                "library root authority changed before state persistence"
            ));
        }
    }

    transaction.commit().await?;
    Ok(stored)
}

#[derive(Clone, Debug)]
struct PendingRootTrustScan {
    request_id: Uuid,
    path: PathBuf,
    reason: RootTrustReason,
    marker_identity: String,
    expected_mount_generation: u64,
}

#[derive(Clone, Debug)]
struct ForcedRootTrustConversion {
    marker_identity: String,
    expected_empty: bool,
    expected_mount_generation: u64,
    original_reason: RootTrustReason,
}

#[derive(Clone, Debug)]
struct RootTrustAuthorityGuard {
    marker_identity: String,
    expected_mount_generation: u64,
}

#[derive(Clone, Debug)]
struct RootTrustEvidenceRefresh {
    path: PathBuf,
    marker_identity: String,
    expected_mount_generation: u64,
    original_reason: RootTrustReason,
}

#[derive(Clone, Debug)]
enum RootTrustCommandStart {
    Pending(PendingRootTrustScan),
    Unavailable(RootTrustEvidenceRefresh),
}

#[derive(Clone, Debug)]
struct CompletedRootTrustCommand {
    path: PathBuf,
    reason: RootTrustReason,
    outcome: RootTrustOutcome,
}

async fn emit_root_trust_finished(
    tx: &async_channel::Sender<LibraryEvent>,
    request_id: Uuid,
    path: PathBuf,
    reason: RootTrustReason,
    outcome: RootTrustOutcome,
) {
    let _ = tx
        .send(LibraryEvent::RootTrustFinished {
            request_id,
            path,
            reason,
            outcome,
        })
        .await;
}

async fn stage_root_trust(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    request: &RootTrustRequest,
) -> Result<String, RootTrustError> {
    if !music_dirs.iter().any(|root| root == &request.path) {
        return Err(RootTrustError::Stale(
            "library root is no longer exactly configured",
        ));
    }

    let state_key = request.path.to_string_lossy().into_owned();
    let state = library_root::Entity::find_by_id(state_key.clone())
        .one(db)
        .await
        .map_err(|error| RootTrustError::Failed(error.into()))?
        .ok_or(RootTrustError::Stale(
            "persisted library root state disappeared",
        ))?;
    if !request.expected_state.matches(&state) {
        return Err(RootTrustError::Stale(
            "persisted library root authorization changed after prompting",
        ));
    }

    let marker_request = request.clone();
    let marker_identity =
        tokio::task::spawn_blocking(move || establish_explicit_root_marker(&marker_request))
            .await
            .map_err(|error| RootTrustError::Failed(error.into()))??;

    // Marker creation is intentionally outside SQLite. Recheck filesystem
    // evidence immediately before an atomic compare-and-swap that binds every
    // security-relevant persisted field from the prompt. Timestamp-only drift
    // is intentionally excluded from that predicate.
    let probe_path = request.path.clone();
    let expected_marker = marker_identity.clone();
    let expected_generation = request.observed_mount_generation;
    let (identity_stable, generation_stable) = tokio::task::spawn_blocking(move || {
        (
            filesystem_identity(&probe_path).is_ok_and(|identity| identity == expected_marker),
            root_mount_generation(&probe_path)
                .is_ok_and(|generation| generation == expected_generation),
        )
    })
    .await
    .map_err(|error| RootTrustError::Failed(error.into()))?;
    if !identity_stable || !generation_stable {
        return Err(RootTrustError::Stale(
            "library root changed before confirmation could be persisted",
        ));
    }

    let active = library_root::ActiveModel {
        device_id: Set(Some(marker_identity.clone())),
        identity_confirmed: Set(false),
        is_available: Set(false),
        last_scan_complete: Set(false),
        last_checked_at: Set(Utc::now().to_rfc3339()),
        ..Default::default()
    };

    let update = library_root::Entity::update_many()
        .set(active)
        .filter(library_root::Column::Path.eq(state_key))
        .filter(
            library_root::Column::IdentityConfirmed.eq(request.expected_state.identity_confirmed),
        )
        .filter(library_root::Column::IsAvailable.eq(request.expected_state.is_available))
        .filter(
            library_root::Column::LastScanComplete.eq(request.expected_state.last_scan_complete),
        );
    let update = match request.expected_state.device_id.as_deref() {
        Some(expected) => update.filter(library_root::Column::DeviceId.eq(expected)),
        None => update.filter(library_root::Column::DeviceId.is_null()),
    };
    let result = update
        .exec(db)
        .await
        .map_err(|error| RootTrustError::Failed(error.into()))?;
    if result.rows_affected != 1 {
        return Err(RootTrustError::Stale(
            "persisted library root authorization changed during confirmation",
        ));
    }

    Ok(marker_identity)
}

async fn root_trust_scan_state_matches(
    db: &DatabaseConnection,
    path: &Path,
    marker_identity: &str,
    expected_mount_generation: u64,
    expected_available: bool,
) -> anyhow::Result<bool> {
    let state = library_root::Entity::find_by_id(path.to_string_lossy().into_owned())
        .one(db)
        .await?;
    let state_matches = state.is_some_and(|state| {
        state.device_id.as_deref() == Some(marker_identity)
            && state.identity_confirmed
            && state.is_available == expected_available
            && state.last_scan_complete
    });
    if !state_matches {
        return Ok(false);
    }

    let path = path.to_path_buf();
    let marker_identity = marker_identity.to_string();
    tokio::task::spawn_blocking(move || {
        filesystem_identity(&path).is_ok_and(|identity| identity == marker_identity)
            && root_mount_generation(&path)
                .is_ok_and(|generation| generation == expected_mount_generation)
    })
    .await
    .map_err(Into::into)
}

async fn gate_root_trust_authoritative_scan(
    db: &DatabaseConnection,
    pending: &PendingRootTrustScan,
) -> Result<bool, sea_orm::DbErr> {
    let active = library_root::ActiveModel {
        is_available: Set(false),
        last_scan_complete: Set(false),
        last_checked_at: Set(Utc::now().to_rfc3339()),
        ..Default::default()
    };
    let result = library_root::Entity::update_many()
        .set(active)
        .filter(library_root::Column::Path.eq(pending.path.to_string_lossy().into_owned()))
        .filter(library_root::Column::DeviceId.eq(pending.marker_identity.as_str()))
        .filter(library_root::Column::IdentityConfirmed.eq(true))
        .filter(library_root::Column::IsAvailable.eq(false))
        .filter(library_root::Column::LastScanComplete.eq(true))
        .exec(db)
        .await?;
    Ok(result.rows_affected == 1)
}

async fn begin_root_trust_command(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    request: &RootTrustRequest,
) -> Result<RootTrustCommandStart, RootTrustError> {
    let marker_identity = stage_root_trust(db, music_dirs, request).await?;
    let unavailable_refresh = RootTrustEvidenceRefresh {
        path: request.path.clone(),
        marker_identity: marker_identity.clone(),
        expected_mount_generation: request.observed_mount_generation,
        original_reason: request.reason,
    };
    let forced_conversions = HashMap::from([(
        request.path.clone(),
        ForcedRootTrustConversion {
            marker_identity: marker_identity.clone(),
            expected_empty: request.requires_empty_acknowledgement,
            expected_mount_generation: request.observed_mount_generation,
            original_reason: request.reason,
        },
    )]);
    let authority_guards = HashMap::new();
    let evidence_refreshes = HashMap::new();
    if let Err(error) = initial_scan_with_root_trust_guards(
        db,
        music_dirs,
        tx,
        &forced_conversions,
        &authority_guards,
        &evidence_refreshes,
    )
    .await
    {
        warn!(root = %request.path.display(), %error, "Explicit library root conversion scan failed");
        // Staging and every forced-conversion persistence path are already
        // unavailable. Preserve that state rather than risking a stale prompt
        // by rewriting its completeness token after the scan.
        return Ok(RootTrustCommandStart::Unavailable(unavailable_refresh));
    }

    match root_trust_scan_state_matches(
        db,
        &request.path,
        &marker_identity,
        request.observed_mount_generation,
        false,
    )
    .await
    {
        Ok(true) => Ok(RootTrustCommandStart::Pending(PendingRootTrustScan {
            request_id: request.request_id,
            path: request.path.clone(),
            reason: request.reason,
            marker_identity,
            expected_mount_generation: request.observed_mount_generation,
        })),
        Ok(false) => Ok(RootTrustCommandStart::Unavailable(unavailable_refresh)),
        Err(error) => {
            warn!(root = %request.path.display(), %error, "Could not verify converted library root state");
            Ok(RootTrustCommandStart::Unavailable(unavailable_refresh))
        }
    }
}

async fn complete_root_trust_scan(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    pending: &PendingRootTrustScan,
) -> RootTrustOutcome {
    match gate_root_trust_authoritative_scan(db, pending).await {
        Ok(true) => {}
        Ok(false) => {
            mark_root_path_unavailable(db, &pending.path).await;
            return RootTrustOutcome::TrustedButUnavailable;
        }
        Err(error) => {
            warn!(root = %pending.path.display(), %error, "Could not establish unavailable state before authoritative library scan");
            mark_root_path_unavailable(db, &pending.path).await;
            return RootTrustOutcome::TrustedButUnavailable;
        }
    }

    let forced_conversions = HashMap::new();
    let authority_guards = HashMap::from([(
        pending.path.clone(),
        RootTrustAuthorityGuard {
            marker_identity: pending.marker_identity.clone(),
            expected_mount_generation: pending.expected_mount_generation,
        },
    )]);
    let evidence_refreshes = HashMap::new();
    if let Err(error) = initial_scan_with_root_trust_guards(
        db,
        music_dirs,
        tx,
        &forced_conversions,
        &authority_guards,
        &evidence_refreshes,
    )
    .await
    {
        warn!(root = %pending.path.display(), %error, "Post-enrollment authoritative library scan failed");
        mark_root_path_unavailable(db, &pending.path).await;
        return RootTrustOutcome::TrustedButUnavailable;
    }

    match root_trust_scan_state_matches(
        db,
        &pending.path,
        &pending.marker_identity,
        pending.expected_mount_generation,
        true,
    )
    .await
    {
        Ok(true) => RootTrustOutcome::Active,
        Ok(false) => {
            mark_root_path_unavailable_if_active(db, &pending.path).await;
            RootTrustOutcome::TrustedButUnavailable
        }
        Err(error) => {
            warn!(root = %pending.path.display(), %error, "Could not verify post-enrollment library state");
            mark_root_path_unavailable_if_active(db, &pending.path).await;
            RootTrustOutcome::TrustedButUnavailable
        }
    }
}

async fn finish_root_trust_command(
    tx: &async_channel::Sender<LibraryEvent>,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
    request_id: Uuid,
    completion: CompletedRootTrustCommand,
) {
    if completion.outcome == RootTrustOutcome::Active {
        completed_commands.insert(request_id, completion.clone());
    } else {
        // A deterministic request ID identifies evidence, not a one-shot
        // attempt. Transient failures and stale/unavailable outcomes must be
        // retryable while that evidence remains unchanged.
        completed_commands.remove(&request_id);
    }
    emit_root_trust_finished(
        tx,
        request_id,
        completion.path,
        completion.reason,
        completion.outcome,
    )
    .await;
}

async fn refresh_root_trust_evidence(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
) {
    // A legacy confirmation can create its random marker immediately before
    // a database failure. The marker is deliberately not treated as proof of
    // the earlier command: rescan it as new evidence and require a fresh
    // adoption request instead.
    if let Err(error) = initial_scan(db, music_dirs, tx).await {
        warn!(%error, "Could not refresh library root trust evidence after a rejected command");
    }
}

async fn refresh_unavailable_root_trust_evidence(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    refresh: RootTrustEvidenceRefresh,
) {
    // A staged command can finish unavailable without producing a watcher
    // event (notably when an existing marker was adopted). Run a fail-closed,
    // no-content evidence pass so the UI receives a retryable request for the
    // current observation. The command's mount generation remains pinned
    // during this refresh; an intervening mount is classified as replacement.
    let forced_conversions = HashMap::new();
    let authority_guards = HashMap::new();
    let evidence_refreshes = HashMap::from([(refresh.path.clone(), refresh)]);
    if let Err(error) = initial_scan_with_root_trust_guards(
        db,
        music_dirs,
        tx,
        &forced_conversions,
        &authority_guards,
        &evidence_refreshes,
    )
    .await
    {
        warn!(%error, "Could not refresh library root trust evidence after an unavailable command");
    }
}

async fn process_library_command(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
    command: LibraryCommand,
) -> Option<PendingRootTrustScan> {
    let LibraryCommand::ConfirmRootTrust(request) = command;

    if let Some(completion) = completed_commands.get(&request.request_id).cloned() {
        emit_root_trust_finished(
            tx,
            request.request_id,
            completion.path,
            completion.reason,
            completion.outcome,
        )
        .await;
        return None;
    }

    match begin_root_trust_command(db, music_dirs, tx, &request).await {
        Ok(RootTrustCommandStart::Pending(pending)) => Some(pending),
        Ok(RootTrustCommandStart::Unavailable(refresh)) => {
            finish_root_trust_command(
                tx,
                completed_commands,
                request.request_id,
                CompletedRootTrustCommand {
                    path: request.path,
                    reason: request.reason,
                    outcome: RootTrustOutcome::TrustedButUnavailable,
                },
            )
            .await;
            refresh_unavailable_root_trust_evidence(db, music_dirs, tx, refresh).await;
            None
        }
        Err(RootTrustError::Stale(message)) => {
            warn!(
                root = %request.path.display(),
                request_id = %request.request_id,
                %message,
                "Rejected stale library root trust command"
            );
            finish_root_trust_command(
                tx,
                completed_commands,
                request.request_id,
                CompletedRootTrustCommand {
                    path: request.path,
                    reason: request.reason,
                    outcome: RootTrustOutcome::Stale,
                },
            )
            .await;
            refresh_root_trust_evidence(db, music_dirs, tx).await;
            None
        }
        Err(RootTrustError::Failed(error)) => {
            warn!(
                root = %request.path.display(),
                request_id = %request.request_id,
                %error,
                "Library root trust command failed"
            );
            finish_root_trust_command(
                tx,
                completed_commands,
                request.request_id,
                CompletedRootTrustCommand {
                    path: request.path,
                    reason: request.reason,
                    outcome: RootTrustOutcome::Failed,
                },
            )
            .await;
            refresh_root_trust_evidence(db, music_dirs, tx).await;
            None
        }
    }
}

async fn finish_pending_root_trust_scan(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
    pending: PendingRootTrustScan,
) {
    let outcome = complete_root_trust_scan(db, music_dirs, tx, &pending).await;
    finish_root_trust_command(
        tx,
        completed_commands,
        pending.request_id,
        CompletedRootTrustCommand {
            path: pending.path,
            reason: pending.reason,
            outcome,
        },
    )
    .await;
}

async fn process_library_commands_without_watcher(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    command_rx: &async_channel::Receiver<LibraryCommand>,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
) {
    let mut pending_trust_scan = None;
    loop {
        // A successful conversion is deliberately followed by a distinct
        // ordinary scan at the next engine-loop boundary. This is the first
        // point where the newly confirmed marker may authorize content writes
        // and stale deletion.
        if let Some(pending) = pending_trust_scan.take() {
            finish_pending_root_trust_scan(db, music_dirs, tx, completed_commands, pending).await;
            continue;
        }

        let Ok(command) = command_rx.recv().await else {
            break;
        };
        pending_trust_scan =
            process_library_command(db, music_dirs, tx, completed_commands, command).await;
    }
}

async fn initial_scan(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
) -> anyhow::Result<()> {
    let forced_conversions = HashMap::new();
    let authority_guards = HashMap::new();
    let evidence_refreshes = HashMap::new();
    initial_scan_with_root_trust_guards(
        db,
        music_dirs,
        tx,
        &forced_conversions,
        &authority_guards,
        &evidence_refreshes,
    )
    .await
}

async fn initial_scan_with_root_trust_guards(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    forced_conversions: &HashMap<PathBuf, ForcedRootTrustConversion>,
    authority_guards: &HashMap<PathBuf, RootTrustAuthorityGuard>,
    evidence_refreshes: &HashMap<PathBuf, RootTrustEvidenceRefresh>,
) -> anyhow::Result<()> {
    // The explicit conversion pass establishes only root identity. Track
    // upserts and deletions are reserved for the separate ordinary scan that
    // the engine schedules at its next loop boundary.
    let content_mutations_allowed = forced_conversions.is_empty() && evidence_refreshes.is_empty();
    let mut configured_dirs = music_dirs.to_vec();
    configured_dirs.sort_unstable();
    configured_dirs.dedup();

    // Load persisted roots before enumeration so previously-seen nested
    // mounts remain independent reconciliation scopes even while unmounted.
    let existing_tracks = track::Entity::find().all(db).await?;
    let persisted_roots = library_root::Entity::find().all(db).await?;
    let dirs = expanded_scan_roots(&configured_dirs, &persisted_roots).map_err(|error| {
        anyhow::anyhow!(
            "failed to inspect mounted library scopes; scan disabled to protect metadata: {error}"
        )
    })?;
    let all_roots = dirs.clone();

    // Collect a separate traversal result for every root. Never infer scan
    // completeness from the number of audio files: a healthy empty directory
    // is authoritative, while even a single WalkDir error makes that root's
    // view incomplete and therefore unsafe for stale deletion.
    let mut root_scans = tokio::task::spawn_blocking(move || {
        dirs.into_iter()
            .map(|root| scan_root_with_exclusions(root, &all_roots))
            .collect::<Vec<_>>()
    })
    .await?;

    // Preload existing rows once so the per-file loop can decide needs_update
    // from memory instead of issuing one SELECT per file. The same snapshot is
    // reused for the stale-removal pass below.
    let existing_by_path: HashMap<&str, &track::Model> = existing_tracks
        .iter()
        .map(|model| (model.file_path.as_str(), model))
        .collect();
    let persisted_by_path: HashMap<&str, &library_root::Model> = persisted_roots
        .iter()
        .map(|state| (state.path.as_str(), state))
        .collect();
    let evidence_roots: Vec<PathBuf> = root_scans.iter().map(|scan| scan.root.clone()).collect();
    let mut trust_requests = Vec::new();

    // Marker creation happens only for roots the user explicitly configured.
    // Always discard the pre-marker traversal and rescan through the newly
    // created marker before deciding whether any content may be trusted.
    for scan in &mut root_scans {
        let root_path = scan.root.to_string_lossy();
        let previous = persisted_by_path.get(root_path.as_ref()).copied();
        let existing_track_count = existing_tracks
            .iter()
            .filter(|row| {
                most_specific_root_for_path(Path::new(&row.file_path), &evidence_roots)
                    == Some(scan.root.as_path())
            })
            .count();
        let explicitly_configured = configured_dirs.binary_search(&scan.root).is_ok();

        let RootIdentityPreparation::MarkerCreated { identity } = prepare_durable_root_identity(
            scan,
            previous,
            existing_track_count,
            explicitly_configured,
        ) else {
            continue;
        };

        let root = scan.root.clone();
        let exclusions = evidence_roots.clone();
        let mut marker_scan =
            tokio::task::spawn_blocking(move || scan_root_with_exclusions(root, &exclusions))
                .await?;
        if marker_scan.device_id.as_deref() != Some(identity.as_str()) {
            marker_scan.errors.push(format!(
                "library root marker changed before marker-backed rescan completed: {}",
                marker_scan.root.display()
            ));
        }
        *scan = marker_scan;
    }

    // Detect copies only after enrollment/rescans so markers created during
    // this scan participate in the same fail-closed duplicate check.
    reject_duplicate_marker_identities(&mut root_scans);

    for scan in &root_scans {
        if scan.is_complete() {
            debug!(
                root = %scan.root.display(),
                files = scan.audio_files.len(),
                "Library root traversal complete"
            );
        } else {
            warn!(
                root = %scan.root.display(),
                errors = scan.errors.len(),
                "Library root traversal incomplete — stale deletion disabled"
            );
            for traversal_error in &scan.errors {
                warn!(root = %scan.root.display(), error = %traversal_error, "Library traversal error");
            }
        }
    }

    for scan in &mut root_scans {
        let root_path = scan.root.to_string_lossy();
        let previous = persisted_by_path.get(root_path.as_ref()).copied();
        let existing_track_count = existing_tracks
            .iter()
            .filter(|row| {
                most_specific_root_for_path(Path::new(&row.file_path), &evidence_roots)
                    == Some(scan.root.as_path())
            })
            .count();
        let forced_conversion = forced_conversions.get(&scan.root);
        let forced_storage_matches = forced_conversion.is_some_and(|expected| {
            scan.is_complete()
                && scan.device_id.as_deref() == Some(expected.marker_identity.as_str())
                && scan.mount_generation == Some(expected.expected_mount_generation)
        });
        let forced_evidence_matches = forced_conversion.is_some_and(|expected| {
            forced_storage_matches && scan.audio_files.is_empty() == expected.expected_empty
        });
        // A requested conversion remains a non-authoritative, no-content pass
        // even when its current evidence no longer matches the prompt.
        let conversion_scan = forced_conversion.is_some();
        let authority_guard = authority_guards.get(&scan.root);
        let authority_storage_matches = authority_guard.is_some_and(|expected| {
            scan.device_id.as_deref() == Some(expected.marker_identity.as_str())
                && scan.mount_generation == Some(expected.expected_mount_generation)
        });
        let authority_guard_matches = scan.is_complete() && authority_storage_matches;
        let authority_storage_mismatch =
            authority_guard.is_some() && scan.is_complete() && !authority_storage_matches;
        let evidence_refresh = evidence_refreshes.get(&scan.root);
        let refresh_storage_matches = evidence_refresh.is_some_and(|expected| {
            scan.device_id.as_deref() == Some(expected.marker_identity.as_str())
                && scan.mount_generation == Some(expected.expected_mount_generation)
        });
        let activation_allowed = !conversion_scan
            && evidence_refresh.is_none()
            && (authority_guard.is_none() || authority_guard_matches);
        // Traversal completeness is not storage evidence. Preserve a durable
        // confirmation across a transient incomplete walk, but revoke it when
        // a complete guarded observation proves the marker or mount changed.
        // An unavailable-command refresh deliberately returns a complete
        // observation to the explicit-consent state so it can be retried.
        let demote_identity =
            authority_storage_mismatch || (evidence_refresh.is_some() && scan.is_complete());
        let explicitly_configured = configured_dirs.binary_search(&scan.root).is_ok();
        let confirms_identity = if conversion_scan {
            forced_evidence_matches
        } else if evidence_refresh.is_some()
            || (authority_guard.is_some() && !authority_guard_matches)
        {
            false
        } else {
            scan_confirms_identity_for_scope(
                scan,
                previous,
                existing_track_count,
                explicitly_configured,
            )
        };
        scan.reconciliation_authoritative =
            activation_allowed && reconciliation_is_authoritative(scan, previous);
        let mut trust_reason = root_trust_reason(
            scan,
            previous,
            existing_track_count,
            explicitly_configured,
            confirms_identity,
        );
        if conversion_scan && scan.is_complete() && !forced_evidence_matches {
            trust_reason = Some(
                if !forced_storage_matches
                    || forced_conversion.is_some_and(|expected| {
                        expected.original_reason == RootTrustReason::Replacement
                    })
                {
                    RootTrustReason::Replacement
                } else if scan.audio_files.is_empty() {
                    RootTrustReason::EmptyRoot
                } else {
                    RootTrustReason::LegacyEnrollment
                },
            );
        } else if let Some(refresh) = evidence_refresh.filter(|_| scan.is_complete()) {
            trust_reason = Some(
                if !refresh_storage_matches
                    || refresh.original_reason == RootTrustReason::Replacement
                {
                    RootTrustReason::Replacement
                } else if scan.audio_files.is_empty() {
                    RootTrustReason::EmptyRoot
                } else {
                    RootTrustReason::LegacyEnrollment
                },
            );
        } else if authority_storage_mismatch {
            trust_reason = Some(RootTrustReason::Replacement);
        }

        if !scan.reconciliation_authoritative && scan.is_complete() {
            warn!(
                root = %scan.root.display(),
                observed_device_id = ?scan.device_id,
                expected_device_id = ?previous.and_then(|state| state.device_id.as_deref()),
                "Library root identity is unestablished or changed — stale deletion disabled"
            );
        }
        if confirms_identity && !previous.is_some_and(|state| state.identity_confirmed) {
            info!(root = %scan.root.display(), device_id = ?scan.device_id, "Established library root identity for future reconciliation");
        }
        if forced_evidence_matches {
            info!(root = %scan.root.display(), device_id = ?scan.device_id, "Converted legacy root identity; deletion deferred until the next complete scan");
        } else if let Some(expected) = forced_conversion.filter(|_| scan.is_complete()) {
            warn!(
                root = %scan.root.display(),
                original_reason = ?expected.original_reason,
                expected_mount_generation = expected.expected_mount_generation,
                observed_mount_generation = ?scan.mount_generation,
                expected_empty = expected.expected_empty,
                observed_empty = scan.audio_files.is_empty(),
                "Library root evidence changed after confirmation; fresh trust required"
            );
        } else if let Some(expected) = evidence_refresh.filter(|_| scan.is_complete()) {
            warn!(
                root = %scan.root.display(),
                original_reason = ?expected.original_reason,
                expected_mount_generation = expected.expected_mount_generation,
                observed_mount_generation = ?scan.mount_generation,
                storage_matches = refresh_storage_matches,
                "Refreshed evidence after unavailable library root confirmation"
            );
        } else if let Some(expected) = authority_guard.filter(|_| authority_storage_mismatch) {
            warn!(
                root = %scan.root.display(),
                expected_mount_generation = expected.expected_mount_generation,
                observed_mount_generation = ?scan.mount_generation,
                "Library root mount changed before authoritative reconciliation; fresh trust required"
            );
        }

        // If availability state cannot be persisted, fail closed for this
        // scan: retaining stale metadata is safer than deleting it without a
        // durable device identity for the next startup.
        match persist_root_scan_status(
            db,
            scan,
            previous,
            confirms_identity,
            activation_allowed,
            demote_identity,
        )
        .await
        {
            Ok(stored) => {
                scan.content_authorized = content_mutations_allowed
                    && activation_allowed
                    && (scan.reconciliation_authoritative
                        || (confirms_identity && !conversion_scan));
                if let Some(reason) = trust_reason {
                    if let Some(request) =
                        build_root_trust_request(scan, &stored, reason, existing_track_count)
                    {
                        trust_requests.push(request);
                    }
                }
            }
            Err(error) => {
                warn!(root = %scan.root.display(), %error, "Failed to persist library root state");
                scan.reconciliation_authoritative = false;
                scan.content_authorized = false;
                if error.downcast_ref::<AuthorityTaskJoinFailure>().is_none() {
                    mark_root_path_unavailable_if_active(db, &scan.root).await;
                }
            }
        }
        if !scan.content_authorized && !scan.audio_files.is_empty() {
            warn!(root = %scan.root.display(), "Ignoring files from an unconfirmed or changed library root");
        }
    }

    let audio_files = collect_audio_files(&root_scans);
    let total = audio_files.len() as u64;
    info!(total, "Found authorized audio files to scan");

    let mut scanned: u64 = 0;
    let mut on_disk_paths = HashSet::new();

    for path in &audio_files {
        let path_str = path.to_string_lossy().to_string();
        on_disk_paths.insert(path_str.clone());

        // Look up the existing row (if any) in the preloaded map.
        let existing = existing_by_path.get(path_str.as_str()).copied();

        let needs_update = match existing {
            // Compare FS mtime with stored date_modified.
            Some(row) => get_mtime(path) != row.date_modified,
            None => true,
        };

        if needs_update {
            let (identity_allows_parse, invalidated_root) = match revalidate_scan_root_for_path(
                path,
                &mut root_scans,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    warn!(path = %path.display(), %error, "Initial-scan authority validation task failed — upsert discarded");
                    continue;
                }
            };
            if let Some(root) = invalidated_root {
                mark_root_path_unavailable(db, &root).await;
                warn!(root = %root.display(), path = %path.display(), "Library root changed before parsing — remaining initial-scan writes disabled");
            }
            if !identity_allows_parse {
                continue;
            }

            let Some(authority_lease) =
                root_scan_for_path(path, &root_scans).and_then(|scan| scan.authority_lease.clone())
            else {
                warn!(path = %path.display(), "Authorized audio path has no retained root authority — upsert discarded");
                continue;
            };
            let open_lease = authority_lease.clone();
            let open_path = path.clone();
            let (observed_file, parse_file) = match spawn_authority_probe(move || {
                let observed_file = Arc::new(open_lease.open_regular_file(&open_path)?);
                let parse_file = observed_file.try_clone_file()?;
                Ok::<_, std::io::Error>((observed_file, parse_file))
            })
            .await
            {
                Ok(Ok(opened)) => opened,
                Ok(Err(error)) => {
                    warn!(path = %path.display(), %error, "Audio file could not be opened and cloned through retained root authority — upsert discarded");
                    continue;
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "Initial-scan audio authority task failed — upsert discarded");
                    continue;
                }
            };

            let p = path.clone();
            let parse_result = tokio::task::spawn_blocking(move || {
                tag_parser::parse_audio_file_from_file(parse_file, &p)
            })
            .await;

            match parse_result {
                Ok(Ok(parsed)) => {
                    let (identity_allows_upsert, invalidated_root) =
                        match revalidate_scan_root_for_path(path, &mut root_scans).await {
                            Ok(result) => result,
                            Err(error) => {
                                warn!(path = %path.display(), %error, "Post-parse authority validation task failed — upsert discarded");
                                continue;
                            }
                        };
                    if let Some(root) = invalidated_root {
                        mark_root_path_unavailable(db, &root).await;
                        warn!(root = %root.display(), path = %path.display(), "Library root changed while parsing — remaining initial-scan writes disabled");
                    }
                    if !identity_allows_upsert {
                        continue;
                    }

                    // During the initial scan we do NOT emit a TrackUpserted
                    // per file: the single FullSync below delivers the complete
                    // snapshot, avoiding O(n^2) UI work plus a full track-list
                    // clone per event. The watcher still emits TrackUpserted for
                    // incremental changes, where it is cheap.
                    let mut invalidated_root = None;
                    let mut file_still_current = false;
                    let mut authority_task_failed = false;
                    match upsert_track_with_commit_guard(db, &parsed, existing, || async {
                        let guard_file = observed_file.clone();
                        let guard_lease = authority_lease.clone();
                        file_still_current = match spawn_authority_probe(move || {
                            guard_file.validate(&guard_lease).is_ok()
                        })
                        .await
                        {
                            Ok(still_current) => still_current,
                            Err(_) => {
                                authority_task_failed = true;
                                false
                            }
                        };
                        let root_still_current =
                            match revalidate_scan_root_for_path(path, &mut root_scans).await {
                                Ok((still_current, invalidated)) => {
                                    invalidated_root = invalidated;
                                    still_current
                                }
                                Err(_) => {
                                    authority_task_failed = true;
                                    false
                                }
                            };
                        root_still_current && file_still_current
                    })
                    .await
                    {
                        Ok(GuardedTrackUpsertOutcome::Committed(_)) => {}
                        Ok(GuardedTrackUpsertOutcome::GuardRejected) => {
                            if authority_task_failed {
                                warn!(path = %path.display(), "Initial-upsert authority validation task failed — transaction rolled back");
                            } else if let Some(root) = invalidated_root {
                                mark_root_path_unavailable(db, &root).await;
                                warn!(root = %root.display(), path = %path.display(), "Library root changed before initial upsert commit — remaining writes disabled");
                            } else if !file_still_current {
                                warn!(path = %path.display(), "Audio file changed before initial upsert commit — transaction rolled back");
                            }
                        }
                        Err(error) => {
                            warn!(path = %path_str, %error, "Failed to upsert track transactionally");
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(path = %path_str, error = %e, "Skipping unparseable file");
                }
                Err(e) => {
                    warn!(path = %path_str, error = %e, "spawn_blocking failed");
                }
            }
        }

        scanned += 1;
        if scanned % 50 == 0 || scanned == total {
            let _ = tx.send(LibraryEvent::ScanProgress(scanned, total)).await;
        }
    }

    // Parsing can outlive a removable-media transition. Revalidate each
    // retained root object immediately before the destructive phase; a
    // changed, removed, copied-marker, or unreadable root disables every stale
    // deletion for that scope.
    for scan in &mut root_scans {
        if !scan.reconciliation_authoritative {
            continue;
        }
        let expected = scan.device_id.clone();
        let authority_still_matches = if let Some(lease) = scan.authority_lease.clone() {
            match spawn_authority_probe(move || {
                expected.as_deref() == Some(lease.expected_marker()) && lease.validate().is_ok()
            })
            .await
            {
                Ok(matches) => matches,
                Err(error) => {
                    // Reject reconciliation for this scan, but a task failure
                    // is not evidence that the persisted root changed.
                    scan.reconciliation_authoritative = false;
                    scan.content_authorized = false;
                    warn!(root = %scan.root.display(), %error, "Library-root authority validation task failed before reconciliation — stale deletion disabled");
                    continue;
                }
            }
        } else {
            false
        };
        if authority_still_matches {
            continue;
        }
        scan.reconciliation_authoritative = false;
        scan.content_authorized = false;
        // This root may have been enrolled during the current scan and thus
        // be absent from the pre-scan snapshot. Reload by exact path so the
        // just-persisted available row is revoked as well.
        mark_root_path_unavailable(db, &scan.root).await;
        warn!(root = %scan.root.display(), "Library root authority changed before reconciliation — stale deletion disabled");
    }

    // Remove DB entries for files no longer on disk. Reuse the preloaded
    // snapshot instead of re-querying. A failed individual delete is logged and
    // skipped rather than aborting the whole scan, so a transient DB hiccup
    // can't discard the FullSync/ScanComplete that follow.
    if content_mutations_allowed {
        for row in &existing_tracks {
            let row_path = Path::new(&row.file_path);
            if !should_remove_stale_track(row_path, &on_disk_paths, &root_scans) {
                continue;
            }
            let Some(authority_lease) = root_scan_for_path(row_path, &root_scans)
                .and_then(|scan| scan.authority_lease.clone())
            else {
                warn!(path = %row.file_path, "Stale candidate has no retained root authority — preserving metadata");
                continue;
            };
            let proof_lease = authority_lease.clone();
            let proof_path = row_path.to_path_buf();
            let absence = match spawn_authority_probe(move || {
                proof_lease.prove_absent(&proof_path).map(Arc::new)
            })
            .await
            {
                Ok(Ok(proof)) => proof,
                Ok(Err(error)) => {
                    warn!(path = %row.file_path, %error, "Could not prove stale candidate absent through retained root authority — preserving metadata");
                    continue;
                }
                Err(error) => {
                    warn!(path = %row.file_path, %error, "Stale-candidate authority task failed — preserving metadata");
                    continue;
                }
            };
            let (root_allows_delete, invalidated_root) = match revalidate_scan_root_for_path(
                row_path,
                &mut root_scans,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    warn!(path = %row.file_path, %error, "Stale-delete root validation task failed — preserving metadata");
                    continue;
                }
            };
            if let Some(root) = invalidated_root {
                mark_root_path_unavailable(db, &root).await;
            }
            if !root_allows_delete {
                continue;
            }
            info!(path = %row.file_path, "Removing stale track from database");
            let mut invalidated_root = None;
            let mut absence_still_current = false;
            let mut authority_task_failed = false;
            match delete_track_with_commit_guard(db, &row.id, || async {
                let guard_absence = absence.clone();
                let guard_lease = authority_lease.clone();
                absence_still_current = match spawn_authority_probe(move || {
                    guard_absence.validate(&guard_lease).is_ok()
                })
                .await
                {
                    Ok(still_current) => still_current,
                    Err(_) => {
                        authority_task_failed = true;
                        false
                    }
                };
                let root_still_current =
                    match revalidate_scan_root_for_path(row_path, &mut root_scans).await {
                        Ok((still_current, invalidated)) => {
                            invalidated_root = invalidated;
                            still_current
                        }
                        Err(_) => {
                            authority_task_failed = true;
                            false
                        }
                    };
                root_still_current && absence_still_current
            })
            .await
            {
                Ok(GuardedTrackDeleteOutcome::Deleted) => {
                    let _ = tx
                        .send(LibraryEvent::TrackRemoved(row.file_path.clone()))
                        .await;
                }
                Ok(GuardedTrackDeleteOutcome::Missing) => {}
                Ok(GuardedTrackDeleteOutcome::GuardRejected) => {
                    if authority_task_failed {
                        warn!(path = %row.file_path, "Stale-delete authority validation task failed — transaction rolled back");
                    } else if let Some(root) = invalidated_root {
                        mark_root_path_unavailable(db, &root).await;
                        warn!(root = %root.display(), path = %row.file_path, "Library root changed before stale deletion commit — remaining deletions disabled");
                    } else if !absence_still_current {
                        warn!(path = %row.file_path, "Stale candidate absence proof changed before commit — deletion rolled back");
                    }
                }
                Err(error) => {
                    warn!(path = %row.file_path, %error, "Failed to remove stale track transactionally");
                }
            }
        }
    }

    // Send full sync. A transient failure here is logged but still lets the
    // scan finish (reconcile + ScanComplete) so the UI settles into a synced
    // state instead of hanging on the spinner with no completion signal.
    send_library_snapshot(db, tx).await;

    // Reconcile orphaned playlist entries with newly-discovered tracks.
    let playlist_mgr = super::playlist_manager::PlaylistManager::new(db.clone());
    match playlist_mgr.reconcile_all().await {
        Ok(n) if n > 0 => info!(relinked = n, "Playlist entries reconciled after scan"),
        Ok(_) => debug!("No orphaned playlist entries to reconcile"),
        Err(e) => warn!(error = %e, "Playlist reconciliation failed"),
    }

    // Send playlist list to UI thread for sidebar population.
    // If no playlists exist yet, seed the default smart playlists.
    match playlist_mgr.list_playlists().await {
        Ok(playlists) => {
            let playlists = if playlists.is_empty() {
                info!("No playlists found — seeding defaults");
                match playlist_mgr.seed_defaults().await {
                    Ok(defaults) => defaults,
                    Err(e) => {
                        warn!(error = %e, "Failed to seed default playlists");
                        Vec::new()
                    }
                }
            } else {
                playlists
            };

            let entries: Vec<(String, String, bool)> = playlists
                .iter()
                .map(|p| (p.id.clone(), p.name.clone(), p.is_smart))
                .collect();
            if !entries.is_empty() {
                info!(count = entries.len(), "Sending playlists to UI");
            }
            let _ = tx.send(LibraryEvent::PlaylistsLoaded(entries)).await;
        }
        Err(e) => warn!(error = %e, "Failed to load playlists"),
    }
    let _ = tx.send(LibraryEvent::PlaylistProjectionsInvalidated).await;

    if !trust_requests.is_empty() {
        let _ = tx
            .send(LibraryEvent::RootTrustRequired(trust_requests))
            .await;
    }
    let _ = tx.send(LibraryEvent::ScanComplete).await;

    info!(scanned, "Initial scan complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Filesystem watcher
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct WatcherRootEntry {
    root: PathBuf,
    state: library_root::Model,
    authority_lease: Option<Arc<RootAuthorityLease>>,
}

#[derive(Debug)]
struct WatcherRootCache {
    entries: Vec<WatcherRootEntry>,
    authority_lost: bool,
}

enum WatcherFileBinding {
    Bound {
        file: Arc<BoundFile>,
        parser_file: File,
    },
    Absent,
    Rejected {
        error: std::io::Error,
        authority_stable: bool,
    },
}

impl WatcherRootCache {
    fn from_models(states: Vec<library_root::Model>, music_dirs: &[PathBuf]) -> Self {
        let mut entries: Vec<WatcherRootEntry> = states
            .into_iter()
            .filter_map(|state| {
                let root = PathBuf::from(&state.path);
                music_dirs
                    .iter()
                    .any(|configured| root.starts_with(configured))
                    .then_some(WatcherRootEntry {
                        root,
                        state,
                        authority_lease: None,
                    })
            })
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.root.components().count()));
        Self {
            entries,
            authority_lost: false,
        }
    }

    async fn load(db: &DatabaseConnection, music_dirs: &[PathBuf]) -> anyhow::Result<Self> {
        let states = library_root::Entity::find().all(db).await?;
        let music_dirs = music_dirs.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut cache = Self::from_models(states, &music_dirs);
            for entry in &mut cache.entries {
                if !entry.state.identity_confirmed
                    || !entry.state.is_available
                    || !entry.state.last_scan_complete
                {
                    continue;
                }
                let Some(expected_identity) = entry.state.device_id.as_deref() else {
                    continue;
                };
                if !is_marker_identity(expected_identity) {
                    continue;
                }
                match RootAuthorityLease::acquire(&entry.root, expected_identity) {
                    Ok(lease) => entry.authority_lease = Some(Arc::new(lease)),
                    Err(error) => {
                        warn!(root = %entry.root.display(), %error, "Could not retain watcher root authority");
                    }
                }
            }
            cache
        })
        .await
        .map_err(|error| anyhow::anyhow!("watcher root authority task failed: {error}"))
    }

    fn root_for_path(&self, path: &Path) -> Option<(usize, PathBuf, library_root::Model)> {
        self.entries
            .iter()
            .enumerate()
            .find(|(_, entry)| path.starts_with(&entry.root))
            .map(|(index, entry)| (index, entry.root.clone(), entry.state.clone()))
    }

    fn exact_root(&self, root: &Path) -> Option<usize> {
        self.entries.iter().position(|entry| entry.root == root)
    }

    fn authority_lease(&self, index: usize) -> Option<Arc<RootAuthorityLease>> {
        self.entries.get(index)?.authority_lease.clone()
    }

    fn authority_was_lost(&self) -> bool {
        self.authority_lost
    }

    fn invalidate(&mut self, index: usize) -> Option<library_root::Model> {
        let entry = self.entries.get_mut(index)?;
        entry.state.is_available = false;
        entry.state.last_scan_complete = false;
        entry.state.last_checked_at = Utc::now().to_rfc3339();
        entry.authority_lease = None;
        Some(entry.state.clone())
    }
}

async fn mark_root_unavailable(db: &DatabaseConnection, state: &library_root::Model) {
    let mut active: library_root::ActiveModel = state.clone().into();
    active.is_available = Set(false);
    active.last_scan_complete = Set(false);
    active.last_checked_at = Set(Utc::now().to_rfc3339());
    if let Err(error) = active.update(db).await {
        warn!(root = %state.path, %error, "Failed to mark library root unavailable");
    }
}

async fn mark_root_path_unavailable(db: &DatabaseConnection, root: &Path) {
    let root_path = root.to_string_lossy().into_owned();
    match library_root::Entity::find_by_id(root_path).one(db).await {
        Ok(Some(state)) => mark_root_unavailable(db, &state).await,
        Ok(None) => {
            warn!(root = %root.display(), "Could not find library root state to mark unavailable");
        }
        Err(error) => {
            warn!(root = %root.display(), %error, "Could not load library root state to mark unavailable");
        }
    }
}

async fn mark_root_path_unavailable_if_active(db: &DatabaseConnection, root: &Path) {
    let root_path = root.to_string_lossy().into_owned();
    match library_root::Entity::find_by_id(root_path).one(db).await {
        Ok(Some(state)) if state.is_available => mark_root_unavailable(db, &state).await,
        Ok(Some(_)) => {}
        Ok(None) => {
            warn!(root = %root.display(), "Could not find library root state to verify unavailable");
        }
        Err(error) => {
            warn!(root = %root.display(), %error, "Could not load library root state to verify unavailable");
        }
    }
}

async fn mark_cached_root_unavailable(
    db: &DatabaseConnection,
    roots: &mut WatcherRootCache,
    index: usize,
) {
    // Invalidate memory before awaiting SQLite. The remainder of this batch
    // must fail closed even if persisting the status itself fails.
    roots.authority_lost = true;
    if let Some(state) = roots.invalidate(index) {
        mark_root_unavailable(db, &state).await;
    }
}

async fn root_identity_allows_content(
    db: &DatabaseConnection,
    roots: &mut WatcherRootCache,
    _music_dirs: &[PathBuf],
    path: &Path,
) -> anyhow::Result<bool> {
    let Some((root_index, root, root_state)) = roots.root_for_path(path) else {
        return Ok(false);
    };
    debug_assert!(path.starts_with(&root));
    if !root_state.identity_confirmed || !root_state.is_available || !root_state.last_scan_complete
    {
        return Ok(false);
    }
    let Some(expected_identity) = root_state.device_id.as_deref() else {
        mark_cached_root_unavailable(db, roots, root_index).await;
        return Ok(false);
    };
    if !is_marker_identity(expected_identity) {
        mark_cached_root_unavailable(db, roots, root_index).await;
        return Ok(false);
    }

    let Some(lease) = roots.authority_lease(root_index) else {
        mark_cached_root_unavailable(db, roots, root_index).await;
        return Ok(false);
    };
    if lease.expected_marker() != expected_identity {
        mark_cached_root_unavailable(db, roots, root_index).await;
        return Ok(false);
    }
    let matches = spawn_authority_probe(move || lease.validate().is_ok())
        .await
        .map_err(|error| anyhow::anyhow!("watcher root authority task failed: {error}"))?;
    if !matches {
        mark_cached_root_unavailable(db, roots, root_index).await;
    }
    Ok(matches)
}

/// The shape an authoritative rename pair was observed to have.
///
/// The watcher never reports whether a renamed path was a file or a directory,
/// and the source side no longer exists by the time the batch is processed, so
/// the shape is established from the destination alone — without following
/// symlinks, because neither the traversal nor the watcher follows them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatcherRenameKind {
    File,
    Directory,
}

#[derive(Debug)]
enum FileRenameSourceEvidence {
    Absent(AbsenceProof),
    Alias(BoundFile),
}

#[derive(Debug)]
enum DirectoryRenameSourceEvidence {
    Absent(AbsenceProof),
    Alias(BoundDirectory),
}

#[derive(Debug)]
enum WatcherRenameEvidence {
    File {
        destination: BoundFile,
        source: FileRenameSourceEvidence,
    },
    Directory {
        destination: BoundDirectory,
        source: DirectoryRenameSourceEvidence,
    },
}

impl WatcherRenameEvidence {
    fn kind(&self) -> WatcherRenameKind {
        match self {
            Self::File { .. } => WatcherRenameKind::File,
            Self::Directory { .. } => WatcherRenameKind::Directory,
        }
    }

    fn validate(&self, lease: &RootAuthorityLease) -> std::io::Result<()> {
        match self {
            Self::File {
                destination,
                source,
            } => {
                destination.validate(lease)?;
                match source {
                    FileRenameSourceEvidence::Absent(proof) => proof.validate(lease)?,
                    FileRenameSourceEvidence::Alias(alias) => {
                        alias.validate(lease)?;
                        if !alias.is_same_object_as(destination) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                "rename source alias no longer matches its destination",
                            ));
                        }
                    }
                }
            }
            Self::Directory {
                destination,
                source,
            } => {
                destination.validate(lease)?;
                match source {
                    DirectoryRenameSourceEvidence::Absent(proof) => proof.validate(lease)?,
                    DirectoryRenameSourceEvidence::Alias(alias) => {
                        alias.validate(lease)?;
                        if !alias.is_same_object_as(destination) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                "rename source alias no longer matches its destination",
                            ));
                        }
                    }
                }
            }
        }
        lease.validate()
    }

    fn file(&self) -> Option<&BoundFile> {
        match self {
            Self::File { destination, .. } => Some(destination),
            Self::Directory { .. } => None,
        }
    }

    fn directory(&self) -> Option<&BoundDirectory> {
        match self {
            Self::Directory { destination, .. } => Some(destination),
            Self::File { .. } => None,
        }
    }
}

fn bind_file_rename_source(
    lease: &RootAuthorityLease,
    source: &Path,
    destination: &BoundFile,
) -> std::io::Result<FileRenameSourceEvidence> {
    match lease.prove_absent(source) {
        Ok(proof) => Ok(FileRenameSourceEvidence::Absent(proof)),
        Err(absence_error) => match lease.open_regular_file(source) {
            Ok(alias) if alias.is_same_object_as(destination) => {
                Ok(FileRenameSourceEvidence::Alias(alias))
            }
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rename source still names a different regular file",
            )),
            Err(_) => Err(absence_error),
        },
    }
}

fn bind_directory_rename_source(
    lease: &RootAuthorityLease,
    source: &Path,
    destination: &BoundDirectory,
) -> std::io::Result<DirectoryRenameSourceEvidence> {
    match lease.prove_absent(source) {
        Ok(proof) => Ok(DirectoryRenameSourceEvidence::Absent(proof)),
        Err(absence_error) => match lease.bind_directory(source) {
            Ok(alias) if alias.is_same_object_as(destination) => {
                Ok(DirectoryRenameSourceEvidence::Alias(alias))
            }
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rename source still names a different directory",
            )),
            Err(_) => Err(absence_error),
        },
    }
}

fn bind_watcher_rename_evidence(
    lease: &RootAuthorityLease,
    pair: &WatcherRenamePair,
) -> std::io::Result<WatcherRenameEvidence> {
    let file_error = if same_audio_extension(&pair.from, &pair.to) {
        match lease.open_regular_file(&pair.to) {
            Ok(destination) => {
                let source = bind_file_rename_source(lease, &pair.from, &destination)?;
                return Ok(WatcherRenameEvidence::File {
                    destination,
                    source,
                });
            }
            Err(error) => Some(error),
        }
    } else {
        None
    };

    match lease.bind_directory(&pair.to) {
        Ok(destination) => {
            let source = bind_directory_rename_source(lease, &pair.from, &destination)?;
            Ok(WatcherRenameEvidence::Directory {
                destination,
                source,
            })
        }
        Err(directory_error) => Err(file_error.unwrap_or(directory_error)),
    }
}

#[derive(Clone, Debug)]
struct WatcherRenameGuard {
    root_index: usize,
    authority_lease: Arc<RootAuthorityLease>,
    evidence: Arc<WatcherRenameEvidence>,
}

impl WatcherRenameGuard {
    fn kind(&self) -> WatcherRenameKind {
        self.evidence.kind()
    }

    fn root_is_stable(&self) -> bool {
        self.authority_lease.validate().is_ok()
    }

    fn evidence_is_stable(&self) -> bool {
        self.evidence.validate(&self.authority_lease).is_ok()
    }
}

/// Reject a directory pair whose subtree owns another scan scope.
///
/// Nothing rewrites `library_root.path` on rename, so moving a persisted root
/// would leave a row pointing at a path that no longer exists — and a nested
/// mount must never be traversed through its parent. Both cases fall back to a
/// full reconciliation, which reasons about every scope at once.
fn subtree_owns_another_scope(
    subtree: &Path,
    roots: &WatcherRootCache,
    music_dirs: &[PathBuf],
) -> bool {
    let is_nested = |candidate: &Path| candidate != subtree && candidate.starts_with(subtree);

    if music_dirs.iter().any(|dir| is_nested(dir))
        || roots.entries.iter().any(|entry| is_nested(&entry.root))
    {
        return true;
    }

    match mounted_subroots(music_dirs) {
        Ok(mountpoints) => mountpoints
            .iter()
            .any(|mountpoint| is_nested(mountpoint.as_path())),
        Err(error) => {
            warn!(%error, "Could not inspect mounted library scopes; directory rename rejected");
            true
        }
    }
}

fn same_audio_extension(from: &Path, to: &Path) -> bool {
    tag_parser::is_audio_file(from)
        && tag_parser::is_audio_file(to)
        && from
            .extension()
            .and_then(|extension| extension.to_str())
            .zip(to.extension().and_then(|extension| extension.to_str()))
            .is_some_and(|(from, to)| from.eq_ignore_ascii_case(to))
}

async fn prepare_watcher_rename_guard(
    db: &DatabaseConnection,
    roots: &mut WatcherRootCache,
    music_dirs: &[PathBuf],
    pair: &WatcherRenamePair,
) -> anyhow::Result<Option<WatcherRenameGuard>> {
    if !root_identity_allows_content(db, roots, music_dirs, &pair.from).await?
        || !root_identity_allows_content(db, roots, music_dirs, &pair.to).await?
    {
        return Ok(None);
    }

    let Some((from_index, from_root, _)) = roots.root_for_path(&pair.from) else {
        return Ok(None);
    };
    let Some((to_index, to_root, _)) = roots.root_for_path(&pair.to) else {
        return Ok(None);
    };
    if from_index != to_index || from_root != to_root {
        return Ok(None);
    }
    let Some(authority_lease) = roots.authority_lease(from_index) else {
        mark_cached_root_unavailable(db, roots, from_index).await;
        return Ok(None);
    };
    let binding_lease = authority_lease.clone();
    let binding_pair = pair.clone();
    let binding = spawn_authority_probe(move || {
        match bind_watcher_rename_evidence(&binding_lease, &binding_pair) {
            Ok(evidence) => (Ok(Arc::new(evidence)), true),
            Err(error) => {
                let authority_stable = binding_lease.validate().is_ok();
                (Err(error), authority_stable)
            }
        }
    })
    .await
    .map_err(|error| anyhow::anyhow!("watcher rename authority task failed: {error}"))?;
    let evidence = match binding {
        (Ok(evidence), _) => evidence,
        (Err(error), authority_stable) => {
            if !authority_stable {
                mark_cached_root_unavailable(db, roots, from_index).await;
            }
            warn!(from = %pair.from.display(), to = %pair.to.display(), %error, "Could not bind paired rename beneath its retained root");
            return Ok(None);
        }
    };

    Ok(Some(WatcherRenameGuard {
        root_index: from_index,
        authority_lease,
        evidence,
    }))
}

/// Delete a watcher-reported missing path only while its confirmed root
/// identity remains stable across the database transaction.
async fn delete_track_if_root_stable(
    db: &DatabaseConnection,
    roots: &mut WatcherRootCache,
    music_dirs: &[PathBuf],
    path: &Path,
) -> anyhow::Result<bool> {
    delete_track_if_root_stable_with_guard_hook(db, roots, music_dirs, path, || {}).await
}

async fn delete_track_if_root_stable_with_guard_hook<F>(
    db: &DatabaseConnection,
    roots: &mut WatcherRootCache,
    music_dirs: &[PathBuf],
    path: &Path,
    before_commit_guard: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(),
{
    if !root_identity_allows_content(db, roots, music_dirs, path).await? {
        return Ok(false);
    }
    let Some((root_index, _, _)) = roots.root_for_path(path) else {
        return Ok(false);
    };
    let Some(authority_lease) = roots.authority_lease(root_index) else {
        mark_cached_root_unavailable(db, roots, root_index).await;
        return Ok(false);
    };
    let proof_lease = authority_lease.clone();
    let proof_path = path.to_path_buf();
    let proof_result = spawn_authority_probe(move || {
        let proof = proof_lease.prove_absent(&proof_path).map(Arc::new);
        let authority_stable = proof_lease.validate().is_ok();
        (proof, authority_stable)
    })
    .await
    .map_err(|error| anyhow::anyhow!("watcher absence-proof task failed: {error}"))?;
    let absence_proof = match proof_result {
        (Ok(proof), _) => proof,
        (Err(_), false) => {
            mark_cached_root_unavailable(db, roots, root_index).await;
            return Ok(false);
        }
        (Err(_), true) => return Ok(false),
    };
    let path_key = path.to_string_lossy().into_owned();
    let row = track::Entity::find()
        .filter(track::Column::FilePath.eq(&path_key))
        .one(db)
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };

    let mut authority_stable = true;
    let mut authority_task_failed = false;
    let outcome = delete_track_with_commit_guard(db, &row.id, || async {
        before_commit_guard();
        let guard_absence = absence_proof.clone();
        let guard_lease = authority_lease.clone();
        match spawn_authority_probe(move || {
            let path_is_still_absent = guard_absence.validate(&guard_lease).is_ok();
            let authority_stable = guard_lease.validate().is_ok();
            (path_is_still_absent, authority_stable)
        })
        .await
        {
            Ok((path_is_still_absent, still_stable)) => {
                authority_stable = still_stable;
                path_is_still_absent && authority_stable
            }
            Err(_) => {
                authority_task_failed = true;
                false
            }
        }
    })
    .await?;

    match outcome {
        GuardedTrackDeleteOutcome::Deleted => Ok(true),
        GuardedTrackDeleteOutcome::Missing => Ok(false),
        GuardedTrackDeleteOutcome::GuardRejected => {
            if !authority_task_failed && !authority_stable {
                mark_cached_root_unavailable(db, roots, root_index).await;
            }
            Ok(false)
        }
    }
}

fn marker_event_invalidates_root(kind: notify::EventKind) -> bool {
    use notify::event::{MetadataKind, ModifyKind};
    use notify::EventKind;

    // Reading the marker is part of every authorization probe and some
    // backends report open/read/close (or the resulting atime update) through
    // the same watcher. Those observations must not invalidate the identity
    // they just verified. Every potentially mutating or unknown event remains
    // fail-closed.
    !matches!(
        kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
    )
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WatcherRenamePair {
    from: PathBuf,
    to: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatcherUpsertPathKind {
    RegularFile,
    Directory,
    Missing,
    Unsafe,
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    // Reject every reparse-point flavor, not only the name-surrogate tags that
    // `FileType::is_symlink` recognizes.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Classify a watcher path without following symlinks or reparse points.
///
/// A missing audio path remains useful: the upsert loop treats it as a
/// debounced removal. Every other non-regular object is unsafe to parse and
/// forces an authoritative reconciliation instead.
fn watcher_upsert_path_kind(path: &Path) -> std::io::Result<WatcherUpsertPathKind> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_reparse_point(&metadata) => Ok(WatcherUpsertPathKind::Unsafe),
        Ok(metadata) if metadata.file_type().is_file() => Ok(WatcherUpsertPathKind::RegularFile),
        Ok(metadata) if metadata.file_type().is_dir() => Ok(WatcherUpsertPathKind::Directory),
        Ok(_) => Ok(WatcherUpsertPathKind::Unsafe),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(WatcherUpsertPathKind::Missing)
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, Default)]
struct WatcherBatch {
    upsert_paths: HashSet<PathBuf>,
    remove_paths: HashSet<PathBuf>,
    rename_pairs: HashSet<WatcherRenamePair>,
    paired_paths: HashSet<PathBuf>,
    /// Directory and other non-audio observations whose meaning is not yet
    /// known. A rename half arrives before the pair that explains it, so
    /// deciding to reconcile on sight would make every directory rename fall
    /// back to a full rescan. [`WatcherBatch::finish`] promotes whatever no
    /// authoritative pair claimed.
    deferred_paths: HashSet<PathBuf>,
    /// Explicit folder create/remove observations are independent changes, not
    /// ambiguous rename halves. Pair normalization must never erase them: an
    /// exact destination replacement invalidates every descendant mapping.
    dirty_directory_scopes: HashSet<PathBuf>,
    identity_changed_roots: HashSet<PathBuf>,
    tracked_rename_from: HashMap<usize, PathBuf>,
    adjacent_untracked_rename_from: Option<PathBuf>,
    reconciliation_required: bool,
}

impl WatcherBatch {
    fn collect(&mut self, mut event: notify::Event) {
        use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
        use notify::EventKind;

        let marker_invalidates_root = marker_event_invalidates_root(event.kind);
        event.paths.retain(|path| {
            if path
                .file_name()
                .is_some_and(|name| name == ROOT_IDENTITY_FILE)
            {
                if marker_invalidates_root {
                    if let Some(root) = path.parent() {
                        self.identity_changed_roots.insert(root.to_path_buf());
                    }
                }
                false
            } else {
                true
            }
        });
        if event.paths.is_empty() {
            self.adjacent_untracked_rename_from = None;
            return;
        }

        let tracker = event.tracker();
        let is_adjacent_untracked_to = tracker.is_none()
            && matches!(
                event.kind,
                EventKind::Modify(ModifyKind::Name(RenameMode::To))
            )
            && event.paths.len() == 1;
        if !is_adjacent_untracked_to {
            self.adjacent_untracked_rename_from = None;
        }

        match event.kind {
            EventKind::Remove(kind) => {
                let folder = matches!(kind, RemoveKind::Folder);
                for path in event.paths {
                    if folder {
                        // Retain the scope as well as requesting reconciliation
                        // so a paired parent-directory rename cannot assign old
                        // identities beneath a separately changed subtree.
                        self.dirty_directory_scopes.insert(path.clone());
                        self.deferred_paths.insert(path);
                    } else {
                        self.record_remove(path);
                    }
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                let candidate = (event.paths.len() == 1).then(|| event.paths[0].clone());
                for path in event.paths {
                    self.record_remove(path);
                }
                if let Some(path) = candidate {
                    if let Some(tracker) = tracker {
                        self.tracked_rename_from.insert(tracker, path);
                    } else {
                        self.adjacent_untracked_rename_from = Some(path);
                    }
                } else {
                    self.reconciliation_required = true;
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                let candidate = (event.paths.len() == 1).then(|| event.paths[0].clone());
                for path in event.paths {
                    self.record_upsert(path);
                }
                if let Some(to) = candidate {
                    let from = tracker
                        .and_then(|tracker| self.tracked_rename_from.remove(&tracker))
                        .or_else(|| {
                            tracker
                                .is_none()
                                .then(|| self.adjacent_untracked_rename_from.take())
                                .flatten()
                        });
                    if let Some(from) = from {
                        self.record_rename_pair(from, to);
                    }
                } else {
                    self.reconciliation_required = true;
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                if event.paths.len() == 2 {
                    let from = event.paths[0].clone();
                    let to = event.paths[1].clone();
                    if let Some(tracker) = tracker {
                        self.tracked_rename_from.remove(&tracker);
                    }
                    self.record_rename_pair(from, to);
                } else {
                    self.reconciliation_required = true;
                }
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                // FSEvents and kqueue cannot associate the old and new sides.
                // Never infer identity from metadata; a guarded scan performs
                // the conservative delete/upsert fallback.
                self.reconciliation_required = true;
            }
            EventKind::Create(kind) => {
                let folder = matches!(kind, CreateKind::Folder);
                for path in event.paths {
                    if folder {
                        self.dirty_directory_scopes.insert(path.clone());
                        self.deferred_paths.insert(path);
                    } else {
                        self.record_upsert(path);
                    }
                }
            }
            EventKind::Modify(_) => {
                for path in event.paths {
                    self.record_upsert(path);
                }
            }
            _ => {}
        }
    }

    fn record_remove(&mut self, path: PathBuf) {
        if self.paired_paths.contains(&path) {
            return;
        }
        if tag_parser::is_audio_file(&path) {
            self.upsert_paths.remove(&path);
            self.remove_paths.insert(path);
        } else {
            // The vanished path cannot be stat'd, so a renamed directory and a
            // deleted cover image look identical here. Defer both.
            self.deferred_paths.insert(path);
        }
    }

    fn record_upsert(&mut self, path: PathBuf) {
        let paired = self.paired_paths.contains(&path);
        match watcher_upsert_path_kind(&path) {
            Ok(WatcherUpsertPathKind::Unsafe) | Err(_) => {
                self.upsert_paths.remove(&path);
                self.reconciliation_required = true;
            }
            Ok(_) if paired => {}
            Ok(WatcherUpsertPathKind::Directory) => {
                self.deferred_paths.insert(path);
            }
            Ok(WatcherUpsertPathKind::RegularFile) if tag_parser::is_audio_file(&path) => {
                self.remove_paths.remove(&path);
                self.upsert_paths.insert(path);
            }
            Ok(WatcherUpsertPathKind::Missing) if tag_parser::is_audio_file(&path) => {
                // Keep a vanished debounced upsert in the work set. The
                // guarded removal path below can safely remove its stale row.
                self.remove_paths.remove(&path);
                self.upsert_paths.insert(path);
            }
            Ok(WatcherUpsertPathKind::Missing) => {
                // A vanished non-audio path may have been a directory whose
                // descendants need reconciliation.
                self.deferred_paths.insert(path);
            }
            Ok(WatcherUpsertPathKind::RegularFile) => {}
        }
    }

    fn record_rename_pair(&mut self, from: PathBuf, to: PathBuf) {
        let pair = WatcherRenamePair { from, to };
        if pair.from == pair.to {
            self.record_upsert(pair.to);
            return;
        }
        if self.rename_pairs.contains(&pair) {
            return;
        }
        if self
            .rename_pairs
            .iter()
            .any(|existing| rename_pairs_overlap(existing, &pair))
        {
            // Overlapping, chained, and nested pairs cannot be applied
            // independently without ordering and inode guarantees. Reconcile
            // instead.
            self.reconciliation_required = true;
            self.rename_pairs
                .retain(|existing| !rename_pairs_overlap(existing, &pair));
            return;
        }

        self.upsert_paths.remove(&pair.from);
        self.upsert_paths.remove(&pair.to);
        self.remove_paths.remove(&pair.from);
        self.remove_paths.remove(&pair.to);
        self.deferred_paths.remove(&pair.from);
        self.deferred_paths.remove(&pair.to);
        self.paired_paths.insert(pair.from.clone());
        self.paired_paths.insert(pair.to.clone());
        self.rename_pairs.insert(pair);
    }

    /// Settle the batch once every event in the debounce window has been seen.
    ///
    /// Only now is it known whether a deferred path was one half of an
    /// authoritative rename. Anything unclaimed keeps the conservative
    /// fallback: a guarded reconciliation that never infers identity.
    fn finish(&mut self) {
        self.deferred_paths
            .retain(|path| !self.paired_paths.contains(path));
        if !self.deferred_paths.is_empty() {
            self.reconciliation_required = true;
        }
        if !self.dirty_directory_scopes.is_empty() {
            self.reconciliation_required = true;
        }
    }

    fn is_empty(&self) -> bool {
        self.upsert_paths.is_empty()
            && self.remove_paths.is_empty()
            && self.rename_pairs.is_empty()
            && self.deferred_paths.is_empty()
            && self.dirty_directory_scopes.is_empty()
            && self.identity_changed_roots.is_empty()
            && !self.reconciliation_required
    }

    fn requires_reconciliation_before_incrementals(&self) -> bool {
        !self.identity_changed_roots.is_empty()
    }
}

/// Two pairs overlap when either shares a path with the other, or when one
/// renames a directory that contains the other's source or destination.
///
/// A directory pair moves a whole subtree, so a pair nested inside it can only
/// be interpreted with the event ordering the watcher does not provide.
fn rename_pairs_overlap(left: &WatcherRenamePair, right: &WatcherRenamePair) -> bool {
    [&left.from, &left.to].into_iter().any(|path| {
        [&right.from, &right.to]
            .into_iter()
            .any(|other| path.starts_with(other) || other.starts_with(path))
    })
}

async fn reconcile_playlists_after_watcher_batch(
    db: &DatabaseConnection,
    upsert_committed: bool,
) -> Result<u32, sea_orm::DbErr> {
    if !upsert_committed {
        return Ok(0);
    }

    super::playlist_manager::PlaylistManager::new(db.clone())
        .reconcile_all()
        .await
}

/// Finish the playlist work caused by one watcher batch, then tell the UI to
/// rebuild any active projection. The notification is deliberately emitted
/// even when reconciliation fails: the committed track mutation still needs
/// to become visible, and a later batch or scan can retry orphan relinking.
async fn settle_playlist_projections_after_watcher_batch(
    db: &DatabaseConnection,
    tx: &async_channel::Sender<LibraryEvent>,
    upsert_committed: bool,
    track_mutation_committed: bool,
) -> Result<u32, sea_orm::DbErr> {
    let result = reconcile_playlists_after_watcher_batch(db, upsert_committed).await;
    if track_mutation_committed {
        let _ = tx.send(LibraryEvent::PlaylistProjectionsInvalidated).await;
    }
    result
}

const WATCHER_EVENT_CAPACITY: usize = 256;
const WATCHER_DEBOUNCE_MS: u64 = 1500;
const WATCHER_RECONCILIATION_RETRY_MS: u64 = 1000;

struct DirectoryWatcher {
    watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
    ingress_overflowed: Arc<AtomicBool>,
    watched_directories: HashSet<PathBuf>,
}

impl DirectoryWatcher {
    fn watch_available_directories(&mut self, music_dirs: &[PathBuf]) {
        for dir in music_dirs {
            if self.watched_directories.contains(dir) {
                continue;
            }
            if !dir.is_dir() {
                warn!(dir = %dir.display(), "Library folder does not exist — skipping watch");
                continue;
            }
            if let Err(error) = self.watcher.watch(dir.as_ref(), RecursiveMode::Recursive) {
                warn!(dir = %dir.display(), %error, "Failed to watch directory — skipping");
                continue;
            }
            self.watched_directories.insert(dir.clone());
            info!(dir = %dir.display(), "Watching directory");
        }
    }
}

/// Enqueue one backend callback without ever blocking the notify thread.
/// A full bounded queue means at least one event was lost, so the atomic marks
/// the whole stream as unreliable even though that event could not be queued.
fn enqueue_watcher_result(
    tx: &mpsc::Sender<notify::Result<notify::Event>>,
    ingress_overflowed: &AtomicBool,
    result: notify::Result<notify::Event>,
) {
    match tx.try_send(result) {
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            ingress_overflowed.store(true, Ordering::Release);
        }
    }
}

fn install_directory_watcher(music_dirs: &[PathBuf]) -> notify::Result<DirectoryWatcher> {
    let (notify_tx, notify_rx) = mpsc::channel(WATCHER_EVENT_CAPACITY);
    let ingress_overflowed = Arc::new(AtomicBool::new(false));
    let callback_overflowed = Arc::clone(&ingress_overflowed);

    let watcher = RecommendedWatcher::new(
        move |result| {
            enqueue_watcher_result(&notify_tx, callback_overflowed.as_ref(), result);
        },
        notify::Config::default()
            .with_poll_interval(Duration::from_secs(2))
            .with_follow_symlinks(false),
    )?;

    let mut installed = DirectoryWatcher {
        watcher,
        rx: notify_rx,
        ingress_overflowed,
        watched_directories: HashSet::new(),
    };

    // Watch each directory independently. A missing or unwatchable directory
    // is retried once after the bootstrap scan rather than aborting the whole
    // watcher, so one bad path cannot stop healthy roots from being watched.
    installed.watch_available_directories(music_dirs);
    info!("Filesystem watcher active");

    Ok(installed)
}

#[derive(Debug, Default)]
struct WatcherDebounceBatch {
    batch: WatcherBatch,
    stream_unreliable: bool,
}

impl WatcherDebounceBatch {
    fn collect(&mut self, result: notify::Result<notify::Event>) {
        if self.stream_unreliable {
            return;
        }

        match result {
            Ok(event) if event.need_rescan() => {
                warn!("Filesystem watcher requested an authoritative rescan");
                self.stream_unreliable = true;
                self.batch = WatcherBatch::default();
            }
            Ok(event) => self.batch.collect(event),
            Err(error) => {
                warn!(%error, "Filesystem watcher reported an unreliable stream");
                self.stream_unreliable = true;
                self.batch = WatcherBatch::default();
            }
        }
    }

    fn finish(mut self) -> Option<WatcherBatch> {
        if self.stream_unreliable {
            return None;
        }
        self.batch.finish();
        Some(self.batch)
    }
}

fn discard_watcher_backlog(rx: &mut mpsc::Receiver<notify::Result<notify::Event>>) {
    while rx.try_recv().is_ok() {}
}

async fn reconcile_unreliable_watcher_stream(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    rx: &mut mpsc::Receiver<notify::Result<notify::Event>>,
) -> bool {
    // The queued backlog belongs to the same stream gap and cannot be applied
    // incrementally. Events racing with this drain may be discarded too; the
    // following authoritative scan is what makes that safe. Events arriving
    // after the drain, including during the scan, remain queued for the next
    // loop iteration.
    discard_watcher_backlog(rx);
    info!("Reconciling library after filesystem watcher stream loss");
    match initial_scan(db, music_dirs, tx).await {
        Ok(()) => true,
        Err(error) => {
            warn!(%error, "Watcher stream reconciliation failed; retry remains pending");
            false
        }
    }
}

async fn reconcile_root_marker_mutations(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    roots: &HashSet<PathBuf>,
) -> bool {
    // Invalidate persisted authorization before any asynchronous traversal.
    // A marker created by the bootstrap scan is restored to available by this
    // immediate marker-backed scan; a replaced marker remains unavailable.
    for root in roots {
        mark_root_path_unavailable(db, root).await;
    }
    info!("Reconciling library after library root marker mutation");
    match initial_scan(db, music_dirs, tx).await {
        Ok(()) => true,
        Err(error) => {
            warn!(%error, "Library root marker reconciliation failed; retry remains pending");
            false
        }
    }
}

async fn process_directory_events(
    db: &Arc<DatabaseConnection>,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    command_rx: &async_channel::Receiver<LibraryCommand>,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
    mut watcher: DirectoryWatcher,
) -> anyhow::Result<()> {
    // ── Debounced event processing ──────────────────────────────
    // Collect filesystem events for a short window, deduplicate by
    // path, then process the batch. This collapses the 3-5 duplicate
    // Create/Modify events that Windows fires per file copy into a
    // single parse+upsert, and removes the old per-file 500ms sleep.
    let mut reconciliation_pending = false;
    let mut pending_trust_scan: Option<PendingRootTrustScan> = None;
    let mut commands_open = true;
    loop {
        if let Some(pending) = pending_trust_scan.take() {
            // The conversion scan intentionally performed no track writes.
            // Suppress watcher evidence queued before its distinct ordinary
            // authority scan, then let events arriving during that scan remain
            // for the following boundary.
            discard_watcher_backlog(&mut watcher.rx);
            finish_pending_root_trust_scan(
                db.as_ref(),
                music_dirs,
                tx,
                completed_commands,
                pending,
            )
            .await;
            continue;
        }

        // A trust decision is one serialized engine command. Drain at most one
        // per batch boundary so it cannot interleave with a watcher mutation.
        if commands_open {
            match command_rx.try_recv() {
                Ok(command) => {
                    pending_trust_scan = process_library_command(
                        db.as_ref(),
                        music_dirs,
                        tx,
                        completed_commands,
                        command,
                    )
                    .await;
                    continue;
                }
                Err(async_channel::TryRecvError::Empty) => {}
                Err(async_channel::TryRecvError::Closed) => commands_open = false,
            }
        }

        let overflowed = watcher.ingress_overflowed.swap(false, Ordering::AcqRel);
        if reconciliation_pending || overflowed {
            if overflowed {
                warn!("Filesystem watcher ingress overflowed");
            }
            reconciliation_pending =
                !reconcile_unreliable_watcher_stream(db.as_ref(), music_dirs, tx, &mut watcher.rx)
                    .await;
            if reconciliation_pending {
                tokio::time::sleep(Duration::from_millis(WATCHER_RECONCILIATION_RETRY_MS)).await;
            }
            continue;
        }

        // Wait for either the next watcher batch or the next serialized UI
        // command. A closed command channel must not stop filesystem watching.
        let first = if commands_open {
            tokio::select! {
                biased;
                command = command_rx.recv() => {
                    match command {
                        Ok(command) => {
                            pending_trust_scan = process_library_command(
                                db.as_ref(),
                                music_dirs,
                                tx,
                                completed_commands,
                                command,
                            ).await;
                        }
                        Err(_) => commands_open = false,
                    }
                    continue;
                }
                first = watcher.rx.recv() => first,
            }
        } else {
            watcher.rx.recv().await
        };
        let Some(first) = first else { break };

        // Preserve event order and tracker metadata until rename halves have
        // been normalized. Flattening immediately into unordered path sets
        // would discard the only authoritative identity association.
        let mut ingress = WatcherDebounceBatch::default();
        ingress.collect(first);

        // Drain any additional events that arrive within the debounce window.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(WATCHER_DEBOUNCE_MS);
        while !ingress.stream_unreliable {
            let Ok(Some(result)) = tokio::time::timeout_at(deadline, watcher.rx.recv()).await
            else {
                break;
            };
            ingress.collect(result);
        }

        // Consume only overflow known before this recovery decision. A
        // callback racing with the scan stores a fresh `true` value, which is
        // intentionally left for the next loop iteration.
        let overflowed = watcher.ingress_overflowed.swap(false, Ordering::AcqRel);
        let Some(mut batch) = ingress.finish() else {
            reconciliation_pending = true;
            continue;
        };
        if overflowed {
            warn!("Filesystem watcher ingress overflowed during debounce");
            reconciliation_pending = true;
            continue;
        }

        if batch.is_empty() {
            continue;
        }

        // A marker mutation invalidates the authorization boundary for every
        // other event in this batch. Discard all incrementals and let the
        // hardened scan either restore the same identity (including a marker
        // created during bootstrap) or leave the root unavailable.
        if batch.requires_reconciliation_before_incrementals() {
            reconciliation_pending = !reconcile_root_marker_mutations(
                db.as_ref(),
                music_dirs,
                tx,
                &batch.identity_changed_roots,
            )
            .await;
            if reconciliation_pending {
                tokio::time::sleep(Duration::from_millis(WATCHER_RECONCILIATION_RETRY_MS)).await;
            }
            continue;
        }

        // Root state changes only during scans and watcher processing today.
        // Load one snapshot per debounced batch and mutate it fail-closed as
        // identities are invalidated, eliminating O(files) root-table reads.
        let mut root_cache = match WatcherRootCache::load(db.as_ref(), music_dirs).await {
            Ok(cache) => cache,
            Err(error) => {
                warn!(%error, "Could not load library root state; watcher batch rejected");
                reconciliation_pending = true;
                continue;
            }
        };

        let mut reconciliation_required = batch.reconciliation_required;
        let mut upsert_committed = false;
        let mut track_mutation_committed = false;
        let mut library_snapshot_dirty = false;

        // Apply authoritative same-root rename pairs before standalone removals
        // and upserts. This keeps the source rows alive, preserving their stable
        // IDs, historical fields, and direct playlist references.
        //
        // The removal and upsert loops below are keyed by the paths the watcher
        // reported, which a committed rename has already vacated. They stay
        // correct only because they resolve rows by exact path: a leftover event
        // for a moved-away path matches nothing. Do not reorder them ahead of
        // the renames, and do not make them match by prefix.
        let rename_pairs: Vec<WatcherRenamePair> = batch.rename_pairs.iter().cloned().collect();
        for pair in rename_pairs {
            let guard = match prepare_watcher_rename_guard(
                db.as_ref(),
                &mut root_cache,
                music_dirs,
                &pair,
            )
            .await
            {
                Ok(Some(guard)) => guard,
                Ok(None) => {
                    reconciliation_required = true;
                    continue;
                }
                Err(error) => {
                    warn!(from = %pair.from.display(), to = %pair.to.display(), %error, "Failed to authorize paired rename");
                    reconciliation_required = true;
                    continue;
                }
            };
            let kind = guard.kind();

            match kind {
                WatcherRenameKind::File => {
                    let clone_guard = guard.clone();
                    let clone_result = spawn_authority_probe(move || {
                        let parser_file = clone_guard
                            .evidence
                            .file()
                            .expect("file rename guard must retain a bound file")
                            .try_clone_file();
                        let authority_stable = clone_guard.root_is_stable();
                        (parser_file, authority_stable)
                    })
                    .await;
                    let parser_file = match clone_result {
                        Ok((Ok(file), _)) => file,
                        Ok((Err(error), authority_stable)) => {
                            if !authority_stable {
                                mark_cached_root_unavailable(
                                    db.as_ref(),
                                    &mut root_cache,
                                    guard.root_index,
                                )
                                .await;
                            }
                            warn!(to = %pair.to.display(), %error, "Could not clone bound renamed file for parsing");
                            reconciliation_required = true;
                            continue;
                        }
                        Err(error) => {
                            warn!(to = %pair.to.display(), %error, "Renamed-file authority task failed");
                            reconciliation_required = true;
                            continue;
                        }
                    };
                    let to_parse = pair.to.clone();
                    let parsed = match tokio::task::spawn_blocking(move || {
                        tag_parser::parse_audio_file_from_file(parser_file, &to_parse)
                    })
                    .await
                    {
                        Ok(Ok(parsed)) => Some(parsed),
                        Ok(Err(error)) => {
                            warn!(to = %pair.to.display(), %error, "Renamed file could not be reparsed; preserving identity and scheduling reconciliation");
                            reconciliation_required = true;
                            None
                        }
                        Err(error) => {
                            warn!(to = %pair.to.display(), %error, "Rename parser task failed; preserving identity and scheduling reconciliation");
                            reconciliation_required = true;
                            None
                        }
                    };

                    let preflight_guard = guard.clone();
                    let (evidence_is_stable, authority_is_stable) = match spawn_authority_probe(
                        move || {
                            (
                                preflight_guard.evidence_is_stable(),
                                preflight_guard.root_is_stable(),
                            )
                        },
                    )
                    .await
                    {
                        Ok(stability) => stability,
                        Err(error) => {
                            warn!(to = %pair.to.display(), %error, "Renamed-file validation task failed");
                            reconciliation_required = true;
                            continue;
                        }
                    };
                    if !evidence_is_stable || !authority_is_stable {
                        if !authority_is_stable {
                            mark_cached_root_unavailable(
                                db.as_ref(),
                                &mut root_cache,
                                guard.root_index,
                            )
                            .await;
                        }
                        reconciliation_required = true;
                        continue;
                    }

                    let mut authority_stable_at_commit = true;
                    let mut authority_task_failed = false;
                    let outcome = rename_track_row(
                        db.as_ref(),
                        &pair.from,
                        &pair.to,
                        parsed.as_ref(),
                        || async {
                            let commit_guard = guard.clone();
                            match spawn_authority_probe(move || {
                                (
                                    commit_guard.evidence_is_stable(),
                                    commit_guard.root_is_stable(),
                                )
                            })
                            .await
                            {
                                Ok((evidence_is_stable, authority_stable)) => {
                                    authority_stable_at_commit = authority_stable;
                                    evidence_is_stable && authority_stable_at_commit
                                }
                                Err(_) => {
                                    authority_task_failed = true;
                                    false
                                }
                            }
                        },
                    )
                    .await;
                    match outcome {
                        Ok(RenameTrackOutcome::Renamed { model, displaced }) => {
                            let _ = tx
                                .send(LibraryEvent::TrackRemoved(
                                    pair.from.to_string_lossy().into_owned(),
                                ))
                                .await;
                            if let Some(displaced) = displaced {
                                let displaced = *displaced;
                                let _ = tx
                                    .send(LibraryEvent::TrackRemoved(displaced.file_path))
                                    .await;
                            }
                            let _ = tx
                                .send(LibraryEvent::TrackUpserted(Box::new(db_model_to_track(
                                    &model,
                                ))))
                                .await;
                            upsert_committed = true;
                            track_mutation_committed = true;
                            info!(from = %pair.from.display(), to = %pair.to.display(), id = %model.id, "Preserved track identity across filesystem rename");
                        }
                        Ok(RenameTrackOutcome::SourceMissing) => {
                            debug!(from = %pair.from.display(), to = %pair.to.display(), "Rename source was not indexed; falling back to reconciliation");
                            reconciliation_required = true;
                        }
                        Ok(RenameTrackOutcome::GuardRejected) => {
                            if !authority_task_failed && !authority_stable_at_commit {
                                mark_cached_root_unavailable(
                                    db.as_ref(),
                                    &mut root_cache,
                                    guard.root_index,
                                )
                                .await;
                            }
                            warn!(from = %pair.from.display(), to = %pair.to.display(), "Filesystem changed before paired rename commit; transaction rolled back");
                            reconciliation_required = true;
                        }
                        Err(error) => {
                            warn!(from = %pair.from.display(), to = %pair.to.display(), %error, "Failed to update paired rename transactionally");
                            reconciliation_required = true;
                        }
                    }
                }

                WatcherRenameKind::Directory => {
                    if subtree_owns_another_scope(&pair.from, &root_cache, music_dirs)
                        || subtree_owns_another_scope(&pair.to, &root_cache, music_dirs)
                    {
                        warn!(from = %pair.from.display(), to = %pair.to.display(), "Renamed directory owns another library scope; falling back to reconciliation");
                        reconciliation_required = true;
                        continue;
                    }

                    // Enumerate the destination before opening a transaction: a
                    // whole subtree of blocking filesystem probes must not run
                    // with a SQLite write lock held.
                    let destination = pair.to.clone();
                    let scan_guard = guard.clone();
                    let scan = match tokio::task::spawn_blocking(move || {
                        scan_renamed_directory(
                            &scan_guard.authority_lease,
                            scan_guard
                                .evidence
                                .directory()
                                .expect("directory rename guard must retain a bound directory"),
                            &destination,
                        )
                    })
                    .await
                    {
                        Ok(scan) => Arc::new(scan),
                        Err(error) => {
                            warn!(to = %pair.to.display(), %error, "Renamed directory scan task failed");
                            reconciliation_required = true;
                            continue;
                        }
                    };
                    if !scan.is_complete() {
                        for error in &scan.errors {
                            warn!(to = %pair.to.display(), %error, "Renamed directory could not be fully enumerated");
                        }
                        reconciliation_required = true;
                        continue;
                    }

                    // A child event in the same batch means the file may have
                    // been modified or replaced independently of the directory
                    // move. Do not give that path the old row's identity even
                    // though the scoped traversal observed it; the normal
                    // upsert/reconciliation path will assign it deliberately.
                    let destination_files = directory_identity_destinations(
                        &scan.audio_files,
                        &pair.from,
                        &pair.to,
                        &batch.upsert_paths,
                        &batch.remove_paths,
                        &batch.deferred_paths,
                        &batch.dirty_directory_scopes,
                    );

                    let preflight_guard = guard.clone();
                    let preflight_scan = scan.clone();
                    let (evidence_is_stable, observations_are_current, authority_is_stable) =
                        match spawn_authority_probe(move || {
                            let evidence_is_stable = preflight_guard.evidence_is_stable();
                            let observations_are_current = preflight_scan
                                .observations_still_current(
                                    &preflight_guard.authority_lease,
                                    preflight_guard.evidence.directory().expect(
                                        "directory rename guard must retain a bound directory",
                                    ),
                                );
                            let authority_is_stable = preflight_guard.root_is_stable();
                            (
                                evidence_is_stable,
                                observations_are_current,
                                authority_is_stable,
                            )
                        })
                        .await
                        {
                            Ok(stability) => stability,
                            Err(error) => {
                                warn!(to = %pair.to.display(), %error, "Renamed-directory validation task failed");
                                reconciliation_required = true;
                                continue;
                            }
                        };
                    if !evidence_is_stable || !observations_are_current || !authority_is_stable {
                        if !authority_is_stable {
                            mark_cached_root_unavailable(
                                db.as_ref(),
                                &mut root_cache,
                                guard.root_index,
                            )
                            .await;
                        }
                        reconciliation_required = true;
                        continue;
                    }

                    let mut authority_stable_at_commit = true;
                    let mut authority_task_failed = false;
                    let outcome = rename_directory_rows(
                        db.as_ref(),
                        &pair.from,
                        &pair.to,
                        &destination_files,
                        || async {
                            let commit_guard = guard.clone();
                            let commit_scan = scan.clone();
                            match spawn_authority_probe(move || {
                                let evidence_is_stable = commit_guard.evidence_is_stable();
                                let observations_are_current = commit_scan
                                    .observations_still_current(
                                        &commit_guard.authority_lease,
                                        commit_guard.evidence.directory().expect(
                                            "directory rename guard must retain a bound directory",
                                        ),
                                    );
                                let authority_is_stable = commit_guard.root_is_stable();
                                (
                                    evidence_is_stable,
                                    observations_are_current,
                                    authority_is_stable,
                                )
                            })
                            .await
                            {
                                Ok((
                                    evidence_is_stable,
                                    observations_are_current,
                                    authority_is_stable,
                                )) => {
                                    authority_stable_at_commit = authority_is_stable;
                                    evidence_is_stable
                                        && observations_are_current
                                        && authority_stable_at_commit
                                }
                                Err(_) => {
                                    authority_task_failed = true;
                                    false
                                }
                            }
                        },
                    )
                    .await;
                    match outcome {
                        Ok(RenameDirectoryOutcome::Renamed {
                            moved,
                            displaced,
                            unmapped,
                        }) => {
                            if !moved.is_empty() || displaced > 0 {
                                library_snapshot_dirty = true;
                                track_mutation_committed = true;
                            }
                            // Displacing a row nulls its playlist links through
                            // the foreign key; the surviving row can reclaim them.
                            if displaced > 0 {
                                upsert_committed = true;
                            }

                            // Files the rename carried along that were never
                            // indexed — added while the app was closed, or created
                            // inside the destination during the debounce window —
                            // still need their own parse and insert.
                            let claimed: HashSet<&str> = moved
                                .iter()
                                .map(|(_, model)| model.file_path.as_str())
                                .collect();
                            for file in &scan.audio_files {
                                if !claimed.contains(file.to_string_lossy().as_ref()) {
                                    batch.upsert_paths.insert(file.clone());
                                }
                            }

                            if unmapped > 0 {
                                warn!(from = %pair.from.display(), to = %pair.to.display(), unmapped, "Renamed directory is missing indexed files; scheduling reconciliation");
                                reconciliation_required = true;
                            }
                            info!(from = %pair.from.display(), to = %pair.to.display(), moved = moved.len(), displaced, "Preserved track identity across directory rename");
                        }
                        Ok(RenameDirectoryOutcome::GuardRejected) => {
                            if !authority_task_failed && !authority_stable_at_commit {
                                mark_cached_root_unavailable(
                                    db.as_ref(),
                                    &mut root_cache,
                                    guard.root_index,
                                )
                                .await;
                            }
                            warn!(from = %pair.from.display(), to = %pair.to.display(), "Filesystem changed before directory rename commit; transaction rolled back");
                            reconciliation_required = true;
                        }
                        Err(error) => {
                            warn!(from = %pair.from.display(), to = %pair.to.display(), %error, "Failed to update directory rename transactionally");
                            reconciliation_required = true;
                        }
                    }
                }
            }
        }

        // Process removals.
        for path in &batch.remove_paths {
            let path_str = path.to_string_lossy().to_string();
            debug!(path = %path_str, "File removed (debounced)");
            match delete_track_if_root_stable(db.as_ref(), &mut root_cache, music_dirs, path).await
            {
                Ok(true) => {
                    let _ = tx.send(LibraryEvent::TrackRemoved(path_str)).await;
                    track_mutation_committed = true;
                }
                Ok(false) => {
                    warn!(path = %path.display(), "Ignored removal without a stable confirmed library root");
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "Failed to process watched removal safely");
                    reconciliation_required = true;
                }
            }
        }

        // Process upserts from the shared, batch-scoped root snapshot.
        if !batch.upsert_paths.is_empty() {
            debug!(
                count = batch.upsert_paths.len(),
                "Processing debounced upserts"
            );
            let paths: Vec<PathBuf> = batch.upsert_paths.drain().collect();

            for path in paths {
                match root_identity_allows_content(db.as_ref(), &mut root_cache, music_dirs, &path)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        warn!(path = %path.display(), "Ignored change from an unconfirmed or changed library root");
                        if root_cache.authority_was_lost() {
                            reconciliation_required = true;
                        }
                        continue;
                    }
                    Err(error) => {
                        warn!(path = %path.display(), %error, "Failed to verify watched library root");
                        reconciliation_required = true;
                        continue;
                    }
                }
                let Some((root_index, _, _)) = root_cache.root_for_path(&path) else {
                    continue;
                };
                let Some(authority_lease) = root_cache.authority_lease(root_index) else {
                    mark_cached_root_unavailable(db.as_ref(), &mut root_cache, root_index).await;
                    reconciliation_required = true;
                    continue;
                };
                let binding_lease = authority_lease.clone();
                let binding_path = path.clone();
                let binding = spawn_authority_probe(move || {
                    match binding_lease.open_regular_file(&binding_path) {
                        Ok(file) => {
                            let file = Arc::new(file);
                            match file.try_clone_file() {
                                Ok(parser_file) => WatcherFileBinding::Bound { file, parser_file },
                                Err(error) => WatcherFileBinding::Rejected {
                                    error,
                                    authority_stable: binding_lease.validate().is_ok(),
                                },
                            }
                        }
                        Err(open_error) => match binding_lease.prove_absent(&binding_path) {
                            Ok(_) => WatcherFileBinding::Absent,
                            Err(_) => WatcherFileBinding::Rejected {
                                error: open_error,
                                authority_stable: binding_lease.validate().is_ok(),
                            },
                        },
                    }
                })
                .await;
                let (bound_file, parser_file) = match binding {
                    Ok(WatcherFileBinding::Bound { file, parser_file }) => (file, parser_file),
                    Ok(WatcherFileBinding::Absent) => {
                        // Backstop: a debounced "upsert" whose file no longer
                        // exists is really a move/rename away (or a delete the
                        // watcher reported as an ambiguous Modify). Remove the
                        // stale DB row instead of leaving an orphan that fails
                        // to play.
                        let path_str = path.to_string_lossy().to_string();
                        match delete_track_if_root_stable(
                            db.as_ref(),
                            &mut root_cache,
                            music_dirs,
                            &path,
                        )
                        .await
                        {
                            Ok(true) => {
                                let _ = tx.send(LibraryEvent::TrackRemoved(path_str)).await;
                                track_mutation_committed = true;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                warn!(path = %path.display(), %error, "Failed to process missing watched path safely");
                                reconciliation_required = true;
                            }
                        }
                        continue;
                    }
                    Ok(WatcherFileBinding::Rejected {
                        error,
                        authority_stable,
                    }) => {
                        if !authority_stable {
                            mark_cached_root_unavailable(db.as_ref(), &mut root_cache, root_index)
                                .await;
                        }
                        warn!(path = %path.display(), %error, "Could not clone bound watched audio file for parsing; scheduling reconciliation");
                        reconciliation_required = true;
                        continue;
                    }
                    Err(error) => {
                        warn!(path = %path.display(), %error, "Watched-file authority task failed; scheduling reconciliation");
                        reconciliation_required = true;
                        continue;
                    }
                };
                let p = path.clone();
                match tokio::task::spawn_blocking(move || {
                    tag_parser::parse_audio_file_from_file(parser_file, &p)
                })
                .await
                {
                    Ok(Ok(parsed)) => {
                        // Parsing can take long enough for a removable/network
                        // volume to disappear or be replaced. Revalidate the
                        // root immediately before touching persisted metadata.
                        match root_identity_allows_content(
                            db.as_ref(),
                            &mut root_cache,
                            music_dirs,
                            &path,
                        )
                        .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                warn!(path = %path.display(), "Library root changed while parsing — upsert discarded");
                                if root_cache.authority_was_lost() {
                                    reconciliation_required = true;
                                }
                                continue;
                            }
                            Err(error) => {
                                warn!(path = %path.display(), %error, "Post-parse root authority task failed — upsert discarded");
                                reconciliation_required = true;
                                continue;
                            }
                        }
                        let path_str = parsed.file_path.clone();
                        let existing = track::Entity::find()
                            .filter(track::Column::FilePath.eq(&path_str))
                            .one(db.as_ref())
                            .await
                            .ok()
                            .flatten();

                        let preflight_file = bound_file.clone();
                        let preflight_lease = authority_lease.clone();
                        let file_is_stable = match spawn_authority_probe(move || {
                            preflight_file.validate(&preflight_lease).is_ok()
                        })
                        .await
                        {
                            Ok(still_stable) => still_stable,
                            Err(error) => {
                                warn!(path = %path.display(), %error, "Watched-file validation task failed; scheduling reconciliation");
                                reconciliation_required = true;
                                continue;
                            }
                        };
                        if !file_is_stable {
                            warn!(path = %path.display(), "Watched audio path changed after parsing; scheduling reconciliation");
                            reconciliation_required = true;
                            continue;
                        }

                        let mut authority_stable_at_commit = true;
                        let mut authority_task_failed = false;
                        let outcome = upsert_track_with_commit_guard(
                            db.as_ref(),
                            &parsed,
                            existing.as_ref(),
                            || async {
                                let commit_file = bound_file.clone();
                                let commit_lease = authority_lease.clone();
                                match spawn_authority_probe(move || {
                                    let file_is_stable =
                                        commit_file.validate(&commit_lease).is_ok();
                                    let authority_stable = commit_lease.validate().is_ok();
                                    (file_is_stable, authority_stable)
                                })
                                .await
                                {
                                    Ok((file_is_stable, authority_stable)) => {
                                        authority_stable_at_commit = authority_stable;
                                        file_is_stable && authority_stable_at_commit
                                    }
                                    Err(_) => {
                                        authority_task_failed = true;
                                        false
                                    }
                                }
                            },
                        )
                        .await;
                        match outcome {
                            Ok(GuardedTrackUpsertOutcome::Committed(model)) => {
                                upsert_committed = true;
                                track_mutation_committed = true;
                                let t = db_model_to_track(&model);
                                let _ = tx.send(LibraryEvent::TrackUpserted(Box::new(t))).await;
                            }
                            Ok(GuardedTrackUpsertOutcome::GuardRejected) => {
                                if !authority_task_failed && !authority_stable_at_commit {
                                    mark_cached_root_unavailable(
                                        db.as_ref(),
                                        &mut root_cache,
                                        root_index,
                                    )
                                    .await;
                                }
                                warn!(path = %path.display(), "Filesystem changed before watched upsert commit; transaction rolled back");
                                reconciliation_required = true;
                            }
                            Err(error) => {
                                warn!(%error, path = %path.display(), "Failed to upsert track");
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, path = %path.display(), "Failed to parse audio file");
                    }
                    Err(e) => {
                        warn!(error = %e, "spawn_blocking failed");
                    }
                }
            }
        }

        // Unpairable rename shapes and unclaimed directory changes deliberately
        // avoid guessing identity. Reuse the hardened authoritative scan once
        // per batch as the conservative reconciliation fallback. It publishes
        // its own snapshot, so no separate one is needed here.
        reconciliation_required |= root_cache.authority_was_lost();
        if reconciliation_required {
            info!("Reconciling library after unpaired or unclaimed watcher changes");
            if let Err(error) = initial_scan(db.as_ref(), music_dirs, tx).await {
                warn!(%error, "Watcher-triggered library reconciliation failed");
            }
            continue;
        }

        // A directory rename retargets an unbounded number of rows at once.
        // Publish one snapshot rather than a per-row event storm, which the GTK
        // receiver would resolve against the whole library once per row.
        if library_snapshot_dirty {
            send_library_snapshot(db.as_ref(), tx).await;
        }

        // Deletions null playlist links through the database foreign key.
        // Reconcile once after all successful upserts in this debounced batch
        // so a replacement file can restore those links immediately. A
        // reconciliation error is retryable and must not terminate watching.
        match settle_playlist_projections_after_watcher_batch(
            db.as_ref(),
            tx,
            upsert_committed,
            track_mutation_committed,
        )
        .await
        {
            Ok(relinked) if relinked > 0 => {
                info!(relinked, "Playlist entries reconciled after watcher batch");
            }
            Ok(_) => debug!("No orphaned playlist entries matched watcher upserts"),
            Err(error) => {
                warn!(%error, "Failed to reconcile playlists after watcher batch");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Database helpers
// ---------------------------------------------------------------------------

/// Insert or update a track in the database, returning the final Model.
async fn upsert_track<C>(
    db: &C,
    parsed: &ParsedTrack,
    existing: Option<&track::Model>,
) -> anyhow::Result<track::Model>
where
    C: ConnectionTrait,
{
    let now = Utc::now().to_rfc3339();
    let mtime = parsed.date_modified.to_rfc3339();

    if let Some(row) = existing {
        // Update existing
        let mut active: track::ActiveModel = row.clone().into();
        apply_parsed_track_fields(&mut active, parsed, mtime);

        let model = active.update(db).await?;
        debug!(path = %parsed.file_path, "Updated track in database");
        Ok(model)
    } else {
        // Insert new
        let id = Uuid::new_v4().to_string();
        let active = track::ActiveModel {
            id: Set(id),
            file_path: Set(parsed.file_path.clone()),
            title: Set(parsed.title.clone()),
            artist_name: Set(parsed.artist_name.clone()),
            album_artist_name: Set(parsed.album_artist_name.clone()),
            album_title: Set(parsed.album_title.clone()),
            genre: Set(parsed.genre.clone()),
            year: Set(parsed.year),
            track_number: Set(parsed.track_number.map(|n| n as i32)),
            disc_number: Set(parsed.disc_number.map(|n| n as i32)),
            duration_secs: Set(parsed.duration_secs.map(|d| d as i64)),
            bitrate_kbps: Set(parsed.bitrate_kbps.map(|b| b as i32)),
            sample_rate_hz: Set(parsed.sample_rate_hz.map(|s| s as i32)),
            format: Set(Some(parsed.format.clone())),
            play_count: Set(0),
            date_added: Set(now),
            date_modified: Set(mtime),
            file_size_bytes: Set(parsed.file_size_bytes.map(|s| s as i64)),
        };

        let model = active.insert(db).await?;
        debug!(path = %parsed.file_path, "Inserted new track into database");
        Ok(model)
    }
}

#[derive(Debug)]
enum GuardedTrackUpsertOutcome {
    Committed(Box<track::Model>),
    GuardRejected,
}

/// Apply one parsed-track mutation only if its filesystem evidence still
/// holds after the SQL write and immediately before commit.
async fn upsert_track_with_commit_guard<F, Fut>(
    db: &DatabaseConnection,
    parsed: &ParsedTrack,
    existing: Option<&track::Model>,
    commit_guard: F,
) -> anyhow::Result<GuardedTrackUpsertOutcome>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let transaction = db.begin().await?;
    let result = upsert_track(&transaction, parsed, existing).await;
    match result {
        Ok(model) if commit_guard().await => {
            transaction.commit().await?;
            Ok(GuardedTrackUpsertOutcome::Committed(Box::new(model)))
        }
        Ok(_) => {
            transaction.rollback().await?;
            Ok(GuardedTrackUpsertOutcome::GuardRejected)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum GuardedTrackDeleteOutcome {
    Deleted,
    Missing,
    GuardRejected,
}

/// Delete one track only if its root and path evidence still hold after the
/// SQL write and immediately before commit.
async fn delete_track_with_commit_guard<F, Fut>(
    db: &DatabaseConnection,
    id: &str,
    commit_guard: F,
) -> anyhow::Result<GuardedTrackDeleteOutcome>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let transaction = db.begin().await?;
    let result = track::Entity::delete_by_id(id).exec(&transaction).await;
    match result {
        Ok(result) if result.rows_affected == 0 => {
            transaction.rollback().await?;
            Ok(GuardedTrackDeleteOutcome::Missing)
        }
        Ok(_) if commit_guard().await => {
            transaction.commit().await?;
            Ok(GuardedTrackDeleteOutcome::Deleted)
        }
        Ok(_) => {
            transaction.rollback().await?;
            Ok(GuardedTrackDeleteOutcome::GuardRejected)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error.into())
        }
    }
}

fn apply_parsed_track_fields(
    active: &mut track::ActiveModel,
    parsed: &ParsedTrack,
    date_modified: String,
) {
    active.file_path = Set(parsed.file_path.clone());
    active.title = Set(parsed.title.clone());
    active.artist_name = Set(parsed.artist_name.clone());
    active.album_artist_name = Set(parsed.album_artist_name.clone());
    active.album_title = Set(parsed.album_title.clone());
    active.genre = Set(parsed.genre.clone());
    active.year = Set(parsed.year);
    active.track_number = Set(parsed.track_number.map(|n| n as i32));
    active.disc_number = Set(parsed.disc_number.map(|n| n as i32));
    active.duration_secs = Set(parsed.duration_secs.map(|d| d as i64));
    active.bitrate_kbps = Set(parsed.bitrate_kbps.map(|b| b as i32));
    active.sample_rate_hz = Set(parsed.sample_rate_hz.map(|s| s as i32));
    active.format = Set(Some(parsed.format.clone()));
    active.date_modified = Set(date_modified);
    active.file_size_bytes = Set(parsed.file_size_bytes.map(|s| s as i64));
}

#[derive(Debug)]
enum RenameTrackOutcome {
    Renamed {
        model: Box<track::Model>,
        displaced: Option<Box<track::Model>>,
    },
    SourceMissing,
    GuardRejected,
}

/// Atomically retarget one existing track row to an authoritative paired
/// rename destination. The row ID, date-added timestamp, play count, and
/// playlist references remain untouched. If the filesystem rename replaced
/// an already-indexed destination, that displaced row is removed in the same
/// transaction before the source claims its unique path.
async fn rename_track_row<F, Fut>(
    db: &DatabaseConnection,
    from: &Path,
    to: &Path,
    parsed: Option<&ParsedTrack>,
    commit_guard: F,
) -> anyhow::Result<RenameTrackOutcome>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let from_path = from.to_string_lossy().into_owned();
    let to_path = to.to_string_lossy().into_owned();
    if parsed.is_some_and(|parsed| parsed.file_path != to_path) {
        return Err(anyhow::anyhow!(
            "parsed rename destination does not match the paired target path"
        ));
    }
    let transaction = db.begin().await?;

    let result: anyhow::Result<RenameTrackOutcome> = async {
        let Some(source) = track::Entity::find()
            .filter(track::Column::FilePath.eq(&from_path))
            .one(&transaction)
            .await?
        else {
            return Ok(RenameTrackOutcome::SourceMissing);
        };

        let displaced = track::Entity::find()
            .filter(track::Column::FilePath.eq(&to_path))
            .one(&transaction)
            .await?
            .filter(|destination| destination.id != source.id);
        if let Some(destination) = &displaced {
            track::Entity::delete_by_id(&destination.id)
                .exec(&transaction)
                .await?;
        }

        let model = if let Some(parsed) = parsed {
            upsert_track(&transaction, parsed, Some(&source)).await?
        } else {
            let mut active: track::ActiveModel = source.into();
            active.file_path = Set(to_path);
            active.update(&transaction).await?
        };

        if !commit_guard().await {
            return Ok(RenameTrackOutcome::GuardRejected);
        }

        Ok(RenameTrackOutcome::Renamed {
            model: Box::new(model),
            displaced: displaced.map(Box::new),
        })
    }
    .await;

    match result {
        Ok(outcome @ RenameTrackOutcome::Renamed { .. }) => {
            transaction.commit().await?;
            Ok(outcome)
        }
        Ok(outcome) => {
            transaction.rollback().await?;
            Ok(outcome)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

#[derive(Debug)]
enum RenameDirectoryOutcome {
    Renamed {
        /// `(previous path, retargeted row)` for every descendant that kept its
        /// identity.
        moved: Vec<(String, track::Model)>,
        /// Stale rows evicted from a destination path before it was claimed.
        displaced: usize,
        /// Indexed descendants with no file at the mirrored destination. Their
        /// rows are left untouched for reconciliation to resolve.
        unmapped: usize,
    },
    GuardRejected,
}

/// Retarget every indexed descendant of an authoritative paired directory
/// rename in one transaction.
///
/// Row IDs, `date_added`, play counts, and playlist references survive; only
/// `file_path` moves. A directory rename changes no file content, so tags are
/// not reparsed and `date_modified` is left alone.
///
/// `destination_files` is the completed scoped traversal of `to`. A descendant
/// is moved only when a real file was observed at its mirrored destination, and
/// the caller's commit guard revalidates the retained filesystem handles before
/// the transaction commits. A descendant without an observed destination is
/// reported as `unmapped` rather than followed to a path that does not exist.
/// Files under `to` that no row claims are surplus: they were never indexed,
/// and the caller upserts them normally.
async fn rename_directory_rows<F, Fut>(
    db: &DatabaseConnection,
    from: &Path,
    to: &Path,
    destination_files: &HashSet<String>,
    commit_guard: F,
) -> anyhow::Result<RenameDirectoryOutcome>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    // Rows are persisted through `to_string_lossy`, so a non-UTF-8 name never
    // round-trips back to its original bytes. Matching in the database's own
    // lossy namespace keeps those rows reachable; matching through `Path` keeps
    // the prefix component-wise, so `/music/Album` cannot capture the sibling
    // `/music/Album2`.
    let from_prefix = PathBuf::from(from.to_string_lossy().into_owned());
    let to_prefix = PathBuf::from(to.to_string_lossy().into_owned());
    if from_prefix.starts_with(&to_prefix) || to_prefix.starts_with(&from_prefix) {
        return Err(anyhow::anyhow!(
            "paired directory rename source and destination overlap"
        ));
    }

    let transaction = db.begin().await?;

    let result: anyhow::Result<RenameDirectoryOutcome> = async {
        let rows = track::Entity::find().all(&transaction).await?;

        let mut moves: Vec<(track::Model, String)> = Vec::new();
        let mut unmapped = 0usize;
        for row in &rows {
            let Ok(relative) = Path::new(&row.file_path).strip_prefix(&from_prefix) else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }
            let destination = to_prefix.join(relative).to_string_lossy().into_owned();
            if destination_files.contains(&destination) {
                moves.push((row.clone(), destination));
            } else {
                unmapped += 1;
            }
        }

        // `tracks.file_path` is unique. The filesystem cannot leave a file
        // parked at a destination path — a directory rename only succeeds onto
        // an empty destination — but a stale row can still sit there, left by a
        // scan that was never authoritative enough to delete it. Evict it before
        // its path is claimed, or the update aborts on the unique index.
        let claimed: HashSet<&str> = moves
            .iter()
            .map(|(_, destination)| destination.as_str())
            .collect();
        let mut displaced = 0usize;
        for row in &rows {
            if claimed.contains(row.file_path.as_str()) {
                track::Entity::delete_by_id(&row.id)
                    .exec(&transaction)
                    .await?;
                displaced += 1;
            }
        }

        let mut moved = Vec::with_capacity(moves.len());
        for (row, destination) in moves {
            let previous = row.file_path.clone();
            let mut active: track::ActiveModel = row.into();
            active.file_path = Set(destination);
            moved.push((previous, active.update(&transaction).await?));
        }

        if !commit_guard().await {
            return Ok(RenameDirectoryOutcome::GuardRejected);
        }

        Ok(RenameDirectoryOutcome::Renamed {
            moved,
            displaced,
            unmapped,
        })
    }
    .await;

    match result {
        Ok(outcome @ RenameDirectoryOutcome::Renamed { .. }) => {
            transaction.commit().await?;
            Ok(outcome)
        }
        Ok(outcome) => {
            transaction.rollback().await?;
            Ok(outcome)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

/// Publish the committed library as one authoritative snapshot.
///
/// Bulk changes emit this instead of a per-row event storm: the receiving GTK
/// thread rebuilds from a snapshot in one pass, and the playback queue
/// re-resolves its items by their stable track IDs.
async fn send_library_snapshot(db: &DatabaseConnection, tx: &async_channel::Sender<LibraryEvent>) {
    match track::Entity::find().all(db).await {
        Ok(rows) => {
            let all_tracks: Vec<Track> = rows.iter().map(db_model_to_track).collect();
            let _ = tx.send(LibraryEvent::FullSync(all_tracks)).await;
        }
        Err(error) => warn!(%error, "Failed to load tracks for full sync"),
    }
}

/// Get the RFC3339 mtime string for a path (for DB comparison).
fn get_mtime(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| {
            let dt: DateTime<Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default()
}

/// Convert a database `track::Model` to an architecture `Track`.
pub fn db_model_to_track(model: &track::Model) -> Track {
    Track {
        id: Uuid::parse_str(&model.id).unwrap_or_else(|_| Uuid::new_v4()),
        title: model.title.clone(),
        artist_name: model.artist_name.clone(),
        album_artist_name: model.album_artist_name.clone(),
        artist_id: None,
        album_title: model.album_title.clone(),
        album_id: None,
        track_number: model.track_number.map(|n| n as u32),
        disc_number: model.disc_number.map(|n| n as u32),
        duration_secs: model.duration_secs.map(|d| d as u64),
        genre: model.genre.clone(),
        year: model.year,
        file_path: Some(model.file_path.clone()),
        stream_url: None,
        cover_art_url: None,
        date_added: chrono::DateTime::parse_from_rfc3339(&model.date_added)
            .ok()
            .map(|dt| dt.with_timezone(&Utc)),
        date_modified: chrono::DateTime::parse_from_rfc3339(&model.date_modified)
            .ok()
            .map(|dt| dt.with_timezone(&Utc)),
        bitrate_kbps: model.bitrate_kbps.map(|b| b as u32),
        sample_rate_hz: model.sample_rate_hz.map(|s| s as u32),
        format: model.format.clone(),
        play_count: Some(model.play_count as u32),
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::QueryOrder;

    use super::*;

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("tributary-engine-{label}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn test_root_authority(root: &Path) -> RootAuthorityLease {
        let identity = create_root_marker(root)
            .expect("create test root marker")
            .identity;
        RootAuthorityLease::acquire(root, &identity).expect("retain test root authority")
    }

    fn rename_event(
        mode: notify::event::RenameMode,
        paths: &[&str],
        tracker: Option<usize>,
    ) -> notify::Event {
        let mut event = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Name(mode),
        ));
        for path in paths {
            event = event.add_path(PathBuf::from(path));
        }
        if let Some(tracker) = tracker {
            event = event.set_tracker(tracker);
        }
        event
    }

    #[test]
    fn watcher_ingress_overflow_is_nonblocking_and_marks_stream_unreliable() {
        let (tx, mut rx) = mpsc::channel(1);
        let overflowed = AtomicBool::new(false);
        let first = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
            .add_path(PathBuf::from("/music/first.flac"));
        let dropped =
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                .add_path(PathBuf::from("/music/dropped.flac"));

        enqueue_watcher_result(&tx, &overflowed, Ok(first));
        enqueue_watcher_result(&tx, &overflowed, Ok(dropped));

        assert!(overflowed.load(Ordering::Acquire));
        let queued = rx
            .try_recv()
            .expect("the event accepted before overflow remains queued")
            .expect("queued notify event");
        assert_eq!(queued.paths, [PathBuf::from("/music/first.flac")]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn watcher_retries_a_root_that_appears_during_bootstrap() {
        let library = TestDirectory::new("watcher-registration-retry");
        let ready = library.path().join("ready");
        let late = library.path().join("late");
        std::fs::create_dir(&ready).expect("create initially available root");

        let mut watcher = install_directory_watcher(&[ready.clone(), late.clone()])
            .expect("install directory watcher");
        assert!(watcher.watched_directories.contains(&ready));
        assert!(!watcher.watched_directories.contains(&late));

        std::fs::create_dir(&late).expect("make root available during bootstrap");
        watcher.watch_available_directories(&[ready.clone(), late.clone()]);

        assert_eq!(
            watcher.watched_directories,
            HashSet::from([ready, late]),
            "the handoff retry retains old registrations and closes new gaps"
        );
    }

    #[test]
    fn watcher_ingress_replays_buffered_rename_halves_in_order() {
        let (tx, mut rx) = mpsc::channel(2);
        let overflowed = AtomicBool::new(false);
        enqueue_watcher_result(
            &tx,
            &overflowed,
            Ok(rename_event(
                notify::event::RenameMode::From,
                &["/music/old.flac"],
                Some(51),
            )),
        );
        enqueue_watcher_result(
            &tx,
            &overflowed,
            Ok(rename_event(
                notify::event::RenameMode::To,
                &["/music/new.flac"],
                Some(51),
            )),
        );

        let mut ingress = WatcherDebounceBatch::default();
        while let Ok(result) = rx.try_recv() {
            ingress.collect(result);
        }
        let batch = ingress.finish().expect("ordinary event stream is reliable");

        assert!(!overflowed.load(Ordering::Acquire));
        assert_eq!(
            batch.rename_pairs,
            HashSet::from([WatcherRenamePair {
                from: PathBuf::from("/music/old.flac"),
                to: PathBuf::from("/music/new.flac"),
            }])
        );
        assert!(batch.remove_paths.is_empty());
        assert!(batch.upsert_paths.is_empty());
    }

    #[test]
    fn watcher_error_and_rescan_notice_make_debounce_unreliable() {
        let mut failed = WatcherDebounceBatch::default();
        failed.collect(Err(notify::Error::generic("backend failed")));
        assert!(failed.finish().is_none());

        let mut requested = WatcherDebounceBatch::default();
        requested.collect(Ok(
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ));
        assert!(requested.finish().is_none());
    }

    #[test]
    fn watcher_error_discards_mixed_incremental_batch_and_backlog() {
        let mut ingress = WatcherDebounceBatch::default();
        ingress.collect(Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File,
        ))
        .add_path(PathBuf::from("/music/must-not-upsert.flac"))));
        ingress.collect(Err(notify::Error::generic("events were lost")));
        ingress.collect(Ok(notify::Event::new(notify::EventKind::Remove(
            notify::event::RemoveKind::File,
        ))
        .add_path(PathBuf::from("/music/must-not-remove.flac"))));

        assert!(ingress.batch.is_empty());
        assert!(ingress.finish().is_none());

        let (tx, mut rx) = mpsc::channel(2);
        tx.try_send(Ok(notify::Event::new(notify::EventKind::Other)))
            .expect("queue stale backlog");
        discard_watcher_backlog(&mut rx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn watcher_reconciliation_preserves_racing_overflow_and_new_events() {
        let (tx, mut rx) = mpsc::channel(2);
        let overflowed = AtomicBool::new(true);

        assert!(overflowed.swap(false, Ordering::AcqRel));
        discard_watcher_backlog(&mut rx);

        // Simulate callbacks arriving after recovery began. The runtime must
        // not clear either signal at the end of the scan.
        overflowed.store(true, Ordering::Release);
        tx.try_send(Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File,
        ))
        .add_path(PathBuf::from("/music/during-scan.flac"))))
            .expect("queue event arriving during reconciliation");

        assert!(overflowed.load(Ordering::Acquire));
        assert_eq!(
            rx.try_recv()
                .expect("racing event remains queued")
                .expect("notify event")
                .paths,
            [PathBuf::from("/music/during-scan.flac")]
        );
    }

    #[test]
    fn marker_mutation_requires_reconciliation_before_incrementals() {
        let mut batch = WatcherBatch::default();
        batch.collect(
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                .add_path(PathBuf::from(format!("/music/{ROOT_IDENTITY_FILE}"))),
        );
        batch.collect(
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                .add_path(PathBuf::from("/music/mixed.flac")),
        );
        batch.finish();

        assert!(batch.requires_reconciliation_before_incrementals());
        assert_eq!(
            batch.identity_changed_roots,
            HashSet::from([PathBuf::from("/music")])
        );
        assert!(batch.upsert_paths.contains(Path::new("/music/mixed.flac")));
    }

    #[test]
    fn watcher_batch_normalizes_both_rename_without_fallback_paths() {
        let mut batch = WatcherBatch::default();
        batch.collect(rename_event(
            notify::event::RenameMode::Both,
            &["/music/old.flac", "/music/new.flac"],
            Some(7),
        ));

        assert_eq!(
            batch.rename_pairs,
            HashSet::from([WatcherRenamePair {
                from: PathBuf::from("/music/old.flac"),
                to: PathBuf::from("/music/new.flac"),
            }])
        );
        assert!(batch.remove_paths.is_empty());
        assert!(batch.upsert_paths.is_empty());
        assert!(!batch.reconciliation_required);
    }

    #[test]
    fn watcher_batch_deduplicates_linux_from_to_and_both_events() {
        let mut batch = WatcherBatch::default();
        batch.collect(rename_event(
            notify::event::RenameMode::From,
            &["/music/old.flac"],
            Some(41),
        ));
        batch.collect(rename_event(
            notify::event::RenameMode::To,
            &["/music/new.flac"],
            Some(41),
        ));
        batch.collect(rename_event(
            notify::event::RenameMode::Both,
            &["/music/old.flac", "/music/new.flac"],
            Some(41),
        ));

        assert_eq!(batch.rename_pairs.len(), 1);
        assert!(batch.remove_paths.is_empty());
        assert!(batch.upsert_paths.is_empty());
    }

    #[test]
    fn watcher_batch_pairs_only_adjacent_untracked_windows_halves() {
        let mut paired = WatcherBatch::default();
        paired.collect(rename_event(
            notify::event::RenameMode::From,
            &["C:/Music/old.flac"],
            None,
        ));
        paired.collect(rename_event(
            notify::event::RenameMode::To,
            &["C:/Music/new.flac"],
            None,
        ));
        assert_eq!(paired.rename_pairs.len(), 1);
        assert!(paired.remove_paths.is_empty());
        assert!(paired.upsert_paths.is_empty());

        let mut interleaved = WatcherBatch::default();
        interleaved.collect(rename_event(
            notify::event::RenameMode::From,
            &["C:/Music/old.flac"],
            None,
        ));
        interleaved.collect(
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                .add_path(PathBuf::from("C:/Music/unrelated.flac")),
        );
        interleaved.collect(rename_event(
            notify::event::RenameMode::To,
            &["C:/Music/new.flac"],
            None,
        ));
        assert!(interleaved.rename_pairs.is_empty());
        assert!(interleaved
            .remove_paths
            .contains(Path::new("C:/Music/old.flac")));
        assert!(interleaved
            .upsert_paths
            .contains(Path::new("C:/Music/new.flac")));
    }

    #[test]
    fn watcher_batch_routes_unpairable_and_directory_events_to_reconciliation() {
        let mut batch = WatcherBatch::default();
        batch.collect(rename_event(
            notify::event::RenameMode::Any,
            &["/music/unknown"],
            None,
        ));
        batch.collect(
            notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::Folder))
                .add_path(PathBuf::from("/music/album")),
        );

        assert!(batch.reconciliation_required);
        assert!(batch.rename_pairs.is_empty());
        assert!(
            batch.deferred_paths.contains(Path::new("/music/album")),
            "folder changes remain available as dirty scopes for a paired parent rename"
        );
    }

    #[test]
    fn watcher_batch_queues_regular_and_missing_audio_paths_only() {
        let library = TestDirectory::new("watcher-upsert-paths");
        let regular = library.path().join("regular.flac");
        let missing = library.path().join("missing.flac");
        std::fs::write(&regular, b"audio").expect("create regular audio path");

        let mut batch = WatcherBatch::default();
        batch.record_upsert(regular.clone());
        batch.record_upsert(missing.clone());

        assert_eq!(
            watcher_upsert_path_kind(&regular).expect("classify regular path"),
            WatcherUpsertPathKind::RegularFile
        );
        assert_eq!(
            watcher_upsert_path_kind(&missing).expect("classify missing path"),
            WatcherUpsertPathKind::Missing
        );
        assert!(batch.upsert_paths.contains(&regular));
        assert!(
            batch.upsert_paths.contains(&missing),
            "a vanished upsert must reach the guarded removal backstop"
        );
        assert!(!batch.reconciliation_required);
    }

    #[cfg(unix)]
    #[test]
    fn watcher_batch_rejects_symlinked_audio_upserts() {
        let library = TestDirectory::new("watcher-symlink-upsert");
        let target = library.path().join("target.flac");
        let linked = library.path().join("linked.flac");
        std::fs::write(&target, b"audio").expect("create symlink target");
        std::os::unix::fs::symlink(&target, &linked).expect("create audio symlink");

        let mut batch = WatcherBatch::default();
        batch.record_upsert(linked.clone());

        assert_eq!(
            watcher_upsert_path_kind(&linked).expect("classify symlink"),
            WatcherUpsertPathKind::Unsafe
        );
        assert!(!batch.upsert_paths.contains(&linked));
        assert!(
            batch.reconciliation_required,
            "a symlink must use the authoritative no-follow scan"
        );
    }

    async fn rename_test_database() -> DatabaseConnection {
        use sea_orm::Database;
        use sea_orm_migration::MigratorTrait;

        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::db::migration::Migrator::up(&db, None)
            .await
            .expect("run migrations");
        db
    }

    async fn insert_rename_test_track(
        db: &DatabaseConnection,
        id: &str,
        path: &str,
        title: &str,
        play_count: i32,
    ) -> track::Model {
        let model = track::Model {
            id: id.to_string(),
            file_path: path.to_string(),
            title: title.to_string(),
            artist_name: "Original Artist".to_string(),
            album_artist_name: Some("Original Album Artist".to_string()),
            album_title: "Original Album".to_string(),
            genre: Some("Original Genre".to_string()),
            year: Some(2001),
            track_number: Some(1),
            disc_number: Some(1),
            duration_secs: Some(180),
            bitrate_kbps: Some(192),
            sample_rate_hz: Some(44_100),
            format: Some("FLAC".to_string()),
            play_count,
            date_added: "2025-01-02T03:04:05Z".to_string(),
            date_modified: "2025-01-02T03:04:05Z".to_string(),
            file_size_bytes: Some(1_000),
        };
        let active: track::ActiveModel = model.into();
        active.insert(db).await.expect("insert rename test track")
    }

    fn parsed_rename_track(path: &str, title: &str) -> ParsedTrack {
        ParsedTrack {
            file_path: path.to_string(),
            title: title.to_string(),
            artist_name: "Updated Artist".to_string(),
            album_artist_name: Some("Updated Album Artist".to_string()),
            album_title: "Updated Album".to_string(),
            genre: Some("Updated Genre".to_string()),
            year: Some(2026),
            track_number: Some(2),
            disc_number: Some(2),
            duration_secs: Some(240),
            bitrate_kbps: Some(320),
            sample_rate_hz: Some(48_000),
            format: "FLAC".to_string(),
            date_modified: chrono::DateTime::parse_from_rfc3339("2026-07-12T12:34:56Z")
                .expect("parse fixture timestamp")
                .with_timezone(&Utc),
            file_size_bytes: Some(2_000),
        }
    }

    #[tokio::test]
    async fn guarded_track_upserts_roll_back_inserts_and_updates() {
        use std::cell::Cell;

        let db = rename_test_database().await;
        let inserted = parsed_rename_track("/music/guarded.flac", "Inserted");
        let insert_guard_calls = Cell::new(0);

        let insert_outcome = upsert_track_with_commit_guard(&db, &inserted, None, || async {
            insert_guard_calls.set(insert_guard_calls.get() + 1);
            false
        })
        .await
        .expect("reject guarded insert");
        assert!(matches!(
            insert_outcome,
            GuardedTrackUpsertOutcome::GuardRejected
        ));
        assert_eq!(insert_guard_calls.get(), 1);
        assert!(track::Entity::find()
            .one(&db)
            .await
            .expect("query rolled-back insert")
            .is_none());

        let existing = insert_rename_test_track(
            &db,
            "guarded-update-track",
            "/music/guarded.flac",
            "Original",
            11,
        )
        .await;
        let updated = parsed_rename_track("/music/guarded.flac", "Updated");
        let update_guard_calls = Cell::new(0);

        let update_outcome =
            upsert_track_with_commit_guard(&db, &updated, Some(&existing), || async {
                update_guard_calls.set(update_guard_calls.get() + 1);
                false
            })
            .await
            .expect("reject guarded update");
        assert!(matches!(
            update_outcome,
            GuardedTrackUpsertOutcome::GuardRejected
        ));
        assert_eq!(update_guard_calls.get(), 1);

        let unchanged = track::Entity::find_by_id(&existing.id)
            .one(&db)
            .await
            .expect("query rolled-back update")
            .expect("original track remains");
        assert_eq!(unchanged, existing);
    }

    #[tokio::test]
    async fn authority_probe_join_failure_rejects_the_pending_transaction() {
        let db = rename_test_database().await;
        let parsed = parsed_rename_track("/music/join-failure.flac", "Rejected");

        let outcome = upsert_track_with_commit_guard(&db, &parsed, None, || async {
            let probe = spawn_authority_probe(|| -> bool {
                panic!("simulate retained-authority task failure");
            })
            .await;
            assert!(probe.is_err());
            false
        })
        .await
        .expect("reject transaction after authority task failure");

        assert!(matches!(outcome, GuardedTrackUpsertOutcome::GuardRejected));
        assert!(track::Entity::find()
            .one(&db)
            .await
            .expect("query rolled-back join-failure insert")
            .is_none());
    }

    #[tokio::test]
    async fn guarded_track_delete_rollback_preserves_playlist_linkage() {
        use std::cell::Cell;

        use crate::db::entities::playlist_entry;

        let db = rename_test_database().await;
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Guarded delete", false)
            .await
            .expect("create playlist");
        let existing = insert_rename_test_track(
            &db,
            "guarded-delete-track",
            "/music/missing.flac",
            "Remembered",
            7,
        )
        .await;
        manager
            .add_track(&playlist.id, &existing)
            .await
            .expect("link track to playlist");
        let entry_before = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .one(&db)
            .await
            .expect("query playlist entry")
            .expect("playlist entry exists");
        let guard_calls = Cell::new(0);

        let outcome = delete_track_with_commit_guard(&db, &existing.id, || async {
            guard_calls.set(guard_calls.get() + 1);
            false
        })
        .await
        .expect("reject guarded deletion");

        assert_eq!(outcome, GuardedTrackDeleteOutcome::GuardRejected);
        assert_eq!(guard_calls.get(), 1);
        assert_eq!(
            track::Entity::find_by_id(&existing.id)
                .one(&db)
                .await
                .expect("query retained track"),
            Some(existing)
        );
        assert_eq!(
            playlist_entry::Entity::find_by_id(&entry_before.id)
                .one(&db)
                .await
                .expect("query retained playlist entry"),
            Some(entry_before)
        );
    }

    #[tokio::test]
    async fn root_state_promotions_roll_back_when_retained_authority_changes() {
        let db = rename_test_database().await;
        let replacement_identity = format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4());

        let inserted_root = TestDirectory::new("guarded-root-state-insert");
        create_root_marker(inserted_root.path()).expect("create insert root marker");
        let insert_scan = scan_root(inserted_root.path().to_path_buf());
        std::fs::write(
            root_identity_path(inserted_root.path()),
            format!("{replacement_identity}\n"),
        )
        .expect("change insert root authority");

        assert!(
            persist_root_scan_status(&db, &insert_scan, None, true, true, false)
                .await
                .is_err()
        );
        assert!(library_root::Entity::find_by_id(
            inserted_root.path().to_string_lossy().into_owned()
        )
        .one(&db)
        .await
        .expect("query rolled-back root insert")
        .is_none());

        let updated_root = TestDirectory::new("guarded-root-state-update");
        create_root_marker(updated_root.path()).expect("create update root marker");
        let update_scan = scan_root(updated_root.path().to_path_buf());
        let staged = persist_root_scan_status(&db, &update_scan, None, false, false, false)
            .await
            .expect("stage unavailable root state");
        std::fs::write(
            root_identity_path(updated_root.path()),
            format!("{replacement_identity}\n"),
        )
        .expect("change update root authority");

        assert!(
            persist_root_scan_status(&db, &update_scan, Some(&staged), true, true, false)
                .await
                .is_err()
        );
        assert_eq!(
            library_root::Entity::find_by_id(updated_root.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query rolled-back root promotion"),
            Some(staged)
        );
    }

    #[tokio::test]
    async fn paired_rename_preserves_track_history_and_playlist_linkage() {
        use crate::db::entities::playlist_entry;

        let db = rename_test_database().await;
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Rename", false)
            .await
            .expect("create playlist");
        let source = insert_rename_test_track(
            &db,
            "stable-track-id",
            "/music/old.flac",
            "Original Title",
            17,
        )
        .await;
        manager
            .add_track(&playlist.id, &source)
            .await
            .expect("add source to playlist");
        let entry_before = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .one(&db)
            .await
            .expect("load playlist entry")
            .expect("playlist entry exists");
        let parsed = parsed_rename_track("/music/new.flac", "Updated Title");

        let outcome = rename_track_row(
            &db,
            Path::new("/music/old.flac"),
            Path::new("/music/new.flac"),
            Some(&parsed),
            || async { true },
        )
        .await
        .expect("rename track row");
        assert!(matches!(
            outcome,
            RenameTrackOutcome::Renamed {
                displaced: None,
                ..
            }
        ));

        let renamed = track::Entity::find_by_id("stable-track-id")
            .one(&db)
            .await
            .expect("load renamed track")
            .expect("renamed track exists");
        assert_eq!(renamed.file_path, "/music/new.flac");
        assert_eq!(renamed.title, "Updated Title");
        assert_eq!(renamed.artist_name, "Updated Artist");
        assert_eq!(renamed.play_count, 17);
        assert_eq!(renamed.date_added, "2025-01-02T03:04:05Z");

        let entry_after = playlist_entry::Entity::find_by_id(&entry_before.id)
            .one(&db)
            .await
            .expect("reload playlist entry")
            .expect("playlist entry remains");
        assert_eq!(entry_after, entry_before);
        assert_eq!(entry_after.track_id.as_deref(), Some("stable-track-id"));
    }

    #[tokio::test]
    async fn paired_rename_atomically_replaces_an_occupied_destination() {
        use crate::db::entities::playlist_entry;

        let db = rename_test_database().await;
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Overwrite", false)
            .await
            .expect("create playlist");
        let source =
            insert_rename_test_track(&db, "source-track", "/music/source.flac", "Source", 9).await;
        let destination = insert_rename_test_track(
            &db,
            "destination-track",
            "/music/destination.flac",
            "Destination",
            3,
        )
        .await;
        manager
            .add_track(&playlist.id, &source)
            .await
            .expect("add source to playlist");
        manager
            .add_track(&playlist.id, &destination)
            .await
            .expect("add destination to playlist");
        let parsed = parsed_rename_track("/music/destination.flac", "Source Renamed");

        let outcome = rename_track_row(
            &db,
            Path::new("/music/source.flac"),
            Path::new("/music/destination.flac"),
            Some(&parsed),
            || async { true },
        )
        .await
        .expect("overwrite destination transactionally");
        assert!(matches!(
            outcome,
            RenameTrackOutcome::Renamed {
                displaced: Some(ref displaced),
                ..
            } if displaced.id == "destination-track"
        ));
        assert!(track::Entity::find_by_id("destination-track")
            .one(&db)
            .await
            .expect("query displaced track")
            .is_none());
        assert_eq!(
            track::Entity::find_by_id("source-track")
                .one(&db)
                .await
                .expect("query source track")
                .expect("source survives")
                .file_path,
            "/music/destination.flac"
        );

        let entries = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .order_by_asc(playlist_entry::Column::Position)
            .all(&db)
            .await
            .expect("load overwrite playlist entries");
        assert_eq!(entries[0].track_id.as_deref(), Some("source-track"));
        assert_eq!(entries[1].track_id, None);
    }

    #[tokio::test]
    async fn paired_rename_guard_rejection_rolls_back_every_database_change() {
        let db = rename_test_database().await;
        let source =
            insert_rename_test_track(&db, "guard-source", "/music/guard-source.flac", "Source", 4)
                .await;
        let destination = insert_rename_test_track(
            &db,
            "guard-destination",
            "/music/guard-destination.flac",
            "Destination",
            5,
        )
        .await;
        let parsed = parsed_rename_track("/music/guard-destination.flac", "Changed");

        assert!(matches!(
            rename_track_row(
                &db,
                Path::new("/music/guard-source.flac"),
                Path::new("/music/guard-destination.flac"),
                Some(&parsed),
                || async { false },
            )
            .await
            .expect("reject commit guard"),
            RenameTrackOutcome::GuardRejected
        ));
        assert_eq!(
            track::Entity::find_by_id(&source.id)
                .one(&db)
                .await
                .expect("reload guard source")
                .expect("guard source remains"),
            source
        );
        assert_eq!(
            track::Entity::find_by_id(&destination.id)
                .one(&db)
                .await
                .expect("reload guard destination")
                .expect("guard destination remains"),
            destination
        );
    }

    #[tokio::test]
    async fn paired_rename_sql_failure_rolls_back_displacement_and_fk_updates() {
        use crate::db::entities::playlist_entry;

        let db = rename_test_database().await;
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Rollback", false)
            .await
            .expect("create playlist");
        let source = insert_rename_test_track(
            &db,
            "rollback-source",
            "/music/rollback-source.flac",
            "Source",
            4,
        )
        .await;
        let destination = insert_rename_test_track(
            &db,
            "rollback-destination",
            "/music/rollback-destination.flac",
            "Destination",
            5,
        )
        .await;
        manager
            .add_track(&playlist.id, &source)
            .await
            .expect("add rollback source");
        manager
            .add_track(&playlist.id, &destination)
            .await
            .expect("add rollback destination");
        let entries_before = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .order_by_asc(playlist_entry::Column::Position)
            .all(&db)
            .await
            .expect("load entries before rollback");
        db.execute_unprepared(
            "CREATE TRIGGER fail_track_rename
             BEFORE UPDATE OF file_path ON tracks
             WHEN OLD.id = 'rollback-source'
             BEGIN
                 SELECT RAISE(ABORT, 'injected rename failure');
             END",
        )
        .await
        .expect("create failure trigger");
        let parsed = parsed_rename_track("/music/rollback-destination.flac", "Changed");

        assert!(rename_track_row(
            &db,
            Path::new("/music/rollback-source.flac"),
            Path::new("/music/rollback-destination.flac"),
            Some(&parsed),
            || async { true },
        )
        .await
        .is_err());
        assert_eq!(
            track::Entity::find_by_id(&source.id)
                .one(&db)
                .await
                .expect("reload rollback source")
                .expect("rollback source remains"),
            source
        );
        assert_eq!(
            track::Entity::find_by_id(&destination.id)
                .one(&db)
                .await
                .expect("reload rollback destination")
                .expect("rollback destination remains"),
            destination
        );
        assert_eq!(
            playlist_entry::Entity::find()
                .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
                .order_by_asc(playlist_entry::Column::Position)
                .all(&db)
                .await
                .expect("reload entries after rollback"),
            entries_before
        );
    }

    #[tokio::test]
    async fn watcher_batch_invalidates_projections_after_mutation_and_reconciliation() {
        use sea_orm::{ConnectionTrait, Database};
        use sea_orm_migration::MigratorTrait;

        use crate::db::entities::playlist_entry;
        use crate::db::migration::Migrator;

        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, None).await.expect("run migrations");
        db.execute_unprepared(
            "INSERT INTO playlists (id, name, created_at, updated_at)
             VALUES ('watcher-playlist', 'Watcher',
                     '2026-07-12T00:00:00Z', '2026-07-12T00:00:00Z')",
        )
        .await
        .expect("insert playlist");
        db.execute_unprepared(
            "INSERT INTO tracks (
                 id, file_path, title, artist_name, album_title,
                 duration_secs, date_added, date_modified
             )
             VALUES (
                 'watcher-track', '/music/watcher.flac', 'Watcher Song',
                 'Watcher Artist', 'Watcher Album', 180,
                 '2026-07-12T00:00:00Z', '2026-07-12T00:00:00Z'
             )",
        )
        .await
        .expect("insert watcher track");
        db.execute_unprepared(
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, track_id,
                 match_title, match_artist, match_album, match_duration_secs
             )
             VALUES (
                 'watcher-entry', 'watcher-playlist', 0, NULL,
                 'watcher song', 'watcher artist', 'watcher album', 180
             )",
        )
        .await
        .expect("insert orphaned playlist entry");
        let (event_tx, event_rx) = async_channel::unbounded();

        assert_eq!(
            settle_playlist_projections_after_watcher_batch(&db, &event_tx, false, false)
                .await
                .expect("skip unchanged watcher batch"),
            0
        );
        assert!(matches!(
            event_rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        let still_orphaned = playlist_entry::Entity::find_by_id("watcher-entry")
            .one(&db)
            .await
            .expect("query skipped reconciliation")
            .expect("playlist entry remains");
        assert_eq!(still_orphaned.track_id, None);

        assert_eq!(
            settle_playlist_projections_after_watcher_batch(&db, &event_tx, false, true)
                .await
                .expect("settle removal-only watcher batch"),
            0
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(LibraryEvent::PlaylistProjectionsInvalidated)
        ));
        let still_orphaned = playlist_entry::Entity::find_by_id("watcher-entry")
            .one(&db)
            .await
            .expect("query removal-only reconciliation")
            .expect("playlist entry remains");
        assert_eq!(still_orphaned.track_id, None);

        db.execute_unprepared(
            "CREATE TRIGGER fail_watcher_playlist_reconciliation
             BEFORE UPDATE OF track_id ON playlist_entries
             BEGIN
                 SELECT RAISE(ABORT, 'injected watcher reconciliation failure');
             END;",
        )
        .await
        .expect("install reconciliation failure trigger");
        settle_playlist_projections_after_watcher_batch(&db, &event_tx, true, true)
            .await
            .expect_err("surface watcher reconciliation failure");
        assert!(matches!(
            event_rx.try_recv(),
            Ok(LibraryEvent::PlaylistProjectionsInvalidated)
        ));
        db.execute_unprepared("DROP TRIGGER fail_watcher_playlist_reconciliation")
            .await
            .expect("remove reconciliation failure trigger");

        assert_eq!(
            settle_playlist_projections_after_watcher_batch(&db, &event_tx, true, true)
                .await
                .expect("run watcher reconciliation"),
            1
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(LibraryEvent::PlaylistProjectionsInvalidated)
        ));
        let relinked = playlist_entry::Entity::find_by_id("watcher-entry")
            .one(&db)
            .await
            .expect("query watcher reconciliation")
            .expect("playlist entry remains");
        assert_eq!(relinked.track_id.as_deref(), Some("watcher-track"));
    }

    #[tokio::test]
    async fn initial_scan_invalidates_playlist_projections_before_scan_complete() {
        let db = rename_test_database().await;
        let (event_tx, event_rx) = async_channel::unbounded();

        initial_scan(&db, &[], &event_tx)
            .await
            .expect("run empty initial scan");

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let invalidation_index = events
            .iter()
            .position(|event| matches!(event, LibraryEvent::PlaylistProjectionsInvalidated))
            .expect("initial scan invalidates playlist projections");
        let playlists_loaded_index = events
            .iter()
            .position(|event| matches!(event, LibraryEvent::PlaylistsLoaded(_)))
            .expect("initial scan publishes playlist rows");
        let completion_index = events
            .iter()
            .position(|event| matches!(event, LibraryEvent::ScanComplete))
            .expect("initial scan completes");
        assert!(playlists_loaded_index < invalidation_index);
        assert!(invalidation_index < completion_index);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, LibraryEvent::PlaylistProjectionsInvalidated))
                .count(),
            1,
            "one initial scan produces one post-reconciliation invalidation"
        );
    }

    // ── Paired directory renames ────────────────────────────────────────

    /// Build an absolute fixture path with the target platform's separator.
    /// Production rows come from `Path::to_string_lossy`; keeping synthetic DB
    /// paths in the same namespace prevents Windows-only slash mismatches.
    fn directory_fixture_path(path: &str) -> PathBuf {
        path.split('/')
            .filter(|component| !component.is_empty())
            .fold(
                PathBuf::from(std::path::MAIN_SEPARATOR.to_string()),
                |mut result, component| {
                    result.push(component);
                    result
                },
            )
    }

    fn directory_fixture_key(path: &str) -> String {
        directory_fixture_path(path).to_string_lossy().into_owned()
    }

    async fn insert_directory_rename_test_track(
        db: &DatabaseConnection,
        id: &str,
        path: &str,
        title: &str,
        play_count: i32,
    ) -> track::Model {
        insert_rename_test_track(db, id, &directory_fixture_key(path), title, play_count).await
    }

    fn destination_files(paths: &[&str]) -> HashSet<String> {
        paths
            .iter()
            .map(|path| directory_fixture_key(path))
            .collect()
    }

    #[test]
    fn directory_identity_mapping_excludes_dirty_destination_descendants() {
        let source = directory_fixture_path("/music/Album");
        let destination = directory_fixture_path("/music/Renamed");
        let audio_files = vec![
            directory_fixture_path("/music/Renamed/clean.flac"),
            directory_fixture_path("/music/Renamed/modified.flac"),
            directory_fixture_path("/music/Renamed/removed.flac"),
            directory_fixture_path("/music/Renamed/Disc 2/changed.flac"),
        ];
        let upserts = HashSet::from([
            // The old spelling is common when a child event was delivered
            // immediately before the parent rename pair.
            directory_fixture_path("/music/Album/modified.flac"),
            directory_fixture_path("/music/Other/unrelated.flac"),
        ]);
        let removals = HashSet::from([directory_fixture_path("/music/Renamed/removed.flac")]);
        let deferred = HashSet::from([directory_fixture_path("/music/Renamed/Disc 2")]);
        let dirty_directories = HashSet::new();

        assert_eq!(
            directory_identity_destinations(
                &audio_files,
                &source,
                &destination,
                &upserts,
                &removals,
                &deferred,
                &dirty_directories,
            ),
            destination_files(&["/music/Renamed/clean.flac"]),
            "a child with its own event must take the parse/reconciliation path instead"
        );
    }

    #[test]
    fn directory_identity_mapping_rejects_an_exact_destination_folder_replacement() {
        let source = directory_fixture_path("/music/Album");
        let destination = directory_fixture_path("/music/Renamed");
        let mut batch = WatcherBatch::default();
        batch.record_rename_pair(source.clone(), destination.clone());
        batch.collect(
            notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::Folder))
                .add_path(destination.clone()),
        );
        batch.collect(
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::Folder))
                .add_path(destination.clone()),
        );
        batch.finish();

        assert!(batch.rename_pairs.contains(&WatcherRenamePair {
            from: source.clone(),
            to: destination.clone(),
        }));
        assert!(batch.dirty_directory_scopes.contains(&destination));
        assert!(batch.reconciliation_required);
        assert!(
            directory_identity_destinations(
                &[destination.join("01.flac")],
                &source,
                &destination,
                &batch.upsert_paths,
                &batch.remove_paths,
                &batch.deferred_paths,
                &batch.dirty_directory_scopes,
            )
            .is_empty(),
            "a recreated destination directory cannot inherit any old descendant identity"
        );
    }

    #[test]
    fn watcher_batch_defers_directory_rename_halves_until_the_pair_is_known() {
        let library = TestDirectory::new("directory-rename-batch");
        let destination = library.path().join("Renamed Album");
        std::fs::create_dir_all(&destination).expect("create renamed album");
        let source = library.path().join("Album");

        let mut batch = WatcherBatch::default();
        batch.collect(rename_event(
            notify::event::RenameMode::From,
            &[source.to_str().expect("utf-8 fixture path")],
            Some(9),
        ));
        // The source half alone is indistinguishable from a deleted directory.
        assert!(!batch.reconciliation_required);
        batch.collect(rename_event(
            notify::event::RenameMode::To,
            &[destination.to_str().expect("utf-8 fixture path")],
            Some(9),
        ));
        batch.finish();

        assert_eq!(
            batch.rename_pairs,
            HashSet::from([WatcherRenamePair {
                from: source,
                to: destination,
            }])
        );
        assert!(
            !batch.reconciliation_required,
            "an authoritative directory pair must not force a full rescan"
        );
        assert!(batch.remove_paths.is_empty());
        assert!(batch.upsert_paths.is_empty());
    }

    #[test]
    fn watcher_batch_promotes_an_unclaimed_directory_removal_to_reconciliation() {
        let mut batch = WatcherBatch::default();
        batch.collect(rename_event(
            notify::event::RenameMode::From,
            &["/music/Album"],
            Some(3),
        ));

        batch.finish();

        assert!(batch.rename_pairs.is_empty());
        assert!(
            batch.reconciliation_required,
            "a directory that left the library without a destination must reconcile"
        );
    }

    #[test]
    fn watcher_batch_rejects_rename_pairs_nested_in_a_renamed_directory() {
        let mut batch = WatcherBatch::default();
        batch.collect(rename_event(
            notify::event::RenameMode::Both,
            &["/music/Album", "/music/Renamed"],
            Some(1),
        ));
        batch.collect(rename_event(
            notify::event::RenameMode::Both,
            &["/music/Renamed/01.flac", "/music/Renamed/02.flac"],
            Some(2),
        ));
        batch.finish();

        assert!(
            batch.rename_pairs.is_empty(),
            "a pair nested inside a renamed directory cannot be ordered from watcher events"
        );
        assert!(batch.reconciliation_required);
    }

    #[test]
    fn rename_destination_is_bound_without_following_symlinks() {
        let library = TestDirectory::new("rename-classification");
        let album = library.path().join("Album");
        std::fs::create_dir_all(&album).expect("create album");
        let track = library.path().join("01.flac");
        std::fs::write(&track, b"audio").expect("create track");
        let lease = test_root_authority(library.path());

        assert_eq!(
            bind_watcher_rename_evidence(
                &lease,
                &WatcherRenamePair {
                    from: library.path().join("00.flac"),
                    to: track,
                }
            )
            .expect("bind file rename")
            .kind(),
            WatcherRenameKind::File
        );
        assert_eq!(
            bind_watcher_rename_evidence(
                &lease,
                &WatcherRenamePair {
                    from: library.path().join("Old Album"),
                    to: album.clone(),
                }
            )
            .expect("bind directory rename")
            .kind(),
            WatcherRenameKind::Directory
        );
        // A source that still exists was copied, not renamed.
        assert!(bind_watcher_rename_evidence(
            &lease,
            &WatcherRenamePair {
                from: library.path().to_path_buf(),
                to: album,
            }
        )
        .is_err());

        #[cfg(unix)]
        {
            let linked = library.path().join("Linked Album");
            std::os::unix::fs::symlink(library.path().join("Album"), &linked)
                .expect("create symlinked album");
            assert!(
                bind_watcher_rename_evidence(
                    &lease,
                    &WatcherRenamePair {
                        from: library.path().join("Old Album"),
                        to: linked,
                    }
                )
                .is_err(),
                "neither the traversal nor the watcher follows symlinks"
            );
        }
    }

    #[test]
    fn directory_rename_source_accepts_a_case_alias_only_for_the_same_object() {
        let library = TestDirectory::new("directory-case-alias");
        let source = library.path().join("Album");
        let destination = library.path().join("album");
        std::fs::create_dir_all(&source).expect("create source album");
        std::fs::rename(&source, &destination).expect("apply case-only rename");
        let lease = test_root_authority(library.path());

        assert_eq!(
            bind_watcher_rename_evidence(
                &lease,
                &WatcherRenamePair {
                    from: source.clone(),
                    to: destination.clone(),
                }
            )
            .expect("bind case-only directory rename")
            .kind(),
            WatcherRenameKind::Directory
        );

        // On a case-insensitive filesystem the old spelling still resolves;
        // the same-object handle comparison, rather than absence, is what
        // authorizes the pair.
        if source.try_exists().expect("probe old spelling") {
            let source_bound = lease.bind_directory(&source).expect("bind old alias");
            let destination_bound = lease.bind_directory(&destination).expect("bind new alias");
            assert!(source_bound.is_same_object_as(&destination_bound));
        }

        let recreated_source = library.path().join("Other");
        std::fs::create_dir_all(&recreated_source).expect("create distinct source");
        assert!(bind_watcher_rename_evidence(
            &lease,
            &WatcherRenamePair {
                from: recreated_source,
                to: destination,
            }
        )
        .is_err());
    }

    #[test]
    fn bound_rename_evidence_rejects_a_reappearing_source() {
        let library = TestDirectory::new("rename-source-reappears");
        let source = library.path().join("old.flac");
        let destination = library.path().join("new.flac");
        std::fs::write(&destination, b"renamed object").expect("create rename destination");
        let lease = test_root_authority(library.path());
        let evidence = bind_watcher_rename_evidence(
            &lease,
            &WatcherRenamePair {
                from: source.clone(),
                to: destination,
            },
        )
        .expect("bind completed file rename");
        evidence
            .validate(&lease)
            .expect("unchanged rename evidence remains valid");

        std::fs::write(&source, b"different object").expect("recreate rename source");
        assert!(
            evidence.validate(&lease).is_err(),
            "a copied or recreated source must reject the pending database rename"
        );
        lease
            .validate()
            .expect("source reappearance does not invalidate the library root");
    }

    #[tokio::test]
    async fn directory_rename_preserves_descendant_identity_and_playlist_links() {
        use crate::db::entities::playlist_entry;

        let db = rename_test_database().await;
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Album", false)
            .await
            .expect("create playlist");

        let first =
            insert_directory_rename_test_track(&db, "track-one", "/music/Album/01.flac", "One", 11)
                .await;
        insert_directory_rename_test_track(
            &db,
            "track-two",
            "/music/Album/Disc 2/02.flac",
            "Two",
            4,
        )
        .await;
        // A sibling whose path shares a textual prefix with the renamed
        // directory must not be dragged along with it.
        insert_directory_rename_test_track(&db, "track-other", "/music/Album2/03.flac", "Three", 2)
            .await;

        manager
            .add_track(&playlist.id, &first)
            .await
            .expect("add track to playlist");
        let entry_before = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .one(&db)
            .await
            .expect("load playlist entry")
            .expect("playlist entry exists");

        let source_directory = directory_fixture_path("/music/Album");
        let destination_directory = directory_fixture_path("/music/Renamed");

        let outcome = rename_directory_rows(
            &db,
            &source_directory,
            &destination_directory,
            &destination_files(&["/music/Renamed/01.flac", "/music/Renamed/Disc 2/02.flac"]),
            || async { true },
        )
        .await
        .expect("rename directory rows");

        let RenameDirectoryOutcome::Renamed {
            moved,
            displaced,
            unmapped,
        } = outcome
        else {
            panic!("expected the directory rename to commit");
        };
        assert_eq!(moved.len(), 2);
        assert_eq!(displaced, 0);
        assert_eq!(unmapped, 0);

        let renamed = track::Entity::find_by_id("track-one")
            .one(&db)
            .await
            .expect("load renamed track")
            .expect("renamed track exists");
        assert_eq!(
            renamed.file_path,
            directory_fixture_key("/music/Renamed/01.flac")
        );
        assert_eq!(renamed.play_count, 11, "history survives the move");
        assert_eq!(renamed.date_added, "2025-01-02T03:04:05Z");
        assert_eq!(
            renamed.date_modified, "2025-01-02T03:04:05Z",
            "a directory rename changes no file content"
        );

        let nested = track::Entity::find_by_id("track-two")
            .one(&db)
            .await
            .expect("load nested track")
            .expect("nested track exists");
        assert_eq!(
            nested.file_path,
            directory_fixture_key("/music/Renamed/Disc 2/02.flac")
        );

        let sibling = track::Entity::find_by_id("track-other")
            .one(&db)
            .await
            .expect("load sibling track")
            .expect("sibling track exists");
        assert_eq!(
            sibling.file_path,
            directory_fixture_key("/music/Album2/03.flac"),
            "the prefix must match whole path components"
        );

        let entry_after = playlist_entry::Entity::find_by_id(&entry_before.id)
            .one(&db)
            .await
            .expect("reload playlist entry")
            .expect("playlist entry remains");
        assert_eq!(
            entry_after, entry_before,
            "the playlist keeps its direct reference to the moved track"
        );
    }

    #[tokio::test]
    async fn directory_rename_reports_descendants_without_a_destination_file() {
        let db = rename_test_database().await;
        insert_directory_rename_test_track(&db, "moved", "/music/Album/01.flac", "One", 0).await;
        insert_directory_rename_test_track(&db, "vanished", "/music/Album/02.flac", "Two", 0).await;

        let source_directory = directory_fixture_path("/music/Album");
        let destination_directory = directory_fixture_path("/music/Renamed");

        let outcome = rename_directory_rows(
            &db,
            &source_directory,
            &destination_directory,
            // The second file was deleted during the rename window, so nothing
            // observed it at the destination.
            &destination_files(&["/music/Renamed/01.flac"]),
            || async { true },
        )
        .await
        .expect("rename directory rows");

        let RenameDirectoryOutcome::Renamed {
            moved, unmapped, ..
        } = outcome
        else {
            panic!("expected the directory rename to commit");
        };
        assert_eq!(moved.len(), 1);
        assert_eq!(
            unmapped, 1,
            "an unproven descendant is left for reconciliation, not followed to a guess"
        );

        let vanished = track::Entity::find_by_id("vanished")
            .one(&db)
            .await
            .expect("load unmapped track")
            .expect("unmapped track exists");
        assert_eq!(
            vanished.file_path,
            directory_fixture_key("/music/Album/02.flac"),
            "an unproven row keeps its path until a guarded scan can resolve it"
        );
    }

    #[tokio::test]
    async fn directory_rename_displaces_a_stale_row_parked_at_a_destination_path() {
        let db = rename_test_database().await;
        insert_directory_rename_test_track(&db, "moved", "/music/Album/01.flac", "One", 5).await;
        // A row a previous scan was never authoritative enough to delete. The
        // unique path index would otherwise abort the move.
        insert_directory_rename_test_track(&db, "stale", "/music/Renamed/01.flac", "Stale", 0)
            .await;

        let source_directory = directory_fixture_path("/music/Album");
        let destination_directory = directory_fixture_path("/music/Renamed");

        let outcome = rename_directory_rows(
            &db,
            &source_directory,
            &destination_directory,
            &destination_files(&["/music/Renamed/01.flac"]),
            || async { true },
        )
        .await
        .expect("rename directory rows");

        let RenameDirectoryOutcome::Renamed {
            moved, displaced, ..
        } = outcome
        else {
            panic!("expected the directory rename to commit");
        };
        assert_eq!(moved.len(), 1);
        assert_eq!(displaced, 1);

        assert!(track::Entity::find_by_id("stale")
            .one(&db)
            .await
            .expect("query displaced track")
            .is_none());
        let survivor = track::Entity::find_by_id("moved")
            .one(&db)
            .await
            .expect("load moved track")
            .expect("moved track exists");
        assert_eq!(
            survivor.file_path,
            directory_fixture_key("/music/Renamed/01.flac")
        );
        assert_eq!(survivor.play_count, 5);
    }

    #[tokio::test]
    async fn directory_rename_guard_rejection_rolls_back_every_change() {
        let db = rename_test_database().await;
        insert_directory_rename_test_track(&db, "moved", "/music/Album/01.flac", "One", 5).await;
        insert_directory_rename_test_track(&db, "stale", "/music/Renamed/01.flac", "Stale", 0)
            .await;

        let source_directory = directory_fixture_path("/music/Album");
        let destination_directory = directory_fixture_path("/music/Renamed");

        let outcome = rename_directory_rows(
            &db,
            &source_directory,
            &destination_directory,
            &destination_files(&["/music/Renamed/01.flac"]),
            || async { false },
        )
        .await
        .expect("rename directory rows");
        assert!(matches!(outcome, RenameDirectoryOutcome::GuardRejected));

        let source = track::Entity::find_by_id("moved")
            .one(&db)
            .await
            .expect("load source track")
            .expect("source track exists");
        assert_eq!(
            source.file_path,
            directory_fixture_key("/music/Album/01.flac")
        );
        assert!(
            track::Entity::find_by_id("stale")
                .one(&db)
                .await
                .expect("query displaced track")
                .is_some(),
            "a rejected commit must not leave the destination row deleted"
        );
    }

    #[tokio::test]
    async fn directory_rename_refuses_a_destination_inside_its_own_source() {
        let db = rename_test_database().await;
        insert_directory_rename_test_track(&db, "moved", "/music/Album/01.flac", "One", 0).await;

        let source_directory = directory_fixture_path("/music/Album");
        let nested_destination = directory_fixture_path("/music/Album/Nested");

        let error = rename_directory_rows(
            &db,
            &source_directory,
            &nested_destination,
            &destination_files(&["/music/Album/Nested/01.flac"]),
            || async { true },
        )
        .await;
        assert!(error.is_err());

        let unchanged = track::Entity::find_by_id("moved")
            .one(&db)
            .await
            .expect("load track")
            .expect("track exists");
        assert_eq!(
            unchanged.file_path,
            directory_fixture_key("/music/Album/01.flac")
        );
    }

    #[test]
    fn renamed_directory_scan_enumerates_descendants_without_following_symlinks() {
        let library = TestDirectory::new("renamed-directory-scan");
        let album = library.path().join("Renamed");
        std::fs::create_dir_all(album.join("Disc 2")).expect("create nested directory");
        std::fs::write(album.join("01.flac"), b"audio").expect("write track");
        std::fs::write(album.join("Disc 2").join("02.flac"), b"audio").expect("write nested track");
        std::fs::write(album.join("cover.jpg"), b"art").expect("write cover");

        let lease = test_root_authority(library.path());
        let destination = lease.bind_directory(&album).expect("bind renamed album");
        let scan = scan_renamed_directory(&lease, &destination, &album);
        assert!(scan.is_complete());
        let mut found = scan.audio_files.clone();
        found.sort_unstable();
        assert_eq!(
            found,
            vec![album.join("01.flac"), album.join("Disc 2").join("02.flac")]
        );

        #[cfg(unix)]
        {
            let outside = library.path().join("outside.flac");
            std::fs::write(&outside, b"audio").expect("write outside track");
            std::os::unix::fs::symlink(&outside, album.join("linked.flac"))
                .expect("create symlinked track");

            let scan = scan_renamed_directory(&lease, &destination, &album);
            assert!(scan.is_complete());
            assert_eq!(
                scan.audio_files.len(),
                2,
                "a symlinked file is never indexed, so it can never be mapped"
            );
        }
    }

    #[test]
    fn renamed_directory_scan_rejects_file_or_directory_replacement_before_commit() {
        let library = TestDirectory::new("renamed-directory-revalidation");
        let album = library.path().join("Renamed");
        std::fs::create_dir_all(&album).expect("create album");
        let track = album.join("01.flac");
        std::fs::write(&track, b"first object").expect("write original track");

        let lease = test_root_authority(library.path());
        let destination = lease.bind_directory(&album).expect("bind renamed album");
        let file_scan = scan_renamed_directory(&lease, &destination, &album);
        assert!(file_scan.is_complete());
        assert!(file_scan.observations_still_current(&lease, &destination));

        let original_track = album.join("original.flac");
        match std::fs::rename(&track, &original_track) {
            Ok(()) => {
                std::fs::write(&track, b"replacement").expect("write replacement track");
                assert!(
                    !file_scan.observations_still_current(&lease, &destination),
                    "a different file at the same path must not inherit the indexed row"
                );
            }
            Err(error) => {
                #[cfg(windows)]
                {
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        || error.raw_os_error() == Some(32)
                    {
                        assert!(
                            file_scan.observations_still_current(&lease, &destination),
                            "Windows retained handles prevent the file swap outright"
                        );
                    } else {
                        panic!("park original track: {error}");
                    }
                }
                #[cfg(not(windows))]
                panic!("park original track: {error}");
            }
        }
        drop(file_scan);

        let replacement_destination = lease
            .bind_directory(&album)
            .expect("rebind album after file replacement");
        let directory_scan = scan_renamed_directory(&lease, &replacement_destination, &album);
        assert!(directory_scan.is_complete());
        let parked_album = library.path().join("Parked");
        if let Err(error) = std::fs::rename(&album, &parked_album) {
            #[cfg(windows)]
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(32)
            {
                assert!(
                    directory_scan.observations_still_current(&lease, &replacement_destination),
                    "Windows retained handles prevent the directory swap outright"
                );
                return;
            }
            panic!("park original directory: {error}");
        }
        std::fs::create_dir_all(&album).expect("create replacement directory");
        std::fs::write(album.join("01.flac"), b"replacement").expect("mirror old file name");
        std::fs::write(album.join("original.flac"), b"replacement")
            .expect("mirror second file name");
        assert!(
            !directory_scan.observations_still_current(&lease, &replacement_destination),
            "an identical-looking replacement directory must fail the handle guard"
        );
    }

    #[cfg(unix)]
    #[test]
    fn renamed_directory_scan_fails_closed_on_an_unreadable_descendant() {
        use std::os::unix::fs::PermissionsExt;

        let library = TestDirectory::new("renamed-directory-unreadable");
        let album = library.path().join("Renamed");
        let locked = album.join("Disc 2");
        std::fs::create_dir_all(&locked).expect("create nested directory");
        std::fs::write(album.join("01.flac"), b"audio").expect("write track");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("remove directory permissions");

        // Privileged containers can retain directory access despite mode 000.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
                .expect("restore directory permissions");
            return;
        }

        let lease = test_root_authority(library.path());
        let destination = lease.bind_directory(&album).expect("bind renamed album");
        let scan = scan_renamed_directory(&lease, &destination, &album);

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
            .expect("restore directory permissions");
        assert!(
            !scan.is_complete(),
            "a partial view of the destination can never prove where a track moved"
        );
    }

    #[test]
    fn renamed_subtree_owning_another_scope_is_rejected() {
        let library = PathBuf::from("/music");
        let nested = library_root::Model {
            path: "/music/Album/Nested".to_string(),
            device_id: Some("marker:v1:nested".to_string()),
            identity_confirmed: true,
            is_available: true,
            last_scan_complete: true,
            last_checked_at: "2026-07-12T00:00:00Z".to_string(),
        };
        let roots =
            WatcherRootCache::from_models(vec![nested], std::slice::from_ref(&library.clone()));
        let music_dirs = [library];

        assert!(
            subtree_owns_another_scope(Path::new("/music/Album"), &roots, &music_dirs),
            "moving a persisted root would leave its row pointing at a path that no longer exists"
        );
        assert!(!subtree_owns_another_scope(
            Path::new("/music/Other"),
            &roots,
            &music_dirs
        ));
    }

    #[test]
    fn durable_root_marker_is_created_once_and_reused() {
        let directory = TestDirectory::new("root-marker");
        let legacy = filesystem_identity(directory.path()).expect("observe legacy identity");
        assert!(is_legacy_identity(&legacy));

        let created = create_root_marker(directory.path()).expect("create root marker");
        assert!(created.created);
        assert!(is_marker_identity(&created.identity));
        let reused = create_root_marker(directory.path()).expect("reuse root marker");
        assert!(!reused.created);
        assert_eq!(reused.identity, created.identity);
        assert_eq!(
            filesystem_identity(directory.path()).expect("observe durable identity"),
            created.identity
        );
    }

    #[test]
    fn malformed_root_marker_fails_closed() {
        let directory = TestDirectory::new("invalid-root-marker");
        std::fs::write(root_identity_path(directory.path()), "not-a-root-id\n")
            .expect("write invalid marker");

        assert!(filesystem_identity(directory.path()).is_err());
        let scan = scan_root(directory.path().to_path_buf());
        assert!(!scan.is_complete());
        assert!(scan.device_id.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_root_marker_fails_closed_without_blocking() {
        use rustix::fs::{mkfifoat, Mode, CWD};

        let directory = TestDirectory::new("fifo-root-marker");
        mkfifoat(
            CWD,
            root_identity_path(directory.path()),
            Mode::RUSR | Mode::WUSR,
        )
        .expect("create marker FIFO");

        assert!(read_root_marker(directory.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_marker_fails_closed() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("symlink-root-marker");
        let target = directory.path().join("marker-target");
        std::fs::write(
            &target,
            format!("{ROOT_IDENTITY_PREFIX}{}\n", Uuid::new_v4()),
        )
        .expect("write marker target");
        symlink(&target, root_identity_path(directory.path())).expect("symlink marker");

        assert!(filesystem_identity(directory.path()).is_err());
    }

    fn unconfirmed_root_state(scan: &RootScan) -> library_root::Model {
        library_root::Model {
            path: scan.root.to_string_lossy().into_owned(),
            device_id: scan.device_id.clone(),
            identity_confirmed: false,
            is_available: false,
            last_scan_complete: scan.is_complete(),
            last_checked_at: "2026-07-14T00:00:00Z".to_string(),
        }
    }

    fn write_minimal_wav(path: &Path) {
        let data_size = 1_u32;
        let mut bytes = Vec::with_capacity(45);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.push(128);
        std::fs::write(path, bytes).expect("write minimal WAV fixture");
    }

    #[test]
    fn root_trust_reasons_cover_legacy_replacement_and_empty_evidence() {
        let legacy = TestDirectory::new("trust-reason-legacy");
        std::fs::write(legacy.path().join("present.mp3"), []).expect("create legacy audio");
        let legacy_scan = scan_root(legacy.path().to_path_buf());
        let legacy_state = unconfirmed_root_state(&legacy_scan);
        assert_eq!(
            root_trust_reason(&legacy_scan, Some(&legacy_state), 3, true, false),
            Some(RootTrustReason::LegacyEnrollment)
        );

        let empty = TestDirectory::new("trust-reason-empty");
        let empty_scan = scan_root(empty.path().to_path_buf());
        let empty_state = unconfirmed_root_state(&empty_scan);
        assert_eq!(
            root_trust_reason(&empty_scan, Some(&empty_state), 9, true, false),
            Some(RootTrustReason::EmptyRoot),
            "an inherited empty view must not bypass the stronger empty-root warning"
        );
        let empty_request =
            build_root_trust_request(&empty_scan, &empty_state, RootTrustReason::EmptyRoot, 9)
                .expect("build empty request");
        assert!(empty_request.requires_empty_acknowledgement());

        let replacement = TestDirectory::new("trust-reason-replacement");
        create_root_marker(replacement.path()).expect("create replacement marker");
        let replacement_scan = scan_root(replacement.path().to_path_buf());
        let mut intended_state = persisted_root_state(
            &replacement_scan,
            Some(format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4())),
        );
        intended_state.path = replacement.path().to_string_lossy().into_owned();
        assert_eq!(
            root_trust_reason(&replacement_scan, Some(&intended_state), 2, true, false,),
            Some(RootTrustReason::Replacement)
        );
        let replacement_request = build_root_trust_request(
            &replacement_scan,
            &intended_state,
            RootTrustReason::Replacement,
            2,
        )
        .expect("build replacement request");
        assert!(
            replacement_request.requires_empty_acknowledgement(),
            "an empty replacement retains replacement semantics and the empty-risk gate"
        );
    }

    #[test]
    fn root_trust_requires_complete_exact_configured_evidence() {
        let directory = TestDirectory::new("trust-evidence-scope");
        std::fs::write(directory.path().join("present.mp3"), []).expect("create audio");
        let scan = scan_root(directory.path().to_path_buf());
        let state = unconfirmed_root_state(&scan);

        assert_eq!(
            root_trust_reason(&scan, Some(&state), 1, false, false),
            None,
            "a discovered nested root cannot be confirmed"
        );
        let mut incomplete = scan;
        incomplete
            .errors
            .push("simulated traversal gap".to_string());
        assert_eq!(
            root_trust_reason(&incomplete, Some(&state), 1, true, false),
            None
        );
    }

    #[test]
    fn root_trust_request_id_ignores_timestamps_but_binds_security_state() {
        let directory = TestDirectory::new("trust-request-id");
        let scan = scan_root(directory.path().to_path_buf());
        let first = unconfirmed_root_state(&scan);
        let mut timestamp_only = first.clone();
        timestamp_only.last_checked_at = "2099-01-01T00:00:00Z".to_string();

        let first_request = build_root_trust_request(&scan, &first, RootTrustReason::EmptyRoot, 4)
            .expect("build first request");
        let timestamp_request =
            build_root_trust_request(&scan, &timestamp_only, RootTrustReason::EmptyRoot, 4)
                .expect("build timestamp-only request");
        assert_eq!(first_request.request_id(), timestamp_request.request_id());

        let mut security_change = first;
        security_change.is_available = true;
        let changed_request =
            build_root_trust_request(&scan, &security_change, RootTrustReason::EmptyRoot, 4)
                .expect("build changed request");
        assert_ne!(first_request.request_id(), changed_request.request_id());

        let nonempty_id = root_trust_request_id(
            first_request.path(),
            first_request.reason(),
            first_request.remembered_track_count(),
            false,
            &first_request.observed_identity,
            first_request.observed_mount_generation,
            &first_request.expected_state,
        );
        assert_ne!(
            first_request.request_id(),
            nonempty_id,
            "the stronger empty acknowledgement is part of the immutable evidence"
        );
    }

    #[test]
    fn root_trust_request_debug_redacts_private_evidence() {
        let request = RootTrustRequest {
            request_id: Uuid::new_v4(),
            path: PathBuf::from("/displayed/library"),
            reason: RootTrustReason::LegacyEnrollment,
            remembered_track_count: 3,
            requires_empty_acknowledgement: false,
            observed_identity: "secret-observed-identity".to_string(),
            observed_mount_generation: 42,
            expected_state: RootTrustExpectedState {
                device_id: Some("secret-persisted-identity".to_string()),
                identity_confirmed: false,
                is_available: false,
                last_scan_complete: true,
            },
        };

        let rendered = format!("{request:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("secret-observed-identity"));
        assert!(!rendered.contains("secret-persisted-identity"));
    }

    #[test]
    fn explicit_root_marker_adopts_existing_and_rejects_changed_evidence() {
        let adopted = TestDirectory::new("trust-adopt-existing-marker");
        let marker = create_root_marker(adopted.path())
            .expect("create marker")
            .identity;
        let adopted_scan = scan_root(adopted.path().to_path_buf());
        let adopted_state = unconfirmed_root_state(&adopted_scan);
        let adopted_request =
            build_root_trust_request(&adopted_scan, &adopted_state, RootTrustReason::EmptyRoot, 0)
                .expect("build adoption request");
        assert_eq!(
            establish_explicit_root_marker(&adopted_request).expect("adopt existing marker"),
            marker
        );

        let stale = TestDirectory::new("trust-unexpected-marker");
        let stale_scan = scan_root(stale.path().to_path_buf());
        let stale_state = unconfirmed_root_state(&stale_scan);
        let stale_request =
            build_root_trust_request(&stale_scan, &stale_state, RootTrustReason::EmptyRoot, 0)
                .expect("build markerless request");
        create_root_marker(stale.path()).expect("marker appears after prompt");
        assert!(matches!(
            establish_explicit_root_marker(&stale_request),
            Err(RootTrustError::Stale(_))
        ));

        let generation = TestDirectory::new("trust-stale-generation");
        let generation_scan = scan_root(generation.path().to_path_buf());
        let generation_state = unconfirmed_root_state(&generation_scan);
        let mut generation_request = build_root_trust_request(
            &generation_scan,
            &generation_state,
            RootTrustReason::EmptyRoot,
            0,
        )
        .expect("build generation request");
        generation_request.observed_mount_generation =
            generation_request.observed_mount_generation.wrapping_add(1);
        assert!(matches!(
            establish_explicit_root_marker(&generation_request),
            Err(RootTrustError::Stale(_))
        ));
        assert!(!root_identity_path(generation.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn existing_marker_can_be_adopted_from_a_read_only_root() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("trust-read-only-adoption");
        let marker = create_root_marker(directory.path())
            .expect("create marker before making root read-only")
            .identity;
        let scan = scan_root(directory.path().to_path_buf());
        let state = unconfirmed_root_state(&scan);
        let request = build_root_trust_request(&scan, &state, RootTrustReason::EmptyRoot, 0)
            .expect("build adoption request");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o555))
            .expect("make root read-only");

        let result = establish_explicit_root_marker(&request);
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore root permissions");
        assert_eq!(result.expect("adopt read-only marker"), marker);
    }

    #[cfg(unix)]
    #[test]
    fn markerless_read_only_root_remains_untrusted() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("trust-read-only-markerless");
        let scan = scan_root(directory.path().to_path_buf());
        let state = unconfirmed_root_state(&scan);
        let request = build_root_trust_request(&scan, &state, RootTrustReason::EmptyRoot, 0)
            .expect("build markerless request");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o555))
            .expect("make root read-only");

        // Capability-enabled test runners can bypass mode bits; skip only the
        // permission assertion when this fixture cannot model read-only media.
        let probe = directory.path().join("permission-probe");
        if OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .is_ok()
        {
            let _ = std::fs::remove_file(probe);
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .expect("restore root permissions");
            return;
        }

        let result = establish_explicit_root_marker(&request);
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore root permissions");
        assert!(matches!(result, Err(RootTrustError::Failed(_))));
        assert!(!root_identity_path(directory.path()).exists());
    }

    #[tokio::test]
    async fn root_trust_stage_requires_exact_config_and_security_state() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-stage-guards");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist unconfirmed root");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 0)
            .expect("build trust request");

        assert!(matches!(
            stage_root_trust(&db, &[], &request).await,
            Err(RootTrustError::Stale(_))
        ));
        assert!(!root_identity_path(directory.path()).exists());

        let mut changed: library_root::ActiveModel = stored.into();
        changed.is_available = Set(true);
        changed
            .update(&db)
            .await
            .expect("change security-relevant state");
        assert!(matches!(
            stage_root_trust(&db, &[directory.path().to_path_buf()], &request,).await,
            Err(RootTrustError::Stale(_))
        ));
        assert!(!root_identity_path(directory.path()).exists());
    }

    #[tokio::test]
    async fn root_trust_stage_ignores_timestamp_drift_and_stages_unavailable_marker() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-stage-marker");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist unconfirmed root");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 0)
            .expect("build trust request");

        let mut timestamp_only: library_root::ActiveModel = stored.into();
        timestamp_only.last_checked_at = Set("2099-01-01T00:00:00Z".to_string());
        timestamp_only
            .update(&db)
            .await
            .expect("change timestamp only");

        let marker = stage_root_trust(&db, &[directory.path().to_path_buf()], &request)
            .await
            .expect("stage root trust");
        assert!(is_marker_identity(&marker));
        assert_eq!(
            read_root_marker(directory.path())
                .expect("read marker")
                .as_deref(),
            Some(marker.as_str())
        );

        let staged =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query staged root")
                .expect("staged root exists");
        assert_eq!(staged.device_id.as_deref(), Some(marker.as_str()));
        assert!(!staged.identity_confirmed);
        assert!(!staged.is_available);
        assert!(!staged.last_scan_complete);
    }

    #[tokio::test]
    async fn forced_trust_conversion_preserves_all_tracks_until_ordinary_scan() {
        let db = rename_test_database().await;
        let target = TestDirectory::new("trust-conversion-target");
        let target_scan = scan_root(target.path().to_path_buf());
        let target_state = persist_root_scan_status(&db, &target_scan, None, false, true, false)
            .await
            .expect("persist target root");
        let request =
            build_root_trust_request(&target_scan, &target_state, RootTrustReason::EmptyRoot, 1)
                .expect("build target request");

        let other = TestDirectory::new("trust-conversion-other");
        create_root_marker(other.path()).expect("create other marker");
        let new_audio = other.path().join("new.wav");
        write_minimal_wav(&new_audio);
        let other_scan = scan_root(other.path().to_path_buf());
        persist_root_scan_status(&db, &other_scan, None, true, true, false)
            .await
            .expect("persist confirmed other root");

        insert_rename_test_track(
            &db,
            "conversion-target-track",
            target
                .path()
                .join("remembered.flac")
                .to_string_lossy()
                .as_ref(),
            "Target remembered",
            0,
        )
        .await;
        insert_rename_test_track(
            &db,
            "conversion-other-track",
            other
                .path()
                .join("remembered.flac")
                .to_string_lossy()
                .as_ref(),
            "Other remembered",
            0,
        )
        .await;

        let (event_tx, _event_rx) = async_channel::unbounded();
        let music_dirs = vec![target.path().to_path_buf(), other.path().to_path_buf()];
        let RootTrustCommandStart::Pending(pending) =
            begin_root_trust_command(&db, &music_dirs, &event_tx, &request)
                .await
                .expect("run forced conversion")
        else {
            panic!("queue ordinary follow-up");
        };

        assert!(track::Entity::find_by_id("conversion-target-track")
            .one(&db)
            .await
            .expect("query target row after conversion")
            .is_some());
        assert!(track::Entity::find_by_id("conversion-other-track")
            .one(&db)
            .await
            .expect("query other row after conversion")
            .is_some());
        assert!(track::Entity::find()
            .filter(track::Column::FilePath.eq(new_audio.to_string_lossy().as_ref()))
            .one(&db)
            .await
            .expect("query new row after conversion")
            .is_none());

        let converted =
            library_root::Entity::find_by_id(target.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query converted root")
                .expect("converted root exists");
        assert!(converted.identity_confirmed);
        assert!(!converted.is_available);
        assert!(converted.last_scan_complete);

        assert_eq!(
            complete_root_trust_scan(&db, &music_dirs, &event_tx, &pending).await,
            RootTrustOutcome::Active
        );
        assert!(track::Entity::find_by_id("conversion-target-track")
            .one(&db)
            .await
            .expect("query target row after ordinary scan")
            .is_none());
        assert!(track::Entity::find_by_id("conversion-other-track")
            .one(&db)
            .await
            .expect("query other row after ordinary scan")
            .is_none());
        assert!(track::Entity::find()
            .filter(track::Column::FilePath.eq(new_audio.to_string_lossy().as_ref()))
            .one(&db)
            .await
            .expect("query new row after ordinary scan")
            .is_some());
    }

    #[tokio::test]
    async fn nonempty_prompt_becoming_empty_requires_fresh_empty_consent() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-nonempty-to-empty");
        let audio_path = directory.path().join("remembered.wav");
        write_minimal_wav(&audio_path);
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist nonempty legacy root");
        insert_rename_test_track(
            &db,
            "nonempty-to-empty-track",
            audio_path.to_string_lossy().as_ref(),
            "Remembered",
            0,
        )
        .await;
        let request =
            build_root_trust_request(&scan, &stored, RootTrustReason::LegacyEnrollment, 1)
                .expect("build nonempty request");
        assert!(!request.requires_empty_acknowledgement());
        let original_id = request.request_id();
        std::fs::remove_file(&audio_path).expect("root becomes empty after prompt");

        let (event_tx, event_rx) = async_channel::unbounded();
        assert!(matches!(
            begin_root_trust_command(&db, &[directory.path().to_path_buf()], &event_tx, &request,)
                .await
                .expect("run guarded conversion"),
            RootTrustCommandStart::Unavailable(_)
        ));
        assert!(track::Entity::find_by_id("nonempty-to-empty-track")
            .one(&db)
            .await
            .expect("query remembered track")
            .is_some());

        let persisted =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query drifted root")
                .expect("drifted root exists");
        assert!(!persisted.identity_confirmed);
        assert!(!persisted.is_available);
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let fresh = events
            .iter()
            .find_map(|event| match event {
                LibraryEvent::RootTrustRequired(requests) => requests.first(),
                _ => None,
            })
            .expect("empty observation emits fresh consent request");
        assert_ne!(fresh.request_id(), original_id);
        assert_eq!(fresh.reason(), RootTrustReason::EmptyRoot);
        assert!(fresh.requires_empty_acknowledgement());
        assert!(fresh.expected_state.matches(&persisted));
        assert!(!events.iter().any(|event| matches!(
            event,
            LibraryEvent::TrackRemoved(path) if path == audio_path.to_string_lossy().as_ref()
        )));
    }

    #[tokio::test]
    async fn empty_prompt_becoming_nonempty_requires_fresh_nonempty_consent() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-empty-to-nonempty");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist empty legacy root");
        let remembered_path = directory.path().join("remembered.flac");
        insert_rename_test_track(
            &db,
            "empty-to-nonempty-track",
            remembered_path.to_string_lossy().as_ref(),
            "Remembered",
            0,
        )
        .await;
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 1)
            .expect("build empty request");
        assert!(request.requires_empty_acknowledgement());
        let original_id = request.request_id();
        let new_audio = directory.path().join("new.wav");
        write_minimal_wav(&new_audio);

        let (event_tx, event_rx) = async_channel::unbounded();
        assert!(matches!(
            begin_root_trust_command(&db, &[directory.path().to_path_buf()], &event_tx, &request,)
                .await
                .expect("run guarded conversion"),
            RootTrustCommandStart::Unavailable(_)
        ));
        assert!(track::Entity::find_by_id("empty-to-nonempty-track")
            .one(&db)
            .await
            .expect("query remembered track")
            .is_some());
        assert!(track::Entity::find()
            .filter(track::Column::FilePath.eq(new_audio.to_string_lossy().as_ref()))
            .one(&db)
            .await
            .expect("query newly observed track")
            .is_none());

        let persisted =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query drifted root")
                .expect("drifted root exists");
        assert!(!persisted.identity_confirmed);
        assert!(!persisted.is_available);
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let fresh = events
            .iter()
            .find_map(|event| match event {
                LibraryEvent::RootTrustRequired(requests) => requests.first(),
                _ => None,
            })
            .expect("nonempty observation emits fresh consent request");
        assert_ne!(fresh.request_id(), original_id);
        assert_eq!(fresh.reason(), RootTrustReason::LegacyEnrollment);
        assert!(!fresh.requires_empty_acknowledgement());
        assert!(fresh.expected_state.matches(&persisted));
        assert!(!events.iter().any(|event| matches!(
            event,
            LibraryEvent::TrackUpserted(track)
                if track.file_path.as_deref() == Some(new_audio.to_string_lossy().as_ref())
        )));
    }

    #[tokio::test]
    async fn forced_identity_change_emits_a_replacement_request() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-forced-identity-change");
        let audio_path = directory.path().join("present.wav");
        write_minimal_wav(&audio_path);
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist legacy root");
        insert_rename_test_track(
            &db,
            "forced-identity-track",
            audio_path.to_string_lossy().as_ref(),
            "Present",
            0,
        )
        .await;
        let request =
            build_root_trust_request(&scan, &stored, RootTrustReason::LegacyEnrollment, 1)
                .expect("build legacy request");
        let staged_marker = stage_root_trust(&db, &[directory.path().to_path_buf()], &request)
            .await
            .expect("stage original marker");
        let replacement_marker = format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4());
        std::fs::write(
            root_identity_path(directory.path()),
            format!("{replacement_marker}\n"),
        )
        .expect("replace marker before conversion");

        let forced = HashMap::from([(
            directory.path().to_path_buf(),
            ForcedRootTrustConversion {
                marker_identity: staged_marker,
                expected_empty: false,
                expected_mount_generation: request.observed_mount_generation,
                original_reason: request.reason,
            },
        )]);
        let authority_guards = HashMap::new();
        let evidence_refreshes = HashMap::new();
        let (event_tx, event_rx) = async_channel::unbounded();
        initial_scan_with_root_trust_guards(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &forced,
            &authority_guards,
            &evidence_refreshes,
        )
        .await
        .expect("run guarded conversion");

        assert!(track::Entity::find_by_id("forced-identity-track")
            .one(&db)
            .await
            .expect("query protected track")
            .is_some());
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let fresh = events
            .iter()
            .find_map(|event| match event {
                LibraryEvent::RootTrustRequired(requests) => requests.first(),
                _ => None,
            })
            .expect("identity change emits a fresh request");
        assert_eq!(fresh.reason(), RootTrustReason::Replacement);
        assert_eq!(fresh.observed_identity, replacement_marker);
        assert!(!fresh.requires_empty_acknowledgement());
    }

    #[tokio::test]
    async fn replacement_prompt_content_drift_preserves_replacement_classification() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-replacement-content-drift");
        create_root_marker(directory.path()).expect("create replacement marker");
        let audio_path = directory.path().join("present.wav");
        write_minimal_wav(&audio_path);
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist observed replacement");
        let old_marker = format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4());
        let mut previously_confirmed: library_root::ActiveModel = stored.into();
        previously_confirmed.device_id = Set(Some(old_marker));
        previously_confirmed.identity_confirmed = Set(true);
        let previously_confirmed = previously_confirmed
            .update(&db)
            .await
            .expect("persist previous confirmed identity");
        insert_rename_test_track(
            &db,
            "replacement-content-drift-track",
            audio_path.to_string_lossy().as_ref(),
            "Remembered",
            0,
        )
        .await;
        let request = build_root_trust_request(
            &scan,
            &previously_confirmed,
            RootTrustReason::Replacement,
            1,
        )
        .expect("build replacement request");
        let original_id = request.request_id();
        std::fs::remove_file(&audio_path).expect("replacement becomes empty after prompt");

        let (event_tx, event_rx) = async_channel::unbounded();
        assert!(matches!(
            begin_root_trust_command(&db, &[directory.path().to_path_buf()], &event_tx, &request,)
                .await
                .expect("run guarded replacement conversion"),
            RootTrustCommandStart::Unavailable(_)
        ));

        assert!(track::Entity::find_by_id("replacement-content-drift-track")
            .one(&db)
            .await
            .expect("query protected replacement track")
            .is_some());
        let persisted =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query drifted replacement")
                .expect("replacement state exists");
        assert!(!persisted.identity_confirmed);
        assert!(!persisted.is_available);
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let fresh = events
            .iter()
            .find_map(|event| match event {
                LibraryEvent::RootTrustRequired(requests) => requests.first(),
                _ => None,
            })
            .expect("content drift emits a fresh replacement request");
        assert_ne!(fresh.request_id(), original_id);
        assert_eq!(fresh.reason(), RootTrustReason::Replacement);
        assert!(fresh.requires_empty_acknowledgement());
        assert!(fresh.expected_state.matches(&persisted));
    }

    #[tokio::test]
    async fn authoritative_follow_up_requires_the_unavailable_state_gate() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-authority-gate");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist unconfirmed root");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 1)
            .expect("build request");
        insert_rename_test_track(
            &db,
            "authority-gate-track",
            directory
                .path()
                .join("remembered.flac")
                .to_string_lossy()
                .as_ref(),
            "Remembered",
            0,
        )
        .await;
        let (event_tx, _event_rx) = async_channel::unbounded();
        let music_dirs = vec![directory.path().to_path_buf()];
        let RootTrustCommandStart::Pending(pending) =
            begin_root_trust_command(&db, &music_dirs, &event_tx, &request)
                .await
                .expect("convert root")
        else {
            panic!("queue follow-up");
        };

        let converted =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query converted root")
                .expect("converted root exists");
        let mut conflicting: library_root::ActiveModel = converted.into();
        conflicting.is_available = Set(true);
        conflicting
            .update(&db)
            .await
            .expect("inject conflicting active state");

        assert_eq!(
            complete_root_trust_scan(&db, &music_dirs, &event_tx, &pending).await,
            RootTrustOutcome::TrustedButUnavailable
        );
        assert!(track::Entity::find_by_id("authority-gate-track")
            .one(&db)
            .await
            .expect("query protected row")
            .is_some());
        let guarded =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query guarded root")
                .expect("guarded root exists");
        assert!(!guarded.is_available);
        assert!(!guarded.last_scan_complete);
    }

    #[tokio::test]
    async fn authoritative_follow_up_mount_change_demotes_until_fresh_consent() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-authority-mount-change");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist unconfirmed root");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 1)
            .expect("build request");
        let remembered_path = directory.path().join("remembered.flac");
        insert_rename_test_track(
            &db,
            "authority-mount-change-track",
            remembered_path.to_string_lossy().as_ref(),
            "Remembered",
            0,
        )
        .await;
        let (event_tx, event_rx) = async_channel::unbounded();
        let music_dirs = vec![directory.path().to_path_buf()];
        let RootTrustCommandStart::Pending(mut pending) =
            begin_root_trust_command(&db, &music_dirs, &event_tx, &request)
                .await
                .expect("convert root")
        else {
            panic!("queue guarded follow-up");
        };
        pending.expected_mount_generation = pending.expected_mount_generation.wrapping_add(1);

        assert_eq!(
            complete_root_trust_scan(&db, &music_dirs, &event_tx, &pending).await,
            RootTrustOutcome::TrustedButUnavailable
        );
        assert!(track::Entity::find_by_id("authority-mount-change-track")
            .one(&db)
            .await
            .expect("query protected row after guarded mismatch")
            .is_some());
        let demoted =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query demoted root")
                .expect("demoted root exists");
        assert!(!demoted.identity_confirmed);
        assert!(!demoted.is_available);
        let guarded_events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let fresh = guarded_events
            .iter()
            .find_map(|event| match event {
                LibraryEvent::RootTrustRequired(requests) => requests.first(),
                _ => None,
            })
            .expect("mount change emits fresh replacement consent");
        assert_eq!(fresh.reason(), RootTrustReason::Replacement);
        assert!(fresh.requires_empty_acknowledgement());
        assert!(fresh.expected_state.matches(&demoted));
        assert!(!guarded_events.iter().any(|event| matches!(
            event,
            LibraryEvent::TrackRemoved(path) if path == remembered_path.to_string_lossy().as_ref()
        )));

        initial_scan(&db, &music_dirs, &event_tx)
            .await
            .expect("run later ordinary scan");
        assert!(track::Entity::find_by_id("authority-mount-change-track")
            .one(&db)
            .await
            .expect("query protected row after ordinary scan")
            .is_some());
        let still_unconfirmed =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query root after ordinary scan")
                .expect("root still exists");
        assert!(!still_unconfirmed.identity_confirmed);
        assert!(!still_unconfirmed.is_available);
    }

    #[tokio::test]
    async fn incomplete_authoritative_follow_up_preserves_durable_identity() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-authority-incomplete");
        let audio_path = directory.path().join("present.wav");
        write_minimal_wav(&audio_path);
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist legacy root");
        let request =
            build_root_trust_request(&scan, &stored, RootTrustReason::LegacyEnrollment, 1)
                .expect("build request");
        insert_rename_test_track(
            &db,
            "authority-incomplete-track",
            audio_path.to_string_lossy().as_ref(),
            "Present",
            0,
        )
        .await;
        let (event_tx, event_rx) = async_channel::unbounded();
        let music_dirs = vec![directory.path().to_path_buf()];
        let RootTrustCommandStart::Pending(pending) =
            begin_root_trust_command(&db, &music_dirs, &event_tx, &request)
                .await
                .expect("convert root")
        else {
            panic!("queue guarded follow-up");
        };
        let parked = directory.path().with_extension("temporarily-unavailable");
        std::fs::rename(directory.path(), &parked).expect("hide root before follow-up");

        assert_eq!(
            complete_root_trust_scan(&db, &music_dirs, &event_tx, &pending).await,
            RootTrustOutcome::TrustedButUnavailable
        );
        let incomplete =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query incomplete root")
                .expect("incomplete root exists");
        assert!(incomplete.identity_confirmed);
        assert!(!incomplete.is_available);
        assert!(!incomplete.last_scan_complete);
        assert!(track::Entity::find_by_id("authority-incomplete-track")
            .one(&db)
            .await
            .expect("query row after incomplete scan")
            .is_some());
        let incomplete_events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert!(!incomplete_events
            .iter()
            .any(|event| matches!(event, LibraryEvent::RootTrustRequired(_))));

        std::fs::rename(&parked, directory.path()).expect("restore root after transient failure");
        initial_scan(&db, &music_dirs, &event_tx)
            .await
            .expect("rescan restored root");
        let restored =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query restored root")
                .expect("restored root exists");
        assert!(restored.identity_confirmed);
        assert!(restored.is_available);
        assert!(restored.last_scan_complete);
    }

    #[tokio::test]
    async fn unavailable_confirmation_refreshes_retryable_evidence() {
        use sea_orm::ConnectionTrait;

        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-unavailable-refresh");
        create_root_marker(directory.path()).expect("create adoptable marker");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist unconfirmed marker");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 0)
            .expect("build adoption request");
        let request_id = request.request_id();
        db.execute_unprepared(
            "CREATE TRIGGER fail_conversion_persistence
             BEFORE UPDATE ON library_roots
             WHEN OLD.identity_confirmed = 0
              AND NEW.identity_confirmed = 1
             BEGIN
                 SELECT RAISE(ABORT, 'injected conversion persistence failure');
             END",
        )
        .await
        .expect("create conversion failure trigger");

        let (event_tx, event_rx) = async_channel::unbounded();
        let mut completed = HashMap::new();
        assert!(process_library_command(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &mut completed,
            LibraryCommand::ConfirmRootTrust(request),
        )
        .await
        .is_none());
        assert!(completed.is_empty());

        let persisted =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query refreshed root")
                .expect("refreshed root exists");
        assert!(!persisted.identity_confirmed);
        assert!(!persisted.is_available);
        assert!(persisted.last_scan_complete);
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let finished_index = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    LibraryEvent::RootTrustFinished {
                        request_id: finished_id,
                        outcome: RootTrustOutcome::TrustedButUnavailable,
                        ..
                    } if *finished_id == request_id
                )
            })
            .expect("emit unavailable result");
        let (required_index, fresh) = events
            .iter()
            .enumerate()
            .find_map(|(index, event)| match event {
                LibraryEvent::RootTrustRequired(requests) => {
                    requests.first().map(|request| (index, request))
                }
                _ => None,
            })
            .expect("refresh emits retryable evidence");
        assert!(required_index > finished_index);
        assert_eq!(fresh.request_id(), request_id);
        assert_eq!(fresh.reason(), RootTrustReason::EmptyRoot);
        assert!(fresh.requires_empty_acknowledgement());
        assert!(fresh.expected_state.matches(&persisted));

        db.execute_unprepared("DROP TRIGGER fail_conversion_persistence")
            .await
            .expect("remove conversion failure trigger");
    }

    #[tokio::test]
    async fn marker_created_before_db_failure_requires_fresh_adoption() {
        use sea_orm::ConnectionTrait;

        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-db-failure-retry");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist legacy root");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 0)
            .expect("build legacy request");
        let original_id = request.request_id();
        db.execute_unprepared(
            "CREATE TRIGGER fail_root_trust_stage
             BEFORE UPDATE ON library_roots
             WHEN OLD.last_scan_complete = 1
              AND NEW.last_scan_complete = 0
              AND NEW.device_id != OLD.device_id
             BEGIN
                 SELECT RAISE(ABORT, 'injected root trust failure');
             END",
        )
        .await
        .expect("create root trust failure trigger");

        assert!(matches!(
            stage_root_trust(&db, &[directory.path().to_path_buf()], &request).await,
            Err(RootTrustError::Failed(_))
        ));
        let orphan_marker = read_root_marker(directory.path())
            .expect("read orphan marker")
            .expect("marker remains after DB failure");
        assert!(is_marker_identity(&orphan_marker));
        let unchanged =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query unchanged state")
                .expect("root state remains");
        assert_eq!(unchanged.device_id, stored.device_id);

        db.execute_unprepared("DROP TRIGGER fail_root_trust_stage")
            .await
            .expect("drop failure trigger");
        let (event_tx, event_rx) = async_channel::unbounded();
        initial_scan(&db, &[directory.path().to_path_buf()], &event_tx)
            .await
            .expect("rescan orphan marker");
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let fresh = events
            .iter()
            .find_map(|event| match event {
                LibraryEvent::RootTrustRequired(requests) => requests.first(),
                _ => None,
            })
            .expect("orphan marker requires fresh adoption");
        assert_ne!(fresh.request_id(), original_id);
        assert_eq!(fresh.observed_identity, orphan_marker);
    }

    #[tokio::test]
    async fn stale_legacy_confirmation_emits_a_fresh_marker_adoption_request() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-fresh-adoption");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist legacy state");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 0)
            .expect("build legacy request");
        let original_id = request.request_id();
        create_root_marker(directory.path()).expect("marker appears after prompt");

        let (event_tx, event_rx) = async_channel::unbounded();
        let mut completed = HashMap::new();
        assert!(process_library_command(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &mut completed,
            LibraryCommand::ConfirmRootTrust(request),
        )
        .await
        .is_none());

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert!(events.iter().any(|event| matches!(
            event,
            LibraryEvent::RootTrustFinished {
                request_id,
                outcome: RootTrustOutcome::Stale,
                ..
            } if *request_id == original_id
        )));
        let fresh = events
            .iter()
            .find_map(|event| match event {
                LibraryEvent::RootTrustRequired(requests) => requests.first(),
                _ => None,
            })
            .expect("refresh emits a new adoption request");
        assert_ne!(fresh.request_id(), original_id);
        assert!(is_marker_identity(&fresh.observed_identity));
    }

    #[tokio::test]
    async fn command_only_mode_replays_completed_duplicate_without_side_effects() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-command-only");
        create_root_marker(directory.path()).expect("create adoptable marker");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist unconfirmed marker");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 0)
            .expect("build adoption request");
        let request_id = request.request_id();

        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::bounded(2);
        command_tx
            .send(LibraryCommand::ConfirmRootTrust(request.clone()))
            .await
            .expect("send first command");
        command_tx
            .send(LibraryCommand::ConfirmRootTrust(request))
            .await
            .expect("send duplicate command");
        drop(command_tx);
        let mut completed = HashMap::new();
        process_library_commands_without_watcher(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &command_rx,
            &mut completed,
        )
        .await;

        assert_eq!(completed.len(), 1);
        let outcomes: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                LibraryEvent::RootTrustFinished {
                    request_id: finished_id,
                    outcome,
                    ..
                } if finished_id == request_id => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(
            outcomes,
            [RootTrustOutcome::Active, RootTrustOutcome::Active],
            "the duplicate receives the cached terminal result"
        );
    }

    #[tokio::test]
    async fn unchanged_request_can_retry_after_a_transient_failure() {
        use sea_orm::ConnectionTrait;

        let db = rename_test_database().await;
        let directory = TestDirectory::new("trust-retry-same-evidence");
        create_root_marker(directory.path()).expect("create adoptable marker");
        let scan = scan_root(directory.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist unconfirmed marker");
        let request = build_root_trust_request(&scan, &stored, RootTrustReason::EmptyRoot, 0)
            .expect("build retryable request");
        let request_id = request.request_id();
        db.execute_unprepared(
            "CREATE TRIGGER fail_first_trust_attempt
             BEFORE UPDATE ON library_roots
             WHEN NEW.last_scan_complete = 0
             BEGIN
                 SELECT RAISE(ABORT, 'injected transient trust failure');
             END",
        )
        .await
        .expect("create transient failure trigger");

        let (event_tx, event_rx) = async_channel::unbounded();
        let mut completed = HashMap::new();
        assert!(process_library_command(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &mut completed,
            LibraryCommand::ConfirmRootTrust(request.clone()),
        )
        .await
        .is_none());
        assert!(
            completed.is_empty(),
            "a failed attempt must not poison deterministic request-ID retries"
        );

        db.execute_unprepared("DROP TRIGGER fail_first_trust_attempt")
            .await
            .expect("remove transient failure trigger");
        let pending = process_library_command(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &mut completed,
            LibraryCommand::ConfirmRootTrust(request),
        )
        .await
        .expect("same evidence is processed again");
        finish_pending_root_trust_scan(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &mut completed,
            pending,
        )
        .await;

        assert_eq!(completed.len(), 1);
        let outcomes: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                LibraryEvent::RootTrustFinished {
                    request_id: finished_id,
                    outcome,
                    ..
                } if finished_id == request_id => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(
            outcomes,
            [RootTrustOutcome::Failed, RootTrustOutcome::Active]
        );
    }

    #[test]
    fn legacy_identity_with_rows_requires_explicit_trust() {
        let directory = TestDirectory::new("legacy-marker-conversion");
        std::fs::write(directory.path().join("song.mp3"), []).expect("create audio fixture");
        let mut scan = scan_root(directory.path().to_path_buf());
        let previous = persisted_root_state(&scan, scan.device_id.clone());
        assert!(scan.device_id.as_deref().is_some_and(is_legacy_identity));

        assert_eq!(
            prepare_durable_root_identity(&mut scan, Some(&previous), 1, true),
            RootIdentityPreparation::Unchanged
        );
        assert!(!root_identity_path(directory.path()).exists());
        assert!(!reconciliation_is_authoritative(&scan, Some(&previous)));
        assert_eq!(
            root_trust_reason(&scan, Some(&previous), 1, true, false),
            Some(RootTrustReason::LegacyEnrollment)
        );
    }

    #[test]
    fn new_root_enrollment_creates_durable_marker() {
        let directory = TestDirectory::new("new-marker-enrollment");
        std::fs::write(directory.path().join("song.mp3"), []).expect("create audio fixture");
        let mut scan = scan_root(directory.path().to_path_buf());

        assert!(matches!(
            prepare_durable_root_identity(&mut scan, None, 0, true),
            RootIdentityPreparation::MarkerCreated { .. }
        ));
        assert!(scan.device_id.as_deref().is_some_and(is_marker_identity));
        assert!(scan_confirms_identity(&scan, None, 0));
    }

    #[test]
    fn discovered_nested_root_is_not_modified_or_auto_enrolled() {
        let directory = TestDirectory::new("discovered-markerless-root");
        std::fs::write(directory.path().join("song.mp3"), []).expect("create audio fixture");
        let mut scan = scan_root(directory.path().to_path_buf());

        assert_eq!(
            prepare_durable_root_identity(&mut scan, None, 0, false),
            RootIdentityPreparation::Unchanged
        );
        assert!(!root_identity_path(directory.path()).exists());
        assert!(!scan_confirms_identity_for_scope(&scan, None, 0, false));
    }

    #[test]
    fn mount_change_before_marker_creation_aborts_enrollment() {
        let directory = TestDirectory::new("marker-mount-race");
        std::fs::write(directory.path().join("song.mp3"), []).expect("create audio fixture");
        let mut scan = scan_root(directory.path().to_path_buf());
        scan.mount_generation = scan
            .mount_generation
            .map(|generation| generation.wrapping_add(1));

        assert_eq!(
            prepare_durable_root_identity(&mut scan, None, 0, true),
            RootIdentityPreparation::Unchanged
        );
        assert!(!root_identity_path(directory.path()).exists());
        assert!(!scan.is_complete());
    }

    #[tokio::test]
    async fn initial_scan_root_revalidation_disables_remaining_writes() {
        let directory = TestDirectory::new("initial-scan-marker-race");
        let audio_path = directory.path().join("song.mp3");
        std::fs::write(&audio_path, []).expect("create audio fixture");
        create_root_marker(directory.path()).expect("create durable root identity");
        let mut scans = vec![scan_root(directory.path().to_path_buf())];
        scans[0].content_authorized = true;

        assert_eq!(
            revalidate_scan_root_for_path(&audio_path, &mut scans)
                .await
                .expect("run initial authority task"),
            (true, None)
        );
        std::fs::write(
            root_identity_path(directory.path()),
            format!("{ROOT_IDENTITY_PREFIX}{}\n", Uuid::new_v4()),
        )
        .expect("replace root identity");
        assert_eq!(
            revalidate_scan_root_for_path(&audio_path, &mut scans)
                .await
                .expect("run changed authority task"),
            (false, Some(directory.path().to_path_buf()))
        );
        assert!(!scans[0].content_authorized);
        assert!(!scans[0].reconciliation_authoritative);
        assert_eq!(
            revalidate_scan_root_for_path(&audio_path, &mut scans)
                .await
                .expect("skip disabled authority task"),
            (false, None)
        );
    }

    #[test]
    fn duplicate_root_markers_make_every_copy_incomplete() {
        let first = TestDirectory::new("duplicate-marker-first");
        let second = TestDirectory::new("duplicate-marker-second");
        let identity = create_root_marker(first.path())
            .expect("create first marker")
            .identity;
        std::fs::write(root_identity_path(second.path()), format!("{identity}\n"))
            .expect("copy marker");
        let mut scans = vec![
            scan_root(first.path().to_path_buf()),
            scan_root(second.path().to_path_buf()),
        ];

        reject_duplicate_marker_identities(&mut scans);

        assert!(scans.iter().all(|scan| !scan.is_complete()));
    }

    #[test]
    fn watcher_root_cache_prefers_specific_roots_and_retains_invalidation() {
        let parent = PathBuf::from("/music");
        let child = parent.join("removable");
        let state = |root: &Path| library_root::Model {
            path: root.to_string_lossy().into_owned(),
            device_id: Some(format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4())),
            identity_confirmed: true,
            is_available: true,
            last_scan_complete: true,
            last_checked_at: "2026-07-10T00:00:00Z".to_string(),
        };
        let mut cache = WatcherRootCache::from_models(
            vec![state(&parent), state(&child), state(Path::new("/other"))],
            std::slice::from_ref(&parent),
        );

        let (child_index, selected_root, selected_state) = cache
            .root_for_path(&child.join("album/song.flac"))
            .expect("select nested root");
        assert_eq!(selected_root, child);
        assert!(selected_state.is_available);
        assert!(cache.invalidate(child_index).is_some());

        let (_, selected_root, selected_state) = cache
            .root_for_path(&child.join("album/other.flac"))
            .expect("retain nested root");
        assert_eq!(selected_root, child);
        assert!(!selected_state.is_available);
        assert!(!selected_state.last_scan_complete);
        assert!(cache
            .root_for_path(&parent.join("parent-song.flac"))
            .is_some_and(|(_, root, state)| root == parent && state.is_available));
        assert!(cache.root_for_path(Path::new("/other/song.flac")).is_none());
    }

    #[test]
    fn marker_access_events_do_not_invalidate_root_identity() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        };
        use notify::EventKind;

        assert!(!marker_event_invalidates_root(EventKind::Access(
            AccessKind::Open(AccessMode::Read)
        )));
        assert!(!marker_event_invalidates_root(EventKind::Access(
            AccessKind::Read
        )));
        assert!(!marker_event_invalidates_root(EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::AccessTime)
        )));
        assert!(marker_event_invalidates_root(EventKind::Create(
            CreateKind::File
        )));
        assert!(marker_event_invalidates_root(EventKind::Modify(
            ModifyKind::Data(DataChange::Content)
        )));
        assert!(marker_event_invalidates_root(EventKind::Remove(
            RemoveKind::File
        )));
        assert!(marker_event_invalidates_root(EventKind::Any));
    }

    #[test]
    fn missing_root_is_incomplete_and_never_authoritative() {
        let directory = TestDirectory::new("missing");
        let missing = directory.path().join("not-mounted");

        let scan = scan_root(missing.clone());

        assert!(!scan.is_complete());
        assert!(scan.audio_files.is_empty());
        assert!(!should_remove_stale_track(
            &missing.join("remembered.flac"),
            &HashSet::new(),
            &[scan]
        ));
    }

    #[test]
    fn healthy_empty_root_is_authoritative() {
        let directory = TestDirectory::new("empty");
        create_root_marker(directory.path()).expect("create durable root identity");
        let mut scan = scan_root(directory.path().to_path_buf());
        let previous = persisted_root_state(&scan, scan.device_id.clone());
        scan.reconciliation_authoritative = reconciliation_is_authoritative(&scan, Some(&previous));

        assert!(scan.is_complete());
        assert!(scan.audio_files.is_empty());
        assert!(should_remove_stale_track(
            &directory.path().join("deleted-while-offline.mp3"),
            &HashSet::new(),
            &[scan]
        ));
    }

    #[test]
    fn rows_outside_configured_roots_are_not_removed() {
        let configured = TestDirectory::new("configured");
        let unrelated = TestDirectory::new("unrelated");
        let mut scan = scan_root(configured.path().to_path_buf());
        scan.reconciliation_authoritative = true;

        assert!(!should_remove_stale_track(
            &unrelated.path().join("remembered.flac"),
            &HashSet::new(),
            &[scan]
        ));
    }

    #[test]
    fn overlapping_roots_scan_each_audio_path_once() {
        let directory = TestDirectory::new("overlap");
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).expect("create nested root");
        let audio_path = nested.join("song.mp3");
        std::fs::write(&audio_path, []).expect("create audio fixture");

        let mut scans = vec![
            scan_root(directory.path().to_path_buf()),
            scan_root(nested.clone()),
        ];
        for scan in &mut scans {
            scan.content_authorized = true;
        }

        assert_eq!(collect_audio_files(&scans), vec![audio_path]);
    }

    #[test]
    fn configured_child_root_is_excluded_from_parent_and_scanned_independently() {
        let directory = TestDirectory::new("configured-child");
        let child = directory.path().join("child");
        std::fs::create_dir(&child).expect("create child root");
        let audio_path = child.join("song.mp3");
        std::fs::write(&audio_path, []).expect("create audio fixture");
        let roots = vec![directory.path().to_path_buf(), child.clone()];

        let parent_scan = scan_root_with_exclusions(directory.path().to_path_buf(), &roots);
        let child_scan = scan_root_with_exclusions(child, &roots);

        assert!(parent_scan.audio_files.is_empty());
        assert_eq!(child_scan.audio_files, vec![audio_path]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mountinfo_discovers_same_device_bind_mounts_and_decodes_paths() {
        let configured = vec![PathBuf::from("/music")];
        let mountinfo = "31 20 8:1 / / rw,relatime - ext4 /dev/root rw\n\
                         32 31 8:1 /library /music/Bind\\040Mount rw,relatime - ext4 /dev/root rw\n\
                         33 31 8:2 / /other rw,relatime - ext4 /dev/other rw\n";

        assert_eq!(
            mounted_subroots_from_mountinfo(mountinfo, &configured).expect("parse valid mountinfo"),
            vec![PathBuf::from("/music/Bind Mount")]
        );
        assert_eq!(
            mount_generation_from_mountinfo(
                mountinfo,
                Path::new("/music/Bind Mount/album/song.flac")
            )
            .expect("parse valid mountinfo"),
            Some(32)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_mountinfo_fails_closed() {
        let configured = vec![PathBuf::from("/music")];

        assert!(
            mounted_subroots_from_mountinfo("not-a-mount-record /music/bind", &configured).is_err()
        );
        assert!(
            mounted_subroots_from_mountinfo("32 31 8:1 / /music/bind rw -", &configured).is_err()
        );
        assert!(mounted_subroots_from_mountinfo(
            "32 31 8:1 / /music/Bad\\Escape rw - ext4 /dev/root rw",
            &configured
        )
        .is_err());
    }

    #[test]
    fn mount_discovery_failure_aborts_root_expansion() {
        let configured = vec![PathBuf::from("/music")];
        let result = expanded_scan_roots_with_mount_result(
            &configured,
            &[],
            Err(std::io::Error::other("simulated mountinfo failure")),
        );

        assert!(result.is_err());
    }

    #[test]
    fn persisted_nested_root_remains_a_separate_scope_while_unmounted() {
        let configured = vec![PathBuf::from("/music")];
        let persisted = vec![library_root::Model {
            path: "/music/removable".to_string(),
            device_id: Some("remembered-volume".to_string()),
            identity_confirmed: true,
            is_available: false,
            last_scan_complete: false,
            last_checked_at: "2026-07-10T00:00:00Z".to_string(),
        }];

        assert_eq!(
            expanded_scan_roots_with_mounts(&configured, &persisted, Vec::new()),
            vec![PathBuf::from("/music"), PathBuf::from("/music/removable")]
        );
    }

    #[test]
    fn most_specific_incomplete_root_protects_overlapping_rows() {
        let directory = TestDirectory::new("overlap-incomplete");
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).expect("create nested root");

        let mut parent_scan = scan_root(directory.path().to_path_buf());
        parent_scan.reconciliation_authoritative = true;
        let child_scan = RootScan {
            root: nested.clone(),
            audio_files: Vec::new(),
            errors: vec!["simulated permission error".to_string()],
            device_id: Some("simulated-device".to_string()),
            mount_generation: Some(0),
            authority_lease: None,
            reconciliation_authoritative: false,
            content_authorized: false,
        };

        assert!(!should_remove_stale_track(
            &nested.join("remembered.flac"),
            &HashSet::new(),
            &[parent_scan, child_scan]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_root_is_incomplete() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("permission-denied");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o000))
            .expect("remove root permissions");

        // Privileged containers can retain directory access despite mode 000.
        // In that environment no permission-denied traversal can be exercised.
        if std::fs::read_dir(directory.path()).is_ok() {
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .expect("restore root permissions");
            return;
        }

        let scan = scan_root(directory.path().to_path_buf());

        // Restore permissions before asserting so the fixture can always be
        // removed, even when the assertion fails.
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore root permissions");
        assert!(!scan.is_complete());
    }

    #[cfg(unix)]
    #[test]
    fn partially_unreadable_root_keeps_discovered_files_but_is_incomplete() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("partially-unreadable");
        let readable_audio = directory.path().join("readable.flac");
        std::fs::write(&readable_audio, []).expect("create readable audio fixture");

        let locked = directory.path().join("locked");
        std::fs::create_dir(&locked).expect("create locked directory");
        std::fs::write(locked.join("hidden.flac"), []).expect("create hidden audio fixture");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("remove nested permissions");

        // Root and capability-enabled CI containers can bypass Unix mode bits.
        // Skip the assertion when this fixture cannot induce a read failure.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
                .expect("restore nested permissions");
            return;
        }

        let scan = scan_root(directory.path().to_path_buf());

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
            .expect("restore nested permissions");
        assert!(!scan.is_complete());
        assert_eq!(scan.audio_files, vec![readable_audio]);
    }

    fn persisted_root_state(scan: &RootScan, device_id: Option<String>) -> library_root::Model {
        library_root::Model {
            path: scan.root.to_string_lossy().into_owned(),
            device_id,
            identity_confirmed: true,
            is_available: true,
            last_scan_complete: true,
            last_checked_at: "2026-07-10T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn matching_persisted_device_authorizes_an_empty_root() {
        let directory = TestDirectory::new("matching-device");
        create_root_marker(directory.path()).expect("create durable root identity");
        let scan = scan_root(directory.path().to_path_buf());
        let previous = persisted_root_state(&scan, scan.device_id.clone());

        assert!(reconciliation_is_authoritative(&scan, Some(&previous)));
    }

    #[test]
    fn changed_device_identity_blocks_stale_deletion() {
        let directory = TestDirectory::new("changed-device");
        let scan = scan_root(directory.path().to_path_buf());
        let previous = persisted_root_state(&scan, Some("different-device".to_string()));

        assert!(!reconciliation_is_authoritative(&scan, Some(&previous)));
    }

    #[test]
    fn legacy_empty_root_with_tracks_bootstraps_conservatively() {
        let directory = TestDirectory::new("legacy-empty");
        let scan = scan_root(directory.path().to_path_buf());

        assert!(!reconciliation_is_authoritative(&scan, None));
        assert!(!scan_confirms_identity(&scan, None, 1));
    }

    #[test]
    fn ambiguous_small_legacy_root_stays_unconfirmed() {
        let directory = TestDirectory::new("legacy-content");
        std::fs::write(directory.path().join("present.mp3"), []).expect("create audio fixture");
        let scan = scan_root(directory.path().to_path_buf());

        assert!(!reconciliation_is_authoritative(&scan, None));
        assert!(!scan_confirms_identity(&scan, None, 1));
        assert!(!scan_confirms_identity(&scan, None, 2));
    }

    #[test]
    fn complete_multi_track_legacy_clone_stays_unconfirmed() {
        let directory = TestDirectory::new("legacy-complete-evidence");
        std::fs::write(directory.path().join("present.mp3"), []).expect("create audio fixture");
        let scan = scan_root(directory.path().to_path_buf());

        assert!(!scan_confirms_identity(&scan, None, 3));
        assert!(!scan_confirms_identity(&scan, None, 4));
    }

    #[test]
    fn absent_untracked_nested_rows_keep_legacy_parent_unconfirmed() {
        let directory = TestDirectory::new("legacy-absent-nested");
        std::fs::write(directory.path().join("present.mp3"), []).expect("create audio fixture");
        let mut scan = scan_root(directory.path().to_path_buf());
        let unconfirmed = library_root::Model {
            path: directory.path().to_string_lossy().into_owned(),
            device_id: scan.device_id.clone(),
            identity_confirmed: false,
            is_available: false,
            last_scan_complete: true,
            last_checked_at: "2026-07-10T00:00:00Z".to_string(),
        };

        // Observed parent content must not let the parent claim rows remembered
        // below an absent, not-yet-discovered nested mount.
        assert!(!scan_confirms_identity(&scan, None, 10));
        assert!(!scan_confirms_identity(&scan, Some(&unconfirmed), 10));
        scan.reconciliation_authoritative =
            reconciliation_is_authoritative(&scan, Some(&unconfirmed));
        assert!(!should_remove_stale_track(
            &directory.path().join("removable/missing.flac"),
            &HashSet::new(),
            &[scan]
        ));
    }

    #[test]
    fn new_empty_mountpoint_does_not_enroll_or_authorize_deletion() {
        let directory = TestDirectory::new("empty-mountpoint");
        let scan = scan_root(directory.path().to_path_buf());

        assert!(!scan_confirms_identity(&scan, None, 0));
        assert!(!reconciliation_is_authoritative(&scan, None));
    }

    #[test]
    fn empty_mountpoint_real_volume_unmount_cycle_never_trusts_mountpoint() {
        let directory = TestDirectory::new("mount-cycle");
        let root = directory.path().to_path_buf();

        let empty_mountpoint = RootScan {
            root: root.clone(),
            audio_files: Vec::new(),
            errors: Vec::new(),
            device_id: Some("underlying-mountpoint".to_string()),
            mount_generation: Some(0),
            authority_lease: None,
            reconciliation_authoritative: false,
            content_authorized: false,
        };
        assert!(!scan_confirms_identity(&empty_mountpoint, None, 0));

        let unconfirmed_mountpoint = library_root::Model {
            path: root.to_string_lossy().into_owned(),
            device_id: empty_mountpoint.device_id.clone(),
            identity_confirmed: false,
            is_available: false,
            last_scan_complete: true,
            last_checked_at: "2026-07-10T00:00:00Z".to_string(),
        };
        let mounted_volume = RootScan {
            root: root.clone(),
            audio_files: vec![root.join("song.mp3")],
            errors: Vec::new(),
            device_id: Some(format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4())),
            mount_generation: Some(0),
            authority_lease: None,
            reconciliation_authoritative: false,
            content_authorized: false,
        };
        assert!(!scan_confirms_identity(
            &mounted_volume,
            Some(&unconfirmed_mountpoint),
            0
        ));
        assert_eq!(
            root_trust_reason(
                &mounted_volume,
                Some(&unconfirmed_mountpoint),
                0,
                true,
                false,
            ),
            Some(RootTrustReason::LegacyEnrollment)
        );
        assert!(!reconciliation_is_authoritative(
            &mounted_volume,
            Some(&unconfirmed_mountpoint)
        ));

        let confirmed_volume = library_root::Model {
            path: root.to_string_lossy().into_owned(),
            device_id: mounted_volume.device_id.clone(),
            identity_confirmed: true,
            is_available: true,
            last_scan_complete: true,
            last_checked_at: "2026-07-10T00:01:00Z".to_string(),
        };
        assert!(!reconciliation_is_authoritative(
            &empty_mountpoint,
            Some(&confirmed_volume)
        ));
        assert!(!scan_confirms_identity(
            &empty_mountpoint,
            Some(&confirmed_volume),
            1
        ));
    }

    #[test]
    fn confirmed_identity_is_never_replaced_by_different_volume_content() {
        let directory = TestDirectory::new("replacement-volume");
        std::fs::write(directory.path().join("replacement.mp3"), [])
            .expect("create replacement fixture");
        let scan = scan_root(directory.path().to_path_buf());
        let previous = persisted_root_state(&scan, Some("intended-volume".to_string()));

        assert!(!reconciliation_is_authoritative(&scan, Some(&previous)));
        assert!(!scan_confirms_identity(&scan, Some(&previous), 4));
    }

    #[test]
    fn unconfirmed_replacement_files_cannot_self_enroll_across_scans() {
        let directory = TestDirectory::new("unconfirmed-replacement");
        std::fs::write(directory.path().join("replacement.mp3"), [])
            .expect("create replacement fixture");
        let mut replacement_scan = scan_root(directory.path().to_path_buf());
        let unconfirmed = library_root::Model {
            path: directory.path().to_string_lossy().into_owned(),
            device_id: replacement_scan.device_id.clone(),
            identity_confirmed: false,
            is_available: false,
            last_scan_complete: true,
            last_checked_at: "2026-07-10T00:00:00Z".to_string(),
        };

        // Existing metadata belongs to the intended volume, but none of the
        // replacement volume's paths match it. The replacement is neither
        // enrolled nor indexed, so it cannot manufacture a matching row that
        // would let a later scan confirm itself.
        for _ in 0..2 {
            assert!(!scan_confirms_identity(
                &replacement_scan,
                Some(&unconfirmed),
                4
            ));
            assert!(!reconciliation_is_authoritative(
                &replacement_scan,
                Some(&unconfirmed)
            ));
            replacement_scan.content_authorized = false;
            assert!(collect_audio_files(std::slice::from_ref(&replacement_scan)).is_empty());
        }
        assert!(!scan_confirms_identity(
            &replacement_scan,
            Some(&unconfirmed),
            10
        ));
    }

    #[test]
    fn identity_change_during_traversal_marks_scan_incomplete() {
        use std::cell::Cell;

        let directory = TestDirectory::new("identity-race");
        std::fs::write(directory.path().join("song.mp3"), []).expect("create audio fixture");
        let calls = Cell::new(0);
        let scan = scan_root_with_identity_probe(directory.path().to_path_buf(), |_| {
            let call = calls.get();
            calls.set(call + 1);
            Ok(if call == 0 {
                "mounted-volume"
            } else {
                "underlying-mountpoint"
            }
            .to_string())
        });

        assert!(!scan.is_complete());
        assert!(scan
            .errors
            .iter()
            .any(|error| error.contains("identity changed during traversal")));
    }

    #[test]
    fn mount_generation_change_during_traversal_marks_scan_incomplete() {
        use std::cell::Cell;

        let directory = TestDirectory::new("mount-generation-race");
        std::fs::write(directory.path().join("song.mp3"), []).expect("create audio fixture");
        let calls = Cell::new(0);
        let scan = scan_root_with_probes_and_exclusions(
            directory.path().to_path_buf(),
            |_| Ok("stable-volume".to_string()),
            |_| {
                let call = calls.get();
                calls.set(call + 1);
                Ok(if call == 0 { 41 } else { 42 })
            },
            &[],
            false,
        );

        assert!(!scan.is_complete());
        assert!(scan
            .errors
            .iter()
            .any(|error| error.contains("mount generation changed during traversal")));
    }

    #[test]
    fn mount_generation_change_between_scans_keeps_stable_identity_authoritative() {
        let directory = TestDirectory::new("mount-generation-reboot");
        let identity = format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4());
        let first_identity = identity.clone();
        let first_scan = scan_root_with_probes_and_exclusions(
            directory.path().to_path_buf(),
            move |_| Ok(first_identity.clone()),
            |_| Ok(41),
            &[],
            false,
        );
        let previous = persisted_root_state(&first_scan, first_scan.device_id.clone());
        let second_identity = identity;
        let second_scan = scan_root_with_probes_and_exclusions(
            directory.path().to_path_buf(),
            move |_| Ok(second_identity.clone()),
            |_| Ok(99),
            &[],
            false,
        );

        assert!(first_scan.is_complete());
        assert!(second_scan.is_complete());
        assert!(reconciliation_is_authoritative(
            &second_scan,
            Some(&previous)
        ));
    }

    #[tokio::test]
    async fn root_identity_and_availability_are_persisted() {
        use sea_orm::Database;
        use sea_orm_migration::MigratorTrait;

        use crate::db::migration::Migrator;

        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, None).await.expect("run migrations");

        let directory = TestDirectory::new("persisted-state");
        create_root_marker(directory.path()).expect("create durable root identity");
        let mut scan = scan_root(directory.path().to_path_buf());
        persist_root_scan_status(&db, &scan, None, true, true, false)
            .await
            .expect("persist available root");

        let stored = library_root::Entity::find_by_id(scan.root.to_string_lossy().into_owned())
            .one(&db)
            .await
            .expect("query root state")
            .expect("root state exists");
        assert_eq!(stored.device_id, scan.device_id);
        assert!(stored.identity_confirmed);
        assert!(stored.is_available);
        assert!(stored.last_scan_complete);

        scan.errors.push("simulated traversal error".to_string());
        scan.reconciliation_authoritative = false;
        persist_root_scan_status(&db, &scan, Some(&stored), false, true, false)
            .await
            .expect("persist unavailable root");

        let updated = library_root::Entity::find_by_id(scan.root.to_string_lossy().into_owned())
            .one(&db)
            .await
            .expect("query updated root state")
            .expect("updated root state exists");
        assert_eq!(updated.device_id, stored.device_id);
        assert!(!updated.is_available);
        assert!(!updated.last_scan_complete);
    }

    #[tokio::test]
    async fn watcher_removal_rolls_back_if_root_identity_changes() {
        use sea_orm::Database;
        use sea_orm_migration::MigratorTrait;

        use crate::db::migration::Migrator;

        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, None).await.expect("run migrations");

        let directory = TestDirectory::new("watcher-identity-race");
        create_root_marker(directory.path()).expect("create durable root identity");
        let scan = scan_root(directory.path().to_path_buf());
        let expected_identity = scan.device_id.clone().expect("root identity");
        persist_root_scan_status(&db, &scan, None, true, true, false)
            .await
            .expect("persist confirmed root");
        let music_dirs = vec![directory.path().to_path_buf()];
        let mut root_cache = WatcherRootCache::load(&db, &music_dirs)
            .await
            .expect("load watcher root state");

        let removed_path = directory.path().join("removed.mp3");
        let model = track::Model {
            id: "watcher-race-track".to_string(),
            file_path: removed_path.to_string_lossy().into_owned(),
            title: "Removed".to_string(),
            artist_name: "Artist".to_string(),
            album_artist_name: None,
            album_title: "Album".to_string(),
            genre: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: Some("MP3".to_string()),
            play_count: 0,
            date_added: "2026-07-10T00:00:00Z".to_string(),
            date_modified: "2026-07-10T00:00:00Z".to_string(),
            file_size_bytes: None,
        };
        let active: track::ActiveModel = model.into();
        active.insert(&db).await.expect("insert remembered track");

        // A debounced removal that outlives a quick recreate/remount must not
        // delete the now-live path even when the root identity matches.
        let recreated_path = removed_path.clone();
        assert!(!delete_track_if_root_stable_with_guard_hook(
            &db,
            &mut root_cache,
            &music_dirs,
            &removed_path,
            move || {
                std::fs::write(recreated_path, []).expect("recreate path before commit guard");
            },
        )
        .await
        .expect("ignore stale removal"));
        std::fs::remove_file(&removed_path).expect("remove recreated watched path");
        assert!(track::Entity::find_by_id("watcher-race-track")
            .one(&db)
            .await
            .expect("query recreated track")
            .is_some());
        let still_available =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query root after path race")
                .expect("root state exists");
        assert!(still_available.is_available);
        assert!(!root_cache.authority_was_lost());

        let marker_path = root_identity_path(directory.path());
        let replacement_identity = format!("{ROOT_IDENTITY_PREFIX}{}\n", Uuid::new_v4());
        let removed = delete_track_if_root_stable_with_guard_hook(
            &db,
            &mut root_cache,
            &music_dirs,
            &removed_path,
            move || {
                std::fs::write(&marker_path, replacement_identity)
                    .expect("change root marker before commit guard");
            },
        )
        .await
        .expect("process removal");

        assert!(!removed);
        assert!(track::Entity::find_by_id("watcher-race-track")
            .one(&db)
            .await
            .expect("query remembered track")
            .is_some());

        let root_state =
            library_root::Entity::find_by_id(directory.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query root state")
                .expect("root state exists");
        assert!(!root_state.is_available);
        assert!(root_state.identity_confirmed);
        assert!(root_cache.authority_was_lost());
        assert_eq!(
            root_state.device_id.as_deref(),
            Some(expected_identity.as_str())
        );
    }

    #[test]
    fn test_db_model_to_track_basic() {
        let model = track::Model {
            id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            file_path: "/music/song.flac".to_string(),
            title: "Test Song".to_string(),
            artist_name: "Test Artist".to_string(),
            album_artist_name: Some("Test Album Artist".to_string()),
            album_title: "Test Album".to_string(),
            genre: Some("Rock".to_string()),
            year: Some(2020),
            track_number: Some(3),
            disc_number: Some(1),
            duration_secs: Some(240),
            bitrate_kbps: Some(320),
            sample_rate_hz: Some(44100),
            format: Some("FLAC".to_string()),
            play_count: 5,
            date_added: "2025-01-15T10:30:00+00:00".to_string(),
            date_modified: "2025-06-01T14:00:00+00:00".to_string(),
            file_size_bytes: Some(30_000_000),
        };

        let track = db_model_to_track(&model);

        assert_eq!(track.title, "Test Song");
        assert_eq!(track.artist_name, "Test Artist");
        assert_eq!(track.album_title, "Test Album");
        assert_eq!(track.genre, Some("Rock".to_string()));
        assert_eq!(track.year, Some(2020));
        assert_eq!(track.track_number, Some(3));
        assert_eq!(track.disc_number, Some(1));
        assert_eq!(track.duration_secs, Some(240));
        assert_eq!(track.bitrate_kbps, Some(320));
        assert_eq!(track.sample_rate_hz, Some(44100));
        assert_eq!(track.format, Some("FLAC".to_string()));
        assert_eq!(track.play_count, Some(5));
        assert_eq!(track.file_path, Some("/music/song.flac".to_string()));
        assert!(track.stream_url.is_none());
        assert!(track.cover_art_url.is_none());
        assert!(track.date_added.is_some());
        assert!(track.date_modified.is_some());
    }

    #[test]
    fn test_db_model_to_track_none_fields() {
        let model = track::Model {
            id: "550e8400-e29b-41d4-a716-446655440001".to_string(),
            file_path: "/music/unknown.mp3".to_string(),
            title: "Unknown".to_string(),
            artist_name: "Unknown Artist".to_string(),
            album_artist_name: None,
            album_title: "Unknown Album".to_string(),
            genre: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            date_added: "2025-01-01T00:00:00+00:00".to_string(),
            date_modified: "2025-01-01T00:00:00+00:00".to_string(),
            file_size_bytes: None,
        };

        let track = db_model_to_track(&model);

        assert_eq!(track.genre, None);
        assert_eq!(track.year, None);
        assert_eq!(track.track_number, None);
        assert_eq!(track.disc_number, None);
        assert_eq!(track.duration_secs, None);
        assert_eq!(track.bitrate_kbps, None);
        assert_eq!(track.sample_rate_hz, None);
        assert_eq!(track.format, None);
        assert_eq!(track.play_count, Some(0));
    }

    #[test]
    fn test_db_model_to_track_invalid_uuid() {
        let model = track::Model {
            id: "not-a-valid-uuid".to_string(),
            file_path: "/music/song.mp3".to_string(),
            title: "Song".to_string(),
            artist_name: "Artist".to_string(),
            album_artist_name: None,
            album_title: "Album".to_string(),
            genre: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            date_added: "2025-01-01T00:00:00+00:00".to_string(),
            date_modified: "2025-01-01T00:00:00+00:00".to_string(),
            file_size_bytes: None,
        };

        // Should not panic — falls back to a new random UUID.
        let track = db_model_to_track(&model);
        assert!(!track.id.is_nil());
    }

    #[test]
    fn test_db_model_to_track_invalid_date() {
        let model = track::Model {
            id: "550e8400-e29b-41d4-a716-446655440002".to_string(),
            file_path: "/music/song.mp3".to_string(),
            title: "Song".to_string(),
            artist_name: "Artist".to_string(),
            album_artist_name: None,
            album_title: "Album".to_string(),
            genre: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            date_added: "not-a-date".to_string(),
            date_modified: "also-not-a-date".to_string(),
            file_size_bytes: None,
        };

        let track = db_model_to_track(&model);
        // Invalid dates should result in None, not a panic.
        assert!(track.date_added.is_none());
        assert!(track.date_modified.is_none());
    }

    #[test]
    fn test_get_mtime_nonexistent_file() {
        let result = get_mtime(std::path::Path::new("/nonexistent/path/file.flac"));
        // Should return empty string, not panic.
        assert!(result.is_empty());
    }
}
