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

async fn initial_scan(
    db: &DatabaseConnection,
    music_dirs: &[PathBuf],
    tx: &async_channel::Sender<LibraryEvent>,
) -> anyhow::Result<()> {
    let dirs = music_dirs.to_vec();

    // Collect audio files from ALL directories (blocking I/O in spawn_blocking)
    let audio_files: Vec<PathBuf> = tokio::task::spawn_blocking(move || {
        dirs.iter()
            .flat_map(|dir| {
                WalkDir::new(dir)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .filter(|e| tag_parser::is_audio_file(e.path()))
                    .map(|e| e.into_path())
            })
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

    for dir in music_dirs {
        watcher.watch(dir.as_ref(), RecursiveMode::Recursive)?;
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
            use notify::EventKind;
            for path in event.paths {
                if !tag_parser::is_audio_file(&path) {
                    continue;
                }
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        remove_paths.remove(&path);
                        upsert_paths.insert(path);
                    }
                    EventKind::Remove(_) => {
                        upsert_paths.remove(&path);
                        remove_paths.insert(path);
                    }
                    _ => {}
                }
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
            if let Ok(Some(row)) = track::Entity::find()
                .filter(track::Column::FilePath.eq(&path_str))
                .one(db.as_ref())
                .await
            {
                let _ = track::Entity::delete_by_id(&row.id).exec(db.as_ref()).await;
            }
            let _ = tx.send(LibraryEvent::TrackRemoved(path_str)).await;
        }

        // Process upserts concurrently.
        if !upsert_paths.is_empty() {
            debug!(count = upsert_paths.len(), "Processing debounced upserts");
            let paths: Vec<PathBuf> = upsert_paths.into_iter().collect();

            for path in paths {
                if !path.exists() {
                    continue;
                }
                let p = path.clone();
                match tokio::task::spawn_blocking(move || tag_parser::parse_audio_file(&p)).await {
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
