//! Library scanning engine — initial scan + real-time filesystem watching.
//!
//! Runs entirely on the tokio runtime. Sends `LibraryEvent` messages
//! to the GTK main thread via `async_channel`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    TransactionTrait,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

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
    /// An error occurred.
    Error(String),
}

// ---------------------------------------------------------------------------
// LibraryEngine
// ---------------------------------------------------------------------------

/// The background scanning and watching engine.
pub struct LibraryEngine {
    db: DatabaseConnection,
    music_dirs: Vec<PathBuf>,
    tx: async_channel::Sender<LibraryEvent>,
}

impl LibraryEngine {
    /// Create a new engine. Does NOT start scanning yet.
    ///
    /// Accepts multiple music directories — all will be scanned and watched.
    pub fn new(
        db: DatabaseConnection,
        music_dirs: Vec<PathBuf>,
        tx: async_channel::Sender<LibraryEvent>,
    ) -> Self {
        Self { db, music_dirs, tx }
    }

    /// Run the engine: initial scan across all directories, then continuous
    /// FS watching on each.
    pub async fn run(self) {
        let db = Arc::new(self.db);

        // ── Initial scan (all directories) ───────────────────────────
        for dir in &self.music_dirs {
            info!(dir = %dir.display(), "Starting initial library scan");
        }
        if let Err(e) = initial_scan(&db, &self.music_dirs, &self.tx).await {
            error!(error = %e, "Initial scan failed");
            let _ = self.tx.send(LibraryEvent::Error(e.to_string())).await;
        }

        // ── Filesystem watcher (all directories) ─────────────────────
        info!(
            count = self.music_dirs.len(),
            "Starting filesystem watchers"
        );
        if let Err(e) = watch_directories(&db, &self.music_dirs, &self.tx).await {
            error!(error = %e, "Filesystem watcher failed");
            let _ = self.tx.send(LibraryEvent::Error(e.to_string())).await;
        }
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
    reconciliation_authoritative: bool,
    content_authorized: bool,
}

impl RootScan {
    fn is_complete(&self) -> bool {
        self.errors.is_empty()
    }
}

#[cfg(unix)]
fn filesystem_identity(path: &Path) -> std::io::Result<String> {
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
fn filesystem_boundary_id(_path: &Path) -> std::io::Result<u64> {
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
fn root_mount_generation(_path: &Path) -> std::io::Result<u64> {
    // Other platforms still use the stable pre/post filesystem identity. A
    // constant generation keeps the shared traversal implementation portable.
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
fn mounted_subroots(_configured_roots: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
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

#[cfg(windows)]
fn filesystem_identity(path: &Path) -> std::io::Result<String> {
    use std::os::windows::fs::MetadataExt;

    let metadata = std::fs::metadata(path)?;
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // A mounted volume exposes the mounted root's creation time, while an
    // absent directory mount exposes the underlying mountpoint's metadata.
    // Drive-letter and UNC roots become inaccessible when unavailable, which
    // makes their traversal incomplete before this identity is consulted.
    Ok(format!(
        "windows:{}:{}",
        canonical.to_string_lossy(),
        metadata.creation_time()
    ))
}

#[cfg(not(any(unix, windows)))]
fn filesystem_identity(path: &Path) -> std::io::Result<String> {
    Ok(format!("path:{}", path.canonicalize()?.to_string_lossy()))
}

fn scan_root(root: PathBuf) -> RootScan {
    scan_root_with_probes_and_exclusions(root, filesystem_identity, root_mount_generation, &[])
}

fn scan_root_with_identity_probe<F>(root: PathBuf, identity_probe: F) -> RootScan
where
    F: FnMut(&Path) -> std::io::Result<String>,
{
    scan_root_with_probes_and_exclusions(root, identity_probe, root_mount_generation, &[])
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
    )
}

fn scan_root_with_probes_and_exclusions<F, G>(
    root: PathBuf,
    mut identity_probe: F,
    mut mount_generation_probe: G,
    exclusions: &[PathBuf],
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
                reconciliation_authoritative: false,
                content_authorized: false,
            };
        }
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
                reconciliation_authoritative: false,
                content_authorized: false,
            };
        }
    };

    let mut audio_files = Vec::new();
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

    let mut entries = WalkDir::new(&root)
        // Do NOT follow symlinks: the notify watcher does not follow them
        // either, so following here would index files (via symlinked
        // subtrees) that are never watched for changes, and could index one
        // physical file under multiple paths as duplicate rows.
        .follow_links(false)
        .into_iter();
    while let Some(entry) = entries.next() {
        match entry {
            Ok(entry) if entry.depth() > 0 && entry.file_type().is_dir() => {
                if exclusions.iter().any(|path| path == entry.path()) {
                    entries.skip_current_dir();
                    continue;
                }
                if let Some(root_id) = root_boundary {
                    match filesystem_boundary_id(entry.path()) {
                        Ok(entry_id) if root_id != entry_id => {
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
                    // A root whose boundary could not be established is never
                    // authoritative, but avoid crossing any child scope while
                    // collecting diagnostics from the rest of the traversal.
                    if entry.depth() > 0 {
                        errors.push(format!(
                            "skipping directory without a trusted root boundary: {}",
                            entry.path().display()
                        ));
                        entries.skip_current_dir();
                    }
                }
            }
            Ok(entry) if entry.file_type().is_file() && tag_parser::is_audio_file(entry.path()) => {
                audio_files.push(entry.into_path());
            }
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }

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

    RootScan {
        root,
        audio_files,
        errors,
        device_id,
        reconciliation_authoritative: false,
        content_authorized: false,
    }
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
    if !scan.is_complete() || scan.device_id.is_none() || scan.audio_files.is_empty() {
        return false;
    }

    if let Some(state) = previous {
        if state.identity_confirmed {
            return state.device_id.as_deref() == scan.device_id.as_deref();
        }
    }

    existing_track_count == 0
}

async fn persist_root_scan_status(
    db: &DatabaseConnection,
    scan: &RootScan,
    previous: Option<&library_root::Model>,
    confirms_identity: bool,
) -> anyhow::Result<()> {
    let was_confirmed = previous.is_some_and(|state| state.identity_confirmed);
    let recorded_device_id = if confirms_identity {
        scan.device_id.clone()
    } else if was_confirmed || !scan.is_complete() {
        previous.and_then(|state| state.device_id.clone())
    } else {
        // Keep the latest untrusted observation for diagnostics, but it never
        // authorizes deletion until a later scan explicitly confirms it.
        scan.device_id.clone()
    };
    let identity_confirmed = was_confirmed || confirms_identity;
    let is_available =
        scan.is_complete() && (scan.reconciliation_authoritative || confirms_identity);
    let last_checked_at = Utc::now().to_rfc3339();

    if let Some(state) = previous {
        let mut active: library_root::ActiveModel = state.clone().into();
        active.device_id = Set(recorded_device_id);
        active.identity_confirmed = Set(identity_confirmed);
        active.is_available = Set(is_available);
        active.last_scan_complete = Set(scan.is_complete());
        active.last_checked_at = Set(last_checked_at);
        active.update(db).await?;
    } else {
        library_root::ActiveModel {
            path: Set(scan.root.to_string_lossy().into_owned()),
            device_id: Set(recorded_device_id),
            identity_confirmed: Set(identity_confirmed),
            is_available: Set(is_available),
            last_scan_complete: Set(scan.is_complete()),
            last_checked_at: Set(last_checked_at),
        }
        .insert(db)
        .await?;
    }

    Ok(())
}

async fn initial_scan(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
) -> anyhow::Result<()> {
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
        let confirms_identity = scan_confirms_identity(scan, previous, existing_track_count);
        scan.reconciliation_authoritative = reconciliation_is_authoritative(scan, previous);

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

        // If availability state cannot be persisted, fail closed for this
        // scan: retaining stale metadata is safer than deleting it without a
        // durable device identity for the next startup.
        match persist_root_scan_status(db, scan, previous, confirms_identity).await {
            Ok(()) => {
                scan.content_authorized = scan.reconciliation_authoritative || confirms_identity;
            }
            Err(error) => {
                warn!(root = %scan.root.display(), %error, "Failed to persist library root state");
                scan.reconciliation_authoritative = false;
                scan.content_authorized = false;
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
            let p = path.clone();
            let parse_result =
                tokio::task::spawn_blocking(move || tag_parser::parse_audio_file(&p)).await;

            match parse_result {
                Ok(Ok(parsed)) => {
                    // During the initial scan we do NOT emit a TrackUpserted
                    // per file: the single FullSync below delivers the complete
                    // snapshot, avoiding O(n^2) UI work plus a full track-list
                    // clone per event. The watcher still emits TrackUpserted for
                    // incremental changes, where it is cheap.
                    if let Err(e) = upsert_track(db, &parsed, existing).await {
                        warn!(path = %path_str, error = %e, "Failed to upsert track");
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

    // Remove DB entries for files no longer on disk. Reuse the preloaded
    // snapshot instead of re-querying. A failed individual delete is logged and
    // skipped rather than aborting the whole scan, so a transient DB hiccup
    // can't discard the FullSync/ScanComplete that follow.
    for row in &existing_tracks {
        let row_path = Path::new(&row.file_path);
        if !should_remove_stale_track(row_path, &on_disk_paths, &root_scans) {
            continue;
        }
        info!(path = %row.file_path, "Removing stale track from database");
        if let Err(e) = track::Entity::delete_by_id(&row.id).exec(db).await {
            warn!(path = %row.file_path, error = %e, "Failed to remove stale track");
            continue;
        }
        let _ = tx
            .send(LibraryEvent::TrackRemoved(row.file_path.clone()))
            .await;
    }

    // Send full sync. A transient failure here is logged but still lets the
    // scan finish (reconcile + ScanComplete) so the UI settles into a synced
    // state instead of hanging on the spinner with no completion signal.
    match track::Entity::find().all(db).await {
        Ok(rows) => {
            let all_tracks: Vec<Track> = rows.iter().map(db_model_to_track).collect();
            let _ = tx.send(LibraryEvent::FullSync(all_tracks)).await;
        }
        Err(e) => warn!(error = %e, "Failed to load tracks for full sync"),
    }

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

    let _ = tx.send(LibraryEvent::ScanComplete).await;

    info!(scanned, "Initial scan complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Filesystem watcher
// ---------------------------------------------------------------------------

async fn persisted_root_for_path(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    path: &Path,
) -> anyhow::Result<Option<(PathBuf, library_root::Model)>> {
    let states = library_root::Entity::find().all(db).await?;
    Ok(states
        .into_iter()
        .filter_map(|state| {
            let root = PathBuf::from(&state.path);
            let is_configured_scope = music_dirs
                .iter()
                .any(|configured| root.starts_with(configured));
            (is_configured_scope && path.starts_with(&root)).then_some((root, state))
        })
        .max_by_key(|(root, _)| root.components().count()))
}

fn crosses_untracked_nested_mount(root: &Path, path: &Path, music_dirs: &[PathBuf]) -> bool {
    match mounted_subroots(music_dirs) {
        Ok(mountpoints) => mountpoints
            .into_iter()
            .filter(|mountpoint| path.starts_with(mountpoint))
            .max_by_key(|mountpoint| mountpoint.components().count())
            .is_some_and(|mountpoint| !root.starts_with(mountpoint)),
        Err(error) => {
            warn!(%error, "Could not inspect mounted library scopes; watcher event rejected");
            true
        }
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

async fn root_identity_allows_content(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    path: &Path,
) -> anyhow::Result<bool> {
    let Some((root, root_state)) = persisted_root_for_path(db, music_dirs, path).await? else {
        return Ok(false);
    };
    if crosses_untracked_nested_mount(&root, path, music_dirs) {
        return Ok(false);
    }
    if !root_state.identity_confirmed || !root_state.is_available || !root_state.last_scan_complete
    {
        return Ok(false);
    }
    let Some(expected_identity) = root_state.device_id.as_deref() else {
        return Ok(false);
    };

    let matches = filesystem_identity(&root).is_ok_and(|identity| identity == expected_identity);
    if !matches {
        mark_root_unavailable(db, &root_state).await;
    }
    Ok(matches)
}

/// Delete a watcher-reported missing path only while its confirmed root
/// identity remains stable across the database transaction.
async fn delete_track_if_root_stable(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    path: &Path,
) -> anyhow::Result<bool> {
    delete_track_if_root_stable_with_probe(db, music_dirs, path, filesystem_identity).await
}

async fn delete_track_if_root_stable_with_probe<F>(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    path: &Path,
    mut identity_probe: F,
) -> anyhow::Result<bool>
where
    F: FnMut(&Path) -> std::io::Result<String>,
{
    // Debounced remove events can outlive a quick remount or file recreate.
    // Never delete metadata when the path is present again, and fail closed
    // when existence itself cannot be determined.
    if !matches!(path.try_exists(), Ok(false)) {
        return Ok(false);
    }

    let Some((root, root_state)) = persisted_root_for_path(db, music_dirs, path).await? else {
        return Ok(false);
    };
    if crosses_untracked_nested_mount(&root, path, music_dirs) {
        return Ok(false);
    }
    if !root_state.identity_confirmed || !root_state.is_available || !root_state.last_scan_complete
    {
        return Ok(false);
    }
    let Some(expected_identity) = root_state.device_id.clone() else {
        return Ok(false);
    };

    if !identity_probe(&root).is_ok_and(|identity| identity == expected_identity) {
        mark_root_unavailable(db, &root_state).await;
        return Ok(false);
    }

    let transaction = db.begin().await?;
    let path_key = path.to_string_lossy().into_owned();
    let row = track::Entity::find()
        .filter(track::Column::FilePath.eq(&path_key))
        .one(&transaction)
        .await?;
    let Some(row) = row else {
        transaction.rollback().await?;
        return Ok(false);
    };
    track::Entity::delete_by_id(&row.id)
        .exec(&transaction)
        .await?;

    if !matches!(path.try_exists(), Ok(false))
        || !identity_probe(&root).is_ok_and(|identity| identity == expected_identity)
    {
        transaction.rollback().await?;
        mark_root_unavailable(db, &root_state).await;
        return Ok(false);
    }

    transaction.commit().await?;
    Ok(true)
}

async fn watch_directories(
    db: &Arc<DatabaseConnection>,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
) -> anyhow::Result<()> {
    let (notify_tx, mut notify_rx) = mpsc::channel::<notify::Result<notify::Event>>(256);

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = notify_tx.blocking_send(res);
        },
        notify::Config::default().with_poll_interval(Duration::from_secs(2)),
    )?;

    // Watch each directory independently. A missing or unwatchable directory
    // (e.g. a first-launch default that doesn't exist, or a folder that was
    // removed after being configured) is skipped with a warning rather than
    // aborting the whole watcher — so one bad path can't stop the others from
    // being watched, and it never surfaces as a hard scan error to the user.
    for dir in music_dirs {
        if !dir.is_dir() {
            warn!(dir = %dir.display(), "Library folder does not exist — skipping watch");
            continue;
        }
        if let Err(e) = watcher.watch(dir.as_ref(), RecursiveMode::Recursive) {
            warn!(dir = %dir.display(), error = %e, "Failed to watch directory — skipping");
            continue;
        }
        info!(dir = %dir.display(), "Watching directory");
    }
    info!("Filesystem watcher active");

    // Keep watcher alive by holding it in scope
    let _watcher = watcher;

    // ── Debounced event processing ──────────────────────────────
    // Collect filesystem events for a short window, deduplicate by
    // path, then process the batch. This collapses the 3-5 duplicate
    // Create/Modify events that Windows fires per file copy into a
    // single parse+upsert, and removes the old per-file 500ms sleep.
    const DEBOUNCE_MS: u64 = 1500;

    loop {
        // Wait for the first event.
        let first = notify_rx.recv().await;
        let Some(first) = first else { break };

        // Collect the first event + any more that arrive within the
        // debounce window into path sets.
        let mut upsert_paths: HashSet<PathBuf> = HashSet::new();
        let mut remove_paths: HashSet<PathBuf> = HashSet::new();

        let mut collect_event = |event: notify::Event| {
            use notify::event::{ModifyKind, RenameMode};
            use notify::EventKind;

            // Classify each path as a removal or an upsert. A rename is
            // delivered as Modify(Name(...)) rather than Remove: the "from"
            // side must be treated as a removal (the file is gone at the old
            // path) and the "to" side as an upsert. Routing a rename through
            // the generic Modify upsert arm would re-add the new path while
            // leaving the old path orphaned as a stale DB row.
            match event.kind {
                EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    for path in event.paths {
                        if !tag_parser::is_audio_file(&path) {
                            continue;
                        }
                        upsert_paths.remove(&path);
                        remove_paths.insert(path);
                    }
                }
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                    // RenameMode::Both packs [from, to] into event.paths.
                    let mut iter = event.paths.into_iter();
                    if let Some(from) = iter.next() {
                        if tag_parser::is_audio_file(&from) {
                            upsert_paths.remove(&from);
                            remove_paths.insert(from);
                        }
                    }
                    for to in iter {
                        if !tag_parser::is_audio_file(&to) {
                            continue;
                        }
                        remove_paths.remove(&to);
                        upsert_paths.insert(to);
                    }
                }
                EventKind::Create(_) | EventKind::Modify(_) => {
                    for path in event.paths {
                        if !tag_parser::is_audio_file(&path) {
                            continue;
                        }
                        remove_paths.remove(&path);
                        upsert_paths.insert(path);
                    }
                }
                _ => {}
            }
        };

        if let Ok(event) = first {
            collect_event(event);
        }

        // Drain any additional events that arrive within the debounce window.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(DEBOUNCE_MS);
        loop {
            match tokio::time::timeout_at(deadline, notify_rx.recv()).await {
                Ok(Some(Ok(event))) => collect_event(event),
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "Filesystem watcher error");
                }
                _ => break, // Timeout or channel closed
            }
        }

        // Process removals.
        for path in &remove_paths {
            let path_str = path.to_string_lossy().to_string();
            debug!(path = %path_str, "File removed (debounced)");
            match delete_track_if_root_stable(db.as_ref(), music_dirs, path).await {
                Ok(true) => {
                    let _ = tx.send(LibraryEvent::TrackRemoved(path_str)).await;
                }
                Ok(false) => {
                    warn!(path = %path.display(), "Ignored removal without a stable confirmed library root");
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "Failed to process watched removal safely");
                }
            }
        }

        // Process upserts concurrently.
        if !upsert_paths.is_empty() {
            debug!(count = upsert_paths.len(), "Processing debounced upserts");
            let paths: Vec<PathBuf> = upsert_paths.into_iter().collect();

            for path in paths {
                match root_identity_allows_content(db.as_ref(), music_dirs, &path).await {
                    Ok(true) => {}
                    Ok(false) => {
                        warn!(path = %path.display(), "Ignored change from an unconfirmed or changed library root");
                        continue;
                    }
                    Err(error) => {
                        warn!(path = %path.display(), %error, "Failed to verify watched library root");
                        continue;
                    }
                }
                if !path.exists() {
                    // Backstop: a debounced "upsert" whose file no longer
                    // exists is really a move/rename away (or a delete the
                    // watcher reported as an ambiguous Modify). Remove the
                    // stale DB row instead of leaving an orphan that fails to
                    // play.
                    let path_str = path.to_string_lossy().to_string();
                    if delete_track_if_root_stable(db.as_ref(), music_dirs, &path)
                        .await
                        .unwrap_or_else(|error| {
                            warn!(path = %path.display(), %error, "Failed to process missing watched path safely");
                            false
                        })
                    {
                        let _ = tx.send(LibraryEvent::TrackRemoved(path_str)).await;
                    }
                    continue;
                }
                let p = path.clone();
                match tokio::task::spawn_blocking(move || tag_parser::parse_audio_file(&p)).await {
                    Ok(Ok(parsed)) => {
                        // Parsing can take long enough for a removable/network
                        // volume to disappear or be replaced. Revalidate the
                        // root immediately before touching persisted metadata.
                        if !root_identity_allows_content(db.as_ref(), music_dirs, &path)
                            .await
                            .unwrap_or(false)
                        {
                            warn!(path = %path.display(), "Library root changed while parsing — upsert discarded");
                            continue;
                        }
                        let path_str = parsed.file_path.clone();
                        let existing = track::Entity::find()
                            .filter(track::Column::FilePath.eq(&path_str))
                            .one(db.as_ref())
                            .await
                            .ok()
                            .flatten();

                        match upsert_track(db.as_ref(), &parsed, existing.as_ref()).await {
                            Ok(model) => {
                                let t = db_model_to_track(&model);
                                let _ = tx.send(LibraryEvent::TrackUpserted(Box::new(t))).await;
                            }
                            Err(e) => {
                                warn!(error = %e, path = %path.display(), "Failed to upsert track");
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
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Database helpers
// ---------------------------------------------------------------------------

/// Insert or update a track in the database, returning the final Model.
async fn upsert_track(
    db: &DatabaseConnection,
    parsed: &ParsedTrack,
    existing: Option<&track::Model>,
) -> anyhow::Result<track::Model> {
    let now = Utc::now().to_rfc3339();
    let mtime = parsed.date_modified.to_rfc3339();

    if let Some(row) = existing {
        // Update existing
        let mut active: track::ActiveModel = row.clone().into();
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
        active.date_modified = Set(mtime);
        active.file_size_bytes = Set(parsed.file_size_bytes.map(|s| s as i64));

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
            device_id: Some("real-volume".to_string()),
            reconciliation_authoritative: false,
            content_authorized: false,
        };
        assert!(scan_confirms_identity(
            &mounted_volume,
            Some(&unconfirmed_mountpoint),
            0
        ));
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
        let first_scan = scan_root_with_probes_and_exclusions(
            directory.path().to_path_buf(),
            |_| Ok("stable-volume".to_string()),
            |_| Ok(41),
            &[],
        );
        let previous = persisted_root_state(&first_scan, first_scan.device_id.clone());
        let second_scan = scan_root_with_probes_and_exclusions(
            directory.path().to_path_buf(),
            |_| Ok("stable-volume".to_string()),
            |_| Ok(99),
            &[],
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
        let mut scan = scan_root(directory.path().to_path_buf());
        persist_root_scan_status(&db, &scan, None, true)
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
        persist_root_scan_status(&db, &scan, Some(&stored), false)
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
        use std::cell::Cell;

        use sea_orm::Database;
        use sea_orm_migration::MigratorTrait;

        use crate::db::migration::Migrator;

        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, None).await.expect("run migrations");

        let directory = TestDirectory::new("watcher-identity-race");
        let scan = scan_root(directory.path().to_path_buf());
        let expected_identity = scan.device_id.clone().expect("root identity");
        persist_root_scan_status(&db, &scan, None, true)
            .await
            .expect("persist confirmed root");

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
        std::fs::write(&removed_path, []).expect("recreate watched path");
        let stable_identity = expected_identity.clone();
        assert!(!delete_track_if_root_stable_with_probe(
            &db,
            &[directory.path().to_path_buf()],
            &removed_path,
            move |_| Ok(stable_identity.clone()),
        )
        .await
        .expect("ignore stale removal"));
        std::fs::remove_file(&removed_path).expect("remove recreated watched path");
        assert!(track::Entity::find_by_id("watcher-race-track")
            .one(&db)
            .await
            .expect("query recreated track")
            .is_some());

        let probe_calls = Cell::new(0);
        let first_identity = expected_identity.clone();
        let removed = delete_track_if_root_stable_with_probe(
            &db,
            &[directory.path().to_path_buf()],
            &removed_path,
            move |_| {
                let call = probe_calls.get();
                probe_calls.set(call + 1);
                Ok(if call == 0 {
                    first_identity.clone()
                } else {
                    "underlying-mountpoint".to_string()
                })
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
