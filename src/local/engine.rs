//! Library scanning engine — initial scan + real-time filesystem watching.
//!
//! Runs entirely on the tokio runtime. Sends `LibraryEvent` messages
//! to the GTK main thread via `async_channel`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

use super::tag_parser::{self, ParsedTrack};
use crate::architecture::models::Track;
use crate::db::entities::track;

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
    music_dir: PathBuf,
    tx: async_channel::Sender<LibraryEvent>,
}

impl LibraryEngine {
    /// Create a new engine. Does NOT start scanning yet.
    pub fn new(
        db: DatabaseConnection,
        music_dir: PathBuf,
        tx: async_channel::Sender<LibraryEvent>,
    ) -> Self {
        Self { db, music_dir, tx }
    }

    /// Run the engine: initial scan, then continuous FS watching.
    pub async fn run(self) {
        let db = Arc::new(self.db);

        // ── Initial scan ─────────────────────────────────────────────
        info!(dir = %self.music_dir.display(), "Starting initial library scan");
        if let Err(e) = initial_scan(&db, &self.music_dir, &self.tx).await {
            error!(error = %e, "Initial scan failed");
            let _ = self.tx.send(LibraryEvent::Error(e.to_string())).await;
        }

        // ── Filesystem watcher ───────────────────────────────────────
        info!(dir = %self.music_dir.display(), "Starting filesystem watcher");
        if let Err(e) = watch_directory(&db, &self.music_dir, &self.tx).await {
            error!(error = %e, "Filesystem watcher failed");
            let _ = self.tx.send(LibraryEvent::Error(e.to_string())).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Initial scan
// ---------------------------------------------------------------------------

async fn initial_scan(
    db: &DatabaseConnection,
    music_dir: &Path,
    tx: &async_channel::Sender<LibraryEvent>,
) -> anyhow::Result<()> {
    let dir = music_dir.to_path_buf();

    // Collect audio files (blocking I/O in spawn_blocking)
    let audio_files: Vec<PathBuf> = tokio::task::spawn_blocking(move || {
        WalkDir::new(&dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| tag_parser::is_audio_file(e.path()))
            .map(|e| e.into_path())
            .collect()
    })
    .await?;

    let total = audio_files.len() as u64;
    info!(total, "Found audio files to scan");

    let mut scanned: u64 = 0;
    let mut on_disk_paths = HashSet::new();

    for path in &audio_files {
        let path_str = path.to_string_lossy().to_string();
        on_disk_paths.insert(path_str.clone());

        // Check if already in DB with matching mtime
        let existing = track::Entity::find()
            .filter(track::Column::FilePath.eq(&path_str))
            .one(db)
            .await?;

        let needs_update = match &existing {
            Some(row) => {
                // Compare FS mtime with stored date_modified
                let fs_mtime = get_mtime(path);
                let db_mtime = row.date_modified.clone();
                fs_mtime != db_mtime
            }
            None => true,
        };

        if needs_update {
            let p = path.clone();
            let parse_result =
                tokio::task::spawn_blocking(move || tag_parser::parse_audio_file(&p)).await;

            match parse_result {
                Ok(Ok(parsed)) => match upsert_track(db, &parsed, existing.as_ref()).await {
                    Ok(track_model) => {
                        let arch_track = db_model_to_track(&track_model);
                        let _ = tx
                            .send(LibraryEvent::TrackUpserted(Box::new(arch_track)))
                            .await;
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = %e, "Failed to upsert track");
                    }
                },
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

    // Remove DB entries for files no longer on disk
    let all_db_tracks = track::Entity::find().all(db).await?;
    for row in &all_db_tracks {
        if !on_disk_paths.contains(&row.file_path) {
            info!(path = %row.file_path, "Removing stale track from database");
            track::Entity::delete_by_id(&row.id).exec(db).await?;
            let _ = tx
                .send(LibraryEvent::TrackRemoved(row.file_path.clone()))
                .await;
        }
    }

    // Send full sync
    let all_tracks: Vec<Track> = track::Entity::find()
        .all(db)
        .await?
        .iter()
        .map(db_model_to_track)
        .collect();

    let _ = tx.send(LibraryEvent::FullSync(all_tracks)).await;

    // Reconcile orphaned playlist entries with newly-discovered tracks.
    let playlist_mgr = super::playlist_manager::PlaylistManager::new(db.clone());
    match playlist_mgr.reconcile_all().await {
        Ok(n) if n > 0 => info!(relinked = n, "Playlist entries reconciled after scan"),
        Ok(_) => debug!("No orphaned playlist entries to reconcile"),
        Err(e) => warn!(error = %e, "Playlist reconciliation failed"),
    }

    // Send playlist list to UI thread for sidebar population.
    match playlist_mgr.list_playlists().await {
        Ok(playlists) => {
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

async fn watch_directory(
    db: &Arc<DatabaseConnection>,
    music_dir: &Path,
    tx: &async_channel::Sender<LibraryEvent>,
) -> anyhow::Result<()> {
    let (notify_tx, mut notify_rx) = mpsc::channel::<notify::Result<notify::Event>>(256);

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = notify_tx.blocking_send(res);
        },
        notify::Config::default().with_poll_interval(Duration::from_secs(2)),
    )?;

    watcher.watch(music_dir.as_ref(), RecursiveMode::Recursive)?;
    info!("Filesystem watcher active");

    // Keep watcher alive by holding it in scope
    let _watcher = watcher;

    while let Some(event_result) = notify_rx.recv().await {
        match event_result {
            Ok(event) => {
                handle_fs_event(db, tx, event).await;
            }
            Err(e) => {
                warn!(error = %e, "Filesystem watcher error");
            }
        }
    }

    Ok(())
}

async fn handle_fs_event(
    db: &Arc<DatabaseConnection>,
    tx: &async_channel::Sender<LibraryEvent>,
    event: notify::Event,
) {
    use notify::EventKind;

    for path in &event.paths {
        if !tag_parser::is_audio_file(path) {
            continue;
        }

        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                debug!(path = %path.display(), "File created/modified");
                // Small delay to let writes complete
                tokio::time::sleep(Duration::from_millis(500)).await;

                if path.exists() {
                    let p = path.clone();
                    match tokio::task::spawn_blocking(move || tag_parser::parse_audio_file(&p))
                        .await
                    {
                        Ok(Ok(parsed)) => {
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
            EventKind::Remove(_) => {
                let path_str = path.to_string_lossy().to_string();
                debug!(path = %path_str, "File removed");
                if let Ok(Some(row)) = track::Entity::find()
                    .filter(track::Column::FilePath.eq(&path_str))
                    .one(db.as_ref())
                    .await
                {
                    let _ = track::Entity::delete_by_id(&row.id).exec(db.as_ref()).await;
                }
                let _ = tx.send(LibraryEvent::TrackRemoved(path_str)).await;
            }
            _ => {}
        }
    }
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
