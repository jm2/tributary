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
    QuerySelect, Set, Statement, TransactionTrait,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

use super::backend::LocalBackend;
use super::root_authority::{AbsenceProof, BoundDirectory, BoundFile, RootAuthorityLease};
use super::tag_parser::{self, ParsedTrack};
use super::tag_writer;
use crate::architecture::{
    backend::MediaBackend,
    models::{Rating, Track, TrackRating},
    TrackId,
};
use crate::db::entities::{library_root, playlist_entry, root_reauthorization_receipt, track};

use super::playlist_sidebar::{
    PlaylistSidebarRefresh, PlaylistSidebarRefreshReceiver, PlaylistSidebarRefreshRequest,
    PlaylistSidebarSnapshot,
};

/// Frozen namespace for projecting a legacy non-UUID SQLite track key into
/// compatibility APIs that have not yet migrated from `Uuid`.
///
/// Queue and playback identity never use this projection; they preserve the
/// exact database string. Changing these bytes would nevertheless destabilize
/// callers that still inspect `Track::id`, so treat them as data-format state.
const LOCAL_TRACK_COMPAT_NAMESPACE: Uuid =
    Uuid::from_u128(0xa607_efde_6d16_4f0b_b16c_f654_b2df_d7c8);

// ---------------------------------------------------------------------------
// LibraryEvent — messages sent to GTK main thread
// ---------------------------------------------------------------------------

/// Seed the default playlists if this database has never held one, then
/// always attempt one versioned publication through the engine-owned
/// publisher.
async fn seed_default_playlists_and_request(
    playlist_manager: &super::playlist_manager::PlaylistManager,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
) {
    if let Err(error) = playlist_manager.seed_defaults().await {
        warn!(%error, "Failed to seed default playlists");
    }

    if matches!(
        playlist_sidebar_refresh.request(),
        PlaylistSidebarRefreshRequest::Closed
    ) {
        warn!("Playlist sidebar publisher stopped before scan refresh");
    }
}

/// Events sent from the background engine to the GTK main thread.
#[derive(Debug, Clone)]
pub enum LibraryEvent {
    /// Complete library snapshot after initial scan.
    FullSync(Vec<Track>),
    /// A single track was added or updated.
    TrackUpserted(Box<Track>),
    /// A track was removed (by file_path).
    TrackRemoved(String),
    /// Scan progress: (files_scanned, total_files).
    ScanProgress(u64, u64),
    /// Initial scan complete.
    ScanComplete,
    /// Playlists and their authoritative editability/link presentation loaded
    /// from one joined database snapshot.
    PlaylistsLoaded(PlaylistSidebarSnapshot),
    /// Database-backed, content-redacted server-playlist controls are ready.
    /// The facade exposes only opaque browser tokens and local-link recovery;
    /// source/native playlist identity remains Tokio-owned.
    ServerPlaylistRuntimeReady(super::server_playlist_browser::ServerPlaylistUiRuntime),
    /// Persisted track changes and any resulting playlist reconciliation have
    /// settled, so active playlist projections should be loaded again.
    PlaylistProjectionsInvalidated,
    /// One local track's playback-history mutation committed durably. The
    /// boxed value is the row selected in the same transaction as the atomic
    /// increment, converted only after that transaction committed.
    PlaybackHistoryUpdated(Box<Track>),
    /// One local track's app-owned rating committed durably. Consumers must
    /// replace their published row from this value rather than mutating it
    /// optimistically when the command is admitted.
    TrackRatingUpdated(Box<Track>),
    /// A local rating write failed before commit. The storage error is logged
    /// internally but deliberately excluded from this UI-facing event so GTK
    /// can select fixed, localized copy without exposing database details.
    TrackRatingUpdateFailed { track_id: TrackId },
    /// Closed, content-free result of one previewed Rhythmbox migration. The
    /// detailed plan and database error never cross into GTK.
    RhythmboxMigrationFinished {
        request_id: Uuid,
        outcome: super::rhythmbox_migration::RhythmboxMigrationCompletion,
        summary: super::rhythmbox_migration::RhythmboxMigrationSummary,
    },
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
    /// Result of an explicit old-path to portal-path root reauthorization.
    /// Successful outcomes let GTK commit the matching write-ahead config
    /// intent; rejected requests deliberately leave the old configured path
    /// in place so the selected destination cannot mint duplicate track IDs.
    RootReauthorizationFinished {
        request_id: String,
        old_path: PathBuf,
        new_path: PathBuf,
        outcome: RootReauthorizationOutcome,
        message: Option<String>,
    },
    /// Whether each configured library root currently backs playable tracks,
    /// as the engine last established it. Sent after every scan and whenever
    /// the periodic availability probe sees a root go away.
    RootStatusChanged(Vec<LibraryRootStatus>),
    /// The library database could not be opened or upgraded, so the engine
    /// never started. The detailed error is only logged.
    DatabaseUnavailable(crate::db::connection::DatabaseInitFailure),
    /// An error occurred.
    Error(String),
}

/// One configured library root's availability as the engine established it.
///
/// `available` matches the playback resolver's rule: the root's identity is
/// confirmed, it is available, and its last scan completed. A root the engine
/// has never recorded is unavailable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryRootStatus {
    pub path: PathBuf,
    pub available: bool,
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

/// Public result of one startup root-reauthorization request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootReauthorizationOutcome {
    /// Track and root paths moved in one guarded database transaction.
    Applied,
    /// A matching durable receipt proved an earlier transaction committed.
    AlreadyApplied,
    /// Validation failed before a durable receipt was observed. The engine
    /// retains the old root as its only effective scan path.
    Rejected,
    /// Durability cannot be established, or a receipt no longer agrees with
    /// the database. Neither path is scanned because guessing could split one
    /// library into two IDs.
    Inconsistent,
}

impl RootReauthorizationOutcome {
    pub fn committed(self) -> bool {
        matches!(self, Self::Applied | Self::AlreadyApplied)
    }
}

/// Explicit write-ahead intent captured by Preferences and consumed exactly
/// once at startup before watcher installation or library scanning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootReauthorizationRequest {
    request_id: String,
    old_path: PathBuf,
    new_path: PathBuf,
}

impl RootReauthorizationRequest {
    pub fn new(request_id: impl ToString, old_path: PathBuf, new_path: PathBuf) -> Self {
        Self {
            request_id: request_id.to_string(),
            old_path,
            new_path,
        }
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn old_path(&self) -> &Path {
        &self.old_path
    }

    pub fn new_path(&self) -> &Path {
        &self.new_path
    }
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
#[derive(Debug)]
pub enum LibraryCommand {
    ConfirmRootTrust(RootTrustRequest),
    /// Durably count one accepted playback occurrence for an exact local
    /// source-native track identity.
    RecordPlaybackHistory {
        track_id: TrackId,
        counted_at_ms: i64,
    },
    /// Set or clear one app-owned rating for an exact local source-native
    /// track identity. Publication occurs only after the transaction commits.
    SetTrackRating {
        track_id: TrackId,
        rating: Option<Rating>,
    },
    /// Apply one opaque, exact-state Rhythmbox preview through the same FIFO
    /// as app-owned history and rating mutations.
    ApplyRhythmboxMigration(Box<super::rhythmbox_migration::RhythmboxMigrationRequest>),
    /// Re-probe the configured roots and run a full library scan. A request
    /// that arrives while the startup scan is still running is covered by it.
    Rescan,
    /// Acknowledge only after every command queued before this marker has
    /// finished. Normal application shutdown uses this FIFO barrier so an
    /// already-admitted playback-history or rating mutation cannot be lost
    /// while the initial scan or watcher owner is still busy.
    Flush {
        completion: async_channel::Sender<()>,
    },
}

// ---------------------------------------------------------------------------
// LibraryEngine
// ---------------------------------------------------------------------------

/// The background scanning and watching engine.
pub struct LibraryEngine {
    db: DatabaseConnection,
    music_dirs: Vec<PathBuf>,
    pending_root_reauthorizations: Vec<RootReauthorizationRequest>,
    /// Whether `music_dirs` is the user's saved folder list, so tracks under
    /// no configured folder may be forgotten at startup. False when the list
    /// is only a default, so a missing or unreadable config never discards
    /// a library.
    forget_unconfigured_tracks: bool,
    tx: async_channel::Sender<LibraryEvent>,
    command_rx: async_channel::Receiver<LibraryCommand>,
    services: LibraryEngineServices,
    /// Cancelled by the UI admission boundary when the window closes. It bounds
    /// how long the initial scan may keep the reserved `Flush` drain waiting.
    scan_cancellation: CancellationToken,
}

/// Lifecycle-owned services consumed together by one library engine run.
///
/// Grouping these channels and owner handles makes their shared teardown
/// ordering explicit at the construction boundary.
pub struct LibraryEngineServices {
    playlist_sidebar_refresh: PlaylistSidebarRefresh,
    playlist_sidebar_refresh_rx: PlaylistSidebarRefreshReceiver,
    server_playlist_coordinator:
        crate::server_playlist_coordinator::ServerPlaylistCoordinatorHandle,
    server_playlist_coordinator_shutdown:
        crate::server_playlist_coordinator::ServerPlaylistCoordinatorShutdown,
    source_registry: crate::source_registry::SourceRegistry,
    server_playlist_invalidations: tokio::sync::watch::Receiver<u64>,
}

impl LibraryEngineServices {
    pub fn new(
        playlist_sidebar_refresh: PlaylistSidebarRefresh,
        playlist_sidebar_refresh_rx: PlaylistSidebarRefreshReceiver,
        server_playlist_coordinator: crate::server_playlist_coordinator::ServerPlaylistCoordinatorHandle,
        server_playlist_coordinator_shutdown: crate::server_playlist_coordinator::ServerPlaylistCoordinatorShutdown,
        source_registry: crate::source_registry::SourceRegistry,
        server_playlist_invalidations: tokio::sync::watch::Receiver<u64>,
    ) -> Self {
        Self {
            playlist_sidebar_refresh,
            playlist_sidebar_refresh_rx,
            server_playlist_coordinator,
            server_playlist_coordinator_shutdown,
            source_registry,
            server_playlist_invalidations,
        }
    }
}

impl LibraryEngine {
    /// Create a new engine. Does NOT start scanning yet.
    ///
    /// Accepts multiple music directories — all will be scanned and watched.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: DatabaseConnection,
        music_dirs: Vec<PathBuf>,
        pending_root_reauthorizations: Vec<RootReauthorizationRequest>,
        forget_unconfigured_tracks: bool,
        tx: async_channel::Sender<LibraryEvent>,
        command_rx: async_channel::Receiver<LibraryCommand>,
        services: LibraryEngineServices,
        scan_cancellation: CancellationToken,
    ) -> Self {
        Self {
            db,
            music_dirs,
            pending_root_reauthorizations,
            forget_unconfigured_tracks,
            tx,
            command_rx,
            services,
            scan_cancellation,
        }
    }

    /// Run the engine: initial scan across all directories, then continuous
    /// FS watching on each.
    pub async fn run(self) {
        let Self {
            db,
            music_dirs,
            pending_root_reauthorizations,
            forget_unconfigured_tracks,
            tx,
            command_rx,
            services,
            scan_cancellation,
        } = self;
        let LibraryEngineServices {
            playlist_sidebar_refresh,
            playlist_sidebar_refresh_rx,
            server_playlist_coordinator,
            server_playlist_coordinator_shutdown,
            source_registry,
            server_playlist_invalidations,
        } = services;
        let db = Arc::new(db);

        let server_playlist_operations =
            super::server_playlist_runtime::ServerPlaylistOperations::new(
                db.as_ref().clone(),
                server_playlist_coordinator.clone(),
                source_registry.clone(),
                playlist_sidebar_refresh.clone(),
            );
        let (server_playlist_browser, server_playlist_browser_rx) =
            super::server_playlist_browser::server_playlist_browser_channel();
        let server_playlist_browser_shutdown = CancellationToken::new();
        let server_playlist_browser_owner =
            tokio::spawn(super::server_playlist_browser::run_server_playlist_browser(
                server_playlist_browser_rx,
                db.as_ref().clone(),
                server_playlist_coordinator.clone(),
                source_registry,
                playlist_sidebar_refresh.clone(),
                server_playlist_browser_shutdown.clone(),
            ));
        let server_playlist_ui_runtime =
            super::server_playlist_browser::ServerPlaylistUiRuntime::new(
                server_playlist_operations.clone(),
                server_playlist_browser.clone(),
            );
        let _ = tx
            .send(LibraryEvent::ServerPlaylistRuntimeReady(
                server_playlist_ui_runtime,
            ))
            .await;
        let server_playlist_observer_shutdown = CancellationToken::new();
        let server_playlist_observer = tokio::spawn(
            super::server_playlist_runtime::run_server_playlist_reconnect_observer(
                server_playlist_operations,
                server_playlist_invalidations,
                server_playlist_observer_shutdown.clone(),
            ),
        );

        // This engine instance is the sole owner of both the database-backed
        // publisher and its event bridge. The refresh signal can be requested
        // before startup and coalesces while the database owner is busy.
        let (playlist_snapshot_tx, playlist_snapshot_rx) = async_channel::bounded(1);
        let playlist_publisher =
            tokio::spawn(super::playlist_sidebar::run_playlist_sidebar_publisher(
                db.as_ref().clone(),
                playlist_sidebar_refresh_rx,
                playlist_snapshot_tx,
            ));
        let playlist_event_tx = tx.clone();
        let playlist_bridge = tokio::spawn(async move {
            while let Ok(snapshot) = playlist_snapshot_rx.recv().await {
                if playlist_event_tx
                    .send(LibraryEvent::PlaylistsLoaded(snapshot))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // Resolve explicit old→new identity transfers before either path can
        // be watched or scanned. A rejected request keeps only the old path;
        // an inconsistent durable receipt removes both paths fail-closed.
        let configured_roots = music_dirs.clone();
        let music_dirs = resolve_pending_root_reauthorizations(
            db.as_ref(),
            music_dirs,
            &pending_root_reauthorizations,
            &tx,
        )
        .await;

        // A folder removed in Preferences is forgotten here, before the scan
        // publishes the library snapshot.
        if forget_unconfigured_tracks && admit_scan_mutation(&scan_cancellation) {
            match forget_tracks_outside_library_roots(
                db.as_ref(),
                &configured_roots,
                &music_dirs,
                &pending_root_reauthorizations,
            )
            .await
            {
                Ok(0) => {}
                Ok(forgotten) => {
                    info!(
                        forgotten,
                        "Forgot tracks outside every configured library folder"
                    );
                }
                Err(error) => {
                    warn!(%error, "Could not forget tracks outside the configured library folders");
                }
            }
        }

        // Commands are serviced *while* the watcher is installed and the scan
        // runs. Neither notify's recursive registration nor the scan's
        // read-only traversal/parsing can be cancelled while the window is
        // open, so awaiting them before this loop would let slow storage delay
        // every admitted rating/history edit until close. Both share one
        // engine task, so catalogue mutations stay serialized — except while
        // the scan has a write transaction open across an await point, when
        // the shared gate defers commands to the transaction boundary.
        // Report the last known root availability before the startup scan
        // refreshes it, so the folder view is right while the scan runs.
        publish_root_status(db.as_ref(), &music_dirs, &tx).await;

        let mut watcher = None;
        let mut watcher_error = None;
        let mut completed_commands = HashMap::new();
        let scan_write_txn = ScanWriteTxnGate::default();
        let startup = async {
            // Install before traversing so changes observed during the initial
            // scan are retained for replay after its snapshot is published.
            // Construction remains best-effort: a watcher backend failure must
            // not suppress the useful one-shot scan.
            let install_dirs = music_dirs.clone();
            let install =
                tokio::task::spawn_blocking(move || install_directory_watcher(&install_dirs));
            match await_readonly_blocking(&scan_cancellation, install).await {
                Some(Ok(Ok(installed))) => watcher = Some(installed),
                Some(Ok(Err(error))) => {
                    error!(%error, "Filesystem watcher could not be installed");
                    watcher_error = Some(error.to_string());
                }
                Some(Err(error)) => {
                    error!(%error, "Filesystem watcher installation task failed");
                    watcher_error = Some(error.to_string());
                }
                None => info!("Filesystem watcher installation abandoned at shutdown"),
            }

            for dir in &music_dirs {
                info!(dir = %dir.display(), "Starting initial library scan");
            }
            let scan_result = initial_scan_shutdown_aware(
                &db,
                &music_dirs,
                &tx,
                &playlist_sidebar_refresh,
                &scan_cancellation,
                &ScanDiscoveryHold::none(),
                &scan_write_txn,
            )
            .await;

            // A missing or temporarily unwatchable root can become available
            // while another root is being enumerated. Retain every successful
            // pre-scan registration, then retry only the gaps at the handoff so
            // a root the scan just indexed is never left unwatched until
            // restart.
            if let Some(mut installed) = watcher.take() {
                let retry_dirs = music_dirs.clone();
                let retry = tokio::task::spawn_blocking(move || {
                    installed.watch_available_directories(&retry_dirs);
                    installed
                });
                watcher = match await_readonly_blocking(&scan_cancellation, retry).await {
                    Some(Ok(installed)) => Some(installed),
                    Some(Err(error)) => {
                        error!(%error, "Filesystem watcher registration task failed");
                        None
                    }
                    None => None,
                };
            }
            scan_result
        };
        let scan_result = service_commands_while_scanning(
            startup,
            &scan_write_txn,
            &db,
            &music_dirs,
            &tx,
            &command_rx,
            &mut completed_commands,
            &playlist_sidebar_refresh,
        )
        .await;
        if let Err(e) = scan_result {
            error!(error = %e, "Initial scan failed");
            let _ = tx.send(LibraryEvent::Error(e.to_string())).await;
        }

        if let Some(error) = watcher_error {
            let _ = tx.send(LibraryEvent::Error(error)).await;
        }

        // ── Filesystem watcher (all directories) ─────────────────────
        if let Some(watcher) = watcher {
            if let Err(e) = process_directory_events(
                &db,
                &music_dirs,
                &tx,
                &command_rx,
                &mut completed_commands,
                watcher,
                &playlist_sidebar_refresh,
                &scan_cancellation,
            )
            .await
            {
                error!(error = %e, "Filesystem watcher failed");
                let _ = tx.send(LibraryEvent::Error(e.to_string())).await;
            }
        }

        // Keep serialized UI mutations usable even when watcher installation
        // failed or a previously installed watcher backend shut down. Commands
        // use the same engine owner in both modes.
        process_library_commands_without_watcher(
            db.as_ref(),
            &music_dirs,
            &tx,
            &command_rx,
            &mut completed_commands,
            &playlist_sidebar_refresh,
        )
        .await;

        server_playlist_browser.close();
        server_playlist_browser_shutdown.cancel();
        if let Err(error) = server_playlist_browser_owner.await {
            warn!(
                cancelled = error.is_cancelled(),
                panicked = error.is_panic(),
                "Server playlist browser owner task failed"
            );
        }
        server_playlist_observer_shutdown.cancel();
        if let Err(error) = server_playlist_observer.await {
            warn!(
                cancelled = error.is_cancelled(),
                panicked = error.is_panic(),
                "Server playlist reconnect observer task failed"
            );
        }
        server_playlist_coordinator.close();
        if let Err(error) = server_playlist_coordinator_shutdown.shutdown().await {
            warn!(%error, "Server playlist coordinator owner failed");
        }
        playlist_sidebar_refresh.close();
        if let Err(error) = playlist_publisher.await {
            warn!(%error, "Playlist sidebar publisher task failed");
        }
        if let Err(error) = playlist_bridge.await {
            warn!(%error, "Playlist sidebar event bridge task failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Explicit library-root reauthorization
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct PreparedRootReauthorization {
    old_state: Option<library_root::Model>,
    marker_identity: String,
    authority_lease: Arc<RootAuthorityLease>,
}

#[derive(Debug)]
enum RootReauthorizationResolution {
    UseNew(RootReauthorizationOutcome),
    KeepOld(String),
    ScanNeither(String),
}

fn path_has_only_absolute_normal_components(path: &Path) -> bool {
    if !path.is_absolute() || path.to_str().is_none() {
        return false;
    }

    path.components().all(|component| {
        matches!(
            component,
            std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::Normal(_)
        )
    })
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn path_is_descendant(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|relative| !relative.as_os_str().is_empty())
}

fn retarget_descendant_path(path: &Path, old_root: &Path, new_root: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(old_root).ok()?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(new_root.join(relative))
}

fn validate_root_reauthorization_request(
    configured_roots: &[PathBuf],
    requests: &[RootReauthorizationRequest],
    request: &RootReauthorizationRequest,
) -> anyhow::Result<()> {
    Uuid::parse_str(&request.request_id)
        .map_err(|_| anyhow::anyhow!("library root reauthorization request ID is malformed"))?;
    if !path_has_only_absolute_normal_components(&request.old_path)
        || !path_has_only_absolute_normal_components(&request.new_path)
    {
        return Err(anyhow::anyhow!(
            "library root reauthorization requires absolute UTF-8 paths without traversal components"
        ));
    }
    if paths_overlap(&request.old_path, &request.new_path) {
        return Err(anyhow::anyhow!(
            "library root reauthorization endpoints must be distinct and non-overlapping"
        ));
    }
    if configured_roots
        .iter()
        .filter(|root| *root == &request.old_path)
        .count()
        != 1
    {
        return Err(anyhow::anyhow!(
            "reauthorization source is not exactly one configured library root"
        ));
    }
    if configured_roots
        .iter()
        .any(|root| root == &request.new_path)
    {
        return Err(anyhow::anyhow!(
            "reauthorization destination is already configured"
        ));
    }
    if configured_roots.iter().any(|root| {
        root != &request.old_path
            && (paths_overlap(root, &request.old_path) || paths_overlap(root, &request.new_path))
    }) {
        return Err(anyhow::anyhow!(
            "reauthorization endpoint overlaps another configured library root"
        ));
    }

    for other in requests {
        if std::ptr::eq(other, request) {
            continue;
        }
        if other.request_id == request.request_id
            || paths_overlap(&other.old_path, &request.old_path)
            || paths_overlap(&other.old_path, &request.new_path)
            || paths_overlap(&other.new_path, &request.old_path)
            || paths_overlap(&other.new_path, &request.new_path)
        {
            return Err(anyhow::anyhow!(
                "library root reauthorization intents have duplicate or overlapping identities"
            ));
        }
    }

    Ok(())
}

fn replace_effective_root(effective_roots: &mut Vec<PathBuf>, old_root: &Path, new_root: &Path) {
    // A durable receipt takes precedence over any later manual config edit.
    // Remove every spelling that overlaps either endpoint, then install the
    // single receipt-backed destination even if OLD disappeared from config.
    effective_roots.retain(|root| !paths_overlap(root, old_root) && !paths_overlap(root, new_root));
    effective_roots.push(new_root.to_path_buf());
    effective_roots.sort_unstable();
    effective_roots.dedup();
}

fn remove_effective_endpoints(
    effective_roots: &mut Vec<PathBuf>,
    old_root: &Path,
    new_root: &Path,
) {
    effective_roots.retain(|root| !paths_overlap(root, old_root) && !paths_overlap(root, new_root));
}

async fn resolve_pending_root_reauthorizations(
    db: &DatabaseConnection,
    configured_roots: Vec<PathBuf>,
    requests: &[RootReauthorizationRequest],
    tx: &async_channel::Sender<LibraryEvent>,
) -> Vec<PathBuf> {
    let mut effective_roots = configured_roots.clone();

    for request in requests {
        let validation_error =
            validate_root_reauthorization_request(&configured_roots, requests, request)
                .err()
                .map(|error| error.to_string());
        let resolution =
            resolve_root_reauthorization_with_validation(db, request, validation_error).await;

        let (outcome, message) = match resolution {
            RootReauthorizationResolution::UseNew(outcome) => {
                replace_effective_root(&mut effective_roots, &request.old_path, &request.new_path);
                (outcome, None)
            }
            RootReauthorizationResolution::KeepOld(message) => {
                // Even malformed or manually edited config may already list
                // the selected destination or a nested/containing spelling.
                // Quarantine every overlapping endpoint on rejection;
                // otherwise the ordinary scan could mint a second set of IDs
                // while the remembered source rows remain at the old prefix.
                effective_roots.retain(|root| {
                    root == &request.old_path
                        || (!paths_overlap(root, &request.old_path)
                            && !paths_overlap(root, &request.new_path))
                });
                warn!(
                    request_id = %request.request_id,
                    old_root = %request.old_path.display(),
                    new_root = %request.new_path.display(),
                    %message,
                    "Library root reauthorization rejected; retaining old effective root"
                );
                (RootReauthorizationOutcome::Rejected, Some(message))
            }
            RootReauthorizationResolution::ScanNeither(message) => {
                remove_effective_endpoints(
                    &mut effective_roots,
                    &request.old_path,
                    &request.new_path,
                );
                error!(
                    request_id = %request.request_id,
                    old_root = %request.old_path.display(),
                    new_root = %request.new_path.display(),
                    %message,
                    "Library root reauthorization cannot be resolved safely; disabling both paths"
                );
                (RootReauthorizationOutcome::Inconsistent, Some(message))
            }
        };

        let _ = tx
            .send(LibraryEvent::RootReauthorizationFinished {
                request_id: request.request_id.clone(),
                old_path: request.old_path.clone(),
                new_path: request.new_path.clone(),
                outcome,
                message,
            })
            .await;
    }

    effective_roots
}

/// Delete the local tracks that lie under no library root, in one transaction.
///
/// Removing a folder in Preferences only edits the configuration, so its rows
/// are deleted at the next startup. Rows under a configured root, an
/// effective root, or either endpoint of a pending reauthorization are kept
/// whether or not that root is currently available, so an unmounted volume
/// or a relocation awaiting its receipt never loses metadata. A deleted row
/// takes its play count and rating with it, and its playlist entries become
/// unmatched (`local_track_id` is `ON DELETE SET NULL`), exactly as when a
/// scan removes a track that left the disk.
async fn forget_tracks_outside_library_roots(
    db: &DatabaseConnection,
    configured_roots: &[PathBuf],
    effective_roots: &[PathBuf],
    reauthorizations: &[RootReauthorizationRequest],
) -> anyhow::Result<usize> {
    let kept_roots: Vec<&Path> = configured_roots
        .iter()
        .chain(effective_roots)
        .map(PathBuf::as_path)
        .chain(
            reauthorizations
                .iter()
                .flat_map(|request| [request.old_path(), request.new_path()]),
        )
        .collect();
    let transaction = db.begin().await?;
    let forgotten: Vec<String> = track::Entity::find()
        .select_only()
        .column(track::Column::Id)
        .column(track::Column::FilePath)
        .into_tuple::<(String, String)>()
        .all(&transaction)
        .await?
        .into_iter()
        .filter(|(_, path)| {
            !kept_roots
                .iter()
                .any(|root| Path::new(path).starts_with(root))
        })
        .map(|(id, _)| id)
        .collect();
    // Bounded statements keep each delete below SQLite's parameter limit.
    for ids in forgotten.chunks(500) {
        track::Entity::delete_many()
            .filter(track::Column::Id.is_in(ids.iter().cloned()))
            .exec(&transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(forgotten.len())
}

async fn resolve_root_reauthorization(
    db: &DatabaseConnection,
    request: &RootReauthorizationRequest,
) -> RootReauthorizationResolution {
    resolve_root_reauthorization_with_validation(db, request, None).await
}

/// Resolve a startup intent with the durable receipt as the first authority.
/// Config is intentionally mutable and may still contain a stale or malformed
/// intent after the database transaction committed. Only an absent receipt
/// permits validation to decide whether OLD remains safe.
async fn resolve_root_reauthorization_with_validation(
    db: &DatabaseConnection,
    request: &RootReauthorizationRequest,
    validation_error: Option<String>,
) -> RootReauthorizationResolution {
    match root_reauthorization_receipt::Entity::find_by_id(request.request_id.clone())
        .one(db)
        .await
    {
        Ok(Some(receipt)) => {
            return match root_reauthorization_receipt_is_consistent(db, request, &receipt).await {
                Ok(true) => RootReauthorizationResolution::UseNew(
                    RootReauthorizationOutcome::AlreadyApplied,
                ),
                Ok(false) => RootReauthorizationResolution::ScanNeither(
                    "durable reauthorization receipt does not match current library state"
                        .to_string(),
                ),
                Err(error) => RootReauthorizationResolution::ScanNeither(format!(
                    "could not validate durable reauthorization receipt: {error}"
                )),
            };
        }
        Ok(None) => {
            if let Some(error) = validation_error {
                return if Uuid::parse_str(&request.request_id).is_err() {
                    RootReauthorizationResolution::ScanNeither(error)
                } else {
                    RootReauthorizationResolution::KeepOld(error)
                };
            }
        }
        Err(error) => {
            return RootReauthorizationResolution::ScanNeither(format!(
                "could not determine reauthorization durability: {error}"
            ));
        }
    }

    let prepared = match prepare_root_reauthorization(db, request).await {
        Ok(prepared) => prepared,
        Err(error) => return RootReauthorizationResolution::KeepOld(error.to_string()),
    };
    let marker_identity = prepared.marker_identity.clone();
    let authority_lease = prepared.authority_lease.clone();
    let outcome = relocate_library_root_rows(
        db,
        request,
        prepared.old_state.as_ref(),
        &marker_identity,
        move || async move {
            spawn_authority_probe(move || authority_lease.validate().is_ok())
                .await
                .unwrap_or(false)
        },
    )
    .await;

    match outcome {
        Ok(()) => RootReauthorizationResolution::UseNew(RootReauthorizationOutcome::Applied),
        Err(error) => classify_root_reauthorization_error(db, request, error).await,
    }
}

/// Resolve the transaction's durability boundary through the receipt written
/// in that same transaction. A database driver can report a COMMIT error even
/// when storage made the commit durable; scanning OLD in that case would split
/// identity. Only a successful no-receipt query proves that OLD remains safe.
async fn classify_root_reauthorization_error(
    db: &DatabaseConnection,
    request: &RootReauthorizationRequest,
    error: anyhow::Error,
) -> RootReauthorizationResolution {
    let original_error = error.to_string();
    match root_reauthorization_receipt::Entity::find_by_id(request.request_id.clone())
        .one(db)
        .await
    {
        Ok(Some(receipt)) => {
            match root_reauthorization_receipt_is_consistent(db, request, &receipt).await {
                Ok(true) => RootReauthorizationResolution::UseNew(
                    RootReauthorizationOutcome::AlreadyApplied,
                ),
                Ok(false) => RootReauthorizationResolution::ScanNeither(format!(
                    "reauthorization failed ({original_error}) and its durable receipt is inconsistent"
                )),
                Err(receipt_error) => RootReauthorizationResolution::ScanNeither(format!(
                    "reauthorization failed ({original_error}) and receipt validation failed: {receipt_error}"
                )),
            }
        }
        Ok(None) => RootReauthorizationResolution::KeepOld(original_error),
        Err(receipt_error) => RootReauthorizationResolution::ScanNeither(format!(
            "reauthorization failed ({original_error}) and commit durability could not be determined: {receipt_error}"
        )),
    }
}

async fn root_reauthorization_receipt_is_consistent(
    db: &DatabaseConnection,
    request: &RootReauthorizationRequest,
    receipt: &root_reauthorization_receipt::Model,
) -> anyhow::Result<bool> {
    let old_key = request.old_path.to_string_lossy();
    let new_key = request.new_path.to_string_lossy();
    if receipt.old_path != old_key
        || receipt.new_path != new_key
        || !is_marker_identity(&receipt.marker_identity)
    {
        return Ok(false);
    }

    let old_state = library_root::Entity::find_by_id(old_key.as_ref())
        .one(db)
        .await?;
    let new_state = library_root::Entity::find_by_id(new_key.as_ref())
        .one(db)
        .await?;
    if old_state.is_some()
        || !new_state.is_some_and(|state| {
            state.device_id.as_deref() == Some(receipt.marker_identity.as_str())
        })
    {
        return Ok(false);
    }

    let tracks = track::Entity::find().all(db).await?;
    if tracks
        .iter()
        .any(|row| path_is_descendant(Path::new(&row.file_path), &request.old_path))
    {
        return Ok(false);
    }
    Ok(true)
}

fn validate_reauthorization_database_scopes(
    roots: &[library_root::Model],
    request: &RootReauthorizationRequest,
    marker_identity: Option<&str>,
) -> anyhow::Result<()> {
    for state in roots {
        let path = Path::new(&state.path);
        if path == request.old_path {
            continue;
        }
        if path == request.new_path {
            return Err(anyhow::anyhow!(
                "reauthorization destination already has persisted root state"
            ));
        }
        if paths_overlap(path, &request.old_path) || paths_overlap(path, &request.new_path) {
            return Err(anyhow::anyhow!(
                "reauthorization endpoint overlaps another persisted library scope"
            ));
        }
        if marker_identity.is_some_and(|marker| state.device_id.as_deref() == Some(marker)) {
            return Err(anyhow::anyhow!(
                "reauthorization marker is already claimed by another library root"
            ));
        }
    }
    Ok(())
}

fn create_reauthorization_marker(scan: &RootScan) -> anyhow::Result<String> {
    let observed_identity = scan
        .device_id
        .as_deref()
        .filter(|identity| is_legacy_identity(identity))
        .ok_or_else(|| anyhow::anyhow!("reauthorization destination has no usable identity"))?;
    let observed_generation = scan.mount_generation.ok_or_else(|| {
        anyhow::anyhow!("reauthorization destination has no mount-generation evidence")
    })?;
    let current_identity = legacy_filesystem_identity(&scan.root)?;
    let current_generation = root_mount_generation(&scan.root)?;
    if current_identity != observed_identity || current_generation != observed_generation {
        return Err(anyhow::anyhow!(
            "reauthorization destination changed before marker creation"
        ));
    }
    if read_root_marker(&scan.root)?.is_some() {
        return Err(anyhow::anyhow!(
            "a marker appeared at the reauthorization destination"
        ));
    }

    let creation = create_root_marker(&scan.root)?;
    if !creation.created
        || legacy_filesystem_identity(&scan.root)? != current_identity
        || root_mount_generation(&scan.root)? != current_generation
        || read_root_marker(&scan.root)?.as_deref() != Some(creation.identity.as_str())
    {
        return Err(anyhow::anyhow!(
            "reauthorization destination changed while creating its marker"
        ));
    }
    Ok(creation.identity)
}

async fn prepare_root_reauthorization(
    db: &DatabaseConnection,
    request: &RootReauthorizationRequest,
) -> anyhow::Result<PreparedRootReauthorization> {
    let old_key = request.old_path.to_string_lossy().into_owned();
    let old_state = library_root::Entity::find_by_id(&old_key).one(db).await?;
    let roots = library_root::Entity::find().all(db).await?;
    validate_reauthorization_database_scopes(&roots, request, None)?;

    let tracks = track::Entity::find().all(db).await?;
    let remembered_tracks = tracks
        .iter()
        .filter(|row| path_is_descendant(Path::new(&row.file_path), &request.old_path))
        .count();
    if old_state.is_none() && remembered_tracks == 0 {
        return Err(anyhow::anyhow!(
            "reauthorization source has no persisted root or track identity to preserve"
        ));
    }

    let destination = request.new_path.clone();
    let mut scan = spawn_authority_probe(move || scan_root(destination))
        .await
        .map_err(|error| AuthorityTaskJoinFailure(error.to_string()))?;
    if !scan.is_complete() {
        return Err(anyhow::anyhow!(
            "reauthorization destination could not be scanned completely: {}",
            scan.errors.join("; ")
        ));
    }

    let confirmed_marker = old_state.as_ref().and_then(|state| {
        state
            .identity_confirmed
            .then_some(state.device_id.as_deref())
            .flatten()
            .filter(|identity| is_marker_identity(identity))
    });
    if old_state
        .as_ref()
        .is_some_and(|state| state.identity_confirmed)
        && confirmed_marker.is_none()
    {
        return Err(anyhow::anyhow!(
            "confirmed reauthorization source has no supported durable marker"
        ));
    }

    let marker_identity = match scan.device_id.as_deref() {
        Some(identity) if is_marker_identity(identity) => identity.to_string(),
        Some(identity) if is_legacy_identity(identity) && confirmed_marker.is_none() => {
            let marker_scan = scan;
            let marker_identity =
                spawn_authority_probe(move || create_reauthorization_marker(&marker_scan))
                    .await
                    .map_err(|error| AuthorityTaskJoinFailure(error.to_string()))??;
            let destination = request.new_path.clone();
            scan = spawn_authority_probe(move || scan_root(destination))
                .await
                .map_err(|error| AuthorityTaskJoinFailure(error.to_string()))?;
            if !scan.is_complete() || scan.device_id.as_deref() != Some(marker_identity.as_str()) {
                return Err(anyhow::anyhow!(
                    "reauthorization destination marker-backed rescan was incomplete"
                ));
            }
            marker_identity
        }
        Some(_) if confirmed_marker.is_some() => {
            return Err(anyhow::anyhow!(
                "reauthorization destination does not expose the confirmed root marker"
            ));
        }
        _ => {
            return Err(anyhow::anyhow!(
                "reauthorization destination has no supported filesystem identity"
            ));
        }
    };

    if confirmed_marker.is_some_and(|expected| expected != marker_identity) {
        return Err(anyhow::anyhow!(
            "reauthorization destination marker does not match the confirmed source"
        ));
    }
    validate_reauthorization_database_scopes(&roots, request, Some(&marker_identity))?;
    let authority_lease = scan.authority_lease.ok_or_else(|| {
        anyhow::anyhow!("reauthorization destination authority could not be retained")
    })?;

    Ok(PreparedRootReauthorization {
        old_state,
        marker_identity,
        authority_lease,
    })
}

async fn relocate_library_root_rows<F, Fut>(
    db: &DatabaseConnection,
    request: &RootReauthorizationRequest,
    expected_old_state: Option<&library_root::Model>,
    marker_identity: &str,
    commit_guard: F,
) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let transaction = crate::db::begin_write(db).await?;
    let result: anyhow::Result<()> = async {
        if root_reauthorization_receipt::Entity::find_by_id(request.request_id.clone())
            .one(&transaction)
            .await?
            .is_some()
        {
            return Err(anyhow::anyhow!(
                "reauthorization receipt appeared during a new relocation"
            ));
        }

        let old_key = request.old_path.to_string_lossy().into_owned();
        let new_key = request.new_path.to_string_lossy().into_owned();
        let current_old_state = library_root::Entity::find_by_id(&old_key)
            .one(&transaction)
            .await?;
        if current_old_state.as_ref() != expected_old_state {
            return Err(anyhow::anyhow!(
                "reauthorization source state changed before relocation"
            ));
        }
        if current_old_state.as_ref().is_some_and(|state| {
            state.identity_confirmed && state.device_id.as_deref() != Some(marker_identity)
        }) {
            return Err(anyhow::anyhow!(
                "confirmed reauthorization source marker changed"
            ));
        }

        let roots = library_root::Entity::find().all(&transaction).await?;
        validate_reauthorization_database_scopes(&roots, request, Some(marker_identity))?;

        let rows = track::Entity::find().all(&transaction).await?;
        if rows
            .iter()
            .any(|row| path_is_descendant(Path::new(&row.file_path), &request.new_path))
        {
            return Err(anyhow::anyhow!(
                "reauthorization destination already owns indexed tracks"
            ));
        }

        let mut moves = Vec::new();
        let mut destinations = HashSet::new();
        for row in &rows {
            let source_path = Path::new(&row.file_path);
            if !path_is_descendant(source_path, &request.old_path) {
                continue;
            }
            let destination =
                retarget_descendant_path(source_path, &request.old_path, &request.new_path)
                    .ok_or_else(|| {
                        anyhow::anyhow!("reauthorization source contains an unsafe track path")
                    })?;
            let destination = destination.to_str().ok_or_else(|| {
                anyhow::anyhow!("reauthorization produced a non-UTF-8 track path")
            })?;
            if !destinations.insert(destination.to_string()) {
                return Err(anyhow::anyhow!(
                    "reauthorization track paths collide at the destination"
                ));
            }
            moves.push((row.clone(), destination.to_string()));
        }
        if moves.is_empty() && current_old_state.is_none() {
            return Err(anyhow::anyhow!(
                "reauthorization source identity disappeared before relocation"
            ));
        }
        if rows.iter().any(|row| {
            destinations.contains(&row.file_path)
                && !path_is_descendant(Path::new(&row.file_path), &request.old_path)
        }) {
            return Err(anyhow::anyhow!(
                "reauthorization would overwrite an existing destination track"
            ));
        }

        for (row, destination) in moves {
            let mut active: track::ActiveModel = row.into();
            active.file_path = Set(destination);
            active.update(&transaction).await?;
        }

        let entries = playlist_entry::Entity::find()
            .filter(
                playlist_entry::Column::SourceId
                    .eq(crate::architecture::SourceId::local().to_string()),
            )
            .all(&transaction)
            .await?;
        for entry in entries {
            let Some(source_path) = entry.match_file_path.as_deref() else {
                continue;
            };
            let source_path = Path::new(source_path);
            if !path_is_descendant(source_path, &request.old_path) {
                continue;
            }
            let destination =
                retarget_descendant_path(source_path, &request.old_path, &request.new_path)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "reauthorization source contains an unsafe playlist match path"
                        )
                    })?;
            let destination = destination.to_str().ok_or_else(|| {
                anyhow::anyhow!("reauthorization produced a non-UTF-8 playlist match path")
            })?;
            let mut active: playlist_entry::ActiveModel = entry.into();
            active.match_file_path = Set(Some(destination.to_string()));
            active.update(&transaction).await?;
        }

        let identity_confirmed = current_old_state.as_ref().is_some_and(|state| {
            state.identity_confirmed && state.device_id.as_deref() == Some(marker_identity)
        });
        library_root::ActiveModel {
            path: Set(new_key.clone()),
            device_id: Set(Some(marker_identity.to_string())),
            identity_confirmed: Set(identity_confirmed),
            is_available: Set(false),
            last_scan_complete: Set(false),
            last_checked_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(&transaction)
        .await?;
        if current_old_state.is_some() {
            library_root::Entity::delete_by_id(&old_key)
                .exec(&transaction)
                .await?;
        }

        root_reauthorization_receipt::ActiveModel {
            request_id: Set(request.request_id.clone()),
            old_path: Set(old_key),
            new_path: Set(new_key),
            marker_identity: Set(marker_identity.to_string()),
            completed_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(&transaction)
        .await?;

        if !commit_guard().await {
            return Err(anyhow::anyhow!(
                "reauthorization destination changed before commit"
            ));
        }

        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            transaction.commit().await?;
            Ok(())
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
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
    /// Each discovered audio file with its RFC 3339 mtime, read by the
    /// traversal worker so the scan loop never stats files on the engine task.
    audio_files: Vec<(PathBuf, String)>,
    /// Private siblings an interrupted tag save left behind.
    tag_write_debris: Vec<PathBuf>,
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

    /// Record that a shutdown cancelled this root's scan.
    ///
    /// The cancellation is treated as a traversal error so every downstream
    /// completeness check (`is_complete`, reconciliation authority, stale
    /// deletion) fails closed: a cancelled scan never deletes catalogue rows.
    fn mark_cancelled(&mut self, reason: &str) {
        self.errors.push(format!("scan cancelled: {reason}"));
        self.reconciliation_authoritative = false;
        self.content_authorized = false;
    }
}

/// Fail every root in this scan closed when shutdown interrupts it, so no
/// later completeness check can authorize a durable mutation or deletion.
fn mark_scan_cancelled(root_scans: &mut [RootScan], reason: &str) {
    for scan in root_scans {
        scan.mark_cancelled(reason);
    }
}

/// Run one retained-authority filesystem probe outside Tokio's async worker
/// threads. Library roots may live on removable or network filesystems, so
/// even a small handle validation can block indefinitely at the OS boundary.
fn spawn_authority_probe<F, T>(probe: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(probe)
}

/// Shutdown latency budget for read-only blocking scan work.
///
/// `tokio::task::spawn_blocking` cannot cancel a kernel call that has already
/// entered `readdir`/`open`/`read`. When window close cancels the initial scan
/// we therefore give the in-flight read-only traversal/parser this long to
/// return on its own. If it does not, the join handle is dropped and the worker
/// is left to finish detached. That is safe precisely because the abandoned
/// closure only reads: it cannot mutate durable catalogue state, and any result
/// it later produces is discarded.
///
/// Durable mutations (track upserts, stale deletes, root-status persists) are
/// never subject to this budget — they are awaited to settlement so the FIFO
/// `Flush` barrier cannot acknowledge work that did not commit.
const SCAN_READONLY_SETTLE_BUDGET: Duration = Duration::from_millis(2_000);

/// Await a read-only blocking scan job under the shutdown isolation contract.
///
/// Without cancellation this is a plain `await`. Once cancellation is observed
/// (either before the call or while the job runs), the wait is bounded by
/// [`SCAN_READONLY_SETTLE_BUDGET`]; `None` means the read-only worker outlived
/// the budget and was intentionally abandoned. Callers must treat `None` as an
/// incomplete observation: skip the mutation that depended on it and preserve
/// the no-deletion authority semantics.
async fn await_readonly_blocking<T>(
    cancellation: &CancellationToken,
    job: tokio::task::JoinHandle<T>,
) -> Option<Result<T, tokio::task::JoinError>> {
    if cancellation.is_cancelled() {
        return tokio::time::timeout(SCAN_READONLY_SETTLE_BUDGET, job)
            .await
            .ok();
    }

    tokio::pin!(job);
    tokio::select! {
        result = &mut job => Some(result),
        () = cancellation.cancelled() => {
            tokio::time::timeout(SCAN_READONLY_SETTLE_BUDGET, &mut job)
                .await
                .ok()
        }
    }
}

/// Durable-mutation admission boundary for the initial scan.
///
/// `true` means a durable catalogue mutation may *begin*. Once it has begun it
/// must be awaited to settlement — it is never dropped or cancelled — so the
/// reserved `Flush` drain cannot acknowledge work that did not commit. `false`
/// means shutdown has been observed and no new durable work may start.
///
/// The read-only parser deliberately settles inside its grace after
/// cancellation (a `spawn_blocking` kernel call cannot be interrupted). That
/// makes this explicit boundary the only thing standing between a post-cancel
/// parser completion and a brand-new upsert plus unbounded authority probe.
fn admit_scan_mutation(cancellation: &CancellationToken) -> bool {
    !cancellation.is_cancelled()
}

/// Deterministic rendezvous for the initial scan's read-only discovery.
///
/// Production always constructs [`ScanDiscoveryHold::none`]. Tests install a
/// controlling handle so they can hold a specific discovery stage open and
/// observe how cancellation, command service, and the durable-mutation
/// admission boundary behave without depending on filesystem timing. The hold
/// is a plain `Option`, so production carries no shared synchronization state.
#[derive(Clone, Default)]
struct ScanDiscoveryHold {
    inner: Option<std::sync::Arc<ScanDiscoveryHoldInner>>,
}

/// A stage of the initial scan a [`ScanDiscoveryHold`] can hold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanDiscoveryStage {
    /// Before the first read-only traversal is submitted.
    Traversal,
    /// After a file's read-only parse settles and before any durable mutation.
    PostParse,
    /// At the per-root status mutation, inside the scan write-transaction
    /// gate span (`RootScan` persist). Regression seam for the root-status
    /// admission boundary and the write-transaction command-service gate.
    RootStatus,
    /// Inside a track upsert's open write transaction, at the commit-guard
    /// await. Regression seam for the suspended-mid-transaction state the
    /// command-service selector must not switch branches at.
    CommitGuard,
    /// Parked at the root-status boundary's command-settlement wait, before
    /// the write transaction opens. Regression seam for the post-settlement
    /// admission re-check (PR #286 round-4 finding j9j81). Signalled, not
    /// held: the scan parks itself at the reciprocal settlement wait
    /// immediately after, so holding here would deadlock every scan
    /// regression that does not release this stage.
    CommandSettlement,
}

#[derive(Default)]
struct ScanDiscoveryHoldInner {
    traversal: DiscoveryRendezvous,
    post_parse: DiscoveryRendezvous,
    root_status: DiscoveryRendezvous,
    commit_guard: DiscoveryRendezvous,
    command_settlement: DiscoveryRendezvous,
}

#[derive(Default)]
struct DiscoveryRendezvous {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
    engaged: std::sync::atomic::AtomicBool,
}

impl ScanDiscoveryHoldInner {
    fn stage(&self, stage: ScanDiscoveryStage) -> &DiscoveryRendezvous {
        match stage {
            ScanDiscoveryStage::Traversal => &self.traversal,
            ScanDiscoveryStage::PostParse => &self.post_parse,
            ScanDiscoveryStage::RootStatus => &self.root_status,
            ScanDiscoveryStage::CommitGuard => &self.commit_guard,
            ScanDiscoveryStage::CommandSettlement => &self.command_settlement,
        }
    }
}

impl ScanDiscoveryHold {
    fn none() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn controlling() -> (Self, ScanDiscoveryControl) {
        let inner = std::sync::Arc::new(ScanDiscoveryHoldInner::default());
        (
            Self {
                inner: Some(inner.clone()),
            },
            ScanDiscoveryControl { inner },
        )
    }

    /// Hold this stage open the first time the scan reaches it.
    ///
    /// The `engaged` swap makes the hold one-shot: only the first visit to a
    /// stage blocks, so multi-file fixtures still make progress after release.
    async fn arrive(&self, stage: ScanDiscoveryStage) {
        let Some(inner) = &self.inner else {
            return;
        };
        let rendezvous = inner.stage(stage);
        if rendezvous
            .engaged
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        rendezvous.reached.notify_one();
        rendezvous.release.notified().await;
    }

    /// Signal `stage` to a test-side waiter without holding the scan.
    ///
    /// The settlement-wait seam needs only an arrival notification: the scan
    /// parks itself at the very next await (the reciprocal command-settlement
    /// wait) whenever command work is actually in flight, so holding here
    /// would deadlock every regression that never releases this stage.
    /// One-shot `engaged` bookkeeping does not apply — the boundary fires
    /// once per root and repeated notifications to a waiting test are
    /// harmless.
    fn signal(&self, stage: ScanDiscoveryStage) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.stage(stage).reached.notify_one();
    }
}

/// Shared flags between the initial-scan driver and its command selector.
///
/// `open` is held whenever the scan has a SQLite write transaction open across
/// an await point (per-root status persist, track upsert, stale-row delete,
/// and their retained-authority probes). While it is set, the selector's
/// `select!` disables the command branch entirely and keeps polling the scan
/// until the transaction settles: servicing a library command meanwhile would
/// queue its write behind the open transaction and fail at the production
/// five-second busy timeout (PR #286 finding jq5lG).
///
/// `command_in_flight` is the reciprocal invariant (PR #286 round-3 finding
/// cid 4051684281): it is held while *dispatched* command work is still
/// settling inside the selector's interleave. The scan's write boundaries
/// consult it just before opening a write transaction and park THERE — still
/// polled, never holding a connection — until the work settles. Without it,
/// the scan could cross a write boundary after a command was dispatched, park
/// across a retained-authority probe, and hold the writer while the command's
/// own DB write queued at the same five-second busy timeout.
///
/// The scan and the command selector share one engine task, so both flags can
/// only change while the selector is polling the scan.
#[derive(Clone, Default)]
struct ScanWriteTxnGate {
    open: std::sync::Arc<std::sync::atomic::AtomicBool>,
    command_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Wakes scan write boundaries parked on `command_in_flight` when the
    /// in-flight work settles.
    work_settled: std::sync::Arc<tokio::sync::Notify>,
}

impl ScanWriteTxnGate {
    fn is_open(&self) -> bool {
        self.open.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// RAII marker for the open span of a scan write transaction.
///
/// Constructed immediately before the transaction begins and dropped once the
/// whole mutation settles (commit, rollback, or abandonment handling), so an
/// early return or panic cannot leave the gate open and stall command service.
struct ScanWriteTxnGuard<'a> {
    gate: &'a ScanWriteTxnGate,
}

impl<'a> ScanWriteTxnGuard<'a> {
    fn open(gate: &'a ScanWriteTxnGate) -> Self {
        gate.open.store(true, std::sync::atomic::Ordering::Release);
        Self { gate }
    }
}

impl Drop for ScanWriteTxnGuard<'_> {
    fn drop(&mut self) {
        self.gate
            .open
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// RAII arm of the command-in-flight invariant for one dispatched command.
///
/// Armed by the selector around the inner `work`/`scan` interleave and held
/// until that work settles — including when the interleave is abandoned
/// because the scan itself settled first. Clearing the flag also wakes every
/// scan write boundary parked on it.
struct CommandInFlightGuard<'a> {
    gate: &'a ScanWriteTxnGate,
}

impl<'a> CommandInFlightGuard<'a> {
    fn arm(gate: &'a ScanWriteTxnGate) -> Self {
        gate.command_in_flight
            .store(true, std::sync::atomic::Ordering::Release);
        Self { gate }
    }
}

impl Drop for CommandInFlightGuard<'_> {
    fn drop(&mut self) {
        self.gate
            .command_in_flight
            .store(false, std::sync::atomic::Ordering::Release);
        self.gate.work_settled.notify_waiters();
    }
}

/// Parks the scan AT a write boundary until dispatched command work settles.
///
/// Called immediately before a [`ScanWriteTxnGuard::open`] so the scan's
/// write transaction can never overlap in-flight command work: the scan waits
/// here — holding no connection, still polled by the selector's interleave —
/// rather than parking inside the open transaction. The flag/waker pair is
/// race-free for a waiter created inside this poll: the flag is re-read after
/// the waker registration, and the guard stores the flag before notifying.
async fn wait_for_command_settlement(gate: &ScanWriteTxnGate) {
    if !gate
        .command_in_flight
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    std::future::poll_fn(|cx| loop {
        use std::future::Future as _;
        if !gate
            .command_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return std::task::Poll::Ready(());
        }
        let notified = gate.work_settled.notified();
        tokio::pin!(notified);
        if notified.as_mut().poll(cx).is_ready() {
            // A settlement notification fired before this waiter registered;
            // loop and re-read the flag.
            continue;
        }
        if !gate
            .command_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return std::task::Poll::Ready(());
        }
        return std::task::Poll::Pending;
    })
    .await;
}

/// Command receive that goes quiet while a scan write transaction is open.
///
/// While the [`ScanWriteTxnGate`] reports an open transaction, this future
/// stays pending without touching the command channel: the very scan holding
/// the transaction is what re-polls the selecting `select!` (biased, scan
/// branch first), so the gate is re-checked after every step of scan progress
/// and the first queued command is received as soon as the transaction
/// settles. A command arriving mid-transaction is therefore deferred to the
/// transaction boundary — never raced into the open SQLite write, where its
/// own connection acquisition would queue behind the scan's lock and fail at
/// the busy timeout (PR #286 finding jq5lG).
///
/// The channel receive future is created once and retained across polls.
/// `async_channel::Recv` only keeps its channel listener alive while the
/// future lives, so constructing it fresh inside `poll` would unregister the
/// listener every time the poll returned `Pending`: a command sent while the
/// scan branch was parked could not wake this branch at all, and service
/// would stall until the scan's next own wake (PR #286 thread jq0TgN).
struct GatedCommandRecv<'a> {
    // `+ Send`: the engine run future is spawned on the multi-thread GTK
    // bridge runtime (src/ui/window.rs), so every future it composes must
    // stay `Send`.
    recv: std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<LibraryCommand, async_channel::RecvError>>
                + Send
                + 'a,
        >,
    >,
    gate: &'a ScanWriteTxnGate,
}

impl<'a> GatedCommandRecv<'a> {
    fn new(rx: &'a async_channel::Receiver<LibraryCommand>, gate: &'a ScanWriteTxnGate) -> Self {
        Self {
            recv: Box::pin(rx.recv()),
            gate,
        }
    }
}

impl std::future::Future for GatedCommandRecv<'_> {
    type Output = Result<LibraryCommand, async_channel::RecvError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if self.gate.is_open() {
            // No channel waker is registered while the transaction is open,
            // which is what keeps command service exclusive of it. This is
            // safe because the gate only ever closes while the scan is being
            // polled: if the gate is open here, the scan branch has already
            // been polled pending in this same select! poll and its waker
            // will re-poll this branch. A listener registered by an earlier
            // gate-closed poll may still fire while the gate is open; that
            // only produces a spurious wake-and-repark, never a lost or
            // early-received command.
            return std::task::Poll::Pending;
        }
        self.recv.as_mut().poll(cx)
    }
}

/// Test-side control for a [`ScanDiscoveryHold`].
#[cfg(test)]
struct ScanDiscoveryControl {
    inner: std::sync::Arc<ScanDiscoveryHoldInner>,
}

#[cfg(test)]
impl ScanDiscoveryControl {
    /// Wait until the scan reaches `stage`, leaving it held.
    async fn wait_until_reached(&self, stage: ScanDiscoveryStage) {
        self.inner.stage(stage).reached.notified().await;
    }

    /// Release a scan held at `stage`.
    fn release(&self, stage: ScanDiscoveryStage) {
        self.inner.stage(stage).release.notify_one();
    }
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

/// Split persisted roots into those still in force and nested scopes a former
/// mount left behind: a scope strictly inside a configured root that is no
/// longer mounted and holds no track rows would otherwise keep its folder out
/// of the enclosing root forever. A scope that still holds rows stays, so its
/// rows keep their own availability while the mount is away.
fn partition_stale_mount_scopes(
    configured_roots: &[PathBuf],
    persisted_roots: Vec<library_root::Model>,
    mounted_roots: &[PathBuf],
    tracks: &[track::Model],
) -> (Vec<library_root::Model>, Vec<library_root::Model>) {
    persisted_roots.into_iter().partition(|state| {
        let scope = Path::new(&state.path);
        let nested = configured_roots
            .iter()
            .any(|root| scope != root && scope.starts_with(root));
        !nested
            || configured_roots.iter().any(|root| root == scope)
            || mounted_roots.iter().any(|mounted| mounted == scope)
            || tracks
                .iter()
                .any(|track| Path::new(&track.file_path).starts_with(scope))
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
            tag_write_debris: Vec::new(),
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
                tag_write_debris: Vec::new(),
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
                        tag_write_debris: Vec::new(),
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
                tag_write_debris: Vec::new(),
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
                tag_write_debris: Vec::new(),
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

    let (audio_files, tag_write_debris, traversal_errors) =
        enumerate_audio_files(&root, root_boundary, exclusions);
    errors.extend(traversal_errors);
    let audio_files = audio_files
        .into_iter()
        .map(|path| {
            let mtime = get_mtime(&path);
            (path, mtime)
        })
        .collect();

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
        tag_write_debris,
        errors,
        device_id,
        mount_generation: Some(mount_generation),
        authority_lease,
        reconciliation_authoritative: false,
        content_authorized: false,
    }
}

/// Enumerate the audio files under `directory` using the one indexing policy
/// every scope shares, together with any private tag-write siblings.
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
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<String>) {
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
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<String>)
where
    F: FnMut(&Path) -> Result<(), String>,
{
    let mut audio_files = Vec::new();
    let mut private_siblings = Vec::new();
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
            Ok(entry) if entry.file_type().is_file() && is_private_write_sibling(entry.path()) => {
                private_siblings.push(entry.into_path());
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

    (audio_files, private_siblings, errors)
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
    let (audio_files, _, mut errors) =
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

fn collect_audio_files(root_scans: &[RootScan]) -> Vec<(PathBuf, String)> {
    let mut audio_files: Vec<(PathBuf, String)> = root_scans
        .iter()
        .filter(|scan| scan.content_authorized)
        .flat_map(|scan| scan.audio_files.iter().cloned())
        .collect();

    // The same file is visible through every configured ancestor root. Scan
    // it once even if the user's configuration contains overlapping roots.
    audio_files.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    audio_files.dedup_by(|(left, _), (right, _)| left == right);
    audio_files
}

/// Whether `alias` opens the same file object as `file`: on a case-insensitive
/// filesystem, the old spelling of a case-only rename does.
fn path_names_same_file(alias: &Path, file: &File) -> bool {
    let Ok(alias_file) = File::open(alias) else {
        return false;
    };
    matches!(
        (
            super::root_authority::object_identity(&alias_file),
            super::root_authority::object_identity(file),
        ),
        (Ok(alias_identity), Ok(identity)) if alias_identity == identity
    )
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

/// Outcome of rechecking the most-specific authorized root for one pending
/// initial-scan write.
#[derive(Debug)]
enum RootRevalidation {
    /// The root still authorizes durable work for this path.
    Authorized,
    /// The root no longer authorizes work; the named root (if any) was
    /// invalidated and its unavailable state must be persisted.
    Rejected(Option<PathBuf>),
    /// Shutdown abandoned the read-only authority probe before it settled. The
    /// caller must not admit a durable mutation and must preserve the
    /// incomplete-scan/no-deletion semantics.
    Abandoned,
}

/// Recheck the most-specific authorized root for one pending initial-scan
/// write. A failed probe invalidates that root for every later file in this
/// scan; callers persist the returned root's unavailable state.
///
/// `cancellation` is `Some` only for the initial scan's **pre-admission**
/// call sites, where the probe is read-only discovery that must not outlive
/// the shutdown budget. It is `None` for guards that run *inside* an already
/// admitted durable mutation, which must settle rather than be abandoned.
async fn revalidate_scan_root_for_path(
    path: &Path,
    root_scans: &mut [RootScan],
    cancellation: Option<&CancellationToken>,
) -> Result<RootRevalidation, tokio::task::JoinError> {
    let Some(index) = root_scans
        .iter()
        .enumerate()
        .filter(|(_, scan)| path.starts_with(&scan.root))
        .max_by_key(|(_, scan)| scan.root.components().count())
        .map(|(index, _)| index)
    else {
        return Ok(RootRevalidation::Rejected(None));
    };
    if !root_scans[index].content_authorized {
        return Ok(RootRevalidation::Rejected(None));
    }
    let expected = root_scans[index]
        .device_id
        .as_deref()
        .filter(|expected| is_marker_identity(expected))
        .map(str::to_owned);
    let authority_lease = root_scans[index].authority_lease.clone();
    let matches = match (expected, authority_lease) {
        (Some(expected), Some(lease)) => {
            let probe = move || lease.expected_marker() == expected && lease.validate().is_ok();
            match cancellation {
                Some(cancellation) => {
                    match await_readonly_blocking(cancellation, tokio::task::spawn_blocking(probe))
                        .await
                    {
                        Some(result) => result?,
                        None => return Ok(RootRevalidation::Abandoned),
                    }
                }
                None => spawn_authority_probe(probe).await?,
            }
        }
        _ => false,
    };
    if matches {
        return Ok(RootRevalidation::Authorized);
    }

    let scan = &mut root_scans[index];
    scan.content_authorized = false;
    scan.reconciliation_authoritative = false;
    Ok(RootRevalidation::Rejected(Some(scan.root.clone())))
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

    if let Err(error) = RootAuthorityLease::check_root_retainable(&scan.root) {
        scan.errors.push(format!(
            "library root cannot be retained, so no durable identity was written {}: {error}",
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
    let transaction = crate::db::begin_write(db).await?;

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
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
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
        playlist_sidebar_refresh,
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
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
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
        playlist_sidebar_refresh,
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
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
) {
    // A legacy confirmation can create its random marker immediately before
    // a database failure. The marker is deliberately not treated as proof of
    // the earlier command: rescan it as new evidence and require a fresh
    // adoption request instead.
    if let Err(error) = initial_scan(db, music_dirs, tx, playlist_sidebar_refresh).await {
        warn!(%error, "Could not refresh library root trust evidence after a rejected command");
    }
}

async fn refresh_unavailable_root_trust_evidence(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
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
        playlist_sidebar_refresh,
    )
    .await
    {
        warn!(%error, "Could not refresh library root trust evidence after an unavailable command");
    }
}

/// Atomically increment one exact local track's durable playback history.
///
/// The write uses a single bound-parameter statement so a concurrent caller
/// can never lose an increment between a read and a write. SQLite stores the
/// application count as a non-null signed integer, so legacy negative values
/// are repaired to the first legitimate play and the public `u32` projection
/// is capped at the entity's `i32` ceiling. The updated row is selected while
/// the same write transaction is still held and is returned only after COMMIT.
async fn record_playback_history(
    db: &DatabaseConnection,
    track_id: &TrackId,
    counted_at_ms: i64,
) -> anyhow::Result<Option<Track>> {
    let transaction = crate::db::begin_write(db).await?;
    let update = transaction
        .execute_raw(Statement::from_sql_and_values(
            transaction.get_database_backend(),
            "UPDATE tracks
             SET play_count = CASE
                     WHEN play_count < 0 THEN 1
                     WHEN play_count < ? THEN play_count + 1
                     ELSE ?
                 END,
                 last_played_at_ms = CASE
                     WHEN last_played_at_ms IS NULL OR last_played_at_ms < ? THEN ?
                     ELSE last_played_at_ms
                 END
             WHERE id = ?",
            [
                i32::MAX.into(),
                i32::MAX.into(),
                counted_at_ms.into(),
                counted_at_ms.into(),
                track_id.as_str().into(),
            ],
        ))
        .await;

    let update = match update {
        Ok(update) => update,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error.into());
        }
    };

    if update.rows_affected() == 0 {
        transaction.commit().await?;
        return Ok(None);
    }
    if update.rows_affected() != 1 {
        let affected = update.rows_affected();
        let _ = transaction.rollback().await;
        anyhow::bail!("playback-history update for exact track ID affected {affected} rows");
    }

    let updated = match track::Entity::find_by_id(track_id.as_str())
        .one(&transaction)
        .await
    {
        Ok(Some(updated)) => updated,
        Ok(None) => {
            let _ = transaction.rollback().await;
            anyhow::bail!("playback-history row disappeared before commit");
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error.into());
        }
    };

    transaction.commit().await?;
    Ok(Some(db_model_to_track(&updated)))
}

async fn process_library_command(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
    command: LibraryCommand,
) -> Option<PendingRootTrustScan> {
    let request = match command {
        LibraryCommand::ConfirmRootTrust(request) => request,
        LibraryCommand::RecordPlaybackHistory {
            track_id,
            counted_at_ms,
        } => {
            match record_playback_history(db, &track_id, counted_at_ms).await {
                Ok(Some(track)) => {
                    let _ = tx
                        .send(LibraryEvent::PlaybackHistoryUpdated(Box::new(track)))
                        .await;
                }
                Ok(None) => {
                    debug!(
                        ?track_id,
                        "Ignored playback history for a missing local track"
                    );
                }
                Err(error) => {
                    warn!(?track_id, %error, "Failed to record local playback history");
                }
            }
            return None;
        }
        LibraryCommand::SetTrackRating { track_id, rating } => {
            let backend = LocalBackend::new(db.clone());
            match backend.set_track_rating(&track_id, rating).await {
                Ok(Some(track)) => {
                    let _ = tx
                        .send(LibraryEvent::TrackRatingUpdated(Box::new(track)))
                        .await;
                }
                Ok(None) => {
                    debug!(?track_id, "Ignored rating update for a missing local track");
                }
                Err(error) => {
                    warn!(?track_id, %error, "Failed to update local track rating");
                    let _ = tx
                        .send(LibraryEvent::TrackRatingUpdateFailed { track_id })
                        .await;
                }
            }
            return None;
        }
        LibraryCommand::ApplyRhythmboxMigration(request) => {
            use super::rhythmbox_migration::{
                RhythmboxMigrationCompletion, RhythmboxMigrationError, RhythmboxMigrationOutcome,
            };

            let request_id = request.request_id();
            let summary = request.summary().clone();
            let outcome =
                match super::rhythmbox_migration::apply_rhythmbox_migration(db, &request).await {
                    Ok(RhythmboxMigrationOutcome::Applied) => {
                        info!(
                            %request_id,
                            matched_tracks = summary.matched_tracks,
                            playlists = summary
                                .static_playlists_to_create
                                .saturating_add(summary.automatic_playlists_to_create),
                            "Rhythmbox migration committed"
                        );
                        let snapshot = send_library_snapshot(db, tx).await;
                        let projection_event_published = tx
                            .send(LibraryEvent::PlaylistProjectionsInvalidated)
                            .await
                            .is_ok();
                        let sidebar_refresh_requested = !matches!(
                            playlist_sidebar_refresh.request(),
                            PlaylistSidebarRefreshRequest::Closed
                        );
                        let publication_incomplete =
                            !matches!(snapshot, LibrarySnapshotPublication::Published)
                                || !projection_event_published
                                || !sidebar_refresh_requested;
                        if publication_incomplete {
                            warn!(
                                %request_id,
                                snapshot = snapshot.category(),
                                projection_event = if projection_event_published {
                                    "published"
                                } else {
                                    "receiver-closed"
                                },
                                sidebar_refresh = if sidebar_refresh_requested {
                                    "requested"
                                } else {
                                    "publisher-closed"
                                },
                                "Rhythmbox migration committed but publication was incomplete"
                            );
                        }

                        // Failed would falsely claim that the committed transaction rolled
                        // back. Preserve the distinct post-commit failure through the typed
                        // completion while GTK still owns its event lane. A closed lane is
                        // normal during teardown; the command owner must continue to the FIFO
                        // shutdown barrier without trying to surface a late UI result.
                        if publication_incomplete && !tx.is_closed() {
                            RhythmboxMigrationCompletion::AppliedRefreshFailed
                        } else {
                            RhythmboxMigrationCompletion::Applied
                        }
                    }
                    Ok(RhythmboxMigrationOutcome::AlreadyApplied) => {
                        info!(%request_id, "Rhythmbox migration was already applied");
                        RhythmboxMigrationCompletion::AlreadyApplied
                    }
                    Err(RhythmboxMigrationError::Stale) => {
                        info!(%request_id, "Rhythmbox migration preview became stale");
                        RhythmboxMigrationCompletion::Stale
                    }
                    Err(error) => {
                        let category = match error {
                            RhythmboxMigrationError::Stale => "stale",
                            RhythmboxMigrationError::LimitExceeded => "limit",
                            RhythmboxMigrationError::InvalidSnapshot => "invalid-snapshot",
                            RhythmboxMigrationError::Storage(_) => "storage",
                        };
                        warn!(%request_id, category, "Rhythmbox migration failed");
                        RhythmboxMigrationCompletion::Failed
                    }
                };
            let _ = tx
                .send(LibraryEvent::RhythmboxMigrationFinished {
                    request_id,
                    outcome,
                    summary,
                })
                .await;
            return None;
        }
        LibraryCommand::Flush { completion } => {
            let _ = completion.send(()).await;
            return None;
        }
        // Only the watcher-less command loop reaches this arm: the watcher
        // loop turns a rescan into its own cancellable reconciliation, and
        // the startup scan already covers one.
        LibraryCommand::Rescan => {
            info!("Rescanning library on request");
            if let Err(error) = initial_scan(db, music_dirs, tx, playlist_sidebar_refresh).await {
                warn!(%error, "Requested library rescan failed");
            }
            return None;
        }
    };

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

    match begin_root_trust_command(db, music_dirs, tx, playlist_sidebar_refresh, &request).await {
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
            refresh_unavailable_root_trust_evidence(
                db,
                music_dirs,
                tx,
                playlist_sidebar_refresh,
                refresh,
            )
            .await;
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
            refresh_root_trust_evidence(db, music_dirs, tx, playlist_sidebar_refresh).await;
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
            refresh_root_trust_evidence(db, music_dirs, tx, playlist_sidebar_refresh).await;
            None
        }
    }
}

async fn finish_pending_root_trust_scan(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
    pending: PendingRootTrustScan,
) {
    let outcome =
        complete_root_trust_scan(db, music_dirs, tx, playlist_sidebar_refresh, &pending).await;
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
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
) {
    let mut pending_trust_scan = None;
    loop {
        // A successful conversion is deliberately followed by a distinct
        // ordinary scan at the next engine-loop boundary. This is the first
        // point where the newly confirmed marker may authorize content writes
        // and stale deletion.
        if let Some(pending) = pending_trust_scan.take() {
            finish_pending_root_trust_scan(
                db,
                music_dirs,
                tx,
                playlist_sidebar_refresh,
                completed_commands,
                pending,
            )
            .await;
            continue;
        }

        let Ok(command) = command_rx.recv().await else {
            break;
        };
        pending_trust_scan = process_library_command(
            db,
            music_dirs,
            tx,
            playlist_sidebar_refresh,
            completed_commands,
            command,
        )
        .await;
    }
}

/// Service admitted library commands while the initial scan runs.
///
/// The scan's read-only traversal and parsing run on blocking workers that
/// cannot be cancelled while the window is open. Driving command service from
/// the same task with `select!` means a held discovery step cannot delay an
/// admitted rating or history edit until close: while the scan future is
/// pending on its worker, the command branch is polled and the durable
/// mutation settles.
///
/// A scan write transaction is the one exception. Some scan mutations keep a
/// SQLite write transaction open across an await point — the retained-authority
/// probes inside a track upsert, root-status persist, or stale-row delete —
/// and a library command serviced at that moment would queue its own write
/// behind the open transaction and fail at the production five-second busy
/// timeout (PR #286 finding jq5lG). `scan_write_txn` is held exactly over
/// those spans; while it is open the command branch is disabled entirely and
/// the loop keeps polling the scan until the transaction settles. The flag can
/// only change while the scan is being polled, so a command that arrives during
/// a transaction is serviced immediately after it commits — never lost, never
/// starved behind a stuck one.
///
/// The invariant is reciprocal. A command dispatched while the scan is between
/// write boundaries keeps its `command_in_flight` arm held for as long as its
/// work settles, and every scan write boundary parks there — still polled,
/// holding no connection — instead of opening a transaction the in-flight
/// work's own writes would queue behind (PR #286 round-3 finding
/// cid 4051684281). Command service and scan mutations therefore never hold
/// competing SQLite write transactions in either direction, and the wait
/// always resolves because dispatched command work is finite.
///
/// `Flush` is the reserved drain marker. By the time the loop receives it,
/// every earlier admitted command has settled in FIFO order, so the loop waits
/// for the (cancelled) scan to reach settlement before acknowledging. That
/// keeps the close drain behind every already admitted durable mutation.
#[allow(clippy::too_many_arguments)]
async fn service_commands_while_scanning<F>(
    scan: F,
    scan_write_txn: &ScanWriteTxnGate,
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    command_rx: &async_channel::Receiver<LibraryCommand>,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
) -> F::Output
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    // The scan future is large (it embeds the whole traversal/parse state
    // machine); boxing keeps the driver's own future small, which matters
    // because `LibraryEngine::run` is polled inside the GTK main-loop task.
    let mut scan = Box::pin(scan);
    let mut commands_open = true;
    loop {
        if !commands_open {
            return scan.as_mut().await;
        }
        let next = tokio::select! {
            biased;
            result = scan.as_mut() => return result,
            command = GatedCommandRecv::new(command_rx, scan_write_txn) => command,
        };
        match next {
            Ok(LibraryCommand::Flush { completion }) => {
                // Reserved drain: let the cancelled scan settle (admitted
                // durable work completes; held read-only work is abandoned
                // under budget) before acknowledging the writer.
                let result = scan.as_mut().await;
                let _ = completion.send(()).await;
                return result;
            }
            Ok(LibraryCommand::Rescan) => {
                info!(
                    "Library rescan requested during the startup scan; the running scan covers it"
                );
            }
            Ok(command) => {
                // Service the command while KEEPING THE SCAN POLLED. The
                // command's own DB work pends on the connection pool, and the
                // pool may have granted its only connection to the parked
                // scan's queued acquire; a grant held inside a future that is
                // no longer polled would trap the connection and starve the
                // command at the acquire timeout (observed as a 30s sqlx pool
                // timeout in the jq5lG regression). Interleave both futures;
                // if the scan settles first, the pool is idle and the
                // Remaining work finishes alone before the scan result is
                // returned.
                //
                // Hold the reciprocal write-boundary invariant for the whole
                // interleave (PR #286 round-3 finding cid 4051684281): while
                // this work is still settling, the scan's write boundaries
                // park instead of opening a transaction the work's own writes
                // would queue behind. Declared before `work` so the guard
                // outlives it and clears — waking any parked boundary — on
                // every exit path, including the scan-settled-first
                // abandonment below.
                let _command_in_flight_guard = CommandInFlightGuard::arm(scan_write_txn);
                let mut work = Box::pin(async {
                    if let Some(pending) = process_library_command(
                        db,
                        music_dirs,
                        tx,
                        playlist_sidebar_refresh,
                        completed_commands,
                        command,
                    )
                    .await
                    {
                        finish_pending_root_trust_scan(
                            db,
                            music_dirs,
                            tx,
                            playlist_sidebar_refresh,
                            completed_commands,
                            pending,
                        )
                        .await;
                    }
                });
                let mut scan_settled: Option<anyhow::Result<()>> = None;
                loop {
                    if let Some(step) = scan_settled.take() {
                        work.as_mut().await;
                        return step;
                    }
                    tokio::select! {
                        biased;
                        () = work.as_mut() => {
                            break;
                        }
                        step = scan.as_mut() => {
                            scan_settled = Some(step);
                        }
                    }
                }
            }
            Err(_) => commands_open = false,
        }
    }
}

/// Test-only per-file parse delay for the Q4 held/delayed filesystem fixtures.
///
/// The opt-in large-library measurement test sets this to model a slow disk or
/// parser without depending on real storage timing, and R9 (#256) consumes the
/// same seam to hold a scan while exercising command admission and
/// cancellation. It is compiled out of production builds.
#[cfg(test)]
static TEST_ONLY_PARSE_DELAY_MICROS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Counts the parses that pass through this seam, so a measurement can assert
/// the parse path was really exercised per file instead of inferring it from
/// elapsed wall time. When [`TEST_ONLY_PARSE_DELAY_MICROS`] is nonzero, every
/// counted parse also slept for that delay.
#[cfg(test)]
static TEST_ONLY_PARSE_DELAY_INVOCATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
fn apply_test_only_parse_delay() {
    TEST_ONLY_PARSE_DELAY_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
    let micros = TEST_ONLY_PARSE_DELAY_MICROS.load(Ordering::Relaxed);
    if micros > 0 {
        std::thread::sleep(Duration::from_micros(micros));
    }
}

#[cfg(test)]
fn reset_test_only_parse_delay_invocations() {
    TEST_ONLY_PARSE_DELAY_INVOCATIONS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
fn test_only_parse_delay_invocations() -> u64 {
    TEST_ONLY_PARSE_DELAY_INVOCATIONS.load(Ordering::Relaxed)
}

/// Serializes every test-only parse-delay window against the other harnesses
/// that share the seam above.
///
/// The two opt-in Q4 tests live in one test binary, and `cargo test` runs
/// ignored tests on parallel threads: an armed delay window overlapping the
/// startup benchmark would distort its timeline and pollute the invocation
/// counter, and both measurements reset/store the same process-wide statics.
/// [`ParseDelayGuard`] holds this lock for its whole window and the startup
/// benchmark holds it around its engine run. Poisoning is tolerated (the
/// lock is taken) so one panicking measurement cannot wedge the others.
#[cfg(test)]
static TEST_ONLY_PARSE_DELAY_WINDOW: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn initial_scan(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
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
        playlist_sidebar_refresh,
    )
    .await
}

/// Engine-startup initial scan with the window-close cancellation signal.
///
/// Kept separate from [`initial_scan`] so the root-trust command paths, which
/// run *inside* the engine command loop, never observe scan cancellation: they
/// must finish their own authority work. Only the startup scan is bounded by
/// the UI admission boundary's shutdown signal.
async fn initial_scan_shutdown_aware(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
    cancellation: &CancellationToken,
    discovery: &ScanDiscoveryHold,
    scan_write_txn: &ScanWriteTxnGate,
) -> anyhow::Result<()> {
    let forced_conversions = HashMap::new();
    let authority_guards = HashMap::new();
    let evidence_refreshes = HashMap::new();
    initial_scan_with_control(
        db,
        music_dirs,
        tx,
        &forced_conversions,
        &authority_guards,
        &evidence_refreshes,
        playlist_sidebar_refresh,
        cancellation,
        discovery,
        scan_write_txn,
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
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
) -> anyhow::Result<()> {
    // Command-loop scans are never cancelled: a root-trust command must finish
    // its authority work before the FIFO barrier it was admitted behind.
    let cancellation = CancellationToken::new();
    // These scans never share the driver's command-service selector (the
    // command path awaits them directly), so the write-transaction gate is
    // never observed; a private closed gate keeps the invariant local.
    let scan_write_txn = ScanWriteTxnGate::default();
    initial_scan_with_control(
        db,
        music_dirs,
        tx,
        forced_conversions,
        authority_guards,
        evidence_refreshes,
        playlist_sidebar_refresh,
        &cancellation,
        &ScanDiscoveryHold::none(),
        &scan_write_txn,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn initial_scan_with_control(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    forced_conversions: &HashMap<PathBuf, ForcedRootTrustConversion>,
    authority_guards: &HashMap<PathBuf, RootTrustAuthorityGuard>,
    evidence_refreshes: &HashMap<PathBuf, RootTrustEvidenceRefresh>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
    cancellation: &CancellationToken,
    discovery: &ScanDiscoveryHold,
    scan_write_txn: &ScanWriteTxnGate,
) -> anyhow::Result<()> {
    // A window that closes before (or during) database setup cancels the scan
    // before it can admit any mutation. Return immediately so the engine can
    // service the reserved `Flush` drain path without touching the catalogue.
    if cancellation.is_cancelled() {
        info!("Initial scan cancelled before start; no catalogue mutations admitted");
        return Ok(());
    }

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
    let mounted_roots = mounted_subroots(&configured_dirs).map_err(|error| {
        anyhow::anyhow!(
            "failed to inspect mounted library scopes; scan disabled to protect metadata: {error}"
        )
    })?;
    let (persisted_roots, stale_scopes) = partition_stale_mount_scopes(
        &configured_dirs,
        persisted_roots,
        &mounted_roots,
        &existing_tracks,
    );
    if !stale_scopes.is_empty() && admit_scan_mutation(cancellation) {
        wait_for_command_settlement(scan_write_txn).await;
        for scope in &stale_scopes {
            match library_root::Entity::delete_by_id(scope.path.clone())
                .exec(db)
                .await
            {
                Ok(_) => {
                    info!(scope = %scope.path, "Dropped a former mount scope that holds no tracks");
                }
                Err(error) => {
                    warn!(scope = %scope.path, %error, "Could not drop a former mount scope");
                }
            }
        }
    }
    let dirs = expanded_scan_roots_with_mounts(&configured_dirs, &persisted_roots, mounted_roots);
    let all_roots = dirs.clone();

    // Collect a separate traversal result for every root. Never infer scan
    // completeness from the number of audio files: a healthy empty directory
    // is authoritative, while even a single WalkDir error makes that root's
    // view incomplete and therefore unsafe for stale deletion.
    //
    // Traversal is read-only, so it runs under the shutdown isolation contract:
    // if close cancels the scan while a root is enumerating, the join handle is
    // abandoned after the settle budget rather than blocking window teardown.
    //
    // The hold is a production no-op (`ScanDiscoveryHold::none`); tests use it
    // to keep the scan parked in read-only discovery while they exercise
    // command admission and the reserved drain.
    discovery.arrive(ScanDiscoveryStage::Traversal).await;
    let traversal = tokio::task::spawn_blocking(move || {
        dirs.into_iter()
            .map(|root| scan_root_with_exclusions(root, &all_roots))
            .collect::<Vec<_>>()
    });
    let Some(traversal) = await_readonly_blocking(cancellation, traversal).await else {
        info!("Initial scan cancelled while traversal was in flight; abandoning read-only enumeration");
        return Ok(());
    };
    let mut root_scans = traversal?;

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
    for index in 0..root_scans.len() {
        // Marker creation is durable. Once close has cancelled the scan, stop
        // enrolling further roots and fail this run closed.
        if cancellation.is_cancelled() {
            for remaining in &mut root_scans {
                remaining.mark_cancelled("root identity enrollment interrupted at shutdown");
            }
            info!(
                "Initial scan cancelled during root enrollment; no further durable identity writes"
            );
            return Ok(());
        }
        let scan = &mut root_scans[index];
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
        let root_label = scan.root.display().to_string();
        let marker_rescan =
            tokio::task::spawn_blocking(move || scan_root_with_exclusions(root, &exclusions));
        let Some(marker_result) = await_readonly_blocking(cancellation, marker_rescan).await else {
            info!(
                root = %root_label,
                "Initial scan cancelled during marker-backed rescan; abandoning read-only rescan"
            );
            return Ok(());
        };
        let mut marker_scan = marker_result?;
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
        //
        // Admission is re-checked before EVERY root-status mutation, not once
        // before the loop (PR #286 finding jq5ld): when shutdown is observed
        // mid-loop — for example while an earlier root's persist was settling
        // — the remaining roots must not receive status writes that would
        // present them as freshly checked. Fail the remaining scans closed
        // (marked cancelled) and stop without an error: the close drain
        // acknowledges cancelled scans that committed nothing.
        if !admit_scan_mutation(cancellation) {
            let root_display = scan.root.display().to_string();
            mark_scan_cancelled(
                &mut root_scans,
                "root-status persistence interrupted at shutdown",
            );
            info!(
                root = %root_display,
                "Initial scan cancelled before root-status persistence; no further status writes admitted"
            );
            return Ok(());
        }
        // The status persist opens a SQLite write transaction across an await
        // point (the marker probe inside). Hold the write-transaction gate
        // over the whole span so the command-service selector does not
        // dispatch a library command into the open transaction (jq5lG). The
        // RootStatus rendezvous is a test-only seam parked at this boundary.
        // Reciprocal invariant: if a command was already dispatched, park
        // here — outside the transaction — until its work settles instead of
        // making its own writes queue behind this transaction (cid 4051684281).
        // The CommandSettlement seam is a test-only signal fired at this
        // boundary, before the wait: a regression driver learns the pre-wait
        // admission check passed and the scan is about to park on the
        // reciprocal settlement wait (PR #286 round-4 finding j9j81).
        discovery.signal(ScanDiscoveryStage::CommandSettlement);
        wait_for_command_settlement(scan_write_txn).await;
        // Re-check admission once the wait resolves (PR #286 round-4 finding
        // j9j81): the park can span the shutdown cancellation, so refusing
        // only before the wait would still open a post-cancellation write
        // transaction and delay the close drain.
        if !admit_scan_mutation(cancellation) {
            let root_display = scan.root.display().to_string();
            mark_scan_cancelled(
                &mut root_scans,
                "root-status persistence interrupted at shutdown",
            );
            info!(
                root = %root_display,
                "Initial scan cancelled after command settlement, before root-status persistence; no further status writes admitted"
            );
            return Ok(());
        }
        let _scan_write_txn = ScanWriteTxnGuard::open(scan_write_txn);
        discovery.arrive(ScanDiscoveryStage::RootStatus).await;
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

    // Only a root whose identity this scan re-confirmed may have Tributary's
    // private tag-save leftovers repaired or removed.
    if content_mutations_allowed && admit_scan_mutation(cancellation) {
        for scan in &mut root_scans {
            if scan.tag_write_debris.is_empty() || !scan.reconciliation_authoritative {
                continue;
            }
            let Some(lease) = scan.authority_lease.clone() else {
                continue;
            };
            let debris = std::mem::take(&mut scan.tag_write_debris);
            let sweep = tokio::task::spawn_blocking(move || {
                if lease.validate().is_err() {
                    return Vec::new();
                }
                sweep_tag_write_debris(&debris, std::time::SystemTime::now())
            });
            match await_readonly_blocking(cancellation, sweep).await {
                Some(Ok(restored)) => scan.audio_files.extend(restored),
                Some(Err(error)) => {
                    warn!(root = %scan.root.display(), %error, "Tag-save debris sweep task failed");
                }
                None => {
                    info!(root = %scan.root.display(), "Tag-save debris sweep abandoned at shutdown");
                }
            }
        }
    }

    let audio_files = collect_audio_files(&root_scans);
    let total = audio_files.len() as u64;
    info!(total, "Found authorized audio files to scan");

    let mut scanned: u64 = 0;
    let mut on_disk_paths = HashSet::new();

    // After a case-only rename on a case-insensitive filesystem the old row's
    // path still opens the renamed file, so it can never be proven stale.
    // Rows without a file of their own are indexed by folded path so a newly
    // seen spelling can take the row over instead of duplicating the track.
    let mut case_alias_rows: HashMap<String, Vec<&track::Model>> = HashMap::new();
    {
        let enumerated: HashSet<&Path> = audio_files.iter().map(|(p, _)| p.as_path()).collect();
        for row in &existing_tracks {
            if !enumerated.contains(Path::new(&row.file_path)) {
                case_alias_rows
                    .entry(row.file_path.to_lowercase())
                    .or_default()
                    .push(row);
            }
        }
    }

    for (path, mtime) in &audio_files {
        // Check the shutdown boundary before admitting the next parse/upsert.
        // Everything already committed above stands; nothing new is admitted,
        // and the no-deletion phase below is skipped entirely.
        if cancellation.is_cancelled() {
            mark_scan_cancelled(
                &mut root_scans,
                "catalogue mutation loop interrupted at shutdown",
            );
            info!(
                scanned,
                total, "Initial scan cancelled; no further catalogue mutations admitted"
            );
            return Ok(());
        }

        // Count the file before any skip below, so progress reaches the total
        // even when some files are rejected or fail to parse.
        scanned += 1;
        if scanned.is_multiple_of(50) || scanned == total {
            let _ = tx.send(LibraryEvent::ScanProgress(scanned, total)).await;
        }

        let path_str = path.to_string_lossy().to_string();
        on_disk_paths.insert(path_str.clone());

        // Look up the existing row (if any) in the preloaded map.
        let mut existing = existing_by_path.get(path_str.as_str()).copied();
        let case_alias = if existing.is_none() {
            case_alias_rows
                .get_mut(&path_str.to_lowercase())
                .and_then(Vec::pop)
        } else {
            None
        };

        let needs_update = match existing {
            // Compare the traversal's mtime with the stored date_modified.
            Some(row) => *mtime != row.date_modified,
            None => true,
        };

        if needs_update {
            // Pre-admission boundary: no new durable mutation may begin after
            // shutdown. This also bounds the read-only authority probe below,
            // which validates the retained root before the parser runs.
            if !admit_scan_mutation(cancellation) {
                mark_scan_cancelled(
                    &mut root_scans,
                    "catalogue mutation loop interrupted at shutdown",
                );
                info!(
                    scanned,
                    total, "Initial scan cancelled; no further catalogue mutations admitted"
                );
                return Ok(());
            }
            let (identity_allows_parse, invalidated_root) = match revalidate_scan_root_for_path(
                path,
                &mut root_scans,
                Some(cancellation),
            )
            .await
            {
                Ok(RootRevalidation::Authorized) => (true, None),
                Ok(RootRevalidation::Rejected(root)) => (false, root),
                Ok(RootRevalidation::Abandoned) => {
                    mark_scan_cancelled(
                        &mut root_scans,
                        "pre-parse authority probe abandoned at shutdown",
                    );
                    info!(path = %path.display(), "Initial scan cancelled while revalidating before parse");
                    return Ok(());
                }
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
            let alias_path = case_alias.map(|row| PathBuf::from(&row.file_path));
            let open_job = tokio::task::spawn_blocking(move || {
                let observed_file = Arc::new(open_lease.open_regular_file(&open_path)?);
                let parse_file = observed_file.try_clone_file()?;
                let alias_is_same_file =
                    alias_path.is_some_and(|alias| path_names_same_file(&alias, &parse_file));
                Ok::<_, std::io::Error>((observed_file, parse_file, alias_is_same_file))
            });
            let (observed_file, parse_file) = match await_readonly_blocking(cancellation, open_job)
                .await
            {
                Some(Ok(Ok((observed_file, parse_file, alias_is_same_file)))) => {
                    if let Some(row) = case_alias.filter(|_| alias_is_same_file) {
                        // Retarget the old spelling's row, keeping its
                        // identity, history, rating and playlist links. Its
                        // old path is not stale: it names this same file.
                        info!(from = %row.file_path, to = %path_str, "Retargeting track after a case-only rename");
                        on_disk_paths.insert(row.file_path.clone());
                        existing = Some(row);
                    }
                    (observed_file, parse_file)
                }
                Some(Ok(Err(error))) => {
                    warn!(path = %path.display(), %error, "Audio file could not be opened and cloned through retained root authority — upsert discarded");
                    continue;
                }
                Some(Err(error)) => {
                    warn!(path = %path.display(), %error, "Initial-scan audio authority task failed — upsert discarded");
                    continue;
                }
                None => {
                    // Read-only open did not settle inside the shutdown
                    // budget; abandon it rather than blocking close.
                    info!(path = %path.display(), "Initial-scan audio open abandoned at shutdown");
                    return Ok(());
                }
            };

            let p = path.clone();
            let parse_job = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                apply_test_only_parse_delay();
                tag_parser::parse_audio_file_from_file(parse_file, &p)
            });
            let parse_result = await_readonly_blocking(cancellation, parse_job).await;

            match parse_result {
                // The read-only parser did not settle inside the shutdown
                // budget. No mutation depended on it yet, so abandoning it is
                // safe; the next loop boundary observes cancellation.
                None => {
                    info!(path = %path.display(), "Initial-scan parse abandoned at shutdown");
                    return Ok(());
                }
                Some(Ok(Ok(parsed))) => {
                    // Deterministic test rendezvous: the parser has settled.
                    discovery.arrive(ScanDiscoveryStage::PostParse).await;

                    // Durable-mutation admission boundary. The read-only parser
                    // may have settled inside its shutdown grace *after*
                    // cancellation; in that case no new durable work may begin.
                    // Refusing here is what prevents the post-cancel upsert and
                    // its unbounded authority probes.
                    if !admit_scan_mutation(cancellation) {
                        mark_scan_cancelled(
                            &mut root_scans,
                            "post-parse mutation admission refused at shutdown",
                        );
                        info!(path = %path.display(), "Initial scan cancelled before upsert admission; no new durable work");
                        return Ok(());
                    }
                    let (identity_allows_upsert, invalidated_root) =
                        match revalidate_scan_root_for_path(
                            path,
                            &mut root_scans,
                            Some(cancellation),
                        )
                        .await
                        {
                            Ok(RootRevalidation::Authorized) => (true, None),
                            Ok(RootRevalidation::Rejected(root)) => (false, root),
                            Ok(RootRevalidation::Abandoned) => {
                                mark_scan_cancelled(
                                    &mut root_scans,
                                    "post-parse authority probe abandoned at shutdown",
                                );
                                info!(path = %path.display(), "Initial scan cancelled while revalidating after parse");
                                return Ok(());
                            }
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

                    // Final admission boundary: shutdown observed between the
                    // post-parse revalidation and the upsert itself must still
                    // refuse to begin durable work. Once the upsert below is
                    // entered it is awaited to settlement (never dropped).
                    if !admit_scan_mutation(cancellation) {
                        mark_scan_cancelled(
                            &mut root_scans,
                            "upsert admission refused at shutdown",
                        );
                        info!(path = %path.display(), "Initial scan cancelled before upsert; no new durable work");
                        return Ok(());
                    }

                    // During the initial scan we do NOT emit a TrackUpserted
                    // per file: the single FullSync below delivers the complete
                    // snapshot, avoiding O(n^2) UI work plus a full track-list
                    // clone per event. The watcher still emits TrackUpserted for
                    // incremental changes, where it is cheap.
                    let mut invalidated_root = None;
                    let mut file_still_current = false;
                    let mut authority_task_failed = false;
                    // The upsert keeps its SQLite write transaction open while
                    // the commit guard probes the retained authority handle, so
                    // the write-transaction gate spans the whole call: the
                    // command-service selector defers library commands until the
                    // transaction commits or rolls back (jq5lG). Reciprocally,
                    // park here while a dispatched command's work is still in
                    // flight (cid 4051684281).
                    wait_for_command_settlement(scan_write_txn).await;
                    // Re-check admission once the wait resolves (PR #286
                    // round-4 finding j9j81): the park can span the shutdown
                    // cancellation, so refusing only before the wait would
                    // still open a post-cancellation write transaction.
                    if !admit_scan_mutation(cancellation) {
                        mark_scan_cancelled(
                            &mut root_scans,
                            "upsert admission refused at shutdown",
                        );
                        info!(path = %path.display(), "Initial scan cancelled after command settlement, before upsert; no new durable work");
                        return Ok(());
                    }
                    let _scan_write_txn = ScanWriteTxnGuard::open(scan_write_txn);
                    match upsert_track_with_commit_guard(db, &parsed, existing, || async {
                        // Deterministic test rendezvous: suspend INSIDE the open
                        // write transaction, at the very guard await a branch
                        // switch used to race into.
                        discovery.arrive(ScanDiscoveryStage::CommitGuard).await;
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
                        let root_still_current = match revalidate_scan_root_for_path(
                            path,
                            &mut root_scans,
                            None,
                        )
                        .await
                        {
                            Ok(RootRevalidation::Authorized) => true,
                            Ok(RootRevalidation::Rejected(invalidated)) => {
                                invalidated_root = invalidated;
                                false
                            }
                            // The guard runs inside an already admitted
                            // mutation, so it is never abandoned: `None`
                            // cancellation means it cannot return this arm.
                            Ok(RootRevalidation::Abandoned) => {
                                unreachable!("a mutation guard cannot abandon its authority probe")
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
                Some(Ok(Err(e))) => {
                    warn!(path = %path_str, error = %e, "Skipping unparseable file");
                }
                Some(Err(e)) => {
                    warn!(path = %path_str, error = %e, "spawn_blocking failed");
                }
            }
        }
    }

    // A cancelled scan is incomplete by contract. Never enter the destructive
    // phase: preserve every catalogue row and the incomplete-scan authority
    // semantics even if the traversal itself called itself complete.
    if cancellation.is_cancelled() {
        mark_scan_cancelled(
            &mut root_scans,
            "stale-deletion reconciliation skipped at shutdown",
        );
        info!("Initial scan cancelled before stale deletion; preserving all catalogue metadata");
        return Ok(());
    }

    // Parsing can outlive a removable-media transition. Revalidate each
    // retained root object immediately before the destructive phase; a
    // changed, removed, copied-marker, or unreadable root disables every stale
    // deletion for that scope. The probe is read-only pre-admission work, so it
    // is bounded by the shutdown budget: an abandoned probe must never lead to
    // a deletion.
    let mut deletion_preflight_abandoned = false;
    for scan in &mut root_scans {
        if !scan.reconciliation_authoritative {
            continue;
        }
        let expected = scan.device_id.clone();
        let authority_still_matches = if let Some(lease) = scan.authority_lease.clone() {
            let probe = move || {
                expected.as_deref() == Some(lease.expected_marker()) && lease.validate().is_ok()
            };
            match await_readonly_blocking(cancellation, tokio::task::spawn_blocking(probe)).await {
                Some(Ok(matches)) => matches,
                Some(Err(error)) => {
                    // Reject reconciliation for this scan, but a task failure
                    // is not evidence that the persisted root changed.
                    scan.reconciliation_authoritative = false;
                    scan.content_authorized = false;
                    warn!(root = %scan.root.display(), %error, "Library-root authority validation task failed before reconciliation — stale deletion disabled");
                    continue;
                }
                None => {
                    // Shutdown abandoned the pre-deletion authority probe. The
                    // root is unproven, so no stale deletion may follow.
                    scan.reconciliation_authoritative = false;
                    scan.content_authorized = false;
                    deletion_preflight_abandoned = true;
                    break;
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

    if deletion_preflight_abandoned || !admit_scan_mutation(cancellation) {
        mark_scan_cancelled(
            &mut root_scans,
            "destructive-phase admission refused at shutdown",
        );
        info!("Initial scan cancelled before stale deletion; preserving all catalogue metadata");
        return Ok(());
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
            // The absence proof is read-only blocking filesystem work against a
            // possibly removable or network root; the kernel call can block
            // well past window close. It therefore obeys the shutdown settle
            // budget like the traversal and parse jobs (PR #286 finding jq5lT):
            // when the budget is exhausted, abandon the probe, fail the scan
            // closed, and PRESERVE the row — an unproven absence never deletes.
            let absence = match await_readonly_blocking(
                cancellation,
                spawn_authority_probe(move || {
                    // Test-only seam: park the probe the way a hung kernel call
                    // would (jq5lT regression).
                    #[cfg(test)]
                    tests::hold_stale_absence_probe();
                    proof_lease.prove_absent(&proof_path).map(Arc::new)
                }),
            )
            .await
            {
                Some(Ok(Ok(proof))) => proof,
                Some(Ok(Err(error))) => {
                    warn!(path = %row.file_path, %error, "Could not prove stale candidate absent through retained root authority — preserving metadata");
                    continue;
                }
                Some(Err(error)) => {
                    warn!(path = %row.file_path, %error, "Stale-candidate authority task failed — preserving metadata");
                    continue;
                }
                None => {
                    mark_scan_cancelled(
                        &mut root_scans,
                        "stale-delete absence probe abandoned at shutdown",
                    );
                    info!(path = %row.file_path, "Initial scan cancelled while proving stale candidate absent; preserving metadata");
                    return Ok(());
                }
            };
            let (root_allows_delete, invalidated_root) = match revalidate_scan_root_for_path(
                row_path,
                &mut root_scans,
                Some(cancellation),
            )
            .await
            {
                Ok(RootRevalidation::Authorized) => (true, None),
                Ok(RootRevalidation::Rejected(root)) => (false, root),
                Ok(RootRevalidation::Abandoned) => {
                    mark_scan_cancelled(
                        &mut root_scans,
                        "stale-delete authority probe abandoned at shutdown",
                    );
                    info!(path = %row.file_path, "Initial scan cancelled during stale-delete revalidation; preserving metadata");
                    return Ok(());
                }
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
            // Final admission boundary for the destructive phase: shutdown
            // observed after the per-row revalidation must still preserve the
            // row. Once entered, the delete is awaited to settlement.
            if !admit_scan_mutation(cancellation) {
                mark_scan_cancelled(
                    &mut root_scans,
                    "stale-delete admission refused at shutdown",
                );
                info!(path = %row.file_path, "Initial scan cancelled before stale deletion; preserving metadata");
                return Ok(());
            }
            info!(path = %row.file_path, "Removing stale track from database");
            let mut invalidated_root = None;
            let mut absence_still_current = false;
            let mut authority_task_failed = false;
            // The delete keeps its SQLite write transaction open while the
            // commit guard revalidates the absence proof and the root lease, so
            // the write-transaction gate spans the whole call (jq5lG).
            // Reciprocally, park here while a dispatched command's work is
            // still in flight (cid 4051684281).
            wait_for_command_settlement(scan_write_txn).await;
            // Re-check admission once the wait resolves (PR #286 round-4
            // finding j9j81): the park can span the shutdown cancellation, so
            // refusing only before the wait would still open a
            // post-cancellation write transaction and delay the close drain.
            if !admit_scan_mutation(cancellation) {
                mark_scan_cancelled(
                    &mut root_scans,
                    "stale-delete admission refused at shutdown",
                );
                info!(path = %row.file_path, "Initial scan cancelled after command settlement, before stale deletion; preserving metadata");
                return Ok(());
            }
            let _scan_write_txn = ScanWriteTxnGuard::open(scan_write_txn);
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
                    match revalidate_scan_root_for_path(row_path, &mut root_scans, None).await {
                        Ok(RootRevalidation::Authorized) => true,
                        Ok(RootRevalidation::Rejected(invalidated)) => {
                            invalidated_root = invalidated;
                            false
                        }
                        // Admitted-mutation guard: never abandoned.
                        Ok(RootRevalidation::Abandoned) => {
                            unreachable!("a mutation guard cannot abandon its authority probe")
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

    // Root availability goes first, so the snapshot's folder view is built
    // against it.
    publish_root_status(db, &configured_dirs, tx).await;

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

    // Seed first, then ask the sole versioned publisher to re-read the complete
    // joined projection. Repeated scans use the same coalescing path.
    seed_default_playlists_and_request(&playlist_mgr, playlist_sidebar_refresh).await;
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

/// Tributary's own tag-write siblings: the staged copy and the quarantined
/// original. They are never library content; the watcher only uses them to
/// learn that the public path next to them changed.
fn is_private_write_sibling(path: &Path) -> bool {
    tag_writer::is_tag_write_temp_file(path) || super::root_authority::is_quarantine_file(path)
}

/// Tag-write debris younger than this may belong to a save still in progress
/// and is left alone.
const TAG_WRITE_DEBRIS_MIN_AGE: Duration = Duration::from_hours(1);

/// Clean up what interrupted tag saves left under an authoritative root.
///
/// A quarantined original whose public name is vacant is moved back, so the
/// track is found where its row says it is instead of being reconciled away.
/// Otherwise staged copies, and quarantined originals whose public name is
/// occupied, are removed once they are older than
/// [`TAG_WRITE_DEBRIS_MIN_AGE`]. Returns each restored public path with its
/// mtime, ready to join the scan's audio files.
///
/// Blocking — worker threads only.
fn sweep_tag_write_debris(
    debris: &[PathBuf],
    now: std::time::SystemTime,
) -> Vec<(PathBuf, String)> {
    let mut restored = Vec::new();
    for path in debris {
        if super::root_authority::is_quarantine_file(path) {
            let Some(public) = quarantined_public_path(path) else {
                continue;
            };
            match rename_to_vacant_name(path, &public) {
                Ok(()) => {
                    info!(path = %public.display(), "Restored a file left hidden by an interrupted tag save");
                    let mtime = get_mtime(&public);
                    restored.push((public, mtime));
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    warn!(path = %path.display(), %error, "Could not restore a file left hidden by an interrupted tag save");
                    continue;
                }
            }
        }
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            continue;
        };
        if metadata.is_file()
            && debris_age(&metadata, now).is_some_and(|age| age >= TAG_WRITE_DEBRIS_MIN_AGE)
        {
            match std::fs::remove_file(path) {
                Ok(()) => info!(path = %path.display(), "Removed leftover tag-save debris"),
                Err(error) => {
                    warn!(path = %path.display(), %error, "Could not remove leftover tag-save debris");
                }
            }
        }
    }
    restored
}

/// The public path a quarantined original was displaced from, when its name
/// records the whole original leaf. Quarantine names keep at most 96 bytes of
/// the leaf, so a longer prefix may be truncated and is not trusted; a lossy
/// (non-UTF-8) leaf cannot be rebuilt either.
fn quarantined_public_path(quarantine: &Path) -> Option<PathBuf> {
    const QUARANTINE_SUFFIX_LEN: usize = ".tributary-replaced-".len() + 32;
    let name = quarantine.file_name()?.to_str()?;
    let leaf = name
        .get(1..name.len().checked_sub(QUARANTINE_SUFFIX_LEN)?)
        .filter(|leaf| {
            !leaf.is_empty() && leaf.len() <= 92 && !leaf.contains(char::REPLACEMENT_CHARACTER)
        })?;
    Some(quarantine.with_file_name(leaf))
}

/// Move `from` to `to` only while `to` is vacant, failing with
/// `AlreadyExists` instead of replacing an occupant.
fn rename_to_vacant_name(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => return Ok(()),
        Err(rustix::io::Errno::EXIST) => {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        // The filesystem may lack the no-replace flag; the link form below
        // refuses an occupied name just the same.
        Err(_) => {}
    }
    std::fs::hard_link(from, to)?;
    std::fs::remove_file(from)
}

/// How long ago `metadata` was last changed. On Unix the inode change time
/// also moves on rename, so a just-displaced original counts as fresh.
fn debris_age(metadata: &std::fs::Metadata, now: std::time::SystemTime) -> Option<Duration> {
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        std::time::UNIX_EPOCH
            .checked_add(Duration::from_secs(u64::try_from(metadata.ctime()).ok()?))?
    };
    #[cfg(not(unix))]
    let changed = metadata.modified().ok()?;
    now.duration_since(changed).ok()
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

fn watcher_event_kind_is_observational_access(kind: notify::EventKind) -> bool {
    use notify::event::{MetadataKind, ModifyKind};
    use notify::EventKind;

    matches!(
        kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
    )
}

fn marker_event_invalidates_root(kind: notify::EventKind) -> bool {
    // Reading the marker is part of every authorization probe and some
    // backends report open/read/close (or the resulting atime update) through
    // the same watcher. Those observations must not invalidate the identity
    // they just verified. Every potentially mutating or unknown event remains
    // fail-closed.
    !watcher_event_kind_is_observational_access(kind)
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
                // FSEvents and kqueue cannot associate the old and new sides,
                // so identity is never inferred. An audio path still names one
                // file: refresh or remove it by path. Any other name may be a
                // directory, which needs the guarded reconciliation scan.
                for path in event.paths {
                    if is_private_write_sibling(&path) {
                        continue;
                    }
                    if tag_parser::is_audio_file(&path) {
                        self.record_upsert(path);
                    } else {
                        self.reconciliation_required = true;
                    }
                }
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
        if is_private_write_sibling(&path) {
            return;
        }
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
        if is_private_write_sibling(&path) {
            return;
        }
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
        match (
            is_private_write_sibling(&from),
            is_private_write_sibling(&to),
        ) {
            (true, true) => return,
            // A tag write moves the original aside, publishes the staged copy
            // under its name, and (after a failed commit) moves the original
            // back. Each step only means the public path changed: refresh it
            // in place without transferring identity from a private name that
            // was never indexed.
            (true, false) => {
                self.record_upsert(to);
                return;
            }
            (false, true) => {
                self.record_upsert(from);
                return;
            }
            (false, false) => {}
        }

        let pair = WatcherRenamePair { from, to };
        if pair.from == pair.to {
            self.record_upsert(pair.to);
            return;
        }
        // Only a same-extension audio file or a directory carries an indexed
        // identity across a rename. A file renamed across extensions (a sync
        // tool publishing its hidden download, or a track renamed to a
        // non-audio name) is a removal of the source plus an upsert of the
        // destination. A rename keeps the object's type, so the source was a
        // file as well and its deferred observation is not a directory.
        if !same_audio_extension(&pair.from, &pair.to)
            && matches!(
                watcher_upsert_path_kind(&pair.to),
                Ok(WatcherUpsertPathKind::RegularFile)
            )
        {
            self.deferred_paths.remove(&pair.from);
            if tag_parser::is_audio_file(&pair.from) {
                self.record_remove(pair.from);
            }
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

/// Finish the playlist work caused by one watcher batch, then tell the UI to
/// rebuild any active projection. `upserted` holds every row the batch
/// inserted, updated, or relocated; only those can newly resolve an orphaned
/// entry. The notification is deliberately emitted even when reconciliation
/// fails: the committed track mutation still needs to become visible, and a
/// later batch or scan can retry orphan relinking.
async fn settle_playlist_projections_after_watcher_batch(
    db: &DatabaseConnection,
    tx: &async_channel::Sender<LibraryEvent>,
    upserted: &[track::Model],
    track_mutation_committed: bool,
) -> Result<u32, sea_orm::DbErr> {
    let result = super::playlist_manager::PlaylistManager::new(db.clone())
        .reconcile_changed_tracks(upserted)
        .await;
    if track_mutation_committed {
        let _ = tx.send(LibraryEvent::PlaylistProjectionsInvalidated).await;
    }
    result
}

const WATCHER_EVENT_CAPACITY: usize = 256;
const WATCHER_DEBOUNCE_MS: u64 = 1500;
const WATCHER_RECONCILIATION_RETRY_MS: u64 = 1000;
/// How often the watcher re-probes configured roots. Nothing notifies the
/// watcher when a volume is mounted, and notify drops a watch silently when
/// its volume is unmounted.
const ROOT_PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// A cheap observation of one configured root for the availability probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootPresence {
    /// The configured path is not a directory.
    Missing,
    /// The path is a directory on `boundary` (a mount ID on Linux, a device
    /// number on other Unix platforms). Unmounting changes the boundary even
    /// when an empty mountpoint stays behind, and removes the root marker.
    Present { boundary: u64, marked: bool },
}

fn probe_root_presence(root: &Path) -> RootPresence {
    if !root.is_dir() {
        return RootPresence::Missing;
    }
    match filesystem_boundary_id(root) {
        Ok(boundary) => RootPresence::Present {
            boundary,
            marked: root_identity_path(root).is_file(),
        },
        Err(_) => RootPresence::Missing,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootProbeAction {
    Unchanged,
    /// The root disappeared, or an empty mountpoint replaced its marked
    /// volume: mark it unavailable without scanning, so its rows survive.
    Gone,
    /// The root appeared or was remounted: watch it again and rescan.
    Arrived,
}

fn root_probe_action(previous: RootPresence, current: RootPresence) -> RootProbeAction {
    match (previous, current) {
        _ if previous == current => RootProbeAction::Unchanged,
        (_, RootPresence::Missing)
        | (
            RootPresence::Present { marked: true, .. },
            RootPresence::Present { marked: false, .. },
        ) => RootProbeAction::Gone,
        _ => RootProbeAction::Arrived,
    }
}

/// What one availability probe asks of the watcher loop.
#[derive(Debug, Default, Eq, PartialEq)]
struct RootProbeOutcome {
    /// Roots to persist as unavailable. Their rows are never deleted.
    gone: Vec<PathBuf>,
    /// A root appeared, was remounted, or gained its watch, so a scan is due.
    rescan: bool,
}

struct DirectoryWatcher {
    watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
    ingress_overflowed: Arc<AtomicBool>,
    /// Roots with a registered watch. After an unmount the registration may
    /// be dead; an `Arrived` probe replaces it.
    watched_directories: HashSet<PathBuf>,
    /// What the last probe saw at each configured root. Recorded before the
    /// startup scan, so a root that changes during it is rescanned.
    root_presence: HashMap<PathBuf, RootPresence>,
    root_probe_interval: Duration,
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
            self.watch_directory(dir);
        }
    }

    fn watch_directory(&mut self, dir: &Path) -> bool {
        if let Err(error) = self.watcher.watch(dir, RecursiveMode::Recursive) {
            warn!(dir = %dir.display(), %error, "Failed to watch directory — skipping");
            return false;
        }
        self.watched_directories.insert(dir.to_path_buf());
        info!(dir = %dir.display(), "Watching directory");
        true
    }

    /// Probe every configured root, re-watching roots that appeared or were
    /// remounted. Blocking — the probe stats roots and a recursive watch
    /// walks the whole tree.
    fn reprobe_roots(&mut self, music_dirs: &[PathBuf]) -> RootProbeOutcome {
        let mut outcome = RootProbeOutcome::default();
        for dir in music_dirs {
            let current = probe_root_presence(dir);
            let previous = self
                .root_presence
                .insert(dir.clone(), current)
                .unwrap_or(RootPresence::Missing);
            match root_probe_action(previous, current) {
                RootProbeAction::Unchanged => {
                    // Retry a watch that failed earlier, but never on an
                    // unmarked mountpoint left behind by an unmount.
                    if matches!(current, RootPresence::Present { marked: true, .. })
                        && !self.watched_directories.contains(dir)
                        && self.watch_directory(dir)
                    {
                        outcome.rescan = true;
                    }
                }
                RootProbeAction::Gone => {
                    info!(dir = %dir.display(), "Library folder went away");
                    outcome.gone.push(dir.clone());
                }
                RootProbeAction::Arrived => {
                    info!(dir = %dir.display(), "Library folder appeared or was remounted");
                    if self.watched_directories.remove(dir) {
                        // The old registration is usually dead already.
                        let _ = self.watcher.unwatch(dir);
                    }
                    self.watch_directory(dir);
                    outcome.rescan = true;
                }
            }
        }
        outcome
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
    // Tag reads can generate an event per file on Linux (including atime
    // metadata updates under relatime). These events are observational and
    // were already ignored by batching, so keep them out of the bounded queue
    // before a large scan can falsely report stream loss and rescan forever.
    // A backend rescan flag remains authoritative even on an access event.
    if result.as_ref().is_ok_and(|event| {
        !event.need_rescan() && watcher_event_kind_is_observational_access(event.kind)
    }) {
        return;
    }

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
        root_presence: music_dirs
            .iter()
            .map(|dir| (dir.clone(), probe_root_presence(dir)))
            .collect(),
        root_probe_interval: ROOT_PROBE_INTERVAL,
    };

    // Watch each directory independently. A missing or unwatchable directory
    // is retried once after the bootstrap scan rather than aborting the whole
    // watcher, so one bad path cannot stop healthy roots from being watched.
    installed.watch_available_directories(music_dirs);
    info!("Filesystem watcher active");

    Ok(installed)
}

/// One debounce window of watcher events, kept raw and in order.
///
/// Classifying an event stats its paths, which can block on slow storage, so
/// [`WatcherDebounceBatch::finish`] runs on a blocking worker once the window
/// closes rather than on the engine task as each event arrives.
#[derive(Debug, Default)]
struct WatcherDebounceBatch {
    events: Vec<notify::Event>,
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
                self.events.clear();
            }
            Ok(event) => self.events.push(event),
            Err(error) => {
                warn!(%error, "Filesystem watcher reported an unreliable stream");
                self.stream_unreliable = true;
                self.events.clear();
            }
        }
    }

    fn finish(self) -> Option<WatcherBatch> {
        if self.stream_unreliable {
            return None;
        }
        let mut batch = WatcherBatch::default();
        for event in self.events {
            batch.collect(event);
        }
        batch.finish();
        Some(batch)
    }
}

fn discard_watcher_backlog(rx: &mut mpsc::Receiver<notify::Result<notify::Event>>) {
    while rx.try_recv().is_ok() {}
}

/// The watcher's authoritative fallback: an ordinary library scan, bounded by
/// the window-close cancellation like the startup scan so closing the window
/// never waits for a whole-library rescan.
async fn reconcile_watched_library(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
    cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    initial_scan_shutdown_aware(
        db,
        music_dirs,
        tx,
        playlist_sidebar_refresh,
        cancellation,
        &ScanDiscoveryHold::none(),
        &ScanWriteTxnGate::default(),
    )
    .await
}

async fn reconcile_unreliable_watcher_stream(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
    cancellation: &CancellationToken,
    rx: &mut mpsc::Receiver<notify::Result<notify::Event>>,
) -> bool {
    // The queued backlog belongs to the same stream gap and cannot be applied
    // incrementally. Events racing with this drain may be discarded too; the
    // following authoritative scan is what makes that safe. Events arriving
    // after the drain, including during the scan, remain queued for the next
    // loop iteration.
    discard_watcher_backlog(rx);
    info!("Reconciling library after filesystem watcher stream loss");
    match reconcile_watched_library(db, music_dirs, tx, playlist_sidebar_refresh, cancellation)
        .await
    {
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
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
    cancellation: &CancellationToken,
    roots: &HashSet<PathBuf>,
) -> bool {
    // Invalidate persisted authorization before any asynchronous traversal.
    // A marker created by the bootstrap scan is restored to available by this
    // immediate marker-backed scan; a replaced marker remains unavailable.
    for root in roots {
        mark_root_path_unavailable(db, root).await;
    }
    info!("Reconciling library after library root marker mutation");
    match reconcile_watched_library(db, music_dirs, tx, playlist_sidebar_refresh, cancellation)
        .await
    {
        Ok(()) => true,
        Err(error) => {
            warn!(%error, "Library root marker reconciliation failed; retry remains pending");
            false
        }
    }
}

/// What woke the watcher loop.
enum WatcherWake {
    Command(Result<LibraryCommand, async_channel::RecvError>),
    RootProbe,
    Event(Option<notify::Result<notify::Event>>),
}

/// Run one availability probe on a blocking worker, persist roots that went
/// away as unavailable, and report whether a rescan is due.
async fn reprobe_watched_roots(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    mut watcher: DirectoryWatcher,
) -> anyhow::Result<(DirectoryWatcher, bool)> {
    let dirs = music_dirs.to_vec();
    let (watcher, outcome) = tokio::task::spawn_blocking(move || {
        let outcome = watcher.reprobe_roots(&dirs);
        (watcher, outcome)
    })
    .await
    .map_err(|error| anyhow::anyhow!("library root probe task failed: {error}"))?;
    if !outcome.gone.is_empty() {
        for root in &outcome.gone {
            mark_root_path_unavailable_if_active(db, root).await;
        }
        publish_root_status(db, music_dirs, tx).await;
    }
    Ok((watcher, outcome.rescan))
}

/// Tell the UI which configured roots currently back playable tracks.
async fn publish_root_status(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
) {
    let states = match library_root::Entity::find().all(db).await {
        Ok(states) => states,
        Err(error) => {
            warn!(%error, "Could not load library root state to publish");
            return;
        }
    };
    let statuses = music_dirs
        .iter()
        .map(|root| {
            let key = root.to_string_lossy();
            LibraryRootStatus {
                path: root.clone(),
                available: states.iter().any(|state| {
                    state.path == key
                        && state.identity_confirmed
                        && state.is_available
                        && state.last_scan_complete
                }),
            }
        })
        .collect();
    let _ = tx.send(LibraryEvent::RootStatusChanged(statuses)).await;
}

#[allow(clippy::too_many_arguments)]
async fn process_directory_events(
    db: &Arc<DatabaseConnection>,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
    command_rx: &async_channel::Receiver<LibraryCommand>,
    completed_commands: &mut HashMap<Uuid, CompletedRootTrustCommand>,
    mut watcher: DirectoryWatcher,
    playlist_sidebar_refresh: &PlaylistSidebarRefresh,
    scan_cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    // ── Debounced event processing ──────────────────────────────
    // Collect filesystem events for a short window, deduplicate by
    // path, then process the batch. This collapses the 3-5 duplicate
    // Create/Modify events that Windows fires per file copy into a
    // single parse+upsert, and removes the old per-file 500ms sleep.
    let mut reconciliation_pending = false;
    let mut pending_trust_scan: Option<PendingRootTrustScan> = None;
    let mut commands_open = true;
    let mut rescan_requested = false;
    let mut root_probe = tokio::time::interval_at(
        tokio::time::Instant::now() + watcher.root_probe_interval,
        watcher.root_probe_interval,
    );
    root_probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                playlist_sidebar_refresh,
                completed_commands,
                pending,
            )
            .await;
            continue;
        }

        // Each UI mutation is one serialized engine command. Drain at most one
        // per batch boundary so it cannot interleave with a watcher mutation.
        if commands_open {
            match command_rx.try_recv() {
                Ok(LibraryCommand::Rescan) => {
                    rescan_requested = true;
                    continue;
                }
                Ok(command) => {
                    pending_trust_scan = process_library_command(
                        db.as_ref(),
                        music_dirs,
                        tx,
                        playlist_sidebar_refresh,
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

        // A requested rescan first re-probes the roots, so one that has just
        // appeared is watched before the scan indexes it.
        if std::mem::take(&mut rescan_requested) {
            info!("Rescanning library on request");
            let (returned, _) = reprobe_watched_roots(db.as_ref(), music_dirs, tx, watcher).await?;
            watcher = returned;
            reconciliation_pending = true;
        }

        let overflowed = watcher.ingress_overflowed.swap(false, Ordering::AcqRel);
        if reconciliation_pending || overflowed {
            if overflowed {
                warn!("Filesystem watcher ingress overflowed");
            }
            reconciliation_pending = !reconcile_unreliable_watcher_stream(
                db.as_ref(),
                music_dirs,
                tx,
                playlist_sidebar_refresh,
                scan_cancellation,
                &mut watcher.rx,
            )
            .await;
            if reconciliation_pending {
                tokio::time::sleep(Duration::from_millis(WATCHER_RECONCILIATION_RETRY_MS)).await;
            }
            continue;
        }

        // Wait for the next serialized UI command, root probe, or watcher
        // batch. A closed command channel must not stop filesystem watching.
        let wake = tokio::select! {
            biased;
            command = command_rx.recv(), if commands_open => WatcherWake::Command(command),
            _ = root_probe.tick() => WatcherWake::RootProbe,
            first = watcher.rx.recv() => WatcherWake::Event(first),
        };
        let first = match wake {
            WatcherWake::Command(Ok(LibraryCommand::Rescan)) => {
                rescan_requested = true;
                continue;
            }
            WatcherWake::Command(Ok(command)) => {
                pending_trust_scan = process_library_command(
                    db.as_ref(),
                    music_dirs,
                    tx,
                    playlist_sidebar_refresh,
                    completed_commands,
                    command,
                )
                .await;
                continue;
            }
            WatcherWake::Command(Err(_)) => {
                commands_open = false;
                continue;
            }
            WatcherWake::RootProbe => {
                let (returned, rescan) =
                    reprobe_watched_roots(db.as_ref(), music_dirs, tx, watcher).await?;
                watcher = returned;
                reconciliation_pending |= rescan;
                continue;
            }
            WatcherWake::Event(first) => first,
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
        let finished = tokio::task::spawn_blocking(move || ingress.finish()).await;
        if let Err(error) = &finished {
            warn!(%error, "Watcher batch classification task failed");
        }
        let Ok(Some(mut batch)) = finished else {
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
                playlist_sidebar_refresh,
                scan_cancellation,
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
        // Rows this batch inserted, updated, or relocated: the only tracks
        // that can newly resolve an orphaned playlist entry.
        let mut upserted_tracks: Vec<track::Model> = Vec::new();
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
                            track_mutation_committed = true;
                            info!(from = %pair.from.display(), to = %pair.to.display(), id = %model.id, "Preserved track identity across filesystem rename");
                            upserted_tracks.push(*model);
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
                            // the foreign key; the surviving row can reclaim them,
                            // and a relocated row can satisfy retained path evidence.
                            upserted_tracks.extend(moved.iter().map(|(_, model)| model.clone()));

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
                                track_mutation_committed = true;
                                let t = db_model_to_track(&model);
                                let _ = tx.send(LibraryEvent::TrackUpserted(Box::new(t))).await;
                                upserted_tracks.push(*model);
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
            if let Err(error) = reconcile_watched_library(
                db.as_ref(),
                music_dirs,
                tx,
                playlist_sidebar_refresh,
                scan_cancellation,
            )
            .await
            {
                // Retry like a lost stream: this batch's changes are only
                // reflected once a reconciliation succeeds.
                warn!(%error, "Watcher-triggered library reconciliation failed; retry remains pending");
                reconciliation_pending = true;
                tokio::time::sleep(Duration::from_millis(WATCHER_RECONCILIATION_RETRY_MS)).await;
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
            &upserted_tracks,
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
            composer: Set(parsed.composer.clone()),
            genre: Set(parsed.genre.clone()),
            year: Set(parsed.year),
            track_number: Set(parsed.track_number.map(|n| n as i32)),
            disc_number: Set(parsed.disc_number.map(|n| n as i32)),
            duration_secs: Set(parsed.duration_secs.map(|d| d as i64)),
            bitrate_kbps: Set(parsed.bitrate_kbps.map(|b| b as i32)),
            sample_rate_hz: Set(parsed.sample_rate_hz.map(|s| s as i32)),
            format: Set(Some(parsed.format.clone())),
            play_count: Set(0),
            last_played_at_ms: Set(None),
            rating: Set(None),
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
/// holds after the SQL write and immediately before commit. The guard runs
/// while this transaction holds SQLite's write lock, so other connections'
/// writers wait for it within their busy timeout.
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
    let transaction = crate::db::begin_write(db).await?;
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
    let transaction = crate::db::begin_write(db).await?;
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
    active.composer = Set(parsed.composer.clone());
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
    let transaction = crate::db::begin_write(db).await?;

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

    let transaction = crate::db::begin_write(db).await?;

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LibrarySnapshotPublication {
    Published,
    StorageUnavailable,
    EventReceiverClosed,
}

impl LibrarySnapshotPublication {
    const fn category(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::StorageUnavailable => "storage",
            Self::EventReceiverClosed => "receiver-closed",
        }
    }
}

async fn send_library_snapshot(
    db: &DatabaseConnection,
    tx: &async_channel::Sender<LibraryEvent>,
) -> LibrarySnapshotPublication {
    let backend = super::backend::LocalBackend::new(db.clone());
    match crate::architecture::load_track_catalog(&backend).await {
        Ok(tracks) => {
            if tx.send(LibraryEvent::FullSync(tracks)).await.is_ok() {
                LibrarySnapshotPublication::Published
            } else {
                debug!(
                    category = LibrarySnapshotPublication::EventReceiverClosed.category(),
                    "Full library snapshot receiver closed"
                );
                LibrarySnapshotPublication::EventReceiverClosed
            }
        }
        Err(_) => {
            // Backend errors may contain private database values. Keep the
            // engine diagnostic useful without copying that chain into logs.
            warn!(
                category = LibrarySnapshotPublication::StorageUnavailable.category(),
                "Failed to load tracks for full sync"
            );
            LibrarySnapshotPublication::StorageUnavailable
        }
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
    let effective_album_artist = super::backend::effective_album_artist(
        model.album_artist_name.as_deref(),
        &model.artist_name,
    );
    Track {
        // `Track::id` is still required by compatibility APIs that accept a
        // UUID. Keep valid legacy UUIDs unchanged and map every other exact
        // SQLite key deterministically; never manufacture a different random
        // identity each time the same row is read. Queue identity uses the
        // byte-for-byte `native_track_id` below.
        id: Uuid::parse_str(&model.id)
            .unwrap_or_else(|_| Uuid::new_v5(&LOCAL_TRACK_COMPAT_NAMESPACE, model.id.as_bytes())),
        native_track_id: crate::architecture::TrackId::new(model.id.clone()).ok(),
        title: model.title.clone(),
        artist_name: model.artist_name.clone(),
        album_artist_name: model.album_artist_name.clone(),
        artist_id: Some(super::backend::local_artist_id(&model.artist_name)),
        album_title: model.album_title.clone(),
        album_id: Some(super::backend::local_album_id(
            &model.album_title,
            effective_album_artist,
        )),
        track_number: model.track_number.map(|n| n as u32),
        disc_number: model.disc_number.map(|n| n as u32),
        duration_secs: model.duration_secs.map(|d| d as u64),
        composer: model.composer.clone(),
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
        // Legacy/corrupt negative counts must never wrap into enormous UI
        // values while the repair migration is pending or being inspected.
        play_count: Some(u32::try_from(model.play_count).unwrap_or_default()),
        rating: TrackRating::writable(model.rating.and_then(|value| Rating::try_from(value).ok())),
        last_played: model
            .last_played_at_ms
            .and_then(chrono::DateTime::<Utc>::from_timestamp_millis),
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::QueryOrder;

    use super::*;

    /// jq5lT regression seam: when armed, the stale-deletion absence probe
    /// parks its blocking worker thread the way a removable/network root
    /// parks the kernel call, so the shutdown settle budget becomes
    /// observable. The flag is never armed outside the regression test, and
    /// the closure reference is compiled only under `cfg(test)`.
    static STALE_ABSENCE_PROBE_HELD: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Set just before the armed probe starts spinning, so the regression
    /// driver knows the scan is parked inside the absence proof.
    static STALE_ABSENCE_PROBE_ARRIVED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    pub(super) fn hold_stale_absence_probe() {
        if STALE_ABSENCE_PROBE_HELD.load(std::sync::atomic::Ordering::SeqCst) {
            STALE_ABSENCE_PROBE_ARRIVED.store(true, std::sync::atomic::Ordering::SeqCst);
            while STALE_ABSENCE_PROBE_HELD.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

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

    fn test_marker_identity() -> String {
        format!("{ROOT_IDENTITY_PREFIX}{}", Uuid::new_v4())
    }

    fn test_playlist_sidebar_refresh() -> PlaylistSidebarRefresh {
        let (refresh, _receiver) =
            super::super::playlist_sidebar::playlist_sidebar_refresh_channel();
        refresh
    }

    async fn insert_reauthorization_root(
        db: &DatabaseConnection,
        path: &Path,
        marker_identity: &str,
        identity_confirmed: bool,
    ) -> library_root::Model {
        library_root::ActiveModel {
            path: Set(path.to_string_lossy().into_owned()),
            device_id: Set(Some(marker_identity.to_string())),
            identity_confirmed: Set(identity_confirmed),
            is_available: Set(identity_confirmed),
            last_scan_complete: Set(true),
            last_checked_at: Set("2026-07-15T12:00:00Z".to_string()),
        }
        .insert(db)
        .await
        .expect("insert reauthorization root")
    }

    #[test]
    fn reauthorization_path_planning_is_component_aware_and_rejects_nested_scopes() {
        let source = directory_fixture_path("/music");
        let destination = directory_fixture_path("/portal/music");
        assert_eq!(
            retarget_descendant_path(
                &directory_fixture_path("/music/Album/song.flac"),
                &source,
                &destination,
            ),
            Some(directory_fixture_path("/portal/music/Album/song.flac"))
        );
        assert_eq!(
            retarget_descendant_path(
                &directory_fixture_path("/music2/song.flac"),
                &source,
                &destination,
            ),
            None,
            "a textual path prefix is not a descendant"
        );

        let nested = library_root::Model {
            path: directory_fixture_key("/music/nested"),
            device_id: Some(test_marker_identity()),
            identity_confirmed: true,
            is_available: true,
            last_scan_complete: true,
            last_checked_at: "2026-07-15T12:00:00Z".to_string(),
        };
        let request = RootReauthorizationRequest::new(Uuid::new_v4(), source, destination);
        assert!(validate_reauthorization_database_scopes(&[nested], &request, None).is_err());
    }

    #[tokio::test]
    async fn root_reauthorization_preserves_track_and_playlist_identity_atomically() {
        let db = rename_test_database().await;
        let old_root = directory_fixture_path("/legacy/Music");
        let new_root = directory_fixture_path("/portal/Music");
        let marker = test_marker_identity();
        let stored_root = insert_reauthorization_root(&db, &old_root, &marker, true).await;
        let source_path = old_root.join("Album").join("song.flac");
        let original = insert_rename_test_track(
            &db,
            "reauthorized-track",
            source_path.to_string_lossy().as_ref(),
            "Remembered",
            37,
        )
        .await;

        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_regular_playlist("Remembered playlist")
            .await
            .expect("create playlist");
        manager
            .add_track(&playlist.id, &original)
            .await
            .expect("link remembered track");
        let entry = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .one(&db)
            .await
            .expect("load playlist entry")
            .expect("playlist entry exists");
        let mut entry_with_match: playlist_entry::ActiveModel = entry.clone().into();
        entry_with_match.match_file_path = Set(Some(source_path.to_string_lossy().into_owned()));
        let entry_with_match = entry_with_match
            .update(&db)
            .await
            .expect("retain imported match path");

        let request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());
        relocate_library_root_rows(&db, &request, Some(&stored_root), &marker, || async {
            true
        })
        .await
        .expect("relocate library root rows");

        let destination_path = new_root.join("Album").join("song.flac");
        let moved = track::Entity::find_by_id(&original.id)
            .one(&db)
            .await
            .expect("load moved track")
            .expect("moved track exists");
        let mut expected_track = original.clone();
        expected_track.file_path = destination_path.to_string_lossy().into_owned();
        assert_eq!(moved, expected_track, "only the path may change");

        let moved_entry = playlist_entry::Entity::find_by_id(&entry.id)
            .one(&db)
            .await
            .expect("load moved playlist entry")
            .expect("playlist entry survives");
        let mut expected_entry = entry_with_match;
        expected_entry.match_file_path = Some(destination_path.to_string_lossy().into_owned());
        assert_eq!(moved_entry, expected_entry);
        assert_eq!(moved_entry.track_id.as_deref(), Some(original.id.as_str()));
        assert_eq!(
            moved_entry.local_track_id.as_deref(),
            Some(original.id.as_str())
        );

        assert!(
            library_root::Entity::find_by_id(old_root.to_string_lossy().as_ref())
                .one(&db)
                .await
                .expect("query old root")
                .is_none()
        );
        let moved_root = library_root::Entity::find_by_id(new_root.to_string_lossy().as_ref())
            .one(&db)
            .await
            .expect("query new root")
            .expect("new root state exists");
        assert_eq!(moved_root.device_id.as_deref(), Some(marker.as_str()));
        assert!(moved_root.identity_confirmed);
        assert!(!moved_root.is_available);
        assert!(!moved_root.last_scan_complete);

        let receipt = root_reauthorization_receipt::Entity::find_by_id(request.request_id.clone())
            .one(&db)
            .await
            .expect("query completion receipt")
            .expect("completion receipt exists");
        assert_eq!(receipt.old_path, old_root.to_string_lossy());
        assert_eq!(receipt.new_path, new_root.to_string_lossy());
        assert_eq!(receipt.marker_identity, marker);
    }

    #[tokio::test]
    async fn root_reauthorization_ignores_non_local_playlist_match_paths_defensively() {
        let db = rename_test_database().await;
        let old_root = directory_fixture_path("/legacy/Music");
        let new_root = directory_fixture_path("/portal/Music");
        let marker = test_marker_identity();
        let stored_root = insert_reauthorization_root(&db, &old_root, &marker, true).await;
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_regular_playlist("Corrupt remote path evidence")
            .await
            .expect("create playlist");
        let remote_match_path = old_root.join("Remote").join("song.flac");
        let remote_entry_id = Uuid::new_v4().to_string();

        // Migration 13 rejects this state. Insert it only to prove the
        // relocation boundary remains safe if an older or externally modified
        // database nevertheless contains remote path evidence.
        db.execute_unprepared("PRAGMA ignore_check_constraints = ON")
            .await
            .expect("disable checks for corrupt fixture");
        playlist_entry::ActiveModel {
            id: Set(remote_entry_id.clone()),
            playlist_id: Set(playlist.id),
            position: Set(0),
            source_id: Set(crate::architecture::SourceId::random().to_string()),
            track_id: Set(Some("remote-track".to_string())),
            local_track_id: Set(None),
            match_title: Set("Remote song".to_string()),
            match_artist: Set("Remote artist".to_string()),
            match_album: Set(String::new()),
            match_duration_secs: Set(None),
            match_file_path: Set(Some(remote_match_path.to_string_lossy().into_owned())),
        }
        .insert(&db)
        .await
        .expect("insert corrupt remote playlist entry");
        db.execute_unprepared("PRAGMA ignore_check_constraints = OFF")
            .await
            .expect("restore playlist checks");

        let request = RootReauthorizationRequest::new(Uuid::new_v4(), old_root, new_root.clone());
        relocate_library_root_rows(&db, &request, Some(&stored_root), &marker, || async {
            true
        })
        .await
        .expect("relocate local library root rows");

        let remote_entry = playlist_entry::Entity::find_by_id(remote_entry_id)
            .one(&db)
            .await
            .expect("load remote playlist entry")
            .expect("remote playlist entry survives");
        assert_eq!(
            remote_entry.match_file_path.as_deref(),
            Some(remote_match_path.to_string_lossy().as_ref()),
            "root relocation must never rewrite non-local source evidence"
        );
        assert!(
            library_root::Entity::find_by_id(new_root.to_string_lossy().as_ref())
                .one(&db)
                .await
                .expect("query relocated root")
                .is_some()
        );
    }

    #[tokio::test]
    async fn root_reauthorization_collision_and_commit_guard_failures_roll_back_every_row() {
        let db = rename_test_database().await;
        let old_root = directory_fixture_path("/legacy/Music");
        let new_root = directory_fixture_path("/portal/Music");
        let marker = test_marker_identity();
        let stored_root = insert_reauthorization_root(&db, &old_root, &marker, true).await;
        let source = insert_rename_test_track(
            &db,
            "source-identity",
            old_root.join("song.flac").to_string_lossy().as_ref(),
            "Source",
            11,
        )
        .await;
        let destination = insert_rename_test_track(
            &db,
            "destination-identity",
            new_root.join("song.flac").to_string_lossy().as_ref(),
            "Destination",
            22,
        )
        .await;
        let collision_request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());

        assert!(relocate_library_root_rows(
            &db,
            &collision_request,
            Some(&stored_root),
            &marker,
            || async { true },
        )
        .await
        .is_err());
        assert_eq!(
            track::Entity::find_by_id(&source.id)
                .one(&db)
                .await
                .expect("query source after collision"),
            Some(source.clone())
        );
        assert_eq!(
            track::Entity::find_by_id(&destination.id)
                .one(&db)
                .await
                .expect("query destination after collision"),
            Some(destination.clone())
        );
        track::Entity::delete_by_id(&destination.id)
            .exec(&db)
            .await
            .expect("remove collision fixture");

        let guarded_request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());
        assert!(relocate_library_root_rows(
            &db,
            &guarded_request,
            Some(&stored_root),
            &marker,
            || async { false },
        )
        .await
        .is_err());
        assert_eq!(
            track::Entity::find_by_id(&source.id)
                .one(&db)
                .await
                .expect("query source after guard rejection"),
            Some(source)
        );
        assert!(
            library_root::Entity::find_by_id(new_root.to_string_lossy().as_ref())
                .one(&db)
                .await
                .expect("query rolled-back destination root")
                .is_none()
        );
        assert!(root_reauthorization_receipt::Entity::find_by_id(
            guarded_request.request_id.clone()
        )
        .one(&db)
        .await
        .expect("query rolled-back receipt")
        .is_none());
    }

    #[tokio::test]
    async fn unsafe_descendant_rows_reject_reauthorization_without_partial_mutation() {
        let db = rename_test_database().await;
        let old_root = directory_fixture_path("/legacy/Music");
        let new_root = directory_fixture_path("/portal/Music");
        let marker = test_marker_identity();
        let stored_root = insert_reauthorization_root(&db, &old_root, &marker, true).await;
        let unsafe_path = old_root.join("Album").join("..").join("song.flac");
        let unsafe_track = insert_rename_test_track(
            &db,
            "unsafe-source-path",
            unsafe_path.to_string_lossy().as_ref(),
            "Unsafe",
            1,
        )
        .await;
        let track_request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());

        assert!(relocate_library_root_rows(
            &db,
            &track_request,
            Some(&stored_root),
            &marker,
            || async { true },
        )
        .await
        .is_err());
        assert_eq!(
            track::Entity::find_by_id(&unsafe_track.id)
                .one(&db)
                .await
                .expect("query unsafe track"),
            Some(unsafe_track.clone())
        );
        assert!(
            root_reauthorization_receipt::Entity::find_by_id(track_request.request_id.clone())
                .one(&db)
                .await
                .expect("query rejected track receipt")
                .is_none()
        );

        track::Entity::delete_by_id(&unsafe_track.id)
            .exec(&db)
            .await
            .expect("remove unsafe track fixture");
        let safe_track = insert_rename_test_track(
            &db,
            "safe-track-unsafe-match",
            old_root.join("song.flac").to_string_lossy().as_ref(),
            "Safe track",
            2,
        )
        .await;
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_regular_playlist("Unsafe match")
            .await
            .expect("create playlist");
        manager
            .add_track(&playlist.id, &safe_track)
            .await
            .expect("add safe track");
        let entry = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .one(&db)
            .await
            .expect("query playlist entry")
            .expect("playlist entry exists");
        let mut unsafe_entry: playlist_entry::ActiveModel = entry.into();
        unsafe_entry.match_file_path = Set(Some(unsafe_path.to_string_lossy().into_owned()));
        let unsafe_entry = unsafe_entry
            .update(&db)
            .await
            .expect("store unsafe imported path fixture");
        let entry_request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());

        assert!(relocate_library_root_rows(
            &db,
            &entry_request,
            Some(&stored_root),
            &marker,
            || async { true },
        )
        .await
        .is_err());
        assert_eq!(
            track::Entity::find_by_id(&safe_track.id)
                .one(&db)
                .await
                .expect("query rolled-back safe track"),
            Some(safe_track)
        );
        assert_eq!(
            playlist_entry::Entity::find_by_id(&unsafe_entry.id)
                .one(&db)
                .await
                .expect("query rolled-back unsafe match"),
            Some(unsafe_entry)
        );
        assert_eq!(
            library_root::Entity::find_by_id(old_root.to_string_lossy().as_ref())
                .one(&db)
                .await
                .expect("query retained old root"),
            Some(stored_root)
        );
        assert!(
            library_root::Entity::find_by_id(new_root.to_string_lossy().as_ref())
                .one(&db)
                .await
                .expect("query absent new root")
                .is_none()
        );
        assert!(
            root_reauthorization_receipt::Entity::find_by_id(entry_request.request_id.clone())
                .one(&db)
                .await
                .expect("query rejected entry receipt")
                .is_none()
        );
    }

    #[tokio::test]
    async fn markerless_explicit_reauthorization_moves_rows_but_requires_root_trust() {
        let db = rename_test_database().await;
        let fixture = TestDirectory::new("markerless-reauthorization");
        let old_root = fixture.path().join("legacy-path");
        let new_root = fixture.path().join("portal-path");
        std::fs::create_dir(&new_root).expect("create selected portal directory");
        std::fs::write(new_root.join("song.flac"), b"not parsed during preparation")
            .expect("create observed audio path");
        let original = insert_rename_test_track(
            &db,
            "legacy-track",
            old_root.join("song.flac").to_string_lossy().as_ref(),
            "Legacy",
            9,
        )
        .await;
        let request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());

        let resolution = resolve_root_reauthorization(&db, &request).await;
        assert!(matches!(
            resolution,
            RootReauthorizationResolution::UseNew(RootReauthorizationOutcome::Applied)
        ));
        let marker = read_root_marker(&new_root)
            .expect("read created marker")
            .expect("marker was created");
        let moved = track::Entity::find_by_id(&original.id)
            .one(&db)
            .await
            .expect("query moved legacy track")
            .expect("legacy track survives");
        assert_eq!(
            moved.file_path,
            new_root.join("song.flac").to_string_lossy()
        );
        assert_eq!(moved.play_count, original.play_count);

        let state = library_root::Entity::find_by_id(new_root.to_string_lossy().as_ref())
            .one(&db)
            .await
            .expect("query new root state")
            .expect("new root state exists");
        assert_eq!(state.device_id.as_deref(), Some(marker.as_str()));
        assert!(!state.identity_confirmed);
        assert!(!state.is_available);
        assert!(!state.last_scan_complete);
    }

    #[tokio::test]
    async fn confirmed_marker_mismatch_rejects_without_database_mutation() {
        let db = rename_test_database().await;
        let fixture = TestDirectory::new("confirmed-marker-mismatch");
        let old_root = fixture.path().join("legacy-path");
        let new_root = fixture.path().join("portal-path");
        std::fs::create_dir(&new_root).expect("create selected portal directory");
        let expected_marker = test_marker_identity();
        let observed_marker = create_root_marker(&new_root)
            .expect("create different destination marker")
            .identity;
        assert_ne!(expected_marker, observed_marker);
        let stored_root = insert_reauthorization_root(&db, &old_root, &expected_marker, true).await;
        let original = insert_rename_test_track(
            &db,
            "confirmed-track",
            old_root.join("song.flac").to_string_lossy().as_ref(),
            "Confirmed",
            3,
        )
        .await;
        let request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());

        assert!(prepare_root_reauthorization(&db, &request).await.is_err());
        assert_eq!(
            library_root::Entity::find_by_id(old_root.to_string_lossy().as_ref())
                .one(&db)
                .await
                .expect("query source state"),
            Some(stored_root)
        );
        assert_eq!(
            track::Entity::find_by_id(&original.id)
                .one(&db)
                .await
                .expect("query remembered track"),
            Some(original)
        );
        assert!(
            root_reauthorization_receipt::Entity::find_by_id(request.request_id.clone())
                .one(&db)
                .await
                .expect("query absent receipt")
                .is_none()
        );
    }

    #[tokio::test]
    async fn matching_confirmed_marker_reauthorizes_end_to_end() {
        let db = rename_test_database().await;
        let fixture = TestDirectory::new("confirmed-marker-reauthorization");
        let old_root = fixture.path().join("legacy-path");
        let new_root = fixture.path().join("portal-path");
        std::fs::create_dir(&new_root).expect("create selected portal directory");
        let marker = create_root_marker(&new_root)
            .expect("create destination marker")
            .identity;
        std::fs::write(new_root.join("song.flac"), b"enumeration fixture")
            .expect("create destination audio path");
        insert_reauthorization_root(&db, &old_root, &marker, true).await;
        let original = insert_rename_test_track(
            &db,
            "confirmed-track-success",
            old_root.join("song.flac").to_string_lossy().as_ref(),
            "Confirmed",
            8,
        )
        .await;
        let request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());

        assert!(matches!(
            resolve_root_reauthorization(&db, &request).await,
            RootReauthorizationResolution::UseNew(RootReauthorizationOutcome::Applied)
        ));
        let moved = track::Entity::find_by_id(&original.id)
            .one(&db)
            .await
            .expect("query moved confirmed track")
            .expect("confirmed track survives");
        assert_eq!(
            moved.file_path,
            new_root.join("song.flac").to_string_lossy()
        );
        assert_eq!(moved.play_count, original.play_count);
        let state = library_root::Entity::find_by_id(new_root.to_string_lossy().as_ref())
            .one(&db)
            .await
            .expect("query moved confirmed root")
            .expect("confirmed root survives");
        assert_eq!(state.device_id.as_deref(), Some(marker.as_str()));
        assert!(state.identity_confirmed);
    }

    #[tokio::test]
    async fn receipt_retry_is_idempotent_and_inconsistent_receipt_scans_neither_path() {
        let db = rename_test_database().await;
        let old_root = directory_fixture_path("/legacy/Music");
        let new_root = directory_fixture_path("/portal/Music");
        let marker = test_marker_identity();
        insert_reauthorization_root(&db, &new_root, &marker, false).await;
        let request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());
        root_reauthorization_receipt::ActiveModel {
            request_id: Set(request.request_id.clone()),
            old_path: Set(old_root.to_string_lossy().into_owned()),
            new_path: Set(new_root.to_string_lossy().into_owned()),
            marker_identity: Set(marker),
            completed_at: Set("2026-07-15T12:00:00Z".to_string()),
        }
        .insert(&db)
        .await
        .expect("insert committed receipt");

        let (tx, rx) = async_channel::bounded(2);
        let effective = resolve_pending_root_reauthorizations(
            &db,
            vec![old_root.clone()],
            std::slice::from_ref(&request),
            &tx,
        )
        .await;
        assert_eq!(effective.as_slice(), std::slice::from_ref(&new_root));
        assert!(matches!(
            rx.recv().await.expect("receive retry outcome"),
            LibraryEvent::RootReauthorizationFinished {
                outcome: RootReauthorizationOutcome::AlreadyApplied,
                ..
            }
        ));
        assert!(matches!(
            classify_root_reauthorization_error(
                &db,
                &request,
                anyhow::anyhow!("simulated ambiguous COMMIT result"),
            )
            .await,
            RootReauthorizationResolution::UseNew(RootReauthorizationOutcome::AlreadyApplied)
        ));

        let uncommitted = RootReauthorizationRequest::new(
            Uuid::new_v4(),
            directory_fixture_path("/legacy/Other"),
            directory_fixture_path("/portal/Other"),
        );
        assert!(matches!(
            classify_root_reauthorization_error(
                &db,
                &uncommitted,
                anyhow::anyhow!("simulated rejected transaction"),
            )
            .await,
            RootReauthorizationResolution::KeepOld(_)
        ));

        library_root::Entity::delete_by_id(new_root.to_string_lossy().as_ref())
            .exec(&db)
            .await
            .expect("corrupt receipt-backed root state");
        assert!(matches!(
            classify_root_reauthorization_error(
                &db,
                &request,
                anyhow::anyhow!("simulated ambiguous COMMIT result"),
            )
            .await,
            RootReauthorizationResolution::ScanNeither(_)
        ));
        let effective = resolve_pending_root_reauthorizations(
            &db,
            vec![old_root],
            std::slice::from_ref(&request),
            &tx,
        )
        .await;
        assert!(effective.is_empty());
        assert!(matches!(
            rx.recv().await.expect("receive inconsistent outcome"),
            LibraryEvent::RootReauthorizationFinished {
                outcome: RootReauthorizationOutcome::Inconsistent,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn durable_receipt_takes_precedence_over_invalid_later_config() {
        let db = rename_test_database().await;
        let old_root = directory_fixture_path("/legacy/Music");
        let new_root = directory_fixture_path("/portal/Music");
        let marker = test_marker_identity();
        insert_reauthorization_root(&db, &new_root, &marker, false).await;
        let request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());
        root_reauthorization_receipt::ActiveModel {
            request_id: Set(request.request_id.clone()),
            old_path: Set(old_root.to_string_lossy().into_owned()),
            new_path: Set(new_root.to_string_lossy().into_owned()),
            marker_identity: Set(marker),
            completed_at: Set("2026-07-15T12:00:00Z".to_string()),
        }
        .insert(&db)
        .await
        .expect("insert committed receipt");
        // A playlist can be imported after the relocation committed but
        // before config recovery. Its old-path match evidence is not proof
        // that the earlier atomic relocation was partial.
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_regular_playlist("Imported after relocation")
            .await
            .expect("create later playlist");
        playlist_entry::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            playlist_id: Set(playlist.id),
            position: Set(0),
            source_id: Set(crate::architecture::SourceId::local().to_string()),
            track_id: Set(None),
            local_track_id: Set(None),
            match_title: Set("Remembered".to_string()),
            match_artist: Set(String::new()),
            match_album: Set(String::new()),
            match_duration_secs: Set(None),
            match_file_path: Set(Some(
                old_root.join("song.flac").to_string_lossy().into_owned(),
            )),
        }
        .insert(&db)
        .await
        .expect("insert later old-path match evidence");

        let (tx, rx) = async_channel::bounded(1);
        let effective = resolve_pending_root_reauthorizations(
            &db,
            vec![
                old_root,
                new_root.clone(),
                new_root.join("manually-added-nested-root"),
            ],
            std::slice::from_ref(&request),
            &tx,
        )
        .await;

        assert_eq!(effective, [new_root]);
        assert!(matches!(
            rx.recv().await.expect("receive receipt-backed outcome"),
            LibraryEvent::RootReauthorizationFinished {
                outcome: RootReauthorizationOutcome::AlreadyApplied,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn malformed_request_id_quarantines_both_endpoint_scopes() {
        let db = rename_test_database().await;
        let old_root = directory_fixture_path("/legacy/Music");
        let new_root = directory_fixture_path("/portal/Music");
        let unrelated = directory_fixture_path("/other/Music");
        let request =
            RootReauthorizationRequest::new("not-a-uuid", old_root.clone(), new_root.clone());
        let (tx, rx) = async_channel::bounded(1);

        let effective = resolve_pending_root_reauthorizations(
            &db,
            vec![
                old_root.clone(),
                old_root.join("nested"),
                new_root.clone(),
                new_root.join("nested"),
                unrelated.clone(),
            ],
            std::slice::from_ref(&request),
            &tx,
        )
        .await;

        assert_eq!(effective, [unrelated]);
        assert!(matches!(
            rx.recv().await.expect("receive malformed-intent outcome"),
            LibraryEvent::RootReauthorizationFinished {
                request_id,
                outcome: RootReauthorizationOutcome::Inconsistent,
                ..
            } if request_id == "not-a-uuid"
        ));
    }

    #[tokio::test]
    async fn rejected_intent_quarantines_a_destination_already_added_to_config() {
        let db = rename_test_database().await;
        let old_root = directory_fixture_path("/legacy/Music");
        let new_root = directory_fixture_path("/portal/Music");
        let request =
            RootReauthorizationRequest::new(Uuid::new_v4(), old_root.clone(), new_root.clone());
        let (tx, rx) = async_channel::bounded(1);

        let effective = resolve_pending_root_reauthorizations(
            &db,
            vec![
                old_root.clone(),
                old_root.join("nested-config"),
                new_root.join("nested-config"),
                new_root,
            ],
            std::slice::from_ref(&request),
            &tx,
        )
        .await;

        assert_eq!(effective, [old_root]);
        assert!(matches!(
            rx.recv().await.expect("receive rejected outcome"),
            LibraryEvent::RootReauthorizationFinished {
                outcome: RootReauthorizationOutcome::Rejected,
                ..
            }
        ));
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

    fn scanned_paths(audio_files: &[(PathBuf, String)]) -> Vec<PathBuf> {
        audio_files.iter().map(|(path, _)| path.clone()).collect()
    }

    #[test]
    fn watcher_ingress_filters_access_noise_before_the_bounded_queue() {
        use notify::event::{AccessKind, AccessMode, Flag, MetadataKind, ModifyKind};
        use notify::EventKind;

        let (tx, mut rx) = mpsc::channel(1);
        let overflowed = AtomicBool::new(false);

        for index in 0..1_024 {
            let kind = match index % 5 {
                0 => EventKind::Access(AccessKind::Open(AccessMode::Read)),
                1 => EventKind::Access(AccessKind::Open(AccessMode::Any)),
                2 => EventKind::Access(AccessKind::Read),
                3 => EventKind::Access(AccessKind::Close(AccessMode::Read)),
                _ => EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
            };
            enqueue_watcher_result(
                &tx,
                &overflowed,
                Ok(notify::Event::new(kind)
                    .add_path(PathBuf::from(format!("/music/{index}.flac")))),
            );
        }

        assert!(!overflowed.load(Ordering::Acquire));
        assert!(
            rx.try_recv().is_err(),
            "access noise must not enter the queue"
        );

        let create = notify::Event::new(EventKind::Create(notify::event::CreateKind::File))
            .add_path(PathBuf::from("/music/real-change.flac"));
        enqueue_watcher_result(&tx, &overflowed, Ok(create));
        let dropped = notify::Event::new(EventKind::Remove(notify::event::RemoveKind::File))
            .add_path(PathBuf::from("/music/second-real-change.flac"));
        enqueue_watcher_result(&tx, &overflowed, Ok(dropped));

        assert!(
            overflowed.load(Ordering::Acquire),
            "real mutation overflow must remain authoritative"
        );
        assert_eq!(
            rx.try_recv()
                .expect("real change remains queueable after the access storm")
                .expect("queued notify event")
                .paths,
            [PathBuf::from("/music/real-change.flac")]
        );
        overflowed.store(false, Ordering::Release);

        let rescan = notify::Event::new(EventKind::Access(AccessKind::Read))
            .set_flag(Flag::Rescan)
            .add_path(PathBuf::from("/music"));
        enqueue_watcher_result(&tx, &overflowed, Ok(rescan));
        assert!(rx
            .try_recv()
            .expect("backend rescan evidence must survive access filtering")
            .expect("queued notify event")
            .need_rescan());

        enqueue_watcher_result(
            &tx,
            &overflowed,
            Err(notify::Error::generic("backend stream failed")),
        );
        assert!(rx
            .try_recv()
            .expect("watcher errors must survive access filtering")
            .is_err());
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

    /// Construct an idle watcher backend for tests that never install
    /// watches, or `None` only when the host has no watcher capacity.
    ///
    /// `RecommendedWatcher::new` claims one inotify instance, and
    /// `fs.inotify.max_user_instances` is a per-user kernel cap. On a shared
    /// host other tenants can hold every instance, so construction can fail
    /// with EMFILE no matter what the code under test does — host capacity,
    /// not a watcher-contract regression. EMFILE alone is ambiguous (the
    /// same errno also reports per-process descriptor exhaustion), so the
    /// skip only fires when the process's own descriptor table demonstrably
    /// has headroom — see `is_inotify_capacity_errno`. The deterministic
    /// assertions these tests make are unchanged on a healthy host. Any
    /// non-capacity construction error — including EMFILE caused by real
    /// descriptor exhaustion, which a descriptor-leak regression would
    /// produce — panics so the test fails loudly; the skip is scoped by
    /// `is_watcher_backend_capacity_error`, unit-tested below.
    fn idle_watcher_backend_or_skip() -> Option<RecommendedWatcher> {
        match RecommendedWatcher::new(
            |_: notify::Result<notify::Event>| {},
            notify::Config::default(),
        ) {
            Ok(backend) => Some(backend),
            Err(error) if is_watcher_backend_capacity_error(&error) => {
                eprintln!("skipping watcher test: host has no watcher capacity: {error}");
                None
            }
            Err(error) => panic!("construct idle watcher backend: {error}"),
        }
    }

    /// Linux errno values for the inotify capacity failures, kept as raw
    /// numbers to avoid a libc dev-dependency: ENOSPC (28) unambiguously
    /// means `fs.inotify.max_user_watches` is exhausted, while EMFILE (24)
    /// is ambiguous — inotify_init(2) reports it both when
    /// `fs.inotify.max_user_instances` is exhausted and when the process's
    /// open-descriptor limit is reached — so it needs independent evidence
    /// before it may read as capacity. Gated to Linux because
    /// `raw_os_error()` is only meaningful in the target OS's namespace —
    /// see `is_inotify_capacity_errno`.
    #[cfg(target_os = "linux")]
    const EMFILE: i32 = 24;
    #[cfg(target_os = "linux")]
    const ENOSPC: i32 = 28;

    /// A manufactured non-capacity control for the decision tests. On Linux
    /// it is a real errno outside the capacity set; off Linux the predicate
    /// is unconditionally false and this case documents that.
    const EPERM: i32 = 1;

    /// The same numerals the Linux arm matches, kept for the inverse
    /// control below: off Linux they live in the host OS's errno namespace
    /// (per-process fd exhaustion on macOS, unrelated Win32 codes on
    /// Windows) and must never read as inotify capacity.
    #[cfg(not(target_os = "linux"))]
    const RAW_ERRNO_24: i32 = 24;
    #[cfg(not(target_os = "linux"))]
    const RAW_ERRNO_28: i32 = 28;

    /// True only when `error` is the documented shared-host capacity
    /// condition: the kernel refused inotify state because a per-user limit
    /// is exhausted — ENOSPC (watch descriptors) on Linux, EMFILE (instances)
    /// on Linux only with independent descriptor-headroom evidence, or
    /// notify's explicit portable `MaxFilesWatch`. Every other
    /// error — a backend initialization, configuration, or platform
    /// regression such as EPERM — must fail the test instead of skipping
    /// it, so this predicate is deliberately narrow and unit-tested in both
    /// directions below.
    fn is_watcher_backend_capacity_error(error: &notify::Error) -> bool {
        match &error.kind {
            notify::ErrorKind::Io(io_error) => is_inotify_capacity_errno(io_error.raw_os_error()),
            notify::ErrorKind::MaxFilesWatch => true,
            _ => false,
        }
    }

    /// Linux errno namespace: 28 (ENOSPC) is unambiguously the inotify
    /// max_user_watches capacity limit. 24 (EMFILE) is ambiguous between
    /// that per-user saturation and ordinary per-process descriptor
    /// exhaustion, so it classifies as capacity only when the process's own
    /// descriptor table demonstrably has headroom, which rules out
    /// descriptor exhaustion — see `process_fd_table_has_headroom`.
    #[cfg(target_os = "linux")]
    fn is_inotify_capacity_errno(raw_os_error: Option<i32>) -> bool {
        match raw_os_error {
            Some(ENOSPC) => true,
            Some(EMFILE) => emfile_is_inotify_capacity(process_fd_table_has_headroom()),
            _ => false,
        }
    }

    /// Decision core for the ambiguous Linux EMFILE, parameterized on the
    /// independent descriptor-table evidence so both directions stay
    /// unit-testable without mutating host descriptor state: with headroom,
    /// EMFILE means `fs.inotify.max_user_instances` exhaustion (capacity,
    /// skip); without it, EMFILE is descriptor exhaustion in this process —
    /// precisely the leak class these watcher tests exist to catch — and
    /// must fail the test instead of skipping it.
    #[cfg(target_os = "linux")]
    fn emfile_is_inotify_capacity(fd_table_has_headroom: bool) -> bool {
        fd_table_has_headroom
    }

    /// Independent EMFILE discriminator: true only when the process's open
    /// descriptor count is demonstrably below its `RLIMIT_NOFILE` soft
    /// limit. Undeterminable state (`/proc` unreadable or unparsable)
    /// returns false — no evidence, no skip; a skip needs positive proof
    /// the errno cannot be an ordinary descriptor failure.
    #[cfg(target_os = "linux")]
    fn process_fd_table_has_headroom() -> bool {
        match (open_fd_count(), soft_fd_limit()) {
            (Some(used), Some(limit)) => used < limit,
            _ => false,
        }
    }

    /// Number of open descriptors, counted from `/proc/self/fd` (includes
    /// the descriptor `read_dir` itself holds while listing).
    #[cfg(target_os = "linux")]
    fn open_fd_count() -> Option<u64> {
        let entries = std::fs::read_dir("/proc/self/fd").ok()?;
        Some(entries.filter_map(Result::ok).count() as u64)
    }

    /// Soft `RLIMIT_NOFILE` parsed from `/proc/self/limits`; `u64::MAX`
    /// stands in for an `unlimited` soft limit.
    #[cfg(target_os = "linux")]
    fn soft_fd_limit() -> Option<u64> {
        let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
        for line in limits.lines() {
            if let Some(rest) = line.strip_prefix("Max open files") {
                return match rest.split_whitespace().next()? {
                    "unlimited" => Some(u64::MAX),
                    soft => soft.parse().ok(),
                };
            }
        }
        None
    }

    /// Non-Linux errno namespaces: `raw_os_error()` reports the host OS's
    /// codes, where 24 and 28 mean unrelated things (per-process fd
    /// exhaustion on macOS, unrelated Win32 errors on Windows), so no raw
    /// errno identifies inotify capacity here — only notify's own
    /// `MaxFilesWatch` kind does.
    #[cfg(not(target_os = "linux"))]
    fn is_inotify_capacity_errno(raw_os_error: Option<i32>) -> bool {
        let _ = raw_os_error;
        false
    }

    #[test]
    fn watcher_backend_capacity_decision_skips_capacity_errors() {
        // Positive control: the portable capacity class maps to skip on
        // every target. The Linux errno classes are covered on Linux by
        // `watcher_backend_capacity_decision_skips_linux_capacity_errnos`.
        let capacity_errors = [(
            "notify MaxFilesWatch",
            notify::Error::new(notify::ErrorKind::MaxFilesWatch),
        )];
        for (name, error) in capacity_errors {
            assert!(
                is_watcher_backend_capacity_error(&error),
                "{name} is host capacity and must skip"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watcher_backend_capacity_decision_skips_linux_capacity_errnos() {
        // Positive control: ENOSPC is unambiguously the inotify
        // max_user_watches host-capacity errno and maps to skip through the
        // full predicate. Raw errno values are only the inotify capacity set
        // on Linux, so this control is target-gated like the predicate arm
        // it exercises.
        let enospc = notify::Error::new(notify::ErrorKind::Io(std::io::Error::from_raw_os_error(
            ENOSPC,
        )));
        assert!(
            is_watcher_backend_capacity_error(&enospc),
            "ENOSPC (max_user_watches) is host capacity and must skip"
        );
        // EMFILE needs independent descriptor-headroom evidence before it
        // may read as max_user_instances capacity, so the positive
        // assertion runs through the evidence-parameterized decision core
        // the live arm delegates to; the inverse direction is pinned in
        // `watcher_backend_capacity_decision_fails_emfile_without_fd_headroom`.
        assert!(
            emfile_is_inotify_capacity(true),
            "EMFILE with descriptor-table headroom (max_user_instances) is host capacity and must skip"
        );
        // Wiring check: the live predicate's EMFILE arm must consult
        // exactly this evidence, so its verdict tracks the measured
        // descriptor state of this process.
        assert_eq!(
            is_inotify_capacity_errno(Some(EMFILE)),
            process_fd_table_has_headroom(),
            "live EMFILE classification must track descriptor-headroom evidence"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watcher_backend_capacity_decision_fails_emfile_without_fd_headroom() {
        // Discriminating inverse control for the ambiguous EMFILE arm:
        // without descriptor-headroom evidence, EMFILE must NOT read as
        // inotify capacity. inotify_init(2) also returns EMFILE when the
        // process's own descriptor table is exhausted — exactly what a
        // descriptor-leak regression produces — and that failure must fail
        // the watcher tests loudly instead of silently skipping them.
        assert!(
            !emfile_is_inotify_capacity(false),
            "EMFILE without descriptor headroom is process fd exhaustion, not host capacity; it must fail the test"
        );
    }

    #[test]
    fn watcher_backend_capacity_decision_fails_non_capacity_errors() {
        // A backend, configuration, or platform regression is a real defect:
        // the only tests exercising real watcher installation must fail, not
        // skip. Off Linux every Io errno is a non-capacity host error, so
        // these cases additionally pin the portable predicate to
        // MaxFilesWatch-only skips there.
        let non_capacity_errors = [
            (
                "EPERM (hardened runner)",
                notify::Error::new(notify::ErrorKind::Io(std::io::Error::from_raw_os_error(
                    EPERM,
                ))),
            ),
            (
                "non-OS io error",
                notify::Error::new(notify::ErrorKind::Io(std::io::Error::other(
                    "backend initialization failed",
                ))),
            ),
            (
                "notify Generic",
                notify::Error::new(notify::ErrorKind::Generic(
                    "backend initialization failed".to_string(),
                )),
            ),
            (
                "notify InvalidConfig",
                notify::Error::new(notify::ErrorKind::InvalidConfig(notify::Config::default())),
            ),
            (
                "notify WatchNotFound",
                notify::Error::new(notify::ErrorKind::WatchNotFound),
            ),
            (
                "notify PathNotFound",
                notify::Error::new(notify::ErrorKind::PathNotFound),
            ),
        ];
        for (name, error) in non_capacity_errors {
            assert!(
                !is_watcher_backend_capacity_error(&error),
                "{name} is not host capacity and must fail the test"
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn watcher_backend_capacity_decision_fails_capacity_errnos_on_non_linux() {
        // Inverse control for
        // `watcher_backend_capacity_decision_skips_linux_capacity_errnos`:
        // the numerals 24/28 are inotify capacity only in the Linux errno
        // namespace. Off Linux they mean unrelated host conditions (macOS 24
        // is per-process fd exhaustion; Windows 24/28 are unrelated Win32
        // codes), and classifying either as capacity would false-skip the
        // only real-watcher-installation tests on CI's macOS and Windows
        // legs. Pin them to the fail-loudly path here.
        let host_namespace_numerals = [
            ("raw 24 (non-Linux namespace)", RAW_ERRNO_24),
            ("raw 28 (non-Linux namespace)", RAW_ERRNO_28),
        ];
        for (name, raw) in host_namespace_numerals {
            let error = notify::Error::new(notify::ErrorKind::Io(
                std::io::Error::from_raw_os_error(raw),
            ));
            assert!(
                !is_watcher_backend_capacity_error(&error),
                "{name} must not be classified as inotify capacity off Linux"
            );
        }
    }

    #[test]
    fn watcher_retries_a_root_that_appears_during_bootstrap() {
        let library = TestDirectory::new("watcher-registration-retry");
        let ready = library.path().join("ready");
        let late = library.path().join("late");
        std::fs::create_dir(&ready).expect("create initially available root");

        // install_directory_watcher fails only when the backend cannot be
        // constructed (its per-directory watch errors are logged and
        // skipped inside). A capacity error is the saturated-host condition
        // above and skips; any other construction error is a real watcher
        // regression and must still fail this test.
        let mut watcher = match install_directory_watcher(&[ready.clone(), late.clone()]) {
            Ok(watcher) => watcher,
            Err(error) if is_watcher_backend_capacity_error(&error) => {
                eprintln!(
                    "skipping watcher_retries_a_root_that_appears_during_bootstrap: \
                     host has no watcher capacity: {error}"
                );
                return;
            }
            Err(error) => panic!("install directory watcher: {error}"),
        };
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

        assert!(ingress.events.is_empty());
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

    /// End-to-end watcher-backlog/root-confirmation ordering harness
    /// (docs/task.md P3.4). Drives the real `process_directory_events` loop
    /// with a synthetic event channel: a genuine `RecommendedWatcher` backend
    /// with zero installed watches contributes no platform event timing, so
    /// the ordering contract is exercised deterministically without the cost
    /// of a live inotify/FSEvents/ReadDirectoryChangesW fixture.
    ///
    /// A marker mutation mixed with an incremental upsert in one debounced
    /// batch must be consumed entirely by root confirmation: the confirmation
    /// scan publishes `FullSync` + `ScanComplete`, and no per-track
    /// incremental event may precede that boundary. A further event queued
    /// behind the batch (the watcher backlog) applies only after the root is
    /// re-confirmed.
    #[tokio::test]
    async fn marker_mutation_confirms_root_before_backlog_incrementals_end_to_end() {
        let db = Arc::new(rename_test_database().await);
        let fixture = TestDirectory::new("watcher-backlog-root-confirmation-ordering");
        let root = fixture.path().to_path_buf();
        let marker = create_root_marker(&root)
            .expect("create durable root marker")
            .identity;
        insert_reauthorization_root(&db, &root, &marker, true).await;

        let audio = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/audio/silence.flac"
        ));
        let in_batch_path = root.join("in-batch.flac");
        let victim_path = root.join("victim.flac");
        std::fs::write(&in_batch_path, audio).expect("write in-batch audio fixture");
        std::fs::write(&victim_path, audio).expect("write victim audio fixture");
        insert_rename_test_track(
            &db,
            "backlog-victim",
            victim_path.to_string_lossy().as_ref(),
            "Backlog Victim",
            3,
        )
        .await;

        // Synthetic watcher: a real backend with zero installed watches, fed
        // by a deterministic channel the harness controls. The backend is a
        // typed sink only; a shared host with every inotify instance leased
        // by other tenants cannot supply one, which is capacity, not an
        // ordering-contract failure, so the harness skips.
        let (event_tx, event_rx) = mpsc::channel(WATCHER_EVENT_CAPACITY);
        let Some(idle_backend) = idle_watcher_backend_or_skip() else {
            return;
        };
        let watcher = DirectoryWatcher {
            watcher: idle_backend,
            rx: event_rx,
            ingress_overflowed: Arc::new(AtomicBool::new(false)),
            watched_directories: HashSet::new(),
            root_presence: HashMap::new(),
            root_probe_interval: ROOT_PROBE_INTERVAL,
        };

        let (library_events, library_event_rx) = async_channel::unbounded();
        let (_command_tx, command_rx) = async_channel::unbounded::<LibraryCommand>();
        let playlist_sidebar_refresh = test_playlist_sidebar_refresh();
        let mut completed_commands = HashMap::new();

        // Debounced batch 1: a marker mutation mixed with an incremental
        // upsert. The whole batch must be consumed by root confirmation.
        event_tx
            .send(Ok(notify::Event::new(notify::EventKind::Create(
                notify::event::CreateKind::File,
            ))
            .add_path(root_identity_path(&root))))
            .await
            .expect("queue marker mutation");
        event_tx
            .send(Ok(notify::Event::new(notify::EventKind::Create(
                notify::event::CreateKind::File,
            ))
            .add_path(in_batch_path.clone())))
            .await
            .expect("queue in-batch upsert");

        // Land the backlog event only after batch 1's debounce deadline
        // (WATCHER_DEBOUNCE_MS) has passed with margin, so it queues behind
        // the root-confirmation scan instead of joining the marker batch.
        let driver = async {
            tokio::time::sleep(Duration::from_millis(2_500)).await;
            std::fs::remove_file(&victim_path).expect("delete victim file for backlog remove");
            event_tx
                .send(Ok(notify::Event::new(notify::EventKind::Remove(
                    notify::event::RemoveKind::File,
                ))
                .add_path(victim_path.clone())))
                .await
                .expect("queue backlog remove");
            drop(event_tx);
        };

        let never_cancelled = CancellationToken::new();
        let (loop_result, ()) = tokio::join!(
            process_directory_events(
                &db,
                std::slice::from_ref(&root),
                &library_events,
                &command_rx,
                &mut completed_commands,
                watcher,
                &playlist_sidebar_refresh,
                &never_cancelled,
            ),
            driver,
        );
        loop_result.expect("watcher loop exits cleanly");

        let events: Vec<LibraryEvent> =
            std::iter::from_fn(|| library_event_rx.try_recv().ok()).collect();

        // Root confirmation is the scan boundary: no per-track incremental
        // event may precede the confirmation scan's ScanComplete.
        let scan_complete = events
            .iter()
            .position(|event| matches!(event, LibraryEvent::ScanComplete))
            .expect("confirmation scan completes");
        for event in &events[..scan_complete] {
            assert!(
                !matches!(event, LibraryEvent::TrackUpserted(_)),
                "incremental upsert applied before root confirmation: {event:?}"
            );
        }

        // The confirmation scan — not the discarded marker batch — indexed
        // both on-disk files through the authoritative snapshot.
        let full_sync = events[..scan_complete]
            .iter()
            .find_map(|event| match event {
                LibraryEvent::FullSync(tracks) => Some(tracks),
                _ => None,
            })
            .expect("confirmation scan publishes FullSync");
        assert!(full_sync.iter().any(|track| {
            track.file_path.as_deref() == Some(in_batch_path.to_string_lossy().as_ref())
        }));
        assert!(full_sync.iter().any(|track| {
            track.file_path.as_deref() == Some(victim_path.to_string_lossy().as_ref())
        }));

        // The queued backlog remove applied only after the confirmation
        // boundary, against the re-confirmed root.
        assert!(events[scan_complete + 1..].iter().any(|event| matches!(
            event,
            LibraryEvent::TrackRemoved(path)
                if path == victim_path.to_string_lossy().as_ref()
        )));

        let root_state = library_root::Entity::find_by_id(root.to_string_lossy().as_ref())
            .one(db.as_ref())
            .await
            .expect("query root state")
            .expect("root row survives");
        assert!(root_state.identity_confirmed);
        assert!(root_state.is_available);
        assert!(root_state.last_scan_complete);

        assert!(track::Entity::find()
            .filter(track::Column::FilePath.eq(victim_path.to_string_lossy().as_ref()))
            .one(db.as_ref())
            .await
            .expect("query removed victim")
            .is_none());
        assert!(track::Entity::find()
            .filter(track::Column::FilePath.eq(in_batch_path.to_string_lossy().as_ref()))
            .one(db.as_ref())
            .await
            .expect("query scan-indexed file")
            .is_some());
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
    fn watcher_batch_name_any_alone_demands_reconciliation_without_identity() {
        // Standalone coverage for the backend rename shape FSEvents and kqueue
        // emit: one unpaired Name::Any event, with no folder removal or other
        // event that could mask the routing decision under test.
        let mut batch = WatcherBatch::default();
        batch.collect(rename_event(
            notify::event::RenameMode::Any,
            &["/music/unknown"],
            None,
        ));
        batch.finish();

        assert!(
            batch.reconciliation_required,
            "an unpaired Name::Any rename must request the guarded reconciliation scan"
        );
        assert!(
            batch.rename_pairs.is_empty()
                && batch.upsert_paths.is_empty()
                && batch.remove_paths.is_empty()
                && batch.deferred_paths.is_empty()
                && batch.dirty_directory_scopes.is_empty(),
            "Name::Any alone must never infer identity, defer, or dirty a scope"
        );
        assert!(!batch.requires_reconciliation_before_incrementals());
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

    #[test]
    fn library_enumeration_ignores_private_tag_write_siblings() {
        let library = TestDirectory::new("tag-write-scan-exclusion");
        let track = library.path().join("track.flac");
        let sibling = library
            .path()
            .join(".tributary-tag-00000000-0000-4000-8000-000000000000.flac");
        std::fs::write(&track, b"audio").expect("create public audio path");
        std::fs::write(&sibling, b"copy").expect("create private tag sibling");

        let (audio_files, private_siblings, errors) =
            enumerate_audio_files(library.path(), None, &[]);

        assert!(errors.is_empty());
        assert_eq!(audio_files, vec![track]);
        assert_eq!(private_siblings, vec![sibling]);
    }

    #[test]
    fn watcher_ignores_tag_siblings_and_refreshes_the_replaced_track() {
        let library = TestDirectory::new("tag-write-watcher-exclusion");
        let track = library.path().join("track.flac");
        let sibling = library
            .path()
            .join(".tributary-tag-00000000-0000-4000-8000-000000000000.flac");
        std::fs::write(&sibling, b"copy in progress").expect("create private tag sibling");

        let mut batch = WatcherBatch::default();
        batch.record_upsert(sibling.clone());
        std::fs::remove_file(&sibling).expect("finish private copy");
        batch.record_remove(sibling.clone());
        assert!(
            batch.is_empty(),
            "private sibling create/remove events must be invisible"
        );

        std::fs::write(&track, b"tagged audio").expect("publish tagged track");
        batch.record_remove(track.clone());
        batch.collect(rename_event(
            notify::event::RenameMode::Both,
            &[sibling.to_str().unwrap(), track.to_str().unwrap()],
            None,
        ));

        assert_eq!(batch.upsert_paths, HashSet::from([track.clone()]));
        assert!(batch.rename_pairs.is_empty());
        assert!(batch.deferred_paths.is_empty());
        assert!(!batch.reconciliation_required);

        let mut tracked_split = WatcherBatch::default();
        tracked_split.collect(rename_event(
            notify::event::RenameMode::From,
            &[sibling.to_str().unwrap()],
            Some(7),
        ));
        tracked_split.collect(rename_event(
            notify::event::RenameMode::To,
            &[track.to_str().unwrap()],
            Some(7),
        ));
        assert_eq!(tracked_split.upsert_paths, HashSet::from([track.clone()]));
        assert!(tracked_split.rename_pairs.is_empty());
        assert!(tracked_split.deferred_paths.is_empty());

        let mut adjacent_split = WatcherBatch::default();
        adjacent_split.collect(rename_event(
            notify::event::RenameMode::From,
            &[sibling.to_str().unwrap()],
            None,
        ));
        adjacent_split.collect(rename_event(
            notify::event::RenameMode::To,
            &[track.to_str().unwrap()],
            None,
        ));
        assert_eq!(adjacent_split.upsert_paths, HashSet::from([track]));
        assert!(adjacent_split.rename_pairs.is_empty());
        assert!(adjacent_split.deferred_paths.is_empty());
    }

    const TEST_TAG_STAGING_NAME: &str = ".tributary-tag-00000000-0000-4000-8000-000000000000.flac";
    const TEST_QUARANTINE_NAME: &str =
        ".track.flac.tributary-replaced-0123456789abcdef0123456789abcdef";

    fn path_str(path: &Path) -> &str {
        path.to_str().expect("test paths are UTF-8")
    }

    /// Rename halves as a backend reports them: inotify tags both halves with
    /// one cookie and adds a `Both` event; Windows reports adjacent untracked
    /// halves only.
    fn rename_halves(from: &Path, to: &Path, tracker: Option<usize>) -> Vec<notify::Event> {
        use notify::event::RenameMode;

        let mut events = vec![
            rename_event(RenameMode::From, &[path_str(from)], tracker),
            rename_event(RenameMode::To, &[path_str(to)], tracker),
        ];
        if tracker.is_some() {
            events.push(rename_event(
                RenameMode::Both,
                &[path_str(from), path_str(to)],
                tracker,
            ));
        }
        events
    }

    /// The events of one tag save: the staged copy is written, the original
    /// moves to a quarantine sibling, the staged copy is renamed onto the
    /// public name, and the quarantine is unlinked.
    fn tag_commit_events(
        track: &Path,
        staged: &Path,
        quarantine: &Path,
        trackers: Option<(usize, usize)>,
    ) -> Vec<notify::Event> {
        use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
        use notify::{Event, EventKind};

        let mut events = vec![
            Event::new(EventKind::Create(CreateKind::File)).add_path(staged.to_path_buf()),
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                .add_path(staged.to_path_buf()),
        ];
        events.extend(rename_halves(
            track,
            quarantine,
            trackers.map(|(first, _)| first),
        ));
        events.extend(rename_halves(
            staged,
            track,
            trackers.map(|(_, second)| second),
        ));
        events.push(
            Event::new(EventKind::Remove(RemoveKind::File)).add_path(quarantine.to_path_buf()),
        );
        events
    }

    fn debounced_batch(events: Vec<notify::Event>) -> WatcherBatch {
        let mut ingress = WatcherDebounceBatch::default();
        for event in events {
            ingress.collect(Ok(event));
        }
        ingress.finish().expect("ordinary event stream is reliable")
    }

    #[test]
    fn watcher_batch_turns_a_quarantine_tag_commit_into_one_public_upsert() {
        let library = TestDirectory::new("tag-commit-quarantine");
        let track = library.path().join("track.flac");
        let staged = library.path().join(TEST_TAG_STAGING_NAME);
        let quarantine = library.path().join(TEST_QUARANTINE_NAME);
        // When the debounce window closes only the committed track remains.
        std::fs::write(&track, b"tagged audio").expect("publish tagged track");

        for trackers in [Some((11, 12)), None] {
            let batch = debounced_batch(tag_commit_events(&track, &staged, &quarantine, trackers));

            assert_eq!(
                batch.upsert_paths,
                HashSet::from([track.clone()]),
                "{trackers:?}"
            );
            assert!(batch.remove_paths.is_empty(), "{trackers:?}");
            assert!(batch.rename_pairs.is_empty(), "{trackers:?}");
            assert!(batch.deferred_paths.is_empty(), "{trackers:?}");
            assert!(!batch.reconciliation_required, "{trackers:?}");
        }
    }

    #[test]
    fn watcher_batch_resolves_unassociated_audio_renames_by_path() {
        use notify::event::RenameMode;

        // FSEvents reports each side of a tag save as its own Name::Any.
        let library = TestDirectory::new("tag-commit-name-any");
        let track = library.path().join("track.flac");
        let staged = library.path().join(TEST_TAG_STAGING_NAME);
        let quarantine = library.path().join(TEST_QUARANTINE_NAME);
        std::fs::write(&track, b"tagged audio").expect("publish tagged track");

        let batch = debounced_batch(
            [&track, &quarantine, &staged, &track]
                .into_iter()
                .map(|path| rename_event(RenameMode::Any, &[path_str(path)], None))
                .collect(),
        );

        assert_eq!(batch.upsert_paths, HashSet::from([track]));
        assert!(batch.remove_paths.is_empty());
        assert!(batch.deferred_paths.is_empty());
        assert!(!batch.reconciliation_required);
    }

    #[test]
    fn watcher_batch_turns_cross_extension_file_renames_into_path_changes() {
        let library = TestDirectory::new("cross-extension-renames");
        let root = library.path();

        // rsync, Syncthing, and browsers publish a finished download by
        // renaming a hidden temporary file onto the final audio name.
        let published = root.join("song.flac");
        std::fs::write(&published, b"audio").expect("publish synced track");
        let batch = debounced_batch(rename_halves(
            &root.join(".song.flac.XyZ123"),
            &published,
            Some(21),
        ));
        assert_eq!(batch.upsert_paths, HashSet::from([published]));
        assert!(batch.remove_paths.is_empty());
        assert!(batch.rename_pairs.is_empty());
        assert!(batch.deferred_paths.is_empty());
        assert!(!batch.reconciliation_required);

        // A track renamed to a non-audio name is a removal of the track.
        let original = root.join("old.flac");
        let backup = root.join("old.flac.bak");
        std::fs::write(&backup, b"audio").expect("rename track to backup");
        let batch = debounced_batch(rename_halves(&original, &backup, None));
        assert_eq!(batch.remove_paths, HashSet::from([original]));
        assert!(batch.upsert_paths.is_empty());
        assert!(batch.rename_pairs.is_empty());
        assert!(batch.deferred_paths.is_empty());
        assert!(!batch.reconciliation_required);

        // A directory keeps its pair so its tracks keep their identities.
        let old_album = root.join("Album");
        let new_album = root.join("Album (2020)");
        std::fs::create_dir(&new_album).expect("rename album folder");
        let batch = debounced_batch(rename_halves(&old_album, &new_album, Some(22)));
        assert_eq!(
            batch.rename_pairs,
            HashSet::from([WatcherRenamePair {
                from: old_album,
                to: new_album,
            }])
        );
        assert!(!batch.reconciliation_required);
    }

    #[tokio::test]
    async fn tag_commit_refreshes_the_track_in_place_without_a_library_rescan() {
        let db = Arc::new(rename_test_database().await);
        let fixture = TestDirectory::new("watcher-tag-commit-end-to-end");
        let root = fixture.path().to_path_buf();
        let marker = create_root_marker(&root)
            .expect("create durable root marker")
            .identity;
        insert_reauthorization_root(&db, &root, &marker, true).await;

        let track_path = root.join("track.flac");
        std::fs::write(
            &track_path,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/audio/silence.flac"
            )),
        )
        .expect("write committed track");
        let track_key = track_path.to_string_lossy().into_owned();
        insert_rename_test_track(&db, "tagged-track", &track_key, "Before Tag Save", 3).await;

        let (event_tx, event_rx) = mpsc::channel(WATCHER_EVENT_CAPACITY);
        let Some(idle_backend) = idle_watcher_backend_or_skip() else {
            return;
        };
        let watcher = DirectoryWatcher {
            watcher: idle_backend,
            rx: event_rx,
            ingress_overflowed: Arc::new(AtomicBool::new(false)),
            watched_directories: HashSet::new(),
            root_presence: HashMap::new(),
            root_probe_interval: ROOT_PROBE_INTERVAL,
        };
        for event in tag_commit_events(
            &track_path,
            &root.join(TEST_TAG_STAGING_NAME),
            &root.join(TEST_QUARANTINE_NAME),
            Some((31, 32)),
        ) {
            event_tx
                .send(Ok(event))
                .await
                .expect("queue tag commit event");
        }
        drop(event_tx);

        let (library_events, library_event_rx) = async_channel::unbounded();
        let (_command_tx, command_rx) = async_channel::unbounded::<LibraryCommand>();
        let mut completed_commands = HashMap::new();
        process_directory_events(
            &db,
            std::slice::from_ref(&root),
            &library_events,
            &command_rx,
            &mut completed_commands,
            watcher,
            &test_playlist_sidebar_refresh(),
            &CancellationToken::new(),
        )
        .await
        .expect("watcher loop exits cleanly");

        let events: Vec<LibraryEvent> =
            std::iter::from_fn(|| library_event_rx.try_recv().ok()).collect();
        let upserted: Vec<&Track> = events
            .iter()
            .filter_map(|event| match event {
                LibraryEvent::TrackUpserted(track) => Some(track.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(upserted.len(), 1, "{events:?}");
        assert_eq!(upserted[0].file_path.as_deref(), Some(track_key.as_str()));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                LibraryEvent::FullSync(_)
                    | LibraryEvent::ScanComplete
                    | LibraryEvent::TrackRemoved(_)
            )),
            "a tag save must not reconcile the library: {events:?}"
        );

        let row = track::Entity::find()
            .filter(track::Column::FilePath.eq(&track_key))
            .one(db.as_ref())
            .await
            .expect("query tagged track")
            .expect("tagged track keeps its row");
        assert_eq!(row.id, "tagged-track");
        assert_eq!(row.play_count, 3);
        assert_ne!(row.title, "Before Tag Save", "the new tags were read");
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
            composer: None,
            year: Some(2001),
            track_number: Some(1),
            disc_number: Some(1),
            duration_secs: Some(180),
            bitrate_kbps: Some(192),
            sample_rate_hz: Some(44_100),
            format: Some("FLAC".to_string()),
            play_count,
            last_played_at_ms: Some(1_748_776_400_123),
            rating: Some(88),
            date_added: "2025-01-02T03:04:05Z".to_string(),
            date_modified: "2025-01-02T03:04:05Z".to_string(),
            file_size_bytes: Some(1_000),
        };
        let active: track::ActiveModel = model.into();
        active.insert(db).await.expect("insert rename test track")
    }

    async fn rhythmbox_history_import_fixture(
        db: &DatabaseConnection,
        track_id: &str,
    ) -> super::super::rhythmbox_import::RhythmboxImport {
        #[cfg(windows)]
        let imported_path = PathBuf::from(format!(r"C:\history\{track_id}.flac"));
        #[cfg(not(windows))]
        let imported_path = PathBuf::from(format!("/history/{track_id}.flac"));
        insert_rename_test_track(
            db,
            track_id,
            imported_path.to_str().expect("Unicode fixture path"),
            "Rhythmbox command",
            2,
        )
        .await;
        let location = url::Url::from_file_path(&imported_path)
            .expect("absolute fixture path")
            .to_string();
        let xml = format!(
            "<rhythmdb version=\"2.0\"><entry type=\"song\"><location>{location}</location><play-count>9</play-count></entry></rhythmdb>"
        );
        super::super::rhythmbox_import::parse_rhythmbox_documents(
            xml.as_bytes(),
            None,
            super::super::rhythmbox_import::RhythmboxImportLimits::default(),
        )
        .expect("parse command fixture")
    }

    async fn insert_playback_history_test_track(
        db: &DatabaseConnection,
        id: &str,
        play_count: i32,
        last_played_at_ms: Option<i64>,
    ) -> track::Model {
        let model =
            insert_rename_test_track(db, id, &format!("/history/{id}.flac"), id, play_count).await;
        let mut active: track::ActiveModel = model.into();
        active.last_played_at_ms = Set(last_played_at_ms);
        active
            .update(db)
            .await
            .expect("set playback-history test timestamp")
    }

    #[tokio::test]
    async fn playback_history_atomically_updates_only_the_exact_track_id() {
        let db = rename_test_database().await;
        insert_playback_history_test_track(&db, "history-exact", 7, Some(1_000)).await;
        insert_playback_history_test_track(&db, "history-exact-sibling", 3, Some(900)).await;
        let exact = TrackId::new("history-exact").expect("valid exact track ID");

        let first = record_playback_history(&db, &exact, 2_000)
            .await
            .expect("record first occurrence")
            .expect("exact track exists");
        assert_eq!(first.native_track_id.as_ref(), Some(&exact));
        assert_eq!(first.play_count, Some(8));
        assert_eq!(
            first.last_played.map(|value| value.timestamp_millis()),
            Some(2_000)
        );

        let regressed = record_playback_history(&db, &exact, 1_500)
            .await
            .expect("record occurrence with regressed wall clock")
            .expect("exact track still exists");
        assert_eq!(regressed.play_count, Some(9));
        assert_eq!(
            regressed.last_played.map(|value| value.timestamp_millis()),
            Some(2_000),
            "last-played timestamps are monotonic even while every occurrence increments"
        );

        let sibling = track::Entity::find_by_id("history-exact-sibling")
            .one(&db)
            .await
            .expect("query sibling")
            .expect("sibling exists");
        assert_eq!(sibling.play_count, 3);
        assert_eq!(sibling.last_played_at_ms, Some(900));
    }

    #[tokio::test]
    async fn playback_history_repairs_negative_counts_and_saturates_at_i32_max() {
        let db = rename_test_database().await;
        insert_playback_history_test_track(&db, "history-negative", -17, None).await;
        insert_playback_history_test_track(&db, "history-saturated", i32::MAX - 1, Some(100)).await;

        let negative = TrackId::new("history-negative").expect("valid negative fixture ID");
        let repaired = record_playback_history(&db, &negative, 42)
            .await
            .expect("repair negative count")
            .expect("negative fixture exists");
        assert_eq!(repaired.play_count, Some(1));
        assert_eq!(
            repaired.last_played.map(|value| value.timestamp_millis()),
            Some(42)
        );

        let saturated = TrackId::new("history-saturated").expect("valid saturated fixture ID");
        let updated = record_playback_history(&db, &saturated, 200)
            .await
            .expect("record saturated occurrence")
            .expect("saturated fixture exists");
        assert_eq!(updated.play_count, Some(i32::MAX as u32));
        assert_eq!(
            updated.last_played.map(|value| value.timestamp_millis()),
            Some(200),
            "timestamp advances as the count reaches its storage ceiling"
        );

        let regressed = record_playback_history(&db, &saturated, 150)
            .await
            .expect("record saturated occurrence with regressed timestamp")
            .expect("saturated fixture still exists");
        assert_eq!(regressed.play_count, Some(i32::MAX as u32));
        assert_eq!(
            regressed.last_played.map(|value| value.timestamp_millis()),
            Some(200)
        );
    }

    #[tokio::test]
    async fn playback_history_missing_rows_are_clean_no_ops() {
        let db = rename_test_database().await;
        insert_playback_history_test_track(&db, "history-bystander", 4, Some(321)).await;
        let missing = TrackId::new("history-missing").expect("valid missing track ID");

        assert!(record_playback_history(&db, &missing, 999)
            .await
            .expect("missing history update is not an error")
            .is_none());
        let bystander = track::Entity::find_by_id("history-bystander")
            .one(&db)
            .await
            .expect("query bystander")
            .expect("bystander exists");
        assert_eq!(bystander.play_count, 4);
        assert_eq!(bystander.last_played_at_ms, Some(321));
    }

    #[tokio::test]
    async fn playback_history_commands_emit_only_committed_rows_before_flush_ack() {
        let db = rename_test_database().await;
        insert_playback_history_test_track(&db, "history-committed", 10, Some(1_000)).await;
        insert_playback_history_test_track(&db, "history-rejected", 20, Some(2_000)).await;
        db.execute_unprepared(
            "CREATE TRIGGER reject_playback_history
             BEFORE UPDATE OF play_count, last_played_at_ms ON tracks
             WHEN OLD.id = 'history-rejected'
             BEGIN
                 SELECT RAISE(ABORT, 'injected playback-history failure');
             END",
        )
        .await
        .expect("create playback-history failure trigger");

        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        command_tx
            .send(LibraryCommand::RecordPlaybackHistory {
                track_id: TrackId::new("history-rejected").expect("valid rejected ID"),
                counted_at_ms: 3_000,
            })
            .await
            .expect("send rejected command");
        command_tx
            .send(LibraryCommand::RecordPlaybackHistory {
                track_id: TrackId::new("history-missing").expect("valid missing ID"),
                counted_at_ms: 3_000,
            })
            .await
            .expect("send missing command");
        command_tx
            .send(LibraryCommand::RecordPlaybackHistory {
                track_id: TrackId::new("history-committed").expect("valid committed ID"),
                counted_at_ms: 3_000,
            })
            .await
            .expect("send committed command after rejected transaction");
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        command_tx
            .send(LibraryCommand::Flush {
                completion: flush_tx,
            })
            .await
            .expect("queue FIFO shutdown flush after history commands");
        drop(command_tx);

        let mut completed = HashMap::new();
        process_library_commands_without_watcher(
            &db,
            &[],
            &event_tx,
            &command_rx,
            &mut completed,
            &test_playlist_sidebar_refresh(),
        )
        .await;
        flush_rx
            .recv()
            .await
            .expect("flush is acknowledged after preceding commands finish");
        assert!(
            completed.is_empty(),
            "history commands are never trust receipts"
        );

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert_eq!(events.len(), 1, "missing and failed updates emit nothing");
        let LibraryEvent::PlaybackHistoryUpdated(updated) = &events[0] else {
            panic!("only the committed playback-history event is expected");
        };
        assert_eq!(
            updated.native_track_id.as_ref().map(TrackId::as_str),
            Some("history-committed")
        );
        assert_eq!(updated.play_count, Some(11));
        assert_eq!(
            updated.last_played.map(|value| value.timestamp_millis()),
            Some(3_000)
        );

        let committed = track::Entity::find_by_id("history-committed")
            .one(&db)
            .await
            .expect("query committed row after event")
            .expect("committed row exists");
        assert_eq!(committed.play_count, 11);
        assert_eq!(committed.last_played_at_ms, Some(3_000));
        let rejected = track::Entity::find_by_id("history-rejected")
            .one(&db)
            .await
            .expect("query rejected row")
            .expect("rejected row exists");
        assert_eq!(rejected.play_count, 20);
        assert_eq!(rejected.last_played_at_ms, Some(2_000));
    }

    #[tokio::test]
    async fn rhythmbox_migration_publishes_once_and_exact_retry_is_a_no_op_before_flush() {
        let db = rename_test_database().await;
        let import = rhythmbox_history_import_fixture(&db, "rhythmbox-command").await;
        let repeated_request = super::super::rhythmbox_migration::prepare_rhythmbox_migration(
            &db,
            import.clone(),
            super::super::rhythmbox_migration::RhythmboxMigrationPolicy::default(),
        )
        .await
        .expect("prepare repeated command fixture");
        let request = super::super::rhythmbox_migration::prepare_rhythmbox_migration(
            &db,
            import,
            super::super::rhythmbox_migration::RhythmboxMigrationPolicy::default(),
        )
        .await
        .expect("prepare command fixture");

        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let (playlist_sidebar_refresh, _playlist_sidebar_refresh_rx) =
            super::super::playlist_sidebar::playlist_sidebar_refresh_channel();
        for request in [request, repeated_request] {
            command_tx
                .send(LibraryCommand::ApplyRhythmboxMigration(Box::new(request)))
                .await
                .expect("queue migration command");
        }
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        command_tx
            .send(LibraryCommand::Flush {
                completion: flush_tx,
            })
            .await
            .expect("queue flush after migrations");
        drop(command_tx);

        let mut completed = HashMap::new();
        process_library_commands_without_watcher(
            &db,
            &[],
            &event_tx,
            &command_rx,
            &mut completed,
            &playlist_sidebar_refresh,
        )
        .await;
        flush_rx
            .recv()
            .await
            .expect("flush follows both migration settlements");

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], LibraryEvent::FullSync(tracks) if tracks.len() == 1));
        assert!(matches!(
            &events[1],
            LibraryEvent::PlaylistProjectionsInvalidated
        ));
        assert!(matches!(
            &events[2],
            LibraryEvent::RhythmboxMigrationFinished {
                outcome: super::super::rhythmbox_migration::RhythmboxMigrationCompletion::Applied,
                ..
            }
        ));
        assert!(matches!(
            &events[3],
            LibraryEvent::RhythmboxMigrationFinished {
                outcome:
                    super::super::rhythmbox_migration::RhythmboxMigrationCompletion::AlreadyApplied,
                ..
            }
        ));
        let updated = track::Entity::find_by_id("rhythmbox-command")
            .one(&db)
            .await
            .expect("query migrated track")
            .expect("migrated track exists");
        assert_eq!(updated.play_count, 9);
        assert_eq!(
            crate::db::entities::rhythmbox_import_receipt::Entity::find()
                .all(&db)
                .await
                .expect("query exact receipts")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn rhythmbox_migration_reports_incomplete_refresh_before_applied_completion() {
        let db = rename_test_database().await;
        let import = rhythmbox_history_import_fixture(&db, "rhythmbox-refresh-failure").await;
        let request = super::super::rhythmbox_migration::prepare_rhythmbox_migration(
            &db,
            import,
            super::super::rhythmbox_migration::RhythmboxMigrationPolicy::default(),
        )
        .await
        .expect("prepare refresh-failure fixture");
        let request_id = request.request_id();

        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        command_tx
            .send(LibraryCommand::ApplyRhythmboxMigration(Box::new(request)))
            .await
            .expect("queue migration command");
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        command_tx
            .send(LibraryCommand::Flush {
                completion: flush_tx,
            })
            .await
            .expect("queue flush after migration");
        drop(command_tx);

        let (playlist_sidebar_refresh, playlist_sidebar_refresh_rx) =
            super::super::playlist_sidebar::playlist_sidebar_refresh_channel();
        drop(playlist_sidebar_refresh_rx);
        let mut completed = HashMap::new();
        process_library_commands_without_watcher(
            &db,
            &[],
            &event_tx,
            &command_rx,
            &mut completed,
            &playlist_sidebar_refresh,
        )
        .await;
        flush_rx
            .recv()
            .await
            .expect("refresh failure does not bypass the FIFO flush");

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], LibraryEvent::FullSync(tracks) if tracks.len() == 1));
        assert!(matches!(
            &events[1],
            LibraryEvent::PlaylistProjectionsInvalidated
        ));
        assert!(matches!(
            &events[2],
            LibraryEvent::RhythmboxMigrationFinished {
                request_id: finished_request_id,
                outcome:
                    super::super::rhythmbox_migration::RhythmboxMigrationCompletion::AppliedRefreshFailed,
                ..
            } if *finished_request_id == request_id
        ));
        let updated = track::Entity::find_by_id("rhythmbox-refresh-failure")
            .one(&db)
            .await
            .expect("query committed migration after refresh failure")
            .expect("migrated track survives refresh failure");
        assert_eq!(updated.play_count, 9);
        assert_eq!(
            crate::db::entities::rhythmbox_import_receipt::Entity::find()
                .all(&db)
                .await
                .expect("query committed refresh-failure receipt")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn rhythmbox_migration_drains_to_flush_when_the_event_lane_closes_for_shutdown() {
        let db = rename_test_database().await;
        let import = rhythmbox_history_import_fixture(&db, "rhythmbox-closed-event-lane").await;
        let request = super::super::rhythmbox_migration::prepare_rhythmbox_migration(
            &db,
            import,
            super::super::rhythmbox_migration::RhythmboxMigrationPolicy::default(),
        )
        .await
        .expect("prepare shutdown fixture");

        let (event_tx, event_rx) = async_channel::unbounded();
        drop(event_rx);
        let (command_tx, command_rx) = async_channel::unbounded();
        command_tx
            .send(LibraryCommand::ApplyRhythmboxMigration(Box::new(request)))
            .await
            .expect("queue admitted migration during shutdown");
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        command_tx
            .send(LibraryCommand::Flush {
                completion: flush_tx,
            })
            .await
            .expect("queue shutdown flush");
        drop(command_tx);

        let (playlist_sidebar_refresh, _playlist_sidebar_refresh_rx) =
            super::super::playlist_sidebar::playlist_sidebar_refresh_channel();
        let mut completed = HashMap::new();
        process_library_commands_without_watcher(
            &db,
            &[],
            &event_tx,
            &command_rx,
            &mut completed,
            &playlist_sidebar_refresh,
        )
        .await;
        flush_rx
            .recv()
            .await
            .expect("closed UI lane still permits the admitted command to settle");

        let updated = track::Entity::find_by_id("rhythmbox-closed-event-lane")
            .one(&db)
            .await
            .expect("query committed migration after UI shutdown")
            .expect("migration committed before the shutdown barrier");
        assert_eq!(updated.play_count, 9);
        assert_eq!(
            crate::db::entities::rhythmbox_import_receipt::Entity::find()
                .all(&db)
                .await
                .expect("query committed shutdown receipt")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn rating_commands_publish_only_committed_rows_before_flush_ack() {
        let db = rename_test_database().await;
        insert_playback_history_test_track(&db, "rating-committed", 10, Some(1_000)).await;
        insert_playback_history_test_track(&db, "rating-rejected", 20, Some(2_000)).await;
        insert_playback_history_test_track(&db, "rating-bystander", 30, Some(3_000)).await;
        db.execute_unprepared(
            "CREATE TRIGGER reject_track_rating
             BEFORE UPDATE OF rating ON tracks
             WHEN OLD.id = 'rating-rejected'
             BEGIN
                 SELECT RAISE(ABORT, 'injected rating failure with unsafe details');
             END",
        )
        .await
        .expect("create rating failure trigger");

        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        command_tx
            .send(LibraryCommand::SetTrackRating {
                track_id: TrackId::new("rating-rejected").expect("valid rejected ID"),
                rating: Some(Rating::new(25).unwrap()),
            })
            .await
            .expect("send rejected rating command");
        command_tx
            .send(LibraryCommand::SetTrackRating {
                track_id: TrackId::new("rating-missing").expect("valid missing ID"),
                rating: Some(Rating::new(50).unwrap()),
            })
            .await
            .expect("send missing rating command");
        command_tx
            .send(LibraryCommand::SetTrackRating {
                track_id: TrackId::new("rating-committed").expect("valid committed ID"),
                rating: Some(Rating::new(73).unwrap()),
            })
            .await
            .expect("send committed rating command");
        command_tx
            .send(LibraryCommand::SetTrackRating {
                track_id: TrackId::new("rating-committed").expect("valid committed ID"),
                rating: None,
            })
            .await
            .expect("send committed rating-clear command");
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        command_tx
            .send(LibraryCommand::Flush {
                completion: flush_tx,
            })
            .await
            .expect("queue FIFO shutdown flush after rating commands");
        drop(command_tx);

        let mut completed = HashMap::new();
        process_library_commands_without_watcher(
            &db,
            &[],
            &event_tx,
            &command_rx,
            &mut completed,
            &test_playlist_sidebar_refresh(),
        )
        .await;
        flush_rx
            .recv()
            .await
            .expect("flush is acknowledged after preceding rating commands finish");
        assert!(
            completed.is_empty(),
            "rating commands are never trust receipts"
        );

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert_eq!(
            events.len(),
            3,
            "a missing row is a clean no-op while failure and commits are explicit"
        );
        assert!(matches!(
            &events[0],
            LibraryEvent::TrackRatingUpdateFailed { track_id }
                if track_id.as_str() == "rating-rejected"
        ));

        let LibraryEvent::TrackRatingUpdated(rated) = &events[1] else {
            panic!("the first committed rating must be published after the failure");
        };
        assert_eq!(
            rated.native_track_id.as_ref().map(TrackId::as_str),
            Some("rating-committed")
        );
        assert_eq!(
            rated.rating,
            TrackRating::writable(Some(Rating::new(73).unwrap()))
        );

        let LibraryEvent::TrackRatingUpdated(cleared) = &events[2] else {
            panic!("the committed clear must retain FIFO publication order");
        };
        assert_eq!(
            cleared.native_track_id.as_ref().map(TrackId::as_str),
            Some("rating-committed")
        );
        assert_eq!(cleared.rating, TrackRating::writable(None));

        let committed = track::Entity::find_by_id("rating-committed")
            .one(&db)
            .await
            .expect("query committed row after events")
            .expect("committed row exists");
        assert_eq!(committed.rating, None);
        let rejected = track::Entity::find_by_id("rating-rejected")
            .one(&db)
            .await
            .expect("query rejected row")
            .expect("rejected row exists");
        assert_eq!(rejected.rating, Some(88));
        let bystander = track::Entity::find_by_id("rating-bystander")
            .one(&db)
            .await
            .expect("query bystander row")
            .expect("bystander row exists");
        assert_eq!(bystander.rating, Some(88));
    }

    fn parsed_rename_track(path: &str, title: &str) -> ParsedTrack {
        ParsedTrack {
            file_path: path.to_string(),
            title: title.to_string(),
            title_from_tag: true,
            artist_name: "Updated Artist".to_string(),
            artist_from_tag: true,
            album_artist_name: Some("Updated Album Artist".to_string()),
            album_title: "Updated Album".to_string(),
            album_from_tag: true,
            genre: Some("Updated Genre".to_string()),
            composer: None,
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
            .create_regular_playlist("Guarded delete")
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
            .create_regular_playlist("Rename")
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
        assert_eq!(renamed.last_played_at_ms, Some(1_748_776_400_123));
        assert_eq!(
            renamed.rating,
            Some(88),
            "metadata refresh preserves app rating"
        );
        assert_eq!(renamed.date_added, "2025-01-02T03:04:05Z");

        let entry_after = playlist_entry::Entity::find_by_id(&entry_before.id)
            .one(&db)
            .await
            .expect("reload playlist entry")
            .expect("playlist entry remains");
        assert_eq!(entry_after, entry_before);
        assert_eq!(entry_after.track_id.as_deref(), Some("stable-track-id"));
        assert_eq!(
            entry_after.local_track_id.as_deref(),
            Some("stable-track-id")
        );
    }

    #[tokio::test]
    async fn paired_rename_atomically_replaces_an_occupied_destination() {
        use crate::db::entities::playlist_entry;

        let db = rename_test_database().await;
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_regular_playlist("Overwrite")
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
        assert_eq!(entries[0].local_track_id.as_deref(), Some("source-track"));
        assert_eq!(
            entries[1].track_id.as_deref(),
            Some("destination-track"),
            "local deletion preserves durable playlist identity"
        );
        assert_eq!(entries[1].local_track_id, None);
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
            .create_regular_playlist("Rollback")
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
        let local_source_id = crate::architecture::SourceId::local();
        db.execute_unprepared(&format!(
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, source_id, track_id, local_track_id,
                 match_title, match_artist, match_album, match_duration_secs
             )
             VALUES (
                 'watcher-entry', 'watcher-playlist', 0, '{local_source_id}', NULL, NULL,
                 'watcher song', 'watcher artist', 'watcher album', 180
             )"
        ))
        .await
        .expect("insert orphaned playlist entry");
        let watcher_track = track::Entity::find_by_id("watcher-track")
            .one(&db)
            .await
            .expect("query watcher track")
            .expect("watcher track exists");
        let unrelated_track = track::Model {
            id: "unrelated-track".to_string(),
            file_path: "/music/unrelated.flac".to_string(),
            title: "Unrelated Song".to_string(),
            ..watcher_track.clone()
        };
        let (event_tx, event_rx) = async_channel::unbounded();

        assert_eq!(
            settle_playlist_projections_after_watcher_batch(&db, &event_tx, &[], false)
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
        assert_eq!(still_orphaned.local_track_id, None);

        assert_eq!(
            settle_playlist_projections_after_watcher_batch(&db, &event_tx, &[], true)
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
        assert_eq!(still_orphaned.local_track_id, None);

        // An upsert that cannot match any orphan skips reconciliation, even
        // though an unchanged track would match.
        assert_eq!(
            settle_playlist_projections_after_watcher_batch(
                &db,
                &event_tx,
                std::slice::from_ref(&unrelated_track),
                true,
            )
            .await
            .expect("settle unrelated upsert batch"),
            0
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(LibraryEvent::PlaylistProjectionsInvalidated)
        ));
        let still_orphaned = playlist_entry::Entity::find_by_id("watcher-entry")
            .one(&db)
            .await
            .expect("query unrelated-upsert reconciliation")
            .expect("playlist entry remains");
        assert_eq!(still_orphaned.local_track_id, None);

        db.execute_unprepared(
            "CREATE TRIGGER fail_watcher_playlist_reconciliation
             BEFORE UPDATE OF track_id ON playlist_entries
             BEGIN
                 SELECT RAISE(ABORT, 'injected watcher reconciliation failure');
             END;",
        )
        .await
        .expect("install reconciliation failure trigger");
        settle_playlist_projections_after_watcher_batch(
            &db,
            &event_tx,
            std::slice::from_ref(&watcher_track),
            true,
        )
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
            settle_playlist_projections_after_watcher_batch(
                &db,
                &event_tx,
                std::slice::from_ref(&watcher_track),
                true,
            )
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
        assert_eq!(relinked.local_track_id.as_deref(), Some("watcher-track"));
    }

    #[tokio::test]
    async fn initial_scan_requests_sidebar_refresh_and_invalidates_before_scan_complete() {
        let db = rename_test_database().await;
        let (event_tx, event_rx) = async_channel::unbounded();
        let (playlist_sidebar_refresh, _playlist_sidebar_refresh_rx) =
            super::super::playlist_sidebar::playlist_sidebar_refresh_channel();

        initial_scan(&db, &[], &event_tx, &playlist_sidebar_refresh)
            .await
            .expect("run empty initial scan");

        assert_eq!(
            playlist_sidebar_refresh.request(),
            PlaylistSidebarRefreshRequest::Coalesced,
            "the scan leaves one publisher refresh request pending"
        );

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let invalidation_index = events
            .iter()
            .position(|event| matches!(event, LibraryEvent::PlaylistProjectionsInvalidated))
            .expect("initial scan invalidates playlist projections");
        let completion_index = events
            .iter()
            .position(|event| matches!(event, LibraryEvent::ScanComplete))
            .expect("initial scan completes");
        assert!(invalidation_index < completion_index);
        assert!(events
            .iter()
            .all(|event| !matches!(event, LibraryEvent::PlaylistsLoaded(_))));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, LibraryEvent::PlaylistProjectionsInvalidated))
                .count(),
            1,
            "one initial scan produces one post-reconciliation invalidation"
        );
    }

    // ── Initial-scan shutdown cancellation ──────────────────────────────

    #[test]
    fn mark_cancelled_fails_closed_for_stale_deletion() {
        let directory = TestDirectory::new("mark-cancelled");
        let mut scan = scan_root(directory.path().to_path_buf());
        scan.errors.clear();
        scan.reconciliation_authoritative = true;
        scan.content_authorized = true;
        assert!(scan.is_complete());

        scan.mark_cancelled("shutdown");

        assert!(!scan.is_complete(), "a cancelled scan is incomplete");
        assert!(!scan.reconciliation_authoritative);
        assert!(!scan.content_authorized);
        assert!(
            scan.errors.iter().any(|error| error.contains("shutdown")),
            "the cancellation reason is retained for diagnostics"
        );
    }

    #[tokio::test]
    async fn readonly_blocking_job_completes_when_not_cancelled() {
        let cancellation = CancellationToken::new();
        let job = tokio::task::spawn_blocking(|| 41 + 1);

        let result = await_readonly_blocking(&cancellation, job).await;

        assert_eq!(result.expect("awaited").expect("joined"), 42);
    }

    #[tokio::test]
    async fn readonly_blocking_job_that_settles_inside_the_budget_is_kept() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let job = tokio::task::spawn_blocking(|| 7);

        let result = await_readonly_blocking(&cancellation, job).await;

        assert_eq!(result.expect("settled inside budget").expect("joined"), 7);
    }

    #[tokio::test]
    async fn held_readonly_blocking_job_is_abandoned_at_the_settle_budget() {
        // Simulates the "spawn_blocking cannot cancel an in-progress kernel
        // call" case: the worker is held past the budget. The scan must abandon
        // the read-only handle rather than blocking window teardown.
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let job = tokio::task::spawn_blocking(move || {
            let _ = release_rx.recv();
        });

        let started = std::time::Instant::now();
        let result = await_readonly_blocking(&cancellation, job).await;
        let elapsed = started.elapsed();

        assert!(result.is_none(), "held read-only work must be abandoned");
        assert!(
            elapsed >= SCAN_READONLY_SETTLE_BUDGET.saturating_sub(Duration::from_millis(50)),
            "the job was not given the full settle budget: {elapsed:?}"
        );
        // Release the detached worker so it cannot linger across tests.
        let _ = release_tx.send(());
    }

    #[tokio::test]
    async fn cancelled_initial_scan_admits_no_mutations_and_preserves_stale_tracks() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("cancelled-scan");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));
        // A row whose file is absent would normally be a stale-deletion
        // candidate. A cancelled scan must preserve it.
        insert_rename_test_track(
            &db,
            "stale-before-cancel",
            directory
                .path()
                .join("missing.wav")
                .to_string_lossy()
                .as_ref(),
            "Stale",
            0,
        )
        .await;

        let (event_tx, event_rx) = async_channel::unbounded();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        initial_scan_with_control(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &test_playlist_sidebar_refresh(),
            &cancellation,
            &ScanDiscoveryHold::none(),
            &ScanWriteTxnGate::default(),
        )
        .await
        .expect("cancelled scan returns cleanly");

        let tracks = track::Entity::find().all(&db).await.expect("query tracks");
        assert_eq!(
            tracks.len(),
            1,
            "a cancelled scan admits no new tracks and deletes none"
        );
        assert_eq!(tracks[0].id, "stale-before-cancel");
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert!(
            events.is_empty(),
            "a cancelled scan emits no completion events: {events:?}"
        );
    }

    /// A parser that settles inside its shutdown grace is *kept* by the
    /// read-only isolation contract, but the explicit durable-mutation
    /// admission boundary still refuses to begin new work from it (R2).
    #[tokio::test]
    async fn parser_settling_inside_the_grace_is_refused_at_the_mutation_boundary() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let job = tokio::task::spawn_blocking(|| 7_u32);
        let settled = await_readonly_blocking(&cancellation, job).await;
        assert_eq!(
            settled.expect("settled inside the grace").expect("joined"),
            7
        );
        assert!(
            !admit_scan_mutation(&cancellation),
            "a post-cancellation parser completion must not admit durable work"
        );
    }

    /// Deterministic R2 regression: hold a real initial scan right after its
    /// read-only parser settles, cancel while it is held, then release. The
    /// admission boundary must refuse the upsert, so no track is written.
    #[tokio::test]
    async fn post_parse_cancellation_refuses_the_durable_upsert() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("post-parse-cancel");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, _event_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let forced_conversions = HashMap::new();
        let authority_guards = HashMap::new();
        let evidence_refreshes = HashMap::new();

        let scan_write_txn = ScanWriteTxnGate::default();
        let scan = initial_scan_with_control(
            &db,
            &music_dirs,
            &event_tx,
            &forced_conversions,
            &authority_guards,
            &evidence_refreshes,
            &refresh,
            &cancellation,
            &hold,
            &scan_write_txn,
        );
        let driver = async {
            // The traversal hold engages first; release it so the scan can
            // reach the parser and settle. The per-root status persist runs
            // before the parse; let it through uncaptured.
            control
                .wait_until_reached(ScanDiscoveryStage::Traversal)
                .await;
            control.release(ScanDiscoveryStage::Traversal);
            control.release(ScanDiscoveryStage::RootStatus);
            // The parser has settled with the window still open.
            control
                .wait_until_reached(ScanDiscoveryStage::PostParse)
                .await;
            // Shutdown arrives after the parse and before any durable
            // mutation; the boundary must refuse the upsert.
            cancellation.cancel();
            control.release(ScanDiscoveryStage::PostParse);
        };

        let (scan_result, ()) = tokio::join!(scan, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("scan must reach the post-parse hold");
        });
        scan_result.expect("cancelled scan returns cleanly");

        let tracks = track::Entity::find().all(&db).await.expect("query tracks");
        assert!(
            tracks.is_empty(),
            "a post-parse cancellation must not upsert: {tracks:?}"
        );
    }

    /// Deterministic restart reconciliation: the durable work a cancelled scan
    /// refused at the admission boundary is not lost — the next startup's scan
    /// (fresh cancellation token, no discovery hold) finds the same file and
    /// upserts it. The cancelled run must leave the persisted catalogue exactly
    /// as the last committed transaction left it so the restart can reconcile.
    #[tokio::test]
    async fn restart_after_post_parse_cancellation_reconciles_the_track() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("post-parse-cancel-restart");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, _event_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let forced_conversions = HashMap::new();
        let authority_guards = HashMap::new();
        let evidence_refreshes = HashMap::new();

        // ── First startup: cancel right after the parser settles. ──
        let cancelled_scan_write_txn = ScanWriteTxnGate::default();
        let cancelled_scan = initial_scan_with_control(
            &db,
            &music_dirs,
            &event_tx,
            &forced_conversions,
            &authority_guards,
            &evidence_refreshes,
            &refresh,
            &cancellation,
            &hold,
            &cancelled_scan_write_txn,
        );
        let driver = async {
            // The traversal hold engages first; release it so the scan can
            // reach the parser and settle. The per-root status persist runs
            // before the parse; let it through uncaptured.
            control
                .wait_until_reached(ScanDiscoveryStage::Traversal)
                .await;
            control.release(ScanDiscoveryStage::Traversal);
            control.release(ScanDiscoveryStage::RootStatus);
            // Shutdown arrives after the parse and before any durable
            // mutation; the admission boundary must refuse the upsert.
            control
                .wait_until_reached(ScanDiscoveryStage::PostParse)
                .await;
            cancellation.cancel();
            control.release(ScanDiscoveryStage::PostParse);
        };
        let (first_result, ()) = tokio::join!(cancelled_scan, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("cancelled scan must reach the post-parse hold");
        });
        first_result.expect("cancelled scan returns cleanly");

        let tracks = track::Entity::find().all(&db).await.expect("query tracks");
        assert!(
            tracks.is_empty(),
            "the cancelled run must not upsert: {tracks:?}"
        );

        // ── Restart: a fresh scan with a live token must reconcile the track. ──
        let restart_cancellation = CancellationToken::new();
        initial_scan_shutdown_aware(
            &db,
            &music_dirs,
            &event_tx,
            &refresh,
            &restart_cancellation,
            &ScanDiscoveryHold::none(),
            &ScanWriteTxnGate::default(),
        )
        .await
        .expect("the restarting scan completes");

        let restarted = track::Entity::find()
            .all(&db)
            .await
            .expect("query tracks after restart");
        assert_eq!(
            restarted.len(),
            1,
            "the restart must upsert the previously refused track: {restarted:?}"
        );
        assert!(
            restarted[0].file_path.ends_with("present.wav"),
            "the reconciled track is the scanned file: {:?}",
            restarted[0].file_path
        );
    }

    /// Deterministic jq0TgN regression: with the write gate closed, parking
    /// on `GatedCommandRecv` must leave the channel listener alive, so a
    /// command sent while parked wakes the receiver instead of stalling until
    /// some unrelated wake re-polls the selector. The pre-fix implementation
    /// created a fresh `rx.recv()` inside `poll` and dropped it on every
    /// `Pending`, unregistering the listener before any send could fire it.
    #[tokio::test]
    async fn gated_command_recv_wakes_when_a_command_is_sent_while_parked() {
        let (command_tx, command_rx) = async_channel::unbounded::<LibraryCommand>();
        let gate = ScanWriteTxnGate::default();
        let (completion, _completion_rx) = async_channel::unbounded();

        // The sender yields first so the gated receiver is polled to its
        // first Pending — listener registered or, pre-fix, dropped — strictly
        // before the command is sent. `join!` polls both branches on every
        // task poll, so the interleaving is deterministic on the
        // current-thread test runtime.
        let sender = async {
            tokio::task::yield_now().await;
            command_tx
                .send(LibraryCommand::Flush { completion })
                .await
                .expect("send while the gated receiver is parked");
        };
        // Once the sender has settled, nothing else re-polls the receiver, so
        // a missed wake can only surface as this timeout.
        let gated = async {
            let recv = GatedCommandRecv::new(&command_rx, &gate);
            recv.await
        };
        let started = std::time::Instant::now();
        let (received, ()) = tokio::time::timeout(
            Duration::from_secs(10),
            futures::future::join(gated, sender),
        )
        .await
        .expect("a command sent while parked must wake the gated receiver (jq0TgN)");
        // Promptness is the assertion: a dropped channel listener (the
        // pre-fix fresh-per-poll receive) does not lose the command — it
        // drains it on the NEXT unrelated wake. The only timer in play is
        // the failsafe above, so a near-10s completion proves the send did
        // not wake the receiver.
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "the send itself must wake the parked receiver (took {elapsed:?}; a \
             late drain means the channel listener was dropped — jq0TgN)"
        );
        let received = received.expect("the receive succeeds");
        assert!(
            matches!(received, LibraryCommand::Flush { .. }),
            "the parked receiver must wake and take the command: {received:?}"
        );
    }

    /// The jq5lG contract at the future level: with a scan write transaction
    /// open, the gated receiver stays pending even with a command already
    /// queued, and delivers it once the gate closes again.
    #[tokio::test]
    async fn gated_command_recv_defers_queued_commands_while_the_write_gate_is_open() {
        use std::future::Future as _;
        let (command_tx, command_rx) = async_channel::unbounded::<LibraryCommand>();
        let gate = ScanWriteTxnGate::default();
        let open_txn = ScanWriteTxnGuard::open(&gate);
        let (completion, _completion_rx) = async_channel::unbounded();
        command_tx
            .send(LibraryCommand::Flush { completion })
            .await
            .expect("queue the command");

        let mut gated = Box::pin(GatedCommandRecv::new(&command_rx, &gate));
        // Gate open: the queued command must not surface, even on repeated
        // polls (a fresh-per-poll receive would drain it immediately).
        let mut parked = std::task::Context::from_waker(std::task::Waker::noop());
        for _ in 0..3 {
            assert!(
                matches!(gated.as_mut().poll(&mut parked), std::task::Poll::Pending),
                "an open scan write transaction must defer the queued command"
            );
        }
        drop(open_txn);
        let received = tokio::time::timeout(Duration::from_secs(10), gated.as_mut())
            .await
            .expect("closing the gate must deliver the deferred command")
            .expect("the receive succeeds");
        assert!(
            matches!(received, LibraryCommand::Flush { .. }),
            "the deferred command is delivered after the gate closes: {received:?}"
        );
    }

    /// Deterministic R1 regression: hold a real initial scan inside read-only
    /// traversal, then admit a rating edit. The edit must settle while the
    /// discovery step is still held, because command service is driven
    /// independently of the scan's read-only work.
    #[tokio::test]
    async fn startup_services_an_admitted_rating_while_discovery_is_held() {
        let db = rename_test_database().await;
        // The rating target lives outside the scan root, so the scan can never
        // race it and the root stays unenrolled (zero existing tracks).
        let rating_path = std::env::temp_dir().join("tributary-held-rating.flac");
        insert_rename_test_track(
            &db,
            "held-rating-track",
            rating_path.to_string_lossy().as_ref(),
            "Held",
            0,
        )
        .await;
        let directory = TestDirectory::new("startup-held-discovery");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let mut completed = HashMap::new();

        let scan_write_txn = ScanWriteTxnGate::default();
        let engine = service_commands_while_scanning(
            initial_scan_shutdown_aware(
                &db,
                &music_dirs,
                &event_tx,
                &refresh,
                &cancellation,
                &hold,
                &scan_write_txn,
            ),
            &scan_write_txn,
            &db,
            &music_dirs,
            &event_tx,
            &command_rx,
            &mut completed,
            &refresh,
        );
        let driver = async {
            control
                .wait_until_reached(ScanDiscoveryStage::Traversal)
                .await;
            command_tx
                .send(LibraryCommand::SetTrackRating {
                    track_id: TrackId::new("held-rating-track").expect("valid track ID"),
                    rating: Some(Rating::new(80).expect("valid rating")),
                })
                .await
                .expect("admit the rating command");
            // The rating update must settle *before* the held discovery is
            // released; otherwise this receive never completes and the test
            // deadlocks rather than passing.
            loop {
                let event = event_rx.recv().await.expect("event channel stays open");
                if matches!(event, LibraryEvent::TrackRatingUpdated(_)) {
                    break;
                }
            }
            control.release(ScanDiscoveryStage::Traversal);
            // The scan's own parse will engage the post-parse hold next; the
            // rating has already settled, so let the whole scan finish rather
            // than parking the engine on a hold nobody will release. Release
            // the mutation-span holds too so the per-root status persist and
            // the guarded upsert run to settlement.
            control.release(ScanDiscoveryStage::PostParse);
            control.release(ScanDiscoveryStage::RootStatus);
            control.release(ScanDiscoveryStage::CommitGuard);
        };

        let (scan_result, ()) = tokio::join!(engine, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("the held discovery must admit a settling command");
        });
        scan_result.expect("scan completes after the discovery hold is released");

        let rated = track::Entity::find_by_id("held-rating-track")
            .one(&db)
            .await
            .expect("query rated track")
            .expect("rated track exists");
        assert_eq!(rated.rating, Some(80));
    }

    /// Deterministic reserved-drain regression: a `Flush` admitted after an
    /// earlier rating is acknowledged only once the held scan settles, so no
    /// already-admitted command can be reported drained while discovery still
    /// runs.
    #[tokio::test]
    async fn reserved_flush_drain_waits_for_held_discovery_to_settle() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("reserved-drain");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        let mut completed = HashMap::new();

        let scan_write_txn = ScanWriteTxnGate::default();
        let engine = service_commands_while_scanning(
            initial_scan_shutdown_aware(
                &db,
                &music_dirs,
                &event_tx,
                &refresh,
                &cancellation,
                &hold,
                &scan_write_txn,
            ),
            &scan_write_txn,
            &db,
            &music_dirs,
            &event_tx,
            &command_rx,
            &mut completed,
            &refresh,
        );
        let driver = async {
            control
                .wait_until_reached(ScanDiscoveryStage::Traversal)
                .await;
            command_tx
                .send(LibraryCommand::Flush {
                    completion: flush_tx,
                })
                .await
                .expect("admit the reserved flush");
            // Let the engine observe the Flush and park on the held scan.
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            assert!(
                matches!(flush_rx.try_recv(), Err(async_channel::TryRecvError::Empty)),
                "Flush must not be acknowledged while discovery is held"
            );
            control.release(ScanDiscoveryStage::Traversal);
            // The scan's parse engages the post-parse hold on its way to
            // settlement; release it plus the mutation-span holds, or the
            // drain being asserted here would never be acknowledged.
            control.release(ScanDiscoveryStage::PostParse);
            control.release(ScanDiscoveryStage::RootStatus);
            control.release(ScanDiscoveryStage::CommitGuard);
            flush_rx
                .recv()
                .await
                .expect("flush is acknowledged once the scan settles");
        };

        let (scan_result, ()) = tokio::join!(engine, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("the reserved drain must settle once discovery is released");
        });
        scan_result.expect("scan completes after the discovery hold is released");
        // The rating/Flush channel is still open at this point; closing it is
        // not required because the engine returned at the Flush marker.
        drop(command_tx);
        let _ = event_rx.try_recv();
    }

    /// jq5lG regression: a rating command that arrives while the scan is
    /// suspended *inside* an open SQLite write transaction (the upsert commit
    /// guard) must be deferred to the transaction boundary and then serviced —
    /// not raced into the open transaction, where its write would queue behind
    /// the scan's lock and fail at the production five-second busy timeout.
    #[tokio::test]
    async fn command_arriving_inside_a_scan_write_transaction_is_deferred_not_dropped() {
        let db = rename_test_database().await;
        // The rating target lives outside the scan root, so the scan itself
        // never touches it and any busy contention is purely scheduler-level.
        let rating_path = std::env::temp_dir().join("tributary-txn-rating.flac");
        insert_rename_test_track(
            &db,
            "txn-rating-track",
            rating_path.to_string_lossy().as_ref(),
            "Txn",
            0,
        )
        .await;
        let directory = TestDirectory::new("scan-txn-gate");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let scan_write_txn = ScanWriteTxnGate::default();
        let mut completed = HashMap::new();

        let engine = service_commands_while_scanning(
            initial_scan_shutdown_aware(
                &db,
                &music_dirs,
                &event_tx,
                &refresh,
                &cancellation,
                &hold,
                &scan_write_txn,
            ),
            &scan_write_txn,
            &db,
            &music_dirs,
            &event_tx,
            &command_rx,
            &mut completed,
            &refresh,
        );
        let driver = async {
            // Let the read-only phases through uncaptured (traversal, the
            // per-root status persist, and the per-file post-parse rendezvous);
            // the hold this test cares about parks the scan *inside* the
            // upsert's write transaction.
            control.release(ScanDiscoveryStage::Traversal);
            control.release(ScanDiscoveryStage::RootStatus);
            control.release(ScanDiscoveryStage::PostParse);
            control
                .wait_until_reached(ScanDiscoveryStage::CommitGuard)
                .await;
            // The scan is now suspended inside its open write transaction.
            // Admit a rating: the selector must not dispatch it into the open
            // transaction.
            command_tx
                .send(LibraryCommand::SetTrackRating {
                    track_id: TrackId::new("txn-rating-track").expect("valid track ID"),
                    rating: Some(Rating::new(80).expect("valid rating")),
                })
                .await
                .expect("admit the rating command");
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            while let Ok(event) = event_rx.try_recv() {
                assert!(
                    !matches!(event, LibraryEvent::TrackRatingUpdated(_)),
                    "a command must not be serviced while a scan write transaction is open"
                );
            }
            // Settle the transaction. The queued rating is then serviced
            // immediately — never dropped, never failed at the busy timeout.
            control.release(ScanDiscoveryStage::CommitGuard);
            loop {
                let event = tokio::time::timeout(Duration::from_secs(30), event_rx.recv())
                    .await
                    .expect("the deferred rating must be serviced after the transaction settles")
                    .expect("event channel stays open");
                if matches!(event, LibraryEvent::TrackRatingUpdated(_)) {
                    break;
                }
            }
        };

        let (scan_result, ()) = tokio::join!(engine, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("the transaction-gated driver must settle");
        });
        scan_result.expect("scan completes after the write-transaction hold is released");

        let rated = track::Entity::find_by_id("txn-rating-track")
            .one(&db)
            .await
            .expect("query rated track")
            .expect("rated track exists");
        assert_eq!(
            rated.rating,
            Some(80),
            "the command deferred behind the scan write transaction must still commit"
        );
    }

    /// Round-3 regression: a command deferred behind an open scan write
    /// transaction must settle PROMPTLY once that transaction commits — its
    /// write runs against an idle database, far below the production
    /// five-second busy timeout. Fail-closed: if the reciprocal gate ever
    /// regresses to dispatching into the open transaction, the write stalls
    /// at the busy timeout (or errors) and this assertion fires.
    #[tokio::test]
    async fn command_deferred_behind_open_scan_txn_settles_far_below_busy_timeout() {
        let db = rename_test_database().await;
        let rating_path = std::env::temp_dir().join("tributary-txn-settle-rating.flac");
        insert_rename_test_track(
            &db,
            "txn-settle-rating-track",
            rating_path.to_string_lossy().as_ref(),
            "Settle",
            0,
        )
        .await;
        let directory = TestDirectory::new("scan-txn-settle");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let scan_write_txn = ScanWriteTxnGate::default();
        let mut completed = HashMap::new();

        let engine = service_commands_while_scanning(
            initial_scan_shutdown_aware(
                &db,
                &music_dirs,
                &event_tx,
                &refresh,
                &cancellation,
                &hold,
                &scan_write_txn,
            ),
            &scan_write_txn,
            &db,
            &music_dirs,
            &event_tx,
            &command_rx,
            &mut completed,
            &refresh,
        );
        let driver = async {
            control.release(ScanDiscoveryStage::Traversal);
            control.release(ScanDiscoveryStage::RootStatus);
            control.release(ScanDiscoveryStage::PostParse);
            control
                .wait_until_reached(ScanDiscoveryStage::CommitGuard)
                .await;
            command_tx
                .send(LibraryCommand::SetTrackRating {
                    track_id: TrackId::new("txn-settle-rating-track").expect("valid track ID"),
                    rating: Some(Rating::new(80).expect("valid rating")),
                })
                .await
                .expect("admit the rating command");
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            while let Ok(event) = event_rx.try_recv() {
                assert!(
                    !matches!(event, LibraryEvent::TrackRatingUpdated(_)),
                    "a command must not be serviced while a scan write transaction is open"
                );
            }
            let started = std::time::Instant::now();
            control.release(ScanDiscoveryStage::CommitGuard);
            loop {
                let event = tokio::time::timeout(Duration::from_secs(30), event_rx.recv())
                    .await
                    .expect("the deferred rating must be serviced after the transaction settles")
                    .expect("event channel stays open");
                if matches!(event, LibraryEvent::TrackRatingUpdated(_)) {
                    break;
                }
            }
            let settle_elapsed = started.elapsed();
            assert!(
                settle_elapsed < Duration::from_secs(4),
                "the deferred command settled in {settle_elapsed:?}; it must run against an \
                 idle database well below the five-second busy timeout"
            );
        };

        let (scan_result, ()) = tokio::join!(engine, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("the settle-promptness driver must finish");
        });
        scan_result.expect("scan completes after the write-transaction hold is released");
        drop(command_tx);
        let _ = event_rx.try_recv();

        let rated = track::Entity::find_by_id("txn-settle-rating-track")
            .one(&db)
            .await
            .expect("query rated track")
            .expect("rated track exists");
        assert_eq!(
            rated.rating,
            Some(80),
            "the deferred command must still commit promptly"
        );
    }

    /// Round-4 regression (PR #286 finding j9j81): cancellation that arrives
    /// WHILE the scan is parked at a write boundary's command-settlement wait
    /// must still refuse to open a write transaction. The pre-wait admission
    /// check cannot observe a shutdown that lands mid-park, so the boundary
    /// must re-check admission after the wait resolves — refusing exactly as
    /// the pre-wait check does, keeping the write-transaction gate closed,
    /// admitting no durable mutation, and letting the close drain settle
    /// promptly. Fail-closed: a regression to the pre-round-4 behavior opens
    /// the transaction here and the engine future never settles inside the
    /// join timeout.
    #[tokio::test]
    async fn scan_cancelled_during_command_settlement_wait_never_opens_write_txn() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("scan-settlement-cancel");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let scan_write_txn = ScanWriteTxnGate::default();
        let mut completed = HashMap::new();

        // Simulate dispatched command work whose in-flight arm is held when
        // the scan reaches the root-status boundary: the boundary's pre-wait
        // admission check passes (no cancellation yet) and the boundary parks
        // at the settlement wait, because the reciprocal invariant keeps the
        // write transaction closed while work is in flight.
        let in_flight = CommandInFlightGuard::arm(&scan_write_txn);

        let engine = service_commands_while_scanning(
            initial_scan_shutdown_aware(
                &db,
                &music_dirs,
                &event_tx,
                &refresh,
                &cancellation,
                &hold,
                &scan_write_txn,
            ),
            &scan_write_txn,
            &db,
            &music_dirs,
            &event_tx,
            &command_rx,
            &mut completed,
            &refresh,
        );
        let driver = async {
            // Read-only phases pass uncaptured; the seam this test cares
            // about is signalled at the root-status boundary, right before
            // its settlement wait. With command work in flight the scan
            // parks itself there.
            control.release(ScanDiscoveryStage::Traversal);
            control.release(ScanDiscoveryStage::PostParse);
            control
                .wait_until_reached(ScanDiscoveryStage::CommandSettlement)
                .await;
            assert!(
                !scan_write_txn.is_open(),
                "the boundary must park at the settlement wait before any \
                 write transaction opens"
            );
            // Shutdown lands while the scan is parked mid-wait — exactly the
            // state the pre-wait admission check already passed.
            cancellation.cancel();
            // Mirror close_and_flush: the UI queues the reserved drain marker
            // while the scan is still parked. The reciprocal gate keeps the
            // command branch disabled mid-park, so the marker stays queued;
            // settling the in-flight work must let the cancelled scan refuse
            // and return so the drain unwinds promptly instead of waiting
            // behind a post-cancellation write transaction.
            let (completion_tx, _completion_rx) = async_channel::bounded(1);
            command_tx
                .send(LibraryCommand::Flush {
                    completion: completion_tx,
                })
                .await
                .expect("queue the shutdown drain marker");
            // The in-flight work has settled: the settlement wait resolves
            // and the post-settlement admission re-check must now refuse.
            drop(in_flight);
        };

        let (scan_result, ()) = tokio::time::timeout(Duration::from_secs(120), async {
            tokio::join!(engine, driver)
        })
        .await
        .expect(
            "the engine must settle once the in-flight work settles and the \
             park is released",
        );
        scan_result.expect("the cancelled scan returns cleanly after the park");
        assert!(
            !scan_write_txn.is_open(),
            "a scan cancelled mid-park must not open a post-cancellation \
             write transaction"
        );

        // No durable mutation was admitted after cancellation: the root was
        // never persisted and the traversed file was never upserted.
        let tracks = track::Entity::find().all(&db).await.expect("query tracks");
        assert!(
            tracks.is_empty(),
            "a scan cancelled mid-park must admit no durable writes: {tracks:?}"
        );
        drop(command_tx);
        let _ = event_rx.try_recv();
    }

    /// Round-3 reciprocal regression (cid 4051684281): dispatched command work
    /// that is still in flight must hold the scan AT its write boundaries —
    /// the scan may not open a write transaction the work's own writes would
    /// queue behind. Ordering proof: the rating is dispatched while the scan
    /// is parked at the post-parse rendezvous, and by the time the scan
    /// reaches the in-transaction CommitGuard rendezvous the rating has
    /// already settled, because every boundary crossing requires the in-flight
    /// flag to be clear.
    #[tokio::test]
    async fn scan_write_boundary_defers_while_dispatched_command_work_is_in_flight() {
        let db = rename_test_database().await;
        let rating_path = std::env::temp_dir().join("tributary-inflight-rating.flac");
        insert_rename_test_track(
            &db,
            "inflight-rating-track",
            rating_path.to_string_lossy().as_ref(),
            "InFlight",
            0,
        )
        .await;
        let directory = TestDirectory::new("scan-boundary-inflight");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let scan_write_txn = ScanWriteTxnGate::default();
        let mut completed = HashMap::new();

        let engine = service_commands_while_scanning(
            initial_scan_shutdown_aware(
                &db,
                &music_dirs,
                &event_tx,
                &refresh,
                &cancellation,
                &hold,
                &scan_write_txn,
            ),
            &scan_write_txn,
            &db,
            &music_dirs,
            &event_tx,
            &command_rx,
            &mut completed,
            &refresh,
        );
        let driver = async {
            // Hold the scan at the per-file post-parse rendezvous — still
            // OUTSIDE every write transaction.
            control.release(ScanDiscoveryStage::Traversal);
            control.release(ScanDiscoveryStage::RootStatus);
            control
                .wait_until_reached(ScanDiscoveryStage::PostParse)
                .await;
            // Dispatch the rating while the scan is parked before its
            // boundaries. The yields guarantee the command was received and
            // its work armed, not that the work has settled.
            command_tx
                .send(LibraryCommand::SetTrackRating {
                    track_id: TrackId::new("inflight-rating-track").expect("valid track ID"),
                    rating: Some(Rating::new(80).expect("valid rating")),
                })
                .await
                .expect("admit the rating command");
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            // Now let the scan approach its write boundaries. With the
            // reciprocal gate it parks there until the rating settles; with a
            // regression it opens the transaction immediately and reaches the
            // CommitGuard rendezvous first.
            control.release(ScanDiscoveryStage::PostParse);
            control
                .wait_until_reached(ScanDiscoveryStage::CommitGuard)
                .await;
            let mut settled = false;
            while let Ok(event) = event_rx.try_recv() {
                if matches!(event, LibraryEvent::TrackRatingUpdated(_)) {
                    settled = true;
                    break;
                }
            }
            assert!(
                settled,
                "the scan reached its in-transaction commit guard while dispatched command \
                 work was still in flight — the write-boundary wait regressed"
            );
            control.release(ScanDiscoveryStage::CommitGuard);
        };

        let (scan_result, ()) = tokio::join!(engine, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("the in-flight boundary driver must settle");
        });
        scan_result.expect("scan completes after the boundary hold is released");
        drop(command_tx);
        let _ = event_rx.try_recv();

        let rated = track::Entity::find_by_id("inflight-rating-track")
            .one(&db)
            .await
            .expect("query rated track")
            .expect("rated track exists");
        assert_eq!(
            rated.rating,
            Some(80),
            "the command serviced before the scan's write boundary must still commit"
        );
    }

    /// jq5ld regression: shutdown observed while the per-root status loop is
    /// mid-flight must settle only the already-admitted persist and refuse
    /// every remaining root's status write — a later root must not be left
    /// presenting itself as freshly checked, and the cancelled scan must
    /// return cleanly (no error event) so the close drain can acknowledge.
    #[tokio::test]
    async fn mid_loop_shutdown_refuses_remaining_root_status_writes() {
        let db = rename_test_database().await;
        // Two configured roots: the first engages the per-root status hold,
        // the second must be refused after cancellation. Lexicographic order
        // (`configured_dirs.sort_unstable`) makes root A the held one.
        let root_a = TestDirectory::new("mid-loop-cancel-a");
        let root_b = TestDirectory::new("mid-loop-cancel-b");

        let music_dirs = vec![root_a.path().to_path_buf(), root_b.path().to_path_buf()];
        let (event_tx, event_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let (hold, control) = ScanDiscoveryHold::controlling();
        let scan_write_txn = ScanWriteTxnGate::default();
        let forced_conversions = HashMap::new();
        let authority_guards = HashMap::new();
        let evidence_refreshes = HashMap::new();

        let scan = initial_scan_with_control(
            &db,
            &music_dirs,
            &event_tx,
            &forced_conversions,
            &authority_guards,
            &evidence_refreshes,
            &refresh,
            &cancellation,
            &hold,
            &scan_write_txn,
        );
        let driver = async {
            // The traversal is read-only; let it through. The first per-root
            // status persist then parks at the RootStatus hold.
            control.release(ScanDiscoveryStage::Traversal);
            control
                .wait_until_reached(ScanDiscoveryStage::RootStatus)
                .await;
            // Root B's status row does not exist yet: the per-root loop has
            // only reached root A's persist. Shut down mid-loop.
            cancellation.cancel();
            // Root A's already-admitted persist settles; root B's iteration
            // must then be refused before its status write is admitted.
            control.release(ScanDiscoveryStage::RootStatus);
        };

        let (scan_result, ()) = tokio::join!(scan, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("the mid-loop shutdown must settle within the driver budget");
        });
        scan_result.expect("a mid-loop cancelled scan returns cleanly");

        let rows = library_root::Entity::find()
            .all(&db)
            .await
            .expect("query library roots");
        assert_eq!(
            rows.len(),
            1,
            "exactly the already-admitted root may receive a status write: {rows:?}"
        );
        assert_eq!(
            rows[0].path,
            root_a.path().to_string_lossy().as_ref(),
            "the admitted root-status write must be the first root's, not the refused one"
        );
        assert!(
            library_root::Entity::find_by_id(root_b.path().to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query refused root")
                .is_none(),
            "a root whose status write was refused mid-loop must not appear freshly checked"
        );
        // The scan returned early, so it never published a snapshot.
        assert!(
            event_rx.try_recv().is_err(),
            "a mid-loop cancelled scan must not emit FullSync"
        );
    }

    /// jq5lT regression: the stale-deletion absence proof is read-only
    /// blocking work against the root, so a removable/network filesystem can
    /// park it indefinitely past window close. With shutdown observed while
    /// the proof is unsettled, the probe must be abandoned at the settle
    /// budget, the scan must return (so the reserved `Flush` drain is
    /// acknowledged instead of hanging), and the unproven row must be
    /// preserved — an absence that was never proven never deletes.
    #[tokio::test]
    async fn un_settleable_absence_probe_is_abandoned_and_preserves_row_at_shutdown() {
        let db = rename_test_database().await;
        let directory = TestDirectory::new("absence-shutdown");
        create_root_marker(directory.path()).expect("create root marker");
        write_minimal_wav(&directory.path().join("present.wav"));
        // Establish the root's durable identity first: reconciliation
        // authority (and therefore stale-deletion candidacy) requires a
        // previously confirmed marker identity.
        let enrollment = scan_root(directory.path().to_path_buf());
        persist_root_scan_status(&db, &enrollment, None, true, true, false)
            .await
            .expect("seed confirmed root identity");
        // A row whose file is absent on disk: the stale-deletion candidate
        // whose absence proof will be parked for this test.
        insert_rename_test_track(
            &db,
            "stale-absent-track",
            directory.path().join("gone.wav").to_string_lossy().as_ref(),
            "Gone",
            3,
        )
        .await;

        let music_dirs = vec![directory.path().to_path_buf()];
        let (event_tx, _event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let cancellation = CancellationToken::new();
        let scan_write_txn = ScanWriteTxnGate::default();
        let mut completed = HashMap::new();

        STALE_ABSENCE_PROBE_HELD.store(true, std::sync::atomic::Ordering::SeqCst);
        let no_hold = ScanDiscoveryHold::none();
        let engine = service_commands_while_scanning(
            initial_scan_shutdown_aware(
                &db,
                &music_dirs,
                &event_tx,
                &refresh,
                &cancellation,
                &no_hold,
                &scan_write_txn,
            ),
            &scan_write_txn,
            &db,
            &music_dirs,
            &event_tx,
            &command_rx,
            &mut completed,
            &refresh,
        );
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        let driver = async {
            // Wait until the absence probe is parked (it sets the flag before
            // spinning): the scan is now blocked on filesystem work that will
            // never settle on its own.
            loop {
                if STALE_ABSENCE_PROBE_ARRIVED.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Window close while the proof is unsettled.
            cancellation.cancel();
            // The reserved drain: with the probe budgeted, this acknowledges
            // once the scan abandons the probe; un-budgeted it would hang.
            command_tx
                .send(LibraryCommand::Flush {
                    completion: flush_tx,
                })
                .await
                .expect("admit the reserved flush");
            tokio::time::timeout(Duration::from_secs(30), flush_rx.recv())
                .await
                .expect("the drain must be acknowledged even though the probe never settles")
                .expect("flush completion channel stays open");
        };

        let (scan_result, ()) = tokio::join!(engine, async {
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await
                .expect("the shutdown drain must settle while the probe is parked");
        });
        scan_result.expect("a scan whose absence probe was abandoned returns cleanly");
        // Let the abandoned probe thread exit before its fixtures drop.
        STALE_ABSENCE_PROBE_HELD.store(false, std::sync::atomic::Ordering::SeqCst);

        let preserved = track::Entity::find_by_id("stale-absent-track")
            .one(&db)
            .await
            .expect("query preserved row")
            .expect("an unproven absence must preserve the row");
        assert_eq!(
            preserved.rating,
            Some(88),
            "the preserved row keeps its metadata untouched"
        );
        assert!(
            track::Entity::find()
                .filter(
                    track::Column::FilePath.eq(directory
                        .path()
                        .join("present.wav")
                        .to_string_lossy()
                        .as_ref())
                )
                .one(&db)
                .await
                .expect("query upserted track")
                .is_some(),
            "the scan had progressed past the catalogue loop when shutdown arrived"
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
            .create_regular_playlist("Album")
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
        assert_eq!(renamed.last_played_at_ms, Some(1_748_776_400_123));
        assert_eq!(
            renamed.rating,
            Some(88),
            "directory move preserves app rating"
        );
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
        let RootTrustCommandStart::Pending(pending) = begin_root_trust_command(
            &db,
            &music_dirs,
            &event_tx,
            &test_playlist_sidebar_refresh(),
            &request,
        )
        .await
        .expect("run forced conversion") else {
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
            complete_root_trust_scan(
                &db,
                &music_dirs,
                &event_tx,
                &test_playlist_sidebar_refresh(),
                &pending,
            )
            .await,
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

    /// End-to-end pending-root-trust boundary harness. Drives the real
    /// `process_directory_events` loop: the `ConfirmRootTrust` command queues
    /// the pending trust scan inside the loop, and the loop's own
    /// `pending_trust_scan.take()` boundary performs the backlog discard and
    /// the distinct ordinary authority scan. The racing event is injected
    /// while the authority scan is provably still in flight: the library
    /// event channel is bounded to one slot and the driver withholds draining
    /// after the scan's per-file `ScanProgress`, so the scan's `ScanComplete`
    /// send cannot complete until the driver resumes — which happens only
    /// after the injection. The racing evidence is therefore mid-scan,
    /// distinguishable from backlog that escaped the discard, and still
    /// retained at the following boundary.
    #[tokio::test]
    async fn pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events() {
        let db = Arc::new(rename_test_database().await);
        let target = TestDirectory::new("trust-boundary-backlog");
        // The track exists before the request-building scan so the conversion
        // runs as a legacy enrollment of a non-empty root (no empty-root
        // acknowledgement gate) and the authority scan can later deliver it.
        let boundary_audio = target.path().join("boundary.wav");
        write_minimal_wav(&boundary_audio);
        let scan = scan_root(target.path().to_path_buf());
        let stored = persist_root_scan_status(&db, &scan, None, false, true, false)
            .await
            .expect("persist non-empty legacy target root");
        let request =
            build_root_trust_request(&scan, &stored, RootTrustReason::LegacyEnrollment, 0)
                .expect("build legacy-enrollment request");
        assert!(!request.requires_empty_acknowledgement());
        let request_id = request.request_id;

        let music_dirs = vec![target.path().to_path_buf()];
        let (event_tx, event_rx) = mpsc::channel(WATCHER_EVENT_CAPACITY);
        let ingress_overflowed = Arc::new(AtomicBool::new(false));
        // Synthetic watcher: a real backend with zero installed watches, fed
        // by a deterministic channel the harness controls. The backend is a
        // typed sink only; a shared host with every inotify instance leased
        // by other tenants cannot supply one, which is capacity, not an
        // ordering-contract failure, so the harness skips.
        let Some(idle_backend) = idle_watcher_backend_or_skip() else {
            return;
        };
        let watcher = DirectoryWatcher {
            watcher: idle_backend,
            rx: event_rx,
            ingress_overflowed: Arc::clone(&ingress_overflowed),
            watched_directories: HashSet::new(),
            root_presence: HashMap::new(),
            root_probe_interval: ROOT_PROBE_INTERVAL,
        };

        // A healthy watcher stream queues evidence for the track before the
        // `ConfirmRootTrust` command is even processed — provably before the
        // boundary's discard runs.
        enqueue_watcher_result(
            &event_tx,
            ingress_overflowed.as_ref(),
            Ok(
                notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                    .add_path(boundary_audio.clone()),
            ),
        );
        assert!(!ingress_overflowed.load(Ordering::Acquire));

        // That evidence is actionable: processed normally it would upsert the
        // track incrementally. The boundary must still suppress it, because
        // the distinct ordinary authority scan — not the stale incremental —
        // is what converts the pending trust decision into applied content.
        let mut suppressed = WatcherDebounceBatch::default();
        suppressed.collect(Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File,
        ))
        .add_path(boundary_audio.clone())));
        let suppressed_batch = suppressed.finish().expect("healthy stream stays reliable");
        assert!(
            suppressed_batch
                .upsert_paths
                .contains(boundary_audio.as_path()),
            "the suppressed backlog is real incremental evidence, not access noise"
        );

        // The library event channel is deliberately bounded to one slot. The
        // driver below withholds draining after the authority scan's first
        // per-file ScanProgress, so the scan's subsequent sends back up behind
        // the full channel and its ScanComplete send cannot complete until the
        // driver resumes — which happens only after the racing injection. That
        // backpressure is the acknowledgement pinning the injection mid-scan;
        // an unbounded channel would let the scan race to completion first.
        let (library_events, library_event_rx) = async_channel::bounded(1);
        // Everything the driver consumes must stay visible to the final
        // assertions, so each drain phase records its events in order.
        let pre_injection_events: Arc<std::sync::Mutex<Vec<LibraryEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let post_injection_events: Arc<std::sync::Mutex<Vec<LibraryEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let (command_tx, command_rx) = async_channel::unbounded::<LibraryCommand>();
        command_tx
            .send(LibraryCommand::ConfirmRootTrust(request))
            .await
            .expect("queue the root-trust conversion command");
        let playlist_sidebar_refresh = test_playlist_sidebar_refresh();

        let racing_audio = target.path().join("racing.wav");
        let driver_racing_audio = racing_audio.clone();
        let driver_flag = Arc::clone(&ingress_overflowed);
        let driver_event_rx = &library_event_rx;
        let driver_pre_injection = Arc::clone(&pre_injection_events);
        let driver_post_injection = Arc::clone(&post_injection_events);
        // Regressions in the send order this harness pins must fail the test,
        // not hang it: if the authority scan never emits the phase-1 trigger,
        // the driver blocks in recv() still holding the watcher ingress sender
        // (so the loop can never close), and if the racing batch never emits
        // its trailing PlaylistProjectionsInvalidated, the driver blocks in
        // phase-3 recv() with the outer library_events sender keeping the
        // receiver open. The join below is therefore bounded by a timeout, and
        // this flag records whether the driver got past phase 1 so the
        // timeout's panic can name which regression fired.
        let driver_injected = Arc::new(AtomicBool::new(false));
        let driver_injected_flag = Arc::clone(&driver_injected);
        let driver = async move {
            // Deterministic mid-scan synchronization, phase 1: consume library
            // events until the conversion scan's ScanComplete has passed, then
            // until the authority scan's first per-file ScanProgress. That
            // event only fires after the boundary's discard ran (the discard
            // precedes the authority scan), so the racing evidence queued
            // below is provably distinct from suppressed backlog. Recording
            // keeps every consumed event visible to the final assertions.
            let mut completed_scans = 0u32;
            loop {
                match driver_event_rx.recv().await {
                    Ok(LibraryEvent::ScanComplete) => {
                        completed_scans += 1;
                        // Record completions too — the pre-injection count of
                        // ScanComplete events is the mid-scan proof.
                        driver_pre_injection
                            .lock()
                            .expect("pre-injection record poisoned")
                            .push(LibraryEvent::ScanComplete);
                    }
                    Ok(event @ LibraryEvent::ScanProgress(..)) if completed_scans >= 1 => {
                        // Record the trigger event itself: the driver stops
                        // draining exactly here, and the timeline must show it.
                        driver_pre_injection
                            .lock()
                            .expect("pre-injection record poisoned")
                            .push(event);
                        break;
                    }
                    Ok(event) => driver_pre_injection
                        .lock()
                        .expect("pre-injection record poisoned")
                        .push(event),
                    Err(err) => {
                        panic!("library events ended before the authority scan ran: {err}")
                    }
                }
            }
            // Phase 2: inject while the authority scan is provably still in
            // flight. The driver has stopped draining and the channel holds at
            // most one slot, so of the scan's remaining sequential sends
            // (FullSync, PlaylistProjectionsInvalidated, ScanComplete) at most
            // the first can complete before it blocks; ScanComplete — last in
            // that order — cannot be sent until the driver resumes in phase 3.
            // The injection therefore happens-before the authority scan's
            // completion event. The track appears and its watcher evidence
            // queues while the authority scan is still running: the scan
            // itself never consumes the watcher queue, so this evidence must
            // survive the boundary.
            driver_injected_flag.store(true, Ordering::Release);
            write_minimal_wav(&driver_racing_audio);
            enqueue_watcher_result(
                &event_tx,
                driver_flag.as_ref(),
                Ok(
                    notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                        .add_path(driver_racing_audio.clone()),
                ),
            );
            drop(event_tx);
            // Phase 3: resume draining so the blocked scan can finish, the
            // watcher boundary can apply the racing evidence, and the loop can
            // reach its final send. The batch's trailing
            // PlaylistProjectionsInvalidated (emitted by
            // settle_playlist_projections_after_watcher_batch after the racing
            // upsert commits) is the loop's last event for this scenario, so
            // consuming it proves the loop finished mutating and can exit
            // cleanly on its exhausted watcher stream. Events stay recorded so
            // nothing the driver consumed is lost to the assertions.
            let mut authority_scan_complete = false;
            loop {
                match driver_event_rx.recv().await {
                    Ok(event) => {
                        let is_scan_complete = matches!(event, LibraryEvent::ScanComplete);
                        let is_projections =
                            matches!(event, LibraryEvent::PlaylistProjectionsInvalidated);
                        driver_post_injection
                            .lock()
                            .expect("post-injection record poisoned")
                            .push(event);
                        if is_scan_complete {
                            authority_scan_complete = true;
                        } else if is_projections && authority_scan_complete {
                            break;
                        }
                    }
                    Err(err) => panic!(
                        "library events ended before the boundary applied the racing evidence: {err}"
                    ),
                }
            }
        };

        let mut completed_commands = HashMap::new();
        let never_cancelled = CancellationToken::new();
        // Bound the join so the exact regressions this test exists to report
        // fail fast instead of hanging the suite until the CI job timeout.
        // 60s is orders of magnitude above this harness's normal sub-second
        // run (tiny directory, real scan path, deterministic channel
        // synchronization) yet far below any CI job timeout, and the panic
        // names the specific regression from the driver's recorded progress.
        const DRIVER_JOIN_TIMEOUT: Duration = Duration::from_secs(60);
        let (loop_result, ()) = tokio::time::timeout(
            DRIVER_JOIN_TIMEOUT,
            // tokio::join! is itself an async expression (it polls inline and
            // evaluates to the outputs tuple), so the timeout needs a real
            // future here: the async block preserves the join's semantics —
            // both futures driven concurrently to completion on this runtime.
            async {
                tokio::join!(
                    process_directory_events(
                        &db,
                        &music_dirs,
                        &library_events,
                        &command_rx,
                        &mut completed_commands,
                        watcher,
                        &playlist_sidebar_refresh,
                        &never_cancelled,
                    ),
                    driver,
                )
            },
        )
        .await
        .unwrap_or_else(|_: tokio::time::error::Elapsed| {
            // The two regressions are distinguished by the driver's recorded
            // progress at the moment of the timeout: still before the racing
            // injection (phase 1) or already past it (phase 3).
            assert!(
                driver_injected.load(Ordering::Acquire),
                "watcher-boundary harness timed out after {DRIVER_JOIN_TIMEOUT:?} in \
                 phase 1: the authority scan never emitted its first per-file \
                 ScanProgress trigger, so the driver is blocked in recv() still \
                 holding the watcher ingress sender and the loop can never close — \
                 exactly the regression this test exists to report"
            );
            panic!(
                "watcher-boundary harness timed out after {DRIVER_JOIN_TIMEOUT:?} in \
                 phase 3: the racing batch never produced its trailing \
                 PlaylistProjectionsInvalidated (the loop stalled mid-batch, or exited \
                 while the outer library_events sender kept the receiver open), so the \
                 driver is blocked in recv() — exactly the regression this test exists \
                 to report"
            );
        });
        loop_result.expect("watcher loop exits cleanly");

        // The injection was pinned mid-scan by the bounded channel and the
        // withheld drain; the recorded timeline proves it. At injection time
        // exactly one scan had completed — the conversion scan — so the
        // authority scan's ScanComplete was emitted strictly after the racing
        // evidence was injected, and the driver stopped draining right on the
        // authority scan's first per-file ScanProgress.
        assert_eq!(
            pre_injection_events
                .lock()
                .expect("pre-injection record poisoned")
                .iter()
                .filter(|event| matches!(event, LibraryEvent::ScanComplete))
                .count(),
            1,
            "the driver must inject while only the conversion scan has completed"
        );
        assert!(matches!(
            pre_injection_events
                .lock()
                .expect("pre-injection record poisoned")
                .last(),
            Some(LibraryEvent::ScanProgress(..))
        ));

        // Reassemble the full timeline in channel order: everything the
        // driver consumed before the injection, everything it consumed after,
        // then whatever remained channel-resident (empty when the driver's
        // final drain ran to the loop's last send).
        let mut events: Vec<LibraryEvent> = std::mem::take(
            &mut *pre_injection_events
                .lock()
                .expect("pre-injection record poisoned"),
        );
        events.extend(std::mem::take(
            &mut *post_injection_events
                .lock()
                .expect("post-injection record poisoned"),
        ));
        events.extend(std::iter::from_fn(|| library_event_rx.try_recv().ok()));

        let boundary_delivered = boundary_audio.to_string_lossy().into_owned();
        let racing_delivered = racing_audio.to_string_lossy().into_owned();

        // The boundary suppresses the pre-authority backlog even though the
        // stream is healthy: no incremental upsert for the discarded evidence
        // may appear anywhere — the authority scan is what delivered the
        // content instead.
        let scan_boundary = events
            .iter()
            .rposition(|event| matches!(event, LibraryEvent::ScanComplete))
            .expect("the authority scan completes");
        assert!(events[..=scan_boundary].iter().any(|event| matches!(
            event,
            LibraryEvent::FullSync(tracks)
                if tracks.iter().any(|track| {
                    track.file_path.as_deref() == Some(boundary_delivered.as_str())
                })
        )));
        for event in &events {
            assert!(
                !matches!(
                    event,
                    LibraryEvent::TrackUpserted(track)
                        if track.file_path.as_deref() == Some(boundary_delivered.as_str())
                ),
                "suppressed backlog must not be applied incrementally: {event:?}"
            );
        }

        // The racing evidence was queued after the discard and during the
        // scan, so it survived the boundary and applies at the following loop
        // boundary — strictly after the authority scan completed.
        assert!(
            events[scan_boundary + 1..].iter().any(|event| matches!(
                event,
                LibraryEvent::TrackUpserted(track)
                    if track.file_path.as_deref() == Some(racing_delivered.as_str())
            )),
            "racing evidence survives the boundary and applies after it"
        );

        let completion = completed_commands
            .get(&request_id)
            .expect("boundary completes the pending root-trust command");
        assert_eq!(completion.path, target.path());
        assert_eq!(completion.reason, RootTrustReason::LegacyEnrollment);
        assert_eq!(completion.outcome, RootTrustOutcome::Active);
        assert!(
            track::Entity::find()
                .filter(track::Column::FilePath.eq(boundary_delivered.as_str()))
                .one(db.as_ref())
                .await
                .expect("query boundary audio after authority scan")
                .is_some(),
            "suppression loses no content: the authority scan delivers the track"
        );
        assert!(
            !ingress_overflowed.load(Ordering::Acquire),
            "a healthy stream is never marked unreliable at the trust boundary"
        );
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
            begin_root_trust_command(
                &db,
                &[directory.path().to_path_buf()],
                &event_tx,
                &test_playlist_sidebar_refresh(),
                &request,
            )
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
            begin_root_trust_command(
                &db,
                &[directory.path().to_path_buf()],
                &event_tx,
                &test_playlist_sidebar_refresh(),
                &request,
            )
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
            &test_playlist_sidebar_refresh(),
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
            begin_root_trust_command(
                &db,
                &[directory.path().to_path_buf()],
                &event_tx,
                &test_playlist_sidebar_refresh(),
                &request,
            )
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
        let RootTrustCommandStart::Pending(pending) = begin_root_trust_command(
            &db,
            &music_dirs,
            &event_tx,
            &test_playlist_sidebar_refresh(),
            &request,
        )
        .await
        .expect("convert root") else {
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
            complete_root_trust_scan(
                &db,
                &music_dirs,
                &event_tx,
                &test_playlist_sidebar_refresh(),
                &pending,
            )
            .await,
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
        let RootTrustCommandStart::Pending(mut pending) = begin_root_trust_command(
            &db,
            &music_dirs,
            &event_tx,
            &test_playlist_sidebar_refresh(),
            &request,
        )
        .await
        .expect("convert root") else {
            panic!("queue guarded follow-up");
        };
        pending.expected_mount_generation = pending.expected_mount_generation.wrapping_add(1);

        assert_eq!(
            complete_root_trust_scan(
                &db,
                &music_dirs,
                &event_tx,
                &test_playlist_sidebar_refresh(),
                &pending,
            )
            .await,
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

        initial_scan(
            &db,
            &music_dirs,
            &event_tx,
            &test_playlist_sidebar_refresh(),
        )
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
        let RootTrustCommandStart::Pending(pending) = begin_root_trust_command(
            &db,
            &music_dirs,
            &event_tx,
            &test_playlist_sidebar_refresh(),
            &request,
        )
        .await
        .expect("convert root") else {
            panic!("queue guarded follow-up");
        };
        let parked = directory.path().with_extension("temporarily-unavailable");
        std::fs::rename(directory.path(), &parked).expect("hide root before follow-up");

        assert_eq!(
            complete_root_trust_scan(
                &db,
                &music_dirs,
                &event_tx,
                &test_playlist_sidebar_refresh(),
                &pending,
            )
            .await,
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
        initial_scan(
            &db,
            &music_dirs,
            &event_tx,
            &test_playlist_sidebar_refresh(),
        )
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
            &test_playlist_sidebar_refresh(),
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
        initial_scan(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &test_playlist_sidebar_refresh(),
        )
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
            &test_playlist_sidebar_refresh(),
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
            &test_playlist_sidebar_refresh(),
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
            &test_playlist_sidebar_refresh(),
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
            &test_playlist_sidebar_refresh(),
            &mut completed,
            LibraryCommand::ConfirmRootTrust(request),
        )
        .await
        .expect("same evidence is processed again");
        finish_pending_root_trust_scan(
            &db,
            &[directory.path().to_path_buf()],
            &event_tx,
            &test_playlist_sidebar_refresh(),
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

        assert!(matches!(
            revalidate_scan_root_for_path(&audio_path, &mut scans, None)
                .await
                .expect("run initial authority task"),
            RootRevalidation::Authorized
        ));
        std::fs::write(
            root_identity_path(directory.path()),
            format!("{ROOT_IDENTITY_PREFIX}{}\n", Uuid::new_v4()),
        )
        .expect("replace root identity");
        assert!(matches!(
            revalidate_scan_root_for_path(&audio_path, &mut scans, None)
                .await
                .expect("run changed authority task"),
            RootRevalidation::Rejected(Some(root)) if root == directory.path()
        ));
        assert!(!scans[0].content_authorized);
        assert!(!scans[0].reconciliation_authoritative);
        assert!(matches!(
            revalidate_scan_root_for_path(&audio_path, &mut scans, None)
                .await
                .expect("skip disabled authority task"),
            RootRevalidation::Rejected(None)
        ));
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
    fn stale_deletion_ignores_rows_outside_scanned_roots() {
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

        assert_eq!(
            scanned_paths(&collect_audio_files(&scans)),
            vec![audio_path]
        );
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
        assert_eq!(scanned_paths(&child_scan.audio_files), vec![audio_path]);
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
            tag_write_debris: Vec::new(),
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
        assert_eq!(scanned_paths(&scan.audio_files), vec![readable_audio]);
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
            tag_write_debris: Vec::new(),
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
            audio_files: vec![(root.join("song.mp3"), String::new())],
            tag_write_debris: Vec::new(),
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

    async fn tracks_by_path(db: &DatabaseConnection) -> Vec<track::Model> {
        track::Entity::find()
            .order_by_asc(track::Column::FilePath)
            .all(db)
            .await
            .expect("query tracks")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_configured_root_is_indexed_and_rescans_stably() {
        let container = TestDirectory::new("symlinked-root");
        let target = container.path().join("data-music");
        std::fs::create_dir_all(target.join("album")).expect("create symlink target");
        write_minimal_wav(&target.join("album").join("nested.wav"));
        write_minimal_wav(&target.join("top.wav"));
        let root = container.path().join("Music");
        std::os::unix::fs::symlink(&target, &root).expect("link the configured root");
        let db = rename_test_database().await;
        let (event_tx, _event_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        let roots = [root.clone()];

        initial_scan(&db, &roots, &event_tx, &refresh)
            .await
            .expect("first scan");
        let first = tracks_by_path(&db).await;
        assert_eq!(
            first
                .iter()
                .map(|row| PathBuf::from(&row.file_path))
                .collect::<Vec<_>>(),
            [root.join("album").join("nested.wav"), root.join("top.wav")],
            "rows are keyed under the configured spelling of the root"
        );
        let state = library_root::Entity::find_by_id(root.to_string_lossy().into_owned())
            .one(&db)
            .await
            .expect("query root state")
            .expect("root state exists");
        assert!(state.identity_confirmed && state.is_available);

        initial_scan(&db, &roots, &event_tx, &refresh)
            .await
            .expect("rescan");
        assert_eq!(
            tracks_by_path(&db).await,
            first,
            "a rescan keeps every row, ID and path"
        );

        std::fs::remove_file(target.join("top.wav")).expect("delete a track on disk");
        initial_scan(&db, &roots, &event_tx, &refresh)
            .await
            .expect("reconciling scan");
        assert_eq!(
            tracks_by_path(&db).await,
            first[..1],
            "reconciliation through the symlinked root removes only the deleted track"
        );
    }

    /// Run one production engine startup through its initial scan, then tear
    /// it down the way the application does at close.
    async fn run_engine_startup(
        db: &DatabaseConnection,
        roots: Vec<PathBuf>,
        pending_root_reauthorizations: Vec<RootReauthorizationRequest>,
        forget_unconfigured_tracks: bool,
    ) {
        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let (refresh, refresh_rx) =
            super::super::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let source_registry =
            crate::source_registry::SourceRegistry::new(tokio::runtime::Handle::current());
        let invalidations = source_registry.subscribe_invalidations();
        let services = LibraryEngineServices::new(
            refresh,
            refresh_rx,
            coordinator,
            coordinator_shutdown,
            source_registry,
            invalidations,
        );
        let engine = LibraryEngine::new(
            db.clone(),
            roots,
            pending_root_reauthorizations,
            forget_unconfigured_tracks,
            event_tx,
            command_rx,
            services,
            CancellationToken::new(),
        );
        let engine_task = tokio::spawn(engine.run());
        tokio::time::timeout(Duration::from_secs(60), async {
            while let Ok(event) = event_rx.recv().await {
                if matches!(event, LibraryEvent::ScanComplete) {
                    return;
                }
            }
            panic!("engine stopped before its initial scan completed");
        })
        .await
        .expect("engine startup completes its initial scan");
        engine_task.abort();
        drop(command_tx);
    }

    #[tokio::test]
    async fn startup_forgets_tracks_of_removed_roots_only() {
        let container = TestDirectory::new("removed-root");
        let [removed, offline, reauthorizing] =
            ["removed", "offline", "reauthorizing"].map(|name| container.path().join(name));
        for root in [&removed, &offline, &reauthorizing] {
            std::fs::create_dir(root).expect("create library root");
            write_minimal_wav(&root.join("song.wav"));
        }
        let db = rename_test_database().await;
        run_engine_startup(
            &db,
            vec![removed.clone(), offline.clone(), reauthorizing.clone()],
            Vec::new(),
            true,
        )
        .await;
        let indexed = tracks_by_path(&db).await;
        assert_eq!(indexed.len(), 3, "every root is indexed: {indexed:?}");
        let removed_track = indexed
            .iter()
            .find(|row| Path::new(&row.file_path).starts_with(&removed))
            .expect("removed root was indexed")
            .clone();
        let manager = super::super::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_regular_playlist("Removed root")
            .await
            .expect("create playlist");
        manager
            .add_track(&playlist.id, &removed_track)
            .await
            .expect("link removed-root track to playlist");

        // `removed` leaves the configuration, `offline` stays configured but
        // is unmounted, and `reauthorizing` is only named by a pending
        // reauthorization (as after a manual config edit).
        std::fs::rename(&offline, container.path().join("offline-unmounted"))
            .expect("take the offline root away");
        let pending = vec![RootReauthorizationRequest::new(
            Uuid::new_v4(),
            reauthorizing.clone(),
            container.path().join("reauthorized"),
        )];

        run_engine_startup(&db, vec![offline.clone()], pending.clone(), false).await;
        assert_eq!(
            tracks_by_path(&db).await.len(),
            3,
            "a defaulted configuration never forgets tracks"
        );

        run_engine_startup(&db, vec![offline.clone()], pending, true).await;
        let remaining = tracks_by_path(&db).await;
        assert!(
            remaining
                .iter()
                .all(|row| !Path::new(&row.file_path).starts_with(&removed)),
            "the removed root's tracks are forgotten: {remaining:?}"
        );
        assert_eq!(
            indexed
                .iter()
                .filter(|row| row.id != removed_track.id)
                .cloned()
                .collect::<Vec<_>>(),
            remaining,
            "the unavailable and reauthorizing roots keep their rows unchanged"
        );
        let entry = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .one(&db)
            .await
            .expect("query playlist entry")
            .expect("the playlist entry survives as an unmatched entry");
        assert_eq!(entry.local_track_id, None);
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
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: Some("MP3".to_string()),
            play_count: 0,
            last_played_at_ms: None,
            rating: None,
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
            composer: None,
            year: Some(2020),
            track_number: Some(3),
            disc_number: Some(1),
            duration_secs: Some(240),
            bitrate_kbps: Some(320),
            sample_rate_hz: Some(44100),
            format: Some("FLAC".to_string()),
            play_count: 5,
            last_played_at_ms: Some(1_748_776_400_123),
            rating: Some(91),
            date_added: "2025-01-15T10:30:00+00:00".to_string(),
            date_modified: "2025-06-01T14:00:00+00:00".to_string(),
            file_size_bytes: Some(30_000_000),
        };

        let track = db_model_to_track(&model);

        assert_eq!(
            track.native_track_id.as_ref().map(|id| id.as_str()),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
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
        assert_eq!(
            track.rating,
            TrackRating::writable(Some(Rating::new(91).unwrap()))
        );
        assert_eq!(
            track.last_played.map(|instant| instant.timestamp_millis()),
            Some(1_748_776_400_123)
        );
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
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            last_played_at_ms: None,
            rating: None,
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
        assert_eq!(track.rating, TrackRating::writable(None));
        assert_eq!(track.last_played, None);
    }

    #[test]
    fn test_db_model_to_track_rejects_corrupt_stored_ratings() {
        for corrupt in [0, 101] {
            let model = track::Model {
                id: format!("corrupt-rating-{corrupt}"),
                file_path: format!("/music/corrupt-{corrupt}.mp3"),
                title: "Corrupt rating".to_string(),
                artist_name: "Artist".to_string(),
                album_artist_name: None,
                album_title: "Album".to_string(),
                genre: None,
                composer: None,
                year: None,
                track_number: None,
                disc_number: None,
                duration_secs: None,
                bitrate_kbps: None,
                sample_rate_hz: None,
                format: None,
                play_count: 0,
                last_played_at_ms: None,
                rating: Some(corrupt),
                date_added: "2025-01-01T00:00:00Z".to_string(),
                date_modified: "2025-01-01T00:00:00Z".to_string(),
                file_size_bytes: None,
            };

            assert_eq!(
                db_model_to_track(&model).rating,
                TrackRating::writable(None),
                "corrupt stored rating {corrupt} must fail closed"
            );
        }
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
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            last_played_at_ms: None,
            rating: None,
            date_added: "2025-01-01T00:00:00+00:00".to_string(),
            date_modified: "2025-01-01T00:00:00+00:00".to_string(),
            file_size_bytes: None,
        };

        // The exact database key is the source-native identity, and the UUID
        // compatibility projection is deterministic rather than random.
        let first = db_model_to_track(&model);
        let second = db_model_to_track(&model);
        assert_eq!(
            first.native_track_id.as_ref().map(|id| id.as_str()),
            Some("not-a-valid-uuid")
        );
        assert_eq!(first.id, second.id);
        assert_eq!(
            first.id,
            Uuid::parse_str("de7878c3-1d8f-5e45-a0a2-da3616e6a623")
                .expect("frozen compatibility UUID")
        );
        assert!(!first.id.is_nil());
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
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            last_played_at_ms: Some(i64::MAX),
            rating: None,
            date_added: "not-a-date".to_string(),
            date_modified: "also-not-a-date".to_string(),
            file_size_bytes: None,
        };

        let track = db_model_to_track(&model);
        // Invalid dates should result in None, not a panic.
        assert!(track.date_added.is_none());
        assert!(track.date_modified.is_none());
        assert!(track.last_played.is_none());
    }

    #[test]
    fn test_db_model_to_track_repairs_negative_play_count_at_the_read_boundary() {
        let model = track::Model {
            id: "legacy-negative-play-count".to_string(),
            file_path: "/music/legacy.flac".to_string(),
            title: "Legacy".to_string(),
            artist_name: "Artist".to_string(),
            album_artist_name: None,
            album_title: "Album".to_string(),
            genre: None,
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: Some("FLAC".to_string()),
            play_count: -1,
            last_played_at_ms: None,
            rating: None,
            date_added: "2025-01-01T00:00:00Z".to_string(),
            date_modified: "2025-01-01T00:00:00Z".to_string(),
            file_size_bytes: None,
        };

        assert_eq!(db_model_to_track(&model).play_count, Some(0));
    }

    #[test]
    fn test_get_mtime_nonexistent_file() {
        let result = get_mtime(std::path::Path::new("/nonexistent/path/file.flac"));
        // Should return empty string, not panic.
        assert!(result.is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════
    // Q4 engine-loop/GTK responsiveness lane (tr-am6qr).
    //
    // Measurement harness for the engine-side startup endpoints this lane
    // owns: time to publish the startup snapshot (FullSync / ScanComplete)
    // and cancellation settlement of the FIFO barrier when commands and the
    // shutdown Flush are admitted while the initial scan is still in
    // flight. The GTK-side publication/stall endpoints are measured by the
    // env-gated helper in `ui::browser::tests` (see that module). Sibling
    // lane tr-7nguk owns the shared fixture/parse-delay seam; until it
    // lands this harness stands alone on committed fixture bytes.
    //
    // The harness is `#[ignore]`d because it is an explicit measurement
    // run (budgets are recorded per runner in
    // `docs/engine-ui-responsiveness.md`), not a CI gate.
    // ═══════════════════════════════════════════════════════════════════

    /// Fixture size for the startup benchmark (`TRIBUTARY_Q4_TRACKS`).
    ///
    /// Deliberately distinct from the synthetic-library harness's
    /// `TRIBUTARY_Q4_LIBRARY_TRACKS` (see `perf_fixtures`): a shared variable
    /// would let one measurement run silently resize the other's fixture.
    fn q4_startup_fixture_track_count() -> usize {
        std::env::var("TRIBUTARY_Q4_TRACKS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(400)
    }

    /// Write `count` valid audio files (copies of the committed 99-byte
    /// silent FLAC) so the scanner parses real metadata for every row.
    fn q4_write_startup_fixture(root: &Path, count: usize) {
        let audio = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/audio/silence.flac"
        ));
        for index in 0..count {
            let path = root.join(format!("q4-track-{index:05}.flac"));
            std::fs::write(&path, audio).expect("write q4 startup fixture audio");
        }
    }

    /// Observed startup timeline, in microseconds from engine start.
    #[derive(Debug, Default)]
    struct Q4StartupTimeline {
        server_ready_us: Option<u64>,
        fullsync_us: Option<u64>,
        fullsync_rows: usize,
        scan_complete_us: Option<u64>,
        scan_progress_events: usize,
        /// When the during-scan-admitted history command published its
        /// committed-row event (always after `scan_complete_us`).
        during_scan_command_applied_us: Option<u64>,
        flush_ack_us: Option<u64>,
    }

    impl Q4StartupTimeline {
        /// Record one engine event's arrival offset.
        fn record(&mut self, event: &LibraryEvent, elapsed_us: u64, command_track: &str) {
            match event {
                LibraryEvent::ServerPlaylistRuntimeReady(_) => {
                    self.server_ready_us.get_or_insert(elapsed_us);
                }
                LibraryEvent::FullSync(tracks) => {
                    self.fullsync_us.get_or_insert(elapsed_us);
                    self.fullsync_rows = tracks.len();
                }
                LibraryEvent::ScanComplete => {
                    self.scan_complete_us.get_or_insert(elapsed_us);
                }
                LibraryEvent::ScanProgress(_, _) => {
                    self.scan_progress_events += 1;
                }
                LibraryEvent::PlaybackHistoryUpdated(track)
                    if track.native_track_id.as_ref().map(TrackId::as_str)
                        == Some(command_track) =>
                {
                    self.during_scan_command_applied_us = Some(elapsed_us);
                }
                _ => {}
            }
        }
    }

    /// Drive the production `run()` shape and collect the startup timeline.
    ///
    /// The command FIFO (one history mutation plus the Flush barrier) is
    /// enqueued by the caller before the engine task is polled, and the
    /// scan owns the engine from startup — so both are structurally
    /// during-scan admissions regardless of machine speed (the interleaved
    /// `service_commands_while_scanning` loop picks them up at its txn
    /// boundaries).
    async fn q4_collect_startup_timeline(
        event_rx: &async_channel::Receiver<LibraryEvent>,
        flush_rx: async_channel::Receiver<()>,
        command_track: &str,
        start: std::time::Instant,
    ) -> Q4StartupTimeline {
        let mut timeline = Q4StartupTimeline::default();
        let mut events_open = true;
        let ack = loop {
            if events_open {
                tokio::select! {
                    biased;
                    // Event arm FIRST under `biased;`: the engine publishes
                    // ScanComplete (and any during-scan command's event)
                    // before it acks the Flush barrier, so when the collector
                    // is polled with both channels ready the queued events
                    // must be stamped ahead of the ack. Polling the flush arm
                    // first let the ack win that race: the loop broke, the
                    // post-loop drain stamped ScanComplete AFTER flush_ack_us,
                    // and the bench's `scan_complete_us <= flush_ack_us`
                    // settlement assert panicked (tr-kcfmsh, reproduced on
                    // main fc5d0d6a).
                    event = event_rx.recv() => {
                        match event {
                            Ok(event) => {
                                // Stamp arrival, not wait start: sampling
                                // before the select! would record every
                                // event with the previous iteration's time
                                // and underreport the startup endpoints
                                // whenever the loop blocked on an empty
                                // channel.
                                let elapsed_us = start.elapsed().as_micros() as u64;
                                timeline.record(&event, elapsed_us, command_track);
                            }
                            // Sender dropped: every published event has
                            // already been consumed (a closed async_channel
                            // delivers its buffered values before reporting
                            // close). Stop polling this arm — a closed
                            // receiver resolves immediately and would
                            // otherwise preempt the pending ack on every
                            // poll — and wait on the ack alone.
                            Err(_) => { events_open = false; }
                        }
                    }
                    ack = flush_rx.recv() => { break ack; }
                }
            } else {
                break flush_rx.recv().await;
            }
        };
        assert!(ack.is_ok(), "Flush barrier must be acknowledged");
        timeline.flush_ack_us = Some(start.elapsed().as_micros() as u64);
        // The command event was published before the flush ack, so a
        // bounded non-blocking drain settles any remaining race.
        while let Ok(event) = event_rx.try_recv() {
            let elapsed_us = start.elapsed().as_micros() as u64;
            timeline.record(&event, elapsed_us, command_track);
        }
        timeline
    }

    /// Regression (PR #291 review Correction 1): the startup-timeline
    /// sampler must stamp each event when it ARRIVES, not when the
    /// `select!` wait began. Feed two events at known post-entry delays
    /// and require the recorded `*_us` values to reflect those delays —
    /// the defective sampler stamped the first event with ~0 µs (loop
    /// entry) because it sampled before blocking on the empty channel.
    /// Cheap by construction: no fixture tree, no engine, no `#[ignore]`.
    #[tokio::test]
    async fn q4_startup_timeline_stamps_arrival_not_wait_start() {
        let (event_tx, event_rx) = async_channel::unbounded();
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        const FIRST_DELAY_MS: u64 = 60;
        const SECOND_DELAY_MS: u64 = 140;
        let start = std::time::Instant::now();
        let feeder = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(FIRST_DELAY_MS)).await;
            event_tx
                .send(LibraryEvent::ScanComplete)
                .await
                .expect("send first timed event");
            tokio::time::sleep(std::time::Duration::from_millis(
                SECOND_DELAY_MS - FIRST_DELAY_MS,
            ))
            .await;
            event_tx
                .send(LibraryEvent::FullSync(Vec::new()))
                .await
                .expect("send second timed event");
            flush_tx.send(()).await.expect("ack the flush barrier");
        });
        let timeline = q4_collect_startup_timeline(
            &event_rx,
            flush_rx,
            "q4-timing-test-no-command-track",
            start,
        )
        .await;
        feeder.await.expect("feeder task completes");

        // Both stamps are sampled after `recv()` resolves, so each is at
        // least its feeder delay measured from `start` (captured before the
        // feeder spawned; tokio sleeps never fire early).
        let scan_complete_us = timeline
            .scan_complete_us
            .expect("ScanComplete recorded with arrival stamp");
        let fullsync_us = timeline
            .fullsync_us
            .expect("FullSync recorded with arrival stamp");
        assert!(
            scan_complete_us >= FIRST_DELAY_MS * 1_000,
            "ScanComplete must be stamped at arrival (>= {FIRST_DELAY_MS} ms), got {scan_complete_us} us"
        );
        assert!(
            fullsync_us >= SECOND_DELAY_MS * 1_000,
            "FullSync must be stamped at arrival (>= {SECOND_DELAY_MS} ms), got {fullsync_us} us"
        );
        assert!(
            fullsync_us > scan_complete_us,
            "arrival stamps must be strictly monotonic across events"
        );
        // The flush send happens after the second feeder sleep, so its
        // stamp is floored at the second delay. The event arm is polled
        // first under `biased;`, so a queued FullSync is stamped before
        // the ack even when both channels are ready at the same poll —
        // the ack never preempts already-published events (tr-kcfmsh).
        let flush_ack_us = timeline.flush_ack_us.expect("flush ack recorded");
        assert!(
            flush_ack_us >= SECOND_DELAY_MS * 1_000,
            "the flush ack must be stamped at its own arrival (>= {SECOND_DELAY_MS} ms), got {flush_ack_us} us"
        );
        assert!(
            flush_ack_us >= fullsync_us,
            "the flush ack must not preempt an already-queued event (>= {fullsync_us} us), got {flush_ack_us} us"
        );
    }

    /// Regression (tr-kcfmsh): the settlement sampler must drain
    /// already-published events BEFORE selecting the Flush ack. Pre-load
    /// the event channel with a large queue whose LAST entry is
    /// ScanComplete, and ack the Flush barrier before the sampler is ever
    /// polled, so both channels are ready at its first `select!`. Under
    /// the old flush-first bias the ack won that race immediately and the
    /// post-loop drain stamped ScanComplete milliseconds AFTER the ack,
    /// panicking the Q4 bench (`scan_complete_us <= flush_ack_us`). With
    /// event-first bias every queued event is consumed in-loop and the
    /// ack is stamped last. The queue is sized so the defective ordering
    /// fails the assert by a wide, deterministic margin rather than a
    /// clock-granularity flake. Cheap by construction: no fixture tree,
    /// no engine, no `#[ignore]`.
    #[tokio::test]
    async fn q4_startup_timeline_acks_only_after_queued_events_drain() {
        let (event_tx, event_rx) = async_channel::unbounded();
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        const QUEUED_PROGRESS_EVENTS: usize = 50_000;
        // Pre-load both channels BEFORE the sampler runs: unbounded event
        // sends and the single buffered flush permit complete without a
        // receiver, so both arms are ready at the sampler's first poll —
        // the exact interleaving that panicked the bench on main.
        for i in 0..QUEUED_PROGRESS_EVENTS as u64 {
            event_tx
                .send(LibraryEvent::ScanProgress(i, QUEUED_PROGRESS_EVENTS as u64))
                .await
                .expect("queue progress event");
        }
        event_tx
            .send(LibraryEvent::ScanComplete)
            .await
            .expect("queue ScanComplete last");
        flush_tx.send(()).await.expect("ack the flush barrier");

        let start = std::time::Instant::now();
        // Hold the sender open across collection: the harness invariant is
        // that the event channel stays open until the flush ack, and with
        // event-first bias a closed event channel would win the poll over
        // the pending ack.
        let timeline = {
            let _keep_open = event_tx;
            let _keep_open_ack = flush_tx;
            q4_collect_startup_timeline(
                &event_rx,
                flush_rx,
                "q4-timing-test-no-command-track",
                start,
            )
            .await
        };

        let scan_complete_us = timeline
            .scan_complete_us
            .expect("queued ScanComplete recorded");
        let flush_ack_us = timeline.flush_ack_us.expect("flush ack recorded");
        assert_eq!(
            timeline.scan_progress_events, QUEUED_PROGRESS_EVENTS,
            "every queued progress event must be consumed before the ack"
        );
        assert!(
            scan_complete_us <= flush_ack_us,
            "the Flush barrier must not preempt queued events: ScanComplete at \
             {scan_complete_us} us must not land after the ack at {flush_ack_us} us"
        );
    }

    /// Q4 measurement: production engine startup on a sized fixture tree,
    /// with a history command and the shutdown Flush admitted during the
    /// initial scan. Prints `Q4_ENGINE_METRIC` lines and asserts the
    /// settlement contract: the scan publishes its snapshot, the admitted
    /// command is serviced by the interleaved scan-time command loop (it
    /// may commit before or after the scan settles — timing-dependent; the
    /// scan-settle wait it used to block on is the R9 delay this engine
    /// removed), and the FIFO barrier acks only after the scan drains —
    /// nothing admitted during the scan is lost or reordered.
    #[ignore = "explicit Q4 measurement harness; run with --ignored (tr-am6qr)"]
    // The parse-delay window inside intentionally holds a std::MutexGuard
    // across the engine-run awaits — exclusivity with the sibling
    // large-library harness is the point (refinery F2, PR #285). This
    // current-thread tokio test cannot deadlock on it; only sibling
    // test-harness threads block, which is the required serialization.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn q4_engine_startup_and_during_scan_admission_benchmark() {
        let track_count = q4_startup_fixture_track_count();
        let db = rename_test_database().await;
        let fixture = TestDirectory::new("q4-startup-responsiveness");
        let root = fixture.path().to_path_buf();
        q4_write_startup_fixture(&root, track_count);
        let marker = create_root_marker(&root)
            .expect("create q4 root marker")
            .identity;
        insert_reauthorization_root(&db, &root, &marker, true).await;
        const COMMAND_TRACK: &str = "q4-during-scan-cmd";
        insert_playback_history_test_track(&db, COMMAND_TRACK, 0, Some(0)).await;

        let (event_tx, event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let (refresh, refresh_rx) =
            super::super::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let source_registry =
            crate::source_registry::SourceRegistry::new(tokio::runtime::Handle::current());
        let invalidations = source_registry.subscribe_invalidations();
        let services = LibraryEngineServices::new(
            refresh,
            refresh_rx,
            coordinator,
            coordinator_shutdown,
            source_registry,
            invalidations,
        );
        // Never-cancelled token: the harness mirrors a window that stays
        // open for the whole measurement (production aborts the spawned
        // engine task at teardown, mirrored below, so no cancellation
        // signal is needed to end it).
        let scan_cancellation = tokio_util::sync::CancellationToken::new();
        let engine = LibraryEngine::new(
            db.clone(),
            vec![root.clone()],
            Vec::new(),
            // The command track lives outside the fixture root.
            false,
            event_tx,
            command_rx,
            services,
            scan_cancellation,
        );
        let start = std::time::Instant::now();

        // Admit during the scan: the engine task has not been polled yet,
        // so both commands reach the channel while the initial scan owns
        // the engine (service_commands_while_scanning interleaves them at
        // its txn boundaries).
        let (flush_tx, flush_rx) = async_channel::bounded(1);
        command_tx
            .send(LibraryCommand::RecordPlaybackHistory {
                track_id: TrackId::new(COMMAND_TRACK).expect("valid q4 command track ID"),
                counted_at_ms: 1_000,
            })
            .await
            .expect("admit during-scan history command");
        command_tx
            .send(LibraryCommand::Flush {
                completion: flush_tx,
            })
            .await
            .expect("admit during-scan Flush barrier");

        // Serialize the engine run against any concurrent parse-delay
        // window: both opt-in Q4 tests share the process-wide seam, and an
        // overlapping armed delay would distort this timeline and pollute
        // the invocation counter (refinery F2, PR #285). Released after the
        // abort below, mirroring the production teardown shape. (The
        // function-level `allow` covers the intentional hold across the
        // timeline awaits.)
        let parse_delay_window = super::TEST_ONLY_PARSE_DELAY_WINDOW
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let engine_task = tokio::spawn(engine.run());
        let timeline = q4_collect_startup_timeline(&event_rx, flush_rx, COMMAND_TRACK, start).await;

        // ── Settlement contract (R9 concurrent-service semantics) ──
        // The published snapshot covers the fixture rows plus the one
        // pre-inserted command track (the scan does not know it apart).
        assert_eq!(
            timeline.fullsync_rows,
            track_count + 1,
            "startup snapshot must publish every fixture row"
        );
        let scan_complete_us = timeline
            .scan_complete_us
            .expect("initial scan publishes ScanComplete");
        let flush_ack_us = timeline
            .flush_ack_us
            .expect("flush acknowledged by the collector loop");
        let command_applied_us = timeline
            .during_scan_command_applied_us
            .expect("during-scan command commits and publishes after admission");
        assert!(
            scan_complete_us <= flush_ack_us,
            "the Flush barrier drains the scan before it acks"
        );
        // R9 removed the await-scan-before-commands ordering: the admitted
        // command is serviced at the interleaved loop's txn boundaries, so
        // it may commit before OR after the scan settles (timing-dependent
        // on fixture size). The invariant that must hold in both regimes is
        // FIFO: the command settles before the Flush barrier acks.
        assert!(
            command_applied_us <= flush_ack_us,
            "a command admitted during the scan settles before the FIFO barrier acks"
        );
        let command_row = track::Entity::find_by_id(COMMAND_TRACK)
            .one(&db)
            .await
            .expect("query during-scan command row")
            .expect("during-scan command row exists");
        assert_eq!(
            command_row.play_count, 1,
            "the during-scan-admitted occurrence is durably counted"
        );

        // ── Metrics (budgets live in docs/engine-ui-responsiveness.md) ──
        let metric = |name: &str, value: f64| {
            println!("Q4_ENGINE_METRIC name={name} rows={track_count} value={value:.3}");
        };
        if let Some(us) = timeline.server_ready_us {
            metric("startup_server_ready_ms", us as f64 / 1_000.0);
        }
        if let Some(us) = timeline.fullsync_us {
            metric("startup_fullsync_ms", us as f64 / 1_000.0);
        }
        metric("startup_scan_settle_ms", scan_complete_us as f64 / 1_000.0);
        metric(
            "post_scan_drain_ms",
            (flush_ack_us - scan_complete_us) as f64 / 1_000.0,
        );
        metric(
            "cancellation_flush_settle_ms",
            flush_ack_us as f64 / 1_000.0,
        );
        println!(
            "Q4_ENGINE_METRIC name=scan_progress_events rows={track_count} value={}",
            timeline.scan_progress_events
        );

        // Teardown hygiene: production shutdown aborts the spawned engine
        // task rather than joining it (the watcher/command loop lives until
        // abort), so mirror that here instead of awaiting an exit that
        // never comes.
        drop(command_tx);
        engine_task.abort();
        // The measurement window closes only after the engine task is torn
        // down, so a following delayed scan still holds the seam exclusively.
        drop(parse_delay_window);
    }

    /// Opt-in Q4 responsiveness measurement over a fixed synthetic library.
    ///
    /// Ignored because it generates a 10k/100k-track on-disk fixture. Run it
    /// against a named reference runner with:
    ///
    /// ```text
    /// TRIBUTARY_Q4_LIBRARY_TRACKS=100000 \
    ///   cargo test --bin tributary --release -- --ignored --nocapture \
    ///   q4_measured_large_library_responsiveness
    /// ```
    ///
    /// `TRIBUTARY_Q4_PARSE_DELAY_MICROS=<n>` additionally runs a second scan
    /// against a FRESH second database with `n` microseconds of deterministic
    /// per-file parse delay. The fresh database makes every row `needs_update`,
    /// so every measured file really enters the delayed `spawn_blocking` parse
    /// branch; the test asserts the recorded parse-delay invocation count and
    /// the persisted row cardinality instead of inferring anything from wall
    /// time alone.
    #[tokio::test]
    #[ignore = "opt-in Q4 large-library measurement; generates a large fixture"]
    async fn q4_measured_large_library_responsiveness() {
        use std::collections::HashMap;
        use std::time::Instant;

        use crate::architecture::models::{Rating, SortField, SortOrder};

        use super::super::perf_fixtures::{
            catalogue_bytes, expected_album_count, expected_artist_count, track_count_from_env,
            DelayedBackend, ResponsivenessReport, SyntheticLibrary,
        };

        struct ParseDelayGuard {
            /// Held for the guard's whole window so a concurrent harness
            /// cannot arm, poll, or reset the shared seam under us
            /// (refinery F2, PR #285). Released after the delay static is
            /// disarmed in `Drop`.
            _window: std::sync::MutexGuard<'static, ()>,
        }

        impl ParseDelayGuard {
            fn set(micros: u64) -> Self {
                let window = super::TEST_ONLY_PARSE_DELAY_WINDOW
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                super::reset_test_only_parse_delay_invocations();
                super::TEST_ONLY_PARSE_DELAY_MICROS.store(micros, Ordering::Relaxed);
                Self { _window: window }
            }
        }

        impl Drop for ParseDelayGuard {
            fn drop(&mut self) {
                super::TEST_ONLY_PARSE_DELAY_MICROS.store(0, Ordering::Relaxed);
            }
        }

        let track_count = track_count_from_env();
        let library =
            SyntheticLibrary::generate(track_count).expect("generate synthetic library fixture");
        let db = rename_test_database().await;
        let (event_tx, event_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();

        // Baseline scan. The database is fresh, so every file must actually be
        // parsed: assert the parse invocations and the persisted cardinality
        // instead of trusting elapsed wall time.
        let scan_started = Instant::now();
        let baseline_guard = ParseDelayGuard::set(0);
        initial_scan(&db, &[library.root().to_path_buf()], &event_tx, &refresh)
            .await
            .expect("measured initial scan");
        let scan_elapsed = scan_started.elapsed();
        let baseline_parses = super::test_only_parse_delay_invocations();
        drop(baseline_guard);
        let persisted = track::Entity::find()
            .all(&db)
            .await
            .expect("count persisted tracks")
            .len();
        assert_eq!(
            baseline_parses, track_count as u64,
            "baseline scan must parse every fixture file exactly once"
        );
        assert_eq!(
            persisted, track_count,
            "baseline scan must persist one row per fixture file"
        );
        // The fixture's directory fan-out must survive the real scan/parse/
        // persist path into the real backend aggregation: distinct tags per
        // file mean 12-track album groups and 4-album artist groups (with
        // documented final partial groups), never a single collapsed
        // Unknown-Artist/Unknown-Album row pair. Assert BEFORE any metric is
        // recorded so a collapsed catalogue cannot pass as measurement
        // output.
        let catalogue_backend = LocalBackend::new(db.clone());
        let scanned_albums = catalogue_backend
            .list_albums(SortField::Title, SortOrder::Ascending)
            .await
            .expect("list albums for cardinality proof");
        let scanned_artists = catalogue_backend
            .list_artists()
            .await
            .expect("list artists for cardinality proof");
        assert_eq!(
            scanned_albums.len(),
            expected_album_count(track_count),
            "persisted catalogue must fan out to one album group per 12 fixture tracks"
        );
        assert_eq!(
            scanned_artists.len(),
            expected_artist_count(track_count),
            "persisted catalogue must fan out to one artist group per 4 fixture albums"
        );
        let events = std::iter::from_fn(|| event_rx.try_recv().ok()).count();

        let runner = std::env::var("TRIBUTARY_Q4_RUNNER")
            .unwrap_or_else(|_| format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH));
        let mut report = ResponsivenessReport::new(runner);
        report.record(
            "scan_tracks_persisted",
            track_count,
            persisted as f64,
            "tracks",
        );
        report.record(
            "scan_parse_invocations",
            track_count,
            baseline_parses as f64,
            "parses",
        );
        report.record(
            "scan_albums",
            track_count,
            scanned_albums.len() as f64,
            "albums",
        );
        report.record(
            "scan_artists",
            track_count,
            scanned_artists.len() as f64,
            "artists",
        );
        report.record_ms("scan_elapsed", track_count, scan_elapsed);
        report.record("scan_events", track_count, events as f64, "events");
        if scan_elapsed.as_secs_f64() > 0.0 {
            report.record(
                "scan_throughput",
                track_count,
                persisted as f64 / scan_elapsed.as_secs_f64(),
                "tracks_per_second",
            );
        }

        // Source/filter/rebuild latency and retained bytes through the real
        // backend seam, measured over the freshly scanned catalogue.
        let backend = LocalBackend::new(db.clone());
        let tracks_started = Instant::now();
        let catalogue = backend.list_tracks().await.expect("list tracks");
        report.record_ms("backend_list_tracks", track_count, tracks_started.elapsed());
        report.record(
            "catalogue_retained_bytes",
            track_count,
            catalogue_bytes(&catalogue) as f64,
            "bytes",
        );

        let albums_started = Instant::now();
        backend
            .list_albums(SortField::Title, SortOrder::Ascending)
            .await
            .expect("list albums");
        report.record_ms("backend_list_albums", track_count, albums_started.elapsed());

        let artists_started = Instant::now();
        backend.list_artists().await.expect("list artists");
        report.record_ms(
            "backend_list_artists",
            track_count,
            artists_started.elapsed(),
        );

        let search_started = Instant::now();
        backend
            .search("Track0001", 50)
            .await
            .expect("search catalogue");
        report.record_ms("backend_search", track_count, search_started.elapsed());

        let stats_started = Instant::now();
        backend.get_stats().await.expect("read library stats");
        report.record_ms("backend_get_stats", track_count, stats_started.elapsed());

        // Rating mutations over the same catalogue: first directly through the
        // backend seam, then through the production engine command FIFO with a
        // Flush barrier, which is the admission/settlement path the app
        // actually uses for app-owned writes.
        let updates = 100_usize.min(track_count);
        let rating = Rating::new(80).expect("valid rating");
        let track_id = catalogue
            .iter()
            .find_map(|track| track.native_track_id.clone())
            .expect("catalogue exposes a native track id");

        let burst_started = Instant::now();
        for _ in 0..updates {
            backend
                .set_track_rating(&track_id, Some(rating))
                .await
                .expect("update rating");
        }
        let burst = burst_started.elapsed();
        report.record("update_burst_count", track_count, updates as f64, "updates");
        report.record_ms("update_burst_total", track_count, burst);
        report.record(
            "update_burst_per_update",
            track_count,
            burst.as_secs_f64() * 1_000.0 / updates as f64,
            "ms",
        );

        // Command admission + settlement through the production FIFO loop:
        // enqueue `updates` rating commands plus a Flush barrier while the
        // engine command loop is running, and measure the time until the
        // barrier is acknowledged (the production shutdown-settlement
        // observable). The barrier must always settle.
        let (command_tx, command_rx) = async_channel::unbounded();
        let loop_db = db.clone();
        let loop_tx = event_tx.clone();
        let loop_dirs = vec![library.root().to_path_buf()];
        let loop_refresh = test_playlist_sidebar_refresh();
        let loop_task = tokio::spawn(async move {
            let mut completed = HashMap::new();
            process_library_commands_without_watcher(
                &loop_db,
                &loop_dirs,
                &loop_tx,
                &command_rx,
                &mut completed,
                &loop_refresh,
            )
            .await;
        });

        let fifo_started = Instant::now();
        for _ in 0..updates {
            command_tx
                .send(LibraryCommand::SetTrackRating {
                    track_id: track_id.clone(),
                    rating: Some(rating),
                })
                .await
                .expect("send rating command");
        }
        let (flush_tx, flush_rx) = async_channel::unbounded();
        command_tx
            .send(LibraryCommand::Flush {
                completion: flush_tx,
            })
            .await
            .expect("send flush barrier");
        drop(command_tx);
        flush_rx
            .recv()
            .await
            .expect("flush barrier must be acknowledged");
        let fifo_settlement = fifo_started.elapsed();
        loop_task.await.expect("engine command loop task finishes");

        report.record(
            "command_fifo_commands",
            track_count,
            updates as f64,
            "commands",
        );
        report.record_ms(
            "command_fifo_flush_settlement",
            track_count,
            fifo_settlement,
        );

        // Prove the delayed-backend fixture adds deterministic latency.
        let delayed = DelayedBackend::new(LocalBackend::new(db.clone()), Duration::from_millis(5));
        let delayed_started = Instant::now();
        delayed
            .list_tracks()
            .await
            .expect("list tracks through delay");
        let delayed_elapsed = delayed_started.elapsed();
        report.record_ms("delayed_backend_list_tracks", track_count, delayed_elapsed);
        report.record(
            "delayed_backend_calls",
            track_count,
            delayed.calls() as f64,
            "calls",
        );

        // Optional delayed-filesystem/parser pass. It scans the SAME fixture
        // into a FRESH second database so every row is new and every file
        // really goes through the delayed parse branch, then asserts the
        // recorded parse-delay invocation count and persisted cardinality.
        if let Ok(micros) = std::env::var("TRIBUTARY_Q4_PARSE_DELAY_MICROS") {
            if let Ok(micros) = micros.parse::<u64>() {
                if micros > 0 {
                    let delayed_db = rename_test_database().await;
                    let delay_guard = ParseDelayGuard::set(micros);
                    let delayed_scan_started = Instant::now();
                    initial_scan(
                        &delayed_db,
                        &[library.root().to_path_buf()],
                        &event_tx,
                        &refresh,
                    )
                    .await
                    .expect("delayed initial scan");
                    let delayed_scan_elapsed = delayed_scan_started.elapsed();
                    let delayed_parses = super::test_only_parse_delay_invocations();
                    drop(delay_guard);
                    let delayed_persisted = track::Entity::find()
                        .all(&delayed_db)
                        .await
                        .expect("count delayed-scan tracks")
                        .len();

                    assert_eq!(
                        delayed_parses, track_count as u64,
                        "delayed scan must apply the parse delay to every fixture file"
                    );
                    assert_eq!(
                        delayed_persisted, track_count,
                        "delayed scan must persist one row per fixture file"
                    );
                    // The delayed pass must reproduce the same real catalogue
                    // fan-out, not a collapsed Unknown-Artist/Unknown-Album
                    // row pair.
                    let delayed_backend = LocalBackend::new(delayed_db.clone());
                    let delayed_albums = delayed_backend
                        .list_albums(SortField::Title, SortOrder::Ascending)
                        .await
                        .expect("list albums in delayed pass for cardinality proof");
                    let delayed_artists = delayed_backend
                        .list_artists()
                        .await
                        .expect("list artists in delayed pass for cardinality proof");
                    assert_eq!(
                        delayed_albums.len(),
                        expected_album_count(track_count),
                        "delayed scan must persist the full album fan-out"
                    );
                    assert_eq!(
                        delayed_artists.len(),
                        expected_artist_count(track_count),
                        "delayed scan must persist the full artist fan-out"
                    );

                    report.record_ms(
                        "delayed_parse_scan_elapsed",
                        track_count,
                        delayed_scan_elapsed,
                    );
                    report.record(
                        "delayed_parse_files_parsed",
                        track_count,
                        delayed_parses as f64,
                        "parses",
                    );
                    report.record(
                        "delayed_parse_micros_per_file",
                        track_count,
                        micros as f64,
                        "microseconds",
                    );
                    report.record(
                        "delayed_parse_estimated_delay_total_ms",
                        track_count,
                        delayed_parses as f64 * micros as f64 / 1_000.0,
                        "ms",
                    );
                }
            }
        }

        println!("{}", report.render());
    }

    /// Guard the Q4 fixture lane against catalogue collapse.
    ///
    /// Unlike the ignored measurement above, this runs in every `cargo test`:
    /// a 100-track [`SyntheticLibrary`] goes through the real `initial_scan`,
    /// and the production backend must report the documented album/artist
    /// fan-out (100 tracks -> 9 albums across 3 artists, with partial final
    /// groups) rather than one Unknown-Artist/Unknown-Album row pair. The
    /// 10k/100k measurement asserts the same cardinalities before recording
    /// any metric.
    #[tokio::test]
    async fn q4_fixture_scan_produces_real_catalogue_fan_out() {
        use crate::architecture::models::{SortField, SortOrder};

        use super::super::perf_fixtures::{
            expected_album_count, expected_artist_count, SyntheticLibrary,
        };

        const TRACKS: usize = 100;

        let library = SyntheticLibrary::generate(TRACKS).expect("generate small fixture library");
        let db = rename_test_database().await;
        let (event_tx, _event_rx) = async_channel::unbounded();
        let refresh = test_playlist_sidebar_refresh();
        initial_scan(&db, &[library.root().to_path_buf()], &event_tx, &refresh)
            .await
            .expect("scan small fixture library");

        let persisted = track::Entity::find()
            .all(&db)
            .await
            .expect("count persisted fixture tracks")
            .len();
        assert_eq!(persisted, TRACKS, "one row per fixture file");

        let backend = LocalBackend::new(db);
        let albums = backend
            .list_albums(SortField::Title, SortOrder::Ascending)
            .await
            .expect("list fixture albums");
        let artists = backend.list_artists().await.expect("list fixture artists");
        assert_eq!(
            albums.len(),
            expected_album_count(TRACKS),
            "fixture tags must become distinct album rows (100 tracks -> 9 albums)"
        );
        assert_eq!(
            artists.len(),
            expected_artist_count(TRACKS),
            "fixture tags must become distinct artist rows (9 albums -> 3 artists)"
        );

        // Spot-check attribution: tracks 96..100 form the final partial
        // album group (4 tracks); the 9 albums split 4/4/1 across artists,
        // so the final artist owns exactly that one album.
        let last_album = albums
            .iter()
            .find(|album| album.title == super::super::perf_fixtures::album_title_for(8))
            .expect("final album group present");
        assert_eq!(last_album.track_count, 4);
        assert_eq!(
            last_album.artist_name,
            super::super::perf_fixtures::artist_name_for(2)
        );
        let last_artist = artists
            .iter()
            .find(|artist| artist.name == super::super::perf_fixtures::artist_name_for(2))
            .expect("final artist group present");
        assert_eq!(last_artist.album_count, 1);
        assert_eq!(last_artist.track_count, 4);
    }

    // ── Root availability, rescans and engine edge cases ──────────────

    const SILENCE_FLAC: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/audio/silence.flac"
    ));

    fn probe_test_watcher(
        backend: RecommendedWatcher,
        rx: mpsc::Receiver<notify::Result<notify::Event>>,
        roots: &[PathBuf],
        interval: Duration,
    ) -> DirectoryWatcher {
        DirectoryWatcher {
            watcher: backend,
            rx,
            ingress_overflowed: Arc::new(AtomicBool::new(false)),
            watched_directories: HashSet::new(),
            root_presence: roots
                .iter()
                .map(|root| (root.clone(), probe_root_presence(root)))
                .collect(),
            root_probe_interval: interval,
        }
    }

    async fn wait_for_root_status(events: &async_channel::Receiver<LibraryEvent>, available: bool) {
        loop {
            if let LibraryEvent::RootStatusChanged(statuses) =
                events.recv().await.expect("library events stay open")
            {
                if statuses.iter().all(|status| status.available == available) {
                    return;
                }
            }
        }
    }

    #[test]
    fn root_probe_rescans_arrivals_and_only_marks_departures() {
        use RootPresence::{Missing, Present};

        let mounted = Present {
            boundary: 7,
            marked: true,
        };
        let empty_mountpoint = Present {
            boundary: 3,
            marked: false,
        };
        assert_eq!(
            root_probe_action(mounted, mounted),
            RootProbeAction::Unchanged
        );
        assert_eq!(
            root_probe_action(Missing, Missing),
            RootProbeAction::Unchanged
        );

        // Appearing, or coming back on a new mount, calls for a scan.
        assert_eq!(
            root_probe_action(Missing, mounted),
            RootProbeAction::Arrived
        );
        assert_eq!(
            root_probe_action(empty_mountpoint, mounted),
            RootProbeAction::Arrived
        );
        assert_eq!(
            root_probe_action(
                mounted,
                Present {
                    boundary: 8,
                    marked: true
                }
            ),
            RootProbeAction::Arrived
        );
        assert_eq!(
            root_probe_action(Missing, empty_mountpoint),
            RootProbeAction::Arrived
        );

        // Disappearing, or leaving an empty mountpoint behind, does not: a
        // scan of the mountpoint would only ask to trust a stranger.
        assert_eq!(root_probe_action(mounted, Missing), RootProbeAction::Gone);
        assert_eq!(
            root_probe_action(mounted, empty_mountpoint),
            RootProbeAction::Gone
        );
    }

    #[test]
    fn reprobe_watches_a_root_that_appears_and_reports_one_that_leaves() {
        let fixture = TestDirectory::new("root-reprobe");
        let root = fixture.path().join("late");
        let (_event_tx, event_rx) = mpsc::channel(WATCHER_EVENT_CAPACITY);
        let Some(backend) = idle_watcher_backend_or_skip() else {
            return;
        };
        let roots = [root.clone()];
        let mut watcher = probe_test_watcher(backend, event_rx, &roots, ROOT_PROBE_INTERVAL);

        assert_eq!(watcher.reprobe_roots(&roots), RootProbeOutcome::default());

        std::fs::create_dir(&root).expect("root appears");
        create_root_marker(&root).expect("root carries a marker");
        let outcome = watcher.reprobe_roots(&roots);
        assert!(outcome.rescan && outcome.gone.is_empty());
        assert!(watcher.watched_directories.contains(&root));
        assert_eq!(watcher.reprobe_roots(&roots), RootProbeOutcome::default());

        // Losing the marker is how an unmount leaves its mountpoint.
        std::fs::remove_file(root_identity_path(&root)).expect("marker disappears");
        let outcome = watcher.reprobe_roots(&roots);
        assert_eq!(outcome.gone, std::slice::from_ref(&root));
        assert!(!outcome.rescan);

        // The marker coming back is a remount: watch again and rescan.
        create_root_marker(&root).expect("marker returns");
        assert!(watcher.reprobe_roots(&roots).rescan);

        std::fs::remove_dir_all(&root).expect("root goes away");
        assert_eq!(watcher.reprobe_roots(&roots).gone, [root]);
    }

    /// A root missing when watching starts is picked up by the periodic
    /// probe: watched, scanned and reported available. When it goes away it
    /// is reported unavailable and its tracks are kept.
    #[tokio::test]
    async fn root_probe_scans_a_late_root_and_keeps_its_rows_when_it_leaves() {
        let db = Arc::new(rename_test_database().await);
        let fixture = TestDirectory::new("root-probe-late-root");
        let root = fixture.path().join("late");
        let audio_path = root.join("late.flac");

        let (event_tx, event_rx) = mpsc::channel(WATCHER_EVENT_CAPACITY);
        let Some(backend) = idle_watcher_backend_or_skip() else {
            return;
        };
        let watcher = probe_test_watcher(
            backend,
            event_rx,
            std::slice::from_ref(&root),
            Duration::from_millis(100),
        );
        let (library_events, library_event_rx) = async_channel::unbounded();
        let (_command_tx, command_rx) = async_channel::unbounded::<LibraryCommand>();
        let playlist_sidebar_refresh = test_playlist_sidebar_refresh();
        let mut completed_commands = HashMap::new();

        let driver = async {
            std::fs::create_dir(&root).expect("root appears after startup");
            std::fs::write(&audio_path, SILENCE_FLAC).expect("write audio fixture");
            tokio::time::timeout(
                Duration::from_secs(30),
                wait_for_root_status(&library_event_rx, true),
            )
            .await
            .expect("the probe scans the new root");
            std::fs::remove_dir_all(&root).expect("root goes away");
            tokio::time::timeout(
                Duration::from_secs(30),
                wait_for_root_status(&library_event_rx, false),
            )
            .await
            .expect("the probe reports the root unavailable");
            drop(event_tx);
        };
        let never_cancelled = CancellationToken::new();
        let (loop_result, ()) = tokio::join!(
            process_directory_events(
                &db,
                std::slice::from_ref(&root),
                &library_events,
                &command_rx,
                &mut completed_commands,
                watcher,
                &playlist_sidebar_refresh,
                &never_cancelled,
            ),
            driver,
        );
        loop_result.expect("watcher loop exits cleanly");

        let root_state = library_root::Entity::find_by_id(root.to_string_lossy().as_ref())
            .one(db.as_ref())
            .await
            .expect("query root state")
            .expect("the scan recorded the root");
        assert!(root_state.identity_confirmed);
        assert!(!root_state.is_available);
        assert!(track::Entity::find()
            .filter(track::Column::FilePath.eq(audio_path.to_string_lossy().as_ref()))
            .one(db.as_ref())
            .await
            .expect("query track")
            .is_some());
    }

    #[tokio::test]
    async fn rescan_command_indexes_what_the_watcher_missed() {
        let db = Arc::new(rename_test_database().await);
        let fixture = TestDirectory::new("rescan-command");
        let root = fixture.path().to_path_buf();
        let marker = create_root_marker(&root)
            .expect("create durable root marker")
            .identity;
        insert_reauthorization_root(&db, &root, &marker, true).await;
        let audio_path = root.join("missed.flac");
        std::fs::write(&audio_path, SILENCE_FLAC).expect("write audio fixture");

        let (event_tx, event_rx) = mpsc::channel(WATCHER_EVENT_CAPACITY);
        let Some(backend) = idle_watcher_backend_or_skip() else {
            return;
        };
        let watcher = probe_test_watcher(
            backend,
            event_rx,
            std::slice::from_ref(&root),
            ROOT_PROBE_INTERVAL,
        );
        let (library_events, library_event_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded::<LibraryCommand>();
        command_tx
            .send(LibraryCommand::Rescan)
            .await
            .expect("queue rescan");
        let playlist_sidebar_refresh = test_playlist_sidebar_refresh();
        let mut completed_commands = HashMap::new();

        let driver = async {
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if let LibraryEvent::FullSync(tracks) = library_event_rx
                        .recv()
                        .await
                        .expect("library events stay open")
                    {
                        break tracks;
                    }
                }
            })
            .await
            .expect("the rescan publishes a snapshot")
        };
        let never_cancelled = CancellationToken::new();
        let loop_future = process_directory_events(
            &db,
            std::slice::from_ref(&root),
            &library_events,
            &command_rx,
            &mut completed_commands,
            watcher,
            &playlist_sidebar_refresh,
            &never_cancelled,
        );
        let tracks = tokio::select! {
            result = loop_future => panic!("the watcher loop ended early: {result:?}"),
            tracks = driver => tracks,
        };
        drop(event_tx);
        assert!(tracks.iter().any(|track| {
            track.file_path.as_deref() == Some(audio_path.to_string_lossy().as_ref())
        }));
    }

    /// A symlink stands in for a case-insensitive filesystem here: the old
    /// spelling opens the renamed file but is not enumerated as a file of its
    /// own.
    #[cfg(unix)]
    #[tokio::test]
    async fn case_only_rename_retargets_the_existing_row() {
        let db = rename_test_database().await;
        let fixture = TestDirectory::new("case-only-rename");
        let root = fixture.path().to_path_buf();
        let marker = create_root_marker(&root)
            .expect("create durable root marker")
            .identity;
        insert_reauthorization_root(&db, &root, &marker, true).await;

        let renamed = root.join("Song.flac");
        let old_spelling = root.join("song.flac");
        std::fs::write(&renamed, SILENCE_FLAC).expect("write renamed audio");
        std::os::unix::fs::symlink(&renamed, &old_spelling).expect("alias the old spelling");
        insert_rename_test_track(
            &db,
            "case-rename",
            old_spelling.to_str().unwrap(),
            "Kept",
            7,
        )
        .await;

        // A different file whose old spelling is really gone is not an alias.
        let replaced = root.join("Other.flac");
        std::fs::write(&replaced, SILENCE_FLAC).expect("write unrelated audio");
        let gone = root.join("other.flac");
        insert_rename_test_track(&db, "gone", gone.to_str().unwrap(), "Gone", 1).await;

        let (tx, _rx) = async_channel::unbounded();
        initial_scan(
            &db,
            std::slice::from_ref(&root),
            &tx,
            &test_playlist_sidebar_refresh(),
        )
        .await
        .expect("scan");

        let rows = track::Entity::find().all(&db).await.expect("query tracks");
        assert_eq!(rows.len(), 2, "{rows:?}");
        let kept = rows
            .iter()
            .find(|row| row.id == "case-rename")
            .expect("the renamed track keeps its identity");
        assert_eq!(kept.file_path, renamed.to_string_lossy());
        assert_eq!(kept.play_count, 7);
        assert!(rows.iter().all(|row| row.id != "gone"));
        assert!(rows
            .iter()
            .any(|row| row.file_path == replaced.to_string_lossy()));
    }

    #[test]
    fn former_mount_scopes_without_rows_are_dropped() {
        let configured = vec![PathBuf::from("/music")];
        let scope = |path: &str| library_root::Model {
            path: path.to_string(),
            device_id: Some("remembered-volume".to_string()),
            identity_confirmed: true,
            is_available: false,
            last_scan_complete: false,
            last_checked_at: "2026-07-10T00:00:00Z".to_string(),
        };
        let persisted = vec![
            scope("/music"),
            scope("/music/old-mount"),
            scope("/music/away"),
            scope("/music/live"),
            scope("/elsewhere/x"),
        ];
        let track = track::Model {
            id: "away".to_string(),
            file_path: "/music/away/song.flac".to_string(),
            title: "Away".to_string(),
            artist_name: "Artist".to_string(),
            album_artist_name: None,
            album_title: "Album".to_string(),
            genre: None,
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            last_played_at_ms: None,
            rating: None,
            date_added: "2025-01-02T03:04:05Z".to_string(),
            date_modified: "2025-01-02T03:04:05Z".to_string(),
            file_size_bytes: None,
        };

        let (kept, stale) = partition_stale_mount_scopes(
            &configured,
            persisted,
            &[PathBuf::from("/music/live")],
            &[track],
        );

        let paths = |states: &[library_root::Model]| {
            states
                .iter()
                .map(|state| state.path.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths(&kept),
            ["/music", "/music/away", "/music/live", "/elsewhere/x"]
        );
        assert_eq!(paths(&stale), ["/music/old-mount"]);
    }

    #[test]
    fn tag_write_debris_sweep_restores_vacant_originals_and_ages_out_the_rest() {
        let directory = TestDirectory::new("tag-write-debris");
        let hidden_original = directory
            .path()
            .join(".song.flac.tributary-replaced-0123456789abcdef0123456789abcdef");
        std::fs::write(&hidden_original, b"original").expect("write hidden original");
        let published = directory.path().join("kept.flac");
        std::fs::write(&published, b"tagged").expect("write published copy");
        let displaced = directory
            .path()
            .join(".kept.flac.tributary-replaced-fedcba9876543210fedcba9876543210");
        std::fs::write(&displaced, b"displaced").expect("write displaced original");
        let staged = directory
            .path()
            .join(".tributary-tag-00000000-0000-4000-8000-000000000000.flac");
        std::fs::write(&staged, b"staged").expect("write staged copy");

        // A vacant public name is refilled at once; young debris stays.
        let debris = [hidden_original.clone(), displaced.clone(), staged.clone()];
        let restored = sweep_tag_write_debris(&debris, std::time::SystemTime::now());
        let public = directory.path().join("song.flac");
        assert_eq!(
            restored.iter().map(|(path, _)| path).collect::<Vec<_>>(),
            [&public]
        );
        assert_eq!(std::fs::read(&public).expect("read restored"), b"original");
        assert!(!hidden_original.exists());
        assert!(displaced.exists() && staged.exists());

        // Past the safety margin the rest is removed; published files stay.
        let later =
            std::time::SystemTime::now() + TAG_WRITE_DEBRIS_MIN_AGE + Duration::from_secs(60);
        assert!(sweep_tag_write_debris(&[displaced.clone(), staged.clone()], later).is_empty());
        assert!(!displaced.exists() && !staged.exists());
        assert_eq!(
            std::fs::read(&published).expect("read published"),
            b"tagged"
        );
    }

    #[test]
    fn quarantine_names_rebuild_only_whole_public_names() {
        let quarantine = |leaf: &str| {
            PathBuf::from(format!(
                "/music/.{leaf}.tributary-replaced-0123456789abcdef0123456789abcdef"
            ))
        };
        assert_eq!(
            quarantined_public_path(&quarantine("song.flac")),
            Some(PathBuf::from("/music/song.flac"))
        );
        // Names are cut at 96 bytes, so a long prefix may be truncated.
        assert_eq!(quarantined_public_path(&quarantine(&"a".repeat(96))), None);
    }

    /// A save interrupted between hiding the original and publishing the
    /// tagged copy leaves the track's public name empty. The next scan puts
    /// the original back instead of deleting the row.
    #[tokio::test]
    async fn scan_restores_an_original_hidden_by_an_interrupted_tag_save() {
        let db = rename_test_database().await;
        let fixture = TestDirectory::new("interrupted-tag-save");
        let root = fixture.path().to_path_buf();
        let marker = create_root_marker(&root)
            .expect("create durable root marker")
            .identity;
        insert_reauthorization_root(&db, &root, &marker, true).await;
        let public = root.join("song.flac");
        let hidden = root.join(".song.flac.tributary-replaced-0123456789abcdef0123456789abcdef");
        std::fs::write(&hidden, SILENCE_FLAC).expect("write hidden original");
        insert_rename_test_track(&db, "hidden", public.to_str().unwrap(), "Hidden", 4).await;

        let (tx, _rx) = async_channel::unbounded();
        initial_scan(
            &db,
            std::slice::from_ref(&root),
            &tx,
            &test_playlist_sidebar_refresh(),
        )
        .await
        .expect("scan");

        assert!(public.is_file() && !hidden.exists());
        let row = track::Entity::find_by_id("hidden")
            .one(&db)
            .await
            .expect("query track")
            .expect("the row survives");
        assert_eq!(row.play_count, 4);
    }

    /// Progress counts every enumerated file, including one that is skipped.
    #[cfg(unix)]
    #[tokio::test]
    async fn scan_progress_reaches_the_total_when_a_file_is_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let db = rename_test_database().await;
        let fixture = TestDirectory::new("scan-progress");
        let root = fixture.path().to_path_buf();
        let marker = create_root_marker(&root)
            .expect("create durable root marker")
            .identity;
        insert_reauthorization_root(&db, &root, &marker, true).await;
        std::fs::write(root.join("a.flac"), SILENCE_FLAC).expect("write audio");
        let unreadable = root.join("z.flac");
        std::fs::write(&unreadable, SILENCE_FLAC).expect("write audio");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
            .expect("make unreadable");
        if File::open(&unreadable).is_ok() {
            eprintln!("skipping: permissions do not restrict this user");
            return;
        }

        let (tx, rx) = async_channel::unbounded();
        initial_scan(
            &db,
            std::slice::from_ref(&root),
            &tx,
            &test_playlist_sidebar_refresh(),
        )
        .await
        .expect("scan");
        let last_progress = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                LibraryEvent::ScanProgress(done, total) => Some((done, total)),
                _ => None,
            })
            .last();
        assert_eq!(last_progress, Some((2, 2)));
    }
}
