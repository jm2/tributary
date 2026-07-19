//! Playlist manager — CRUD operations and track reconciliation.
//!
//! Manages regular and smart playlists. Regular playlists store track
//! references with fingerprint data for rediscovery after library rebuilds.
//! Smart playlists store rule configurations and evaluate dynamically.

use std::collections::{HashMap, HashSet};

use sea_orm::prelude::*;
use sea_orm::{ActiveValue::Set, DatabaseTransaction, QueryOrder, TransactionTrait};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::playlist_io::{ImportedTrack, ImportedTrackMatchIndex};
use super::smart_rules::{self, SmartRules};
use crate::architecture::{MediaKey, SourceId, TrackId};
use crate::db::entities::{playlist, playlist_entry, track};

/// Per-entry outcome counts for one committed playlist import.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaylistImportCounts {
    pub matched: usize,
    pub unmatched: usize,
    pub failed: usize,
}

/// A newly imported playlist and the outcome for every source entry.
#[derive(Debug)]
pub struct PlaylistImportResult {
    pub playlist: playlist::Model,
    pub counts: PlaylistImportCounts,
}

/// One exact, source-scoped track to append to a regular playlist.
///
/// The identity is deliberately independent of any playable URI. Remote
/// locators and credentials remain owned by the live source session and are
/// never accepted by playlist persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistEntryInput {
    pub media_key: MediaKey,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_secs: Option<u64>,
}

impl PlaylistEntryInput {
    pub fn new(
        media_key: MediaKey,
        title: impl Into<String>,
        artist: impl Into<String>,
        album: impl Into<String>,
        duration_secs: Option<u64>,
    ) -> Self {
        Self {
            media_key,
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration_secs,
        }
    }

    fn local(track: &track::Model) -> Result<Self, DbErr> {
        let track_id = TrackId::new(track.id.clone())
            .map_err(|error| DbErr::Custom(format!("Local track identity is invalid: {error}")))?;
        Ok(Self::new(
            MediaKey::new(SourceId::local(), track_id),
            track.title.clone(),
            track.artist_name.clone(),
            track.album_title.clone(),
            valid_track_match_duration(track).map(|duration| duration as u64),
        ))
    }
}

/// Durable regular-playlist occurrence returned by the storage boundary.
///
/// `track_id` is absent only for an unmatched local import. `local_track_id`
/// is a resolution cache backed by the local-track foreign key; deleting a
/// local library row can clear it without erasing the occurrence's canonical
/// source-scoped identity or match evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPlaylistEntry {
    pub id: String,
    pub playlist_id: String,
    pub position: i32,
    pub source_id: SourceId,
    pub track_id: Option<TrackId>,
    pub local_track_id: Option<TrackId>,
    pub match_title: String,
    pub match_artist: String,
    pub match_album: String,
    pub match_duration_secs: Option<i32>,
    pub match_file_path: Option<String>,
}

/// One durable regular-playlist occurrence aligned with its current local
/// library row, when the occurrence is owned by the built-in local source and
/// its foreign-key cache still resolves.
///
/// Non-local and currently unmatched local occurrences deliberately carry
/// `None`. The durable entry is retained in every case so callers never lose
/// playlist order or confuse repeated media identities with one occurrence.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedPlaylistEntry {
    pub stored: StoredPlaylistEntry,
    pub local_track: Option<track::Model>,
}

impl StoredPlaylistEntry {
    pub fn media_key(&self) -> Option<MediaKey> {
        self.track_id
            .clone()
            .map(|track_id| MediaKey::new(self.source_id, track_id))
    }

    fn from_model(entry: playlist_entry::Model) -> Result<Self, DbErr> {
        if entry.position < 0 {
            return Err(DbErr::Custom(format!(
                "Playlist entry {} has an invalid position",
                entry.id
            )));
        }
        let source_id = entry.source_id.parse::<SourceId>().map_err(|error| {
            DbErr::Custom(format!(
                "Playlist entry {} has an invalid source identity: {error}",
                entry.id
            ))
        })?;
        if source_id.to_string() != entry.source_id {
            return Err(DbErr::Custom(format!(
                "Playlist entry {} has a non-canonical source identity",
                entry.id
            )));
        }
        if source_id.as_uuid().is_nil() {
            return Err(DbErr::Custom(format!(
                "Playlist entry {} has an unavailable source identity",
                entry.id
            )));
        }
        let track_id = entry
            .track_id
            .as_deref()
            .map(|track_id| {
                if source_id == SourceId::local() {
                    TrackId::new(track_id)
                } else {
                    TrackId::remote(track_id)
                }
            })
            .transpose()
            .map_err(|_| {
                DbErr::Custom(format!(
                    "Playlist entry {} has an invalid track identity",
                    entry.id
                ))
            })?;
        let local_track_id = entry
            .local_track_id
            .as_deref()
            .map(TrackId::new)
            .transpose()
            .map_err(|_| {
                DbErr::Custom(format!(
                    "Playlist entry {} has an invalid local track identity",
                    entry.id
                ))
            })?;

        if source_id == SourceId::local() {
            if let Some(local_track_id) = local_track_id.as_ref() {
                if track_id.as_ref() != Some(local_track_id) {
                    return Err(DbErr::Custom(format!(
                        "Playlist entry {} has inconsistent local identities",
                        entry.id
                    )));
                }
            }
            let has_path_evidence = entry
                .match_file_path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty());
            let has_fingerprint =
                !entry.match_title.trim().is_empty() && !entry.match_artist.trim().is_empty();
            if track_id.is_none() && !has_path_evidence && !has_fingerprint {
                return Err(DbErr::Custom(format!(
                    "Playlist entry {} has no usable local identity evidence",
                    entry.id
                )));
            }
        } else if track_id.is_none() || local_track_id.is_some() || entry.match_file_path.is_some()
        {
            return Err(DbErr::Custom(format!(
                "Playlist entry {} has inconsistent remote identity",
                entry.id
            )));
        }

        Ok(Self {
            id: entry.id,
            playlist_id: entry.playlist_id,
            position: entry.position,
            source_id,
            track_id,
            local_track_id,
            match_title: entry.match_title,
            match_artist: entry.match_artist,
            match_album: entry.match_album,
            match_duration_secs: entry.match_duration_secs,
            match_file_path: entry.match_file_path,
        })
    }
}

/// Manages playlist persistence and track reconciliation.
pub struct PlaylistManager {
    db: DatabaseConnection,
}

impl PlaylistManager {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    // ── CRUD ─────────────────────────────────────────────────────────

    /// Create a new playlist (regular or smart).
    pub async fn create_playlist(
        &self,
        name: &str,
        is_smart: bool,
    ) -> Result<playlist::Model, DbErr> {
        let now = now_rfc3339();
        let model = playlist::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            name: Set(name.to_string()),
            is_smart: Set(is_smart),
            smart_rules_json: Set(None),
            limit_enabled: Set(false),
            limit_value: Set(None),
            limit_unit: Set(None),
            limit_sort: Set(None),
            match_mode: Set("all".to_string()),
            live_updating: Set(true),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        };
        let result = model.insert(&self.db).await?;
        info!(id = %result.id, name = %result.name, "Playlist created");
        Ok(result)
    }

    /// Import one regular playlist and all usable entries atomically.
    ///
    /// Matching and persistence share one database transaction and track-table
    /// snapshot. A database error therefore rolls back the playlist and every
    /// entry instead of leaving a partially imported result. Entries with a
    /// usable path or metadata fingerprint are retained even when no current
    /// track matches, so reconciliation can link them after the library
    /// changes. Source rows with no usable identity, or a duration that cannot
    /// be represented by the playlist schema, are counted as failed.
    pub async fn import_regular_playlist(
        &self,
        name: &str,
        imported: &[ImportedTrack],
    ) -> Result<PlaylistImportResult, DbErr> {
        let txn = self.db.begin().await?;
        let all_tracks = track::Entity::find().all(&txn).await?;
        let match_index = ImportedTrackMatchIndex::new(&all_tracks);
        let now = now_rfc3339();
        let playlist = playlist::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            name: Set(name.to_string()),
            is_smart: Set(false),
            smart_rules_json: Set(None),
            limit_enabled: Set(false),
            limit_value: Set(None),
            limit_unit: Set(None),
            limit_sort: Set(None),
            match_mode: Set("all".to_string()),
            live_updating: Set(true),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(&txn)
        .await?;

        let mut counts = PlaylistImportCounts::default();
        let mut position = 0i32;

        for source in imported {
            let has_path = !source.file_path.trim().is_empty();
            let has_fingerprint =
                !source.title.trim().is_empty() && !source.artist.trim().is_empty();
            if !has_path && !has_fingerprint {
                counts.failed += 1;
                continue;
            }

            let imported_duration = match source.duration_secs {
                Some(value) => match i32::try_from(value) {
                    Ok(value) => Some(value),
                    Err(_) => {
                        counts.failed += 1;
                        continue;
                    }
                },
                None => None,
            };
            let matched = match_index.find(source);
            let (track_id, match_file_path, title, artist, album, match_duration) =
                if let Some(track) = matched {
                    if TrackId::new(track.id.as_str()).is_err() {
                        counts.failed += 1;
                        continue;
                    }
                    counts.matched += 1;
                    (
                        Some(track.id.clone()),
                        has_path.then(|| source.file_path.clone()),
                        normalize_fingerprint(&track.title),
                        normalize_fingerprint(&track.artist_name),
                        normalize_fingerprint(&track.album_title),
                        valid_track_match_duration(track),
                    )
                } else {
                    counts.unmatched += 1;
                    (
                        None,
                        has_path.then(|| source.file_path.clone()),
                        normalize_fingerprint(&source.title),
                        normalize_fingerprint(&source.artist),
                        normalize_fingerprint(&source.album),
                        imported_duration,
                    )
                };

            playlist_entry::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                playlist_id: Set(playlist.id.clone()),
                position: Set(position),
                source_id: Set(SourceId::local().to_string()),
                track_id: Set(track_id.clone()),
                local_track_id: Set(track_id),
                match_file_path: Set(match_file_path),
                match_title: Set(title),
                match_artist: Set(artist),
                match_album: Set(album),
                match_duration_secs: Set(match_duration),
            }
            .insert(&txn)
            .await?;

            position = position
                .checked_add(1)
                .ok_or_else(|| DbErr::Custom("Playlist has too many entries".to_string()))?;
        }

        txn.commit().await?;
        info!(
            id = %playlist.id,
            name = %playlist.name,
            matched = counts.matched,
            unmatched = counts.unmatched,
            failed = counts.failed,
            "Playlist import committed"
        );
        Ok(PlaylistImportResult { playlist, counts })
    }

    /// Delete a playlist and all its entries (cascade).
    pub async fn delete_playlist(&self, id: &str) -> Result<(), DbErr> {
        playlist::Entity::delete_by_id(id.to_string())
            .exec(&self.db)
            .await?;
        info!(id = %id, "Playlist deleted");
        Ok(())
    }

    /// Rename a playlist.
    pub async fn rename_playlist(&self, id: &str, new_name: &str) -> Result<(), DbErr> {
        let mut model: playlist::ActiveModel = playlist::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await?
            .ok_or(DbErr::RecordNotFound(format!("Playlist {id} not found")))?
            .into();

        model.name = Set(new_name.to_string());
        model.updated_at = Set(now_rfc3339());
        model.update(&self.db).await?;
        info!(id = %id, name = %new_name, "Playlist renamed");
        Ok(())
    }

    /// List all playlists ordered by creation date.
    pub async fn list_playlists(&self) -> Result<Vec<playlist::Model>, DbErr> {
        playlist::Entity::find()
            .order_by_asc(playlist::Column::CreatedAt)
            .all(&self.db)
            .await
    }

    /// Get a single playlist by ID.
    pub async fn get_playlist(&self, id: &str) -> Result<Option<playlist::Model>, DbErr> {
        playlist::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await
    }

    // ── Regular playlist track management ────────────────────────────

    /// Add a track to a regular playlist.
    ///
    /// Stores fingerprint data (title, artist, album, duration) for
    /// rediscovery after a library rebuild.
    pub async fn add_track(&self, playlist_id: &str, track: &track::Model) -> Result<(), DbErr> {
        let input = PlaylistEntryInput::local(track)?;
        self.add_entries(playlist_id, &[input]).await?;
        debug!(playlist = %playlist_id, track = %track.title, "Track added to playlist");
        Ok(())
    }

    /// Append exact source-scoped tracks to one regular playlist atomically.
    ///
    /// Duplicate identities are intentionally preserved as distinct
    /// occurrences in input order. Local identities are resolved against the
    /// current track table inside the same transaction and receive the local
    /// foreign-key cache. Non-local identities are persisted without a URI or
    /// local foreign key; callers must obtain and recheck live catalogue
    /// authority before entering this storage boundary.
    pub async fn add_entries(
        &self,
        playlist_id: &str,
        inputs: &[PlaylistEntryInput],
    ) -> Result<Vec<StoredPlaylistEntry>, DbErr> {
        let txn = self.db.begin().await?;
        require_regular_playlist(&txn, playlist_id).await?;
        if inputs.is_empty() {
            txn.commit().await?;
            return Ok(Vec::new());
        }

        for input in inputs {
            if input.media_key.source_id.as_uuid().is_nil() {
                return Err(DbErr::Custom(
                    "Playlist source identity is unavailable".to_string(),
                ));
            }
            if input.media_key.source_id != SourceId::local() {
                TrackId::remote(input.media_key.track_id.as_str()).map_err(|_| {
                    DbErr::Custom("Remote playlist track identity is invalid".to_string())
                })?;
            }
        }

        let max_position = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .order_by_desc(playlist_entry::Column::Position)
            .one(&txn)
            .await?
            .map(|entry| entry.position)
            .unwrap_or(-1);
        if max_position < -1 {
            return Err(DbErr::Custom(format!(
                "Playlist {playlist_id} has an invalid entry position"
            )));
        }
        let first_position = max_position
            .checked_add(1)
            .ok_or_else(|| DbErr::Custom("Playlist has too many entries".to_string()))?;

        let local_ids: HashSet<&str> = inputs
            .iter()
            .filter(|input| input.media_key.source_id == SourceId::local())
            .map(|input| input.media_key.track_id.as_str())
            .collect();
        let local_tracks: HashMap<String, track::Model> = if local_ids.is_empty() {
            HashMap::new()
        } else {
            track::Entity::find()
                .filter(track::Column::Id.is_in(local_ids.iter().copied()))
                .all(&txn)
                .await?
                .into_iter()
                .map(|track| (track.id.clone(), track))
                .collect()
        };
        if local_ids
            .iter()
            .any(|track_id| !local_tracks.contains_key(*track_id))
        {
            return Err(DbErr::RecordNotFound(
                "Local playlist track not found".to_string(),
            ));
        }

        let mut inserted = Vec::with_capacity(inputs.len());
        for (offset, input) in inputs.iter().enumerate() {
            let offset = i32::try_from(offset)
                .map_err(|_| DbErr::Custom("Playlist has too many entries".to_string()))?;
            let position = first_position
                .checked_add(offset)
                .ok_or_else(|| DbErr::Custom("Playlist has too many entries".to_string()))?;

            let is_local = input.media_key.source_id == SourceId::local();
            let local_track = is_local.then(|| {
                local_tracks
                    .get(input.media_key.track_id.as_str())
                    .expect("all local playlist inputs were validated")
            });
            let duration = match local_track {
                Some(track) => valid_track_match_duration(track),
                None => input
                    .duration_secs
                    .map(i32::try_from)
                    .transpose()
                    .map_err(|_| {
                        DbErr::Custom("Playlist entry duration is too large".to_string())
                    })?,
            };
            let (title, artist, album) = match local_track {
                Some(track) => (&track.title, &track.artist_name, &track.album_title),
                None => (&input.title, &input.artist, &input.album),
            };
            let track_id = input.media_key.track_id.as_str().to_string();
            let model = playlist_entry::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                playlist_id: Set(playlist_id.to_string()),
                position: Set(position),
                source_id: Set(input.media_key.source_id.to_string()),
                track_id: Set(Some(track_id.clone())),
                local_track_id: Set(is_local.then_some(track_id)),
                // A path is authoritative only when an imported local
                // playlist supplied it as durable location evidence.
                match_file_path: Set(None),
                match_title: Set(normalize_fingerprint(title)),
                match_artist: Set(normalize_fingerprint(artist)),
                match_album: Set(normalize_fingerprint(album)),
                match_duration_secs: Set(duration),
            }
            .insert(&txn)
            .await?;
            inserted.push(StoredPlaylistEntry::from_model(model)?);
        }

        txn.commit().await?;
        Ok(inserted)
    }

    /// Remove an entry from its owning regular playlist and close the gap.
    pub async fn remove_entry(&self, entry_id: &str) -> Result<(), DbErr> {
        let entry = playlist_entry::Entity::find_by_id(entry_id.to_string())
            .one(&self.db)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("Entry {entry_id} not found")))?;
        self.remove_entries(&entry.playlist_id, &[entry_id.to_string()])
            .await
    }

    /// Remove exact durable occurrences atomically and restore contiguous
    /// positions. Every ID must be unique and belong to `playlist_id`.
    pub async fn remove_entries(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), DbErr> {
        let txn = self.db.begin().await?;
        require_regular_playlist(&txn, playlist_id).await?;
        if entry_ids.is_empty() {
            txn.commit().await?;
            return Ok(());
        }
        let current = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .order_by_asc(playlist_entry::Column::Position)
            .all(&txn)
            .await?;
        let requested: HashSet<&str> = entry_ids.iter().map(String::as_str).collect();
        if requested.len() != entry_ids.len() {
            return Err(DbErr::Custom(
                "Playlist removal contains duplicate entry IDs".to_string(),
            ));
        }
        let current_ids: HashSet<&str> = current.iter().map(|entry| entry.id.as_str()).collect();
        if let Some(missing) = requested
            .iter()
            .find(|entry_id| !current_ids.contains(**entry_id))
        {
            return Err(DbErr::RecordNotFound(format!(
                "Entry {missing} not found in playlist {playlist_id}"
            )));
        }

        if !entry_ids.is_empty() {
            let deleted = playlist_entry::Entity::delete_many()
                .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
                .filter(playlist_entry::Column::Id.is_in(entry_ids.iter().map(String::as_str)))
                .exec(&txn)
                .await?;
            let expected = u64::try_from(entry_ids.len())
                .map_err(|_| DbErr::Custom("Too many playlist entries selected".to_string()))?;
            if deleted.rows_affected != expected {
                return Err(DbErr::Custom(
                    "Playlist changed while entries were being removed".to_string(),
                ));
            }
        }

        let remaining_ids: Vec<String> = current
            .into_iter()
            .filter(|entry| !requested.contains(entry.id.as_str()))
            .map(|entry| entry.id)
            .collect();
        assign_contiguous_positions(&txn, playlist_id, &remaining_ids).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Reorder every occurrence in a playlist. `entry_ids` must be one exact,
    /// duplicate-free permutation of the playlist's durable entry IDs.
    pub async fn reorder_entries(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), DbErr> {
        let txn = self.db.begin().await?;
        require_regular_playlist(&txn, playlist_id).await?;
        let current = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .all(&txn)
            .await?;
        let requested: HashSet<&str> = entry_ids.iter().map(String::as_str).collect();
        let current_ids: HashSet<&str> = current.iter().map(|entry| entry.id.as_str()).collect();
        if requested.len() != entry_ids.len()
            || entry_ids.len() != current.len()
            || requested != current_ids
        {
            return Err(DbErr::Custom(format!(
                "Playlist {playlist_id} reorder must contain each entry exactly once"
            )));
        }

        assign_contiguous_positions(&txn, playlist_id, entry_ids).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Load every durable regular-playlist occurrence in stored order.
    /// Unmatched and currently unavailable entries are retained.
    pub async fn get_playlist_entries(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<StoredPlaylistEntry>, DbErr> {
        require_regular_playlist(&self.db, playlist_id).await?;
        let entries = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .order_by_asc(playlist_entry::Column::Position)
            .all(&self.db)
            .await?;
        entries
            .into_iter()
            .map(StoredPlaylistEntry::from_model)
            .collect()
    }

    /// Load every durable regular-playlist occurrence in stored order and
    /// align it with its exact current local row when one exists.
    ///
    /// Playlist validation and the ordered left join share one read
    /// transaction. Remote and unmatched local entries remain present with no
    /// local model; duplicate occurrences remain separate rows even when they
    /// resolve to the same local track.
    pub async fn load_playlist_entries(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<LoadedPlaylistEntry>, DbErr> {
        let txn = self.db.begin().await?;
        require_regular_playlist(&txn, playlist_id).await?;
        let rows = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .order_by_asc(playlist_entry::Column::Position)
            .find_also_related(track::Entity)
            .all(&txn)
            .await?;

        let mut loaded = Vec::with_capacity(rows.len());
        for (entry, local_track) in rows {
            let stored = StoredPlaylistEntry::from_model(entry)?;
            let local_track = if stored.source_id == SourceId::local() {
                local_track
            } else {
                // Typed decoding rejects a non-local foreign-key cache. Keep
                // the projection fail-closed as well if a future relation or
                // schema change ever produces an unexpected joined row.
                None
            };
            loaded.push(LoadedPlaylistEntry {
                stored,
                local_track,
            });
        }
        txn.commit().await?;
        Ok(loaded)
    }

    /// Get all matched tracks for a regular playlist (ordered by position).
    ///
    /// This compatibility projection is deliberately local-only. It returns
    /// entries with a valid local foreign-key cache and excludes remote or
    /// unresolved occurrences until mixed-source UI projection lands.
    pub async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<track::Model>, DbErr> {
        let entries = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .filter(playlist_entry::Column::SourceId.eq(SourceId::local().to_string()))
            .filter(playlist_entry::Column::LocalTrackId.is_not_null())
            .order_by_asc(playlist_entry::Column::Position)
            .all(&self.db)
            .await?;

        // Collect the linked track IDs in playlist order.
        let track_ids: Vec<String> = entries
            .iter()
            .filter_map(|entry| entry.local_track_id.clone())
            .collect();
        if track_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Fetch all referenced tracks in a single query (instead of N+1
        // `find_by_id` round-trips), then re-order them to match entry
        // positions. Duplicate entries for the same track are preserved.
        let by_id: HashMap<String, track::Model> = track::Entity::find()
            .filter(track::Column::Id.is_in(track_ids.iter().map(String::as_str)))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();

        let tracks = track_ids
            .iter()
            .filter_map(|id| by_id.get(id).cloned())
            .collect();
        Ok(tracks)
    }

    // ── Smart playlist management ────────────────────────────────────

    /// Save smart rules to a playlist.
    pub async fn set_smart_rules(
        &self,
        playlist_id: &str,
        rules: &SmartRules,
    ) -> Result<(), DbErr> {
        let json = serde_json::to_string(rules)
            .map_err(|e| DbErr::Custom(format!("Failed to serialize rules: {e}")))?;

        let mut model: playlist::ActiveModel =
            playlist::Entity::find_by_id(playlist_id.to_string())
                .one(&self.db)
                .await?
                .ok_or(DbErr::RecordNotFound(format!(
                    "Playlist {playlist_id} not found"
                )))?
                .into();

        model.smart_rules_json = Set(Some(json));
        model.match_mode = Set(match rules.match_mode {
            smart_rules::MatchMode::All => "all".to_string(),
            smart_rules::MatchMode::Any => "any".to_string(),
        });
        // Kept only for compatibility with the historical NOT NULL column.
        // Smart playlists are always evaluated against the current library.
        model.live_updating = Set(true);

        if let Some(limit) = &rules.limit {
            model.limit_enabled = Set(true);
            model.limit_value = Set(Some(limit.value as i32));
            model.limit_unit = Set(Some(serde_json::to_string(&limit.unit).unwrap_or_default()));
            model.limit_sort = Set(Some(
                serde_json::to_string(&limit.selected_by).unwrap_or_default(),
            ));
        } else {
            model.limit_enabled = Set(false);
            model.limit_value = Set(None);
            model.limit_unit = Set(None);
            model.limit_sort = Set(None);
        }

        model.updated_at = Set(now_rfc3339());
        model.update(&self.db).await?;
        info!(id = %playlist_id, "Smart playlist rules updated");
        Ok(())
    }

    /// Evaluate a smart playlist against all library tracks.
    pub async fn evaluate_smart_playlist(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<track::Model>, DbErr> {
        let playlist = playlist::Entity::find_by_id(playlist_id.to_string())
            .one(&self.db)
            .await?
            .ok_or(DbErr::RecordNotFound(format!(
                "Playlist {playlist_id} not found"
            )))?;

        // A playlist with no rules configured yet (`smart_rules_json` is None)
        // defaults to "match all". But a *parse failure* of stored JSON
        // (corruption or schema drift) must NOT silently become match-all —
        // that would dump the whole library — so it is logged and yields no
        // tracks instead.
        let rules: SmartRules = match playlist.smart_rules_json.as_deref() {
            Some(json) => match serde_json::from_str(json) {
                Ok(rules) => rules,
                Err(e) => {
                    warn!(
                        id = %playlist_id,
                        error = %e,
                        "Failed to parse smart_rules_json; returning no tracks instead of matching all"
                    );
                    return Ok(Vec::new());
                }
            },
            None => SmartRules {
                match_mode: smart_rules::MatchMode::All,
                rules: Vec::new(),
                limit: None,
                sort_order: Vec::new(),
            },
        };

        // The whole table is loaded and evaluated in Rust because the rule
        // engine is generic over the `SmartTrack` trait (it also drives UI
        // `TrackObject`s), so the predicates can't all be expressed in SQL.
        // The per-comparison allocation cost of the compound sort is mitigated
        // in `smart_rules::apply_compound_sort` (decorate-sort-undecorate).
        let all_tracks = track::Entity::find().all(&self.db).await?;
        let results = smart_rules::evaluate(&rules, &all_tracks);
        Ok(results)
    }

    // ── Track reconciliation ─────────────────────────────────────────

    /// Re-link orphaned playlist entries to newly-discovered tracks.
    ///
    /// Called after a library rebuild (FullSync). Finds entries with
    /// the built-in local `source_id` and `local_track_id IS NULL`, then
    /// attempts to match them against current tracks by exact retained path
    /// first, then by a normalized
    /// `(title, artist, album)` fingerprint with optional duration tolerance.
    /// Remote identities are never relinked by local metadata.
    ///
    /// Returns the number of entries re-linked.
    pub async fn reconcile_all(&self) -> Result<u32, DbErr> {
        let orphans = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::SourceId.eq(SourceId::local().to_string()))
            .filter(playlist_entry::Column::LocalTrackId.is_null())
            .all(&self.db)
            .await?;

        if orphans.is_empty() {
            return Ok(0);
        }

        info!(
            orphans = orphans.len(),
            "Reconciling orphaned playlist entries"
        );

        // Load one track-table snapshot. The same pure path/fingerprint
        // resolver is used by import and reconciliation, keeping duration and
        // ambiguity behavior identical across both paths.
        let all_tracks = track::Entity::find().all(&self.db).await?;
        let match_index = ImportedTrackMatchIndex::new(&all_tracks);

        let mut relinked = 0u32;

        for orphan in orphans {
            let duration_secs = match orphan.match_duration_secs {
                Some(value) if value >= 0 => Some(value as u64),
                Some(value) => {
                    warn!(
                        entry = %orphan.id,
                        duration_secs = value,
                        "Ignoring invalid negative playlist match duration"
                    );
                    None
                }
                None => None,
            };
            let imported = ImportedTrack {
                title: orphan.match_title.clone(),
                artist: orphan.match_artist.clone(),
                album: orphan.match_album.clone(),
                file_path: orphan.match_file_path.clone().unwrap_or_default(),
                duration_secs,
            };
            let best = match_index.find(&imported);

            if let Some(best) = best {
                if TrackId::new(best.id.as_str()).is_err() {
                    warn!(
                        entry = %orphan.id,
                        "Skipping playlist reconciliation with an invalid local track identity"
                    );
                    continue;
                }
                let track_id = best.id.clone();
                let match_title = normalize_fingerprint(&best.title);
                let match_artist = normalize_fingerprint(&best.artist_name);
                let match_album = normalize_fingerprint(&best.album_title);
                let match_duration_secs = valid_track_match_duration(best);
                let mut entry: playlist_entry::ActiveModel = orphan.into();
                entry.track_id = Set(Some(track_id.clone()));
                entry.local_track_id = Set(Some(track_id));
                entry.match_title = Set(match_title);
                entry.match_artist = Set(match_artist);
                entry.match_album = Set(match_album);
                entry.match_duration_secs = Set(match_duration_secs);
                entry.update(&self.db).await?;
                relinked += 1;
            }
        }

        info!(
            relinked = relinked,
            "Playlist entry reconciliation complete"
        );
        Ok(relinked)
    }

    // ── Default smart playlists ──────────────────────────────────────

    /// Seed default smart playlists on first launch.
    ///
    /// Creates: Recently Added, Recently Played, Top 25 Most Played.
    /// Called from the engine when the playlist table is empty.
    pub async fn seed_defaults(&self) -> Result<Vec<playlist::Model>, DbErr> {
        let mut created = Vec::new();

        // 1. Recently Added — Date Added is in the last 30 days
        let rules_recently_added = smart_rules::SmartRules {
            match_mode: smart_rules::MatchMode::All,
            rules: vec![smart_rules::SmartRule {
                field: smart_rules::RuleField::DateAdded,
                operator: smart_rules::RuleOperator::IsInTheLast {
                    amount: 30,
                    unit: smart_rules::DateUnit::Days,
                },
                value: smart_rules::RuleValue::Number(30),
            }],
            limit: None,
            sort_order: vec![smart_rules::SortCriterion {
                field: smart_rules::SortField::DateAdded,
                direction: smart_rules::SortDirection::Descending,
            }],
        };
        let pl = self.create_playlist("Recently Added", true).await?;
        self.set_smart_rules(&pl.id, &rules_recently_added).await?;
        info!(id = %pl.id, "Seeded: Recently Added");
        created.push(pl);

        // 2. Recently Played — authoritative playback time in the inclusive
        // last-14-day window, newest first with stable TrackId ties.
        let rules_recently_played = recently_played_default_rules();
        let pl = self.create_playlist("Recently Played", true).await?;
        self.set_smart_rules(&pl.id, &rules_recently_played).await?;
        info!(id = %pl.id, "Seeded: Recently Played");
        created.push(pl);

        // 3. Top 25 Most Played — positive counts only, then count descending,
        // playback time descending (unknown last), and stable TrackId ties.
        let rules_top25 = top_25_most_played_default_rules();
        let pl = self.create_playlist("Top 25 Most Played", true).await?;
        self.set_smart_rules(&pl.id, &rules_top25).await?;
        info!(id = %pl.id, "Seeded: Top 25 Most Played");
        created.push(pl);

        info!(count = created.len(), "Default smart playlists seeded");
        Ok(created)
    }
}

fn recently_played_default_rules() -> smart_rules::SmartRules {
    smart_rules::SmartRules {
        match_mode: smart_rules::MatchMode::All,
        rules: vec![smart_rules::SmartRule {
            field: smart_rules::RuleField::LastPlayed,
            operator: smart_rules::RuleOperator::IsInTheLast {
                amount: 14,
                unit: smart_rules::DateUnit::Days,
            },
            value: smart_rules::RuleValue::Number(14),
        }],
        limit: None,
        sort_order: vec![
            smart_rules::SortCriterion {
                field: smart_rules::SortField::LastPlayed,
                direction: smart_rules::SortDirection::Descending,
            },
            smart_rules::SortCriterion {
                field: smart_rules::SortField::TrackId,
                direction: smart_rules::SortDirection::Ascending,
            },
        ],
    }
}

fn top_25_most_played_default_rules() -> smart_rules::SmartRules {
    smart_rules::SmartRules {
        match_mode: smart_rules::MatchMode::All,
        rules: vec![smart_rules::SmartRule {
            field: smart_rules::RuleField::PlayCount,
            operator: smart_rules::RuleOperator::GreaterThan,
            value: smart_rules::RuleValue::Number(0),
        }],
        limit: Some(smart_rules::SmartLimit {
            value: 25,
            unit: smart_rules::LimitUnit::Items,
            selected_by: smart_rules::LimitSort::MostPlayed,
        }),
        sort_order: vec![
            smart_rules::SortCriterion {
                field: smart_rules::SortField::PlayCount,
                direction: smart_rules::SortDirection::Descending,
            },
            smart_rules::SortCriterion {
                field: smart_rules::SortField::LastPlayed,
                direction: smart_rules::SortDirection::Descending,
            },
            smart_rules::SortCriterion {
                field: smart_rules::SortField::TrackId,
                direction: smart_rules::SortDirection::Ascending,
            },
        ],
    }
}

async fn require_regular_playlist<C>(db: &C, playlist_id: &str) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    let playlist = playlist::Entity::find_by_id(playlist_id.to_string())
        .one(db)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("Playlist {playlist_id} not found")))?;
    if playlist.is_smart {
        return Err(DbErr::Custom(format!(
            "Playlist {playlist_id} is smart and cannot store regular entries"
        )));
    }
    Ok(())
}

/// Assign one exact occurrence order without ever violating the unique
/// `(playlist_id, position)` index. All arithmetic is checked before the
/// first write, and the caller owns the surrounding transaction.
async fn assign_contiguous_positions(
    txn: &DatabaseTransaction,
    playlist_id: &str,
    entry_ids: &[String],
) -> Result<(), DbErr> {
    let current = playlist_entry::Entity::find()
        .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
        .order_by_asc(playlist_entry::Column::Position)
        .all(txn)
        .await?;
    let requested: HashSet<&str> = entry_ids.iter().map(String::as_str).collect();
    let current_ids: HashSet<&str> = current.iter().map(|entry| entry.id.as_str()).collect();
    if requested.len() != entry_ids.len()
        || entry_ids.len() != current.len()
        || requested != current_ids
    {
        return Err(DbErr::Custom(format!(
            "Playlist {playlist_id} changed while positions were being assigned"
        )));
    }

    let already_contiguous = current.iter().enumerate().all(|(position, entry)| {
        i32::try_from(position) == Ok(entry.position) && entry_ids.get(position) == Some(&entry.id)
    });
    if already_contiguous {
        return Ok(());
    }

    let maximum_position = current
        .iter()
        .map(|entry| entry.position)
        .max()
        .unwrap_or(-1);
    if maximum_position < -1 {
        return Err(DbErr::Custom(format!(
            "Playlist {playlist_id} has an invalid entry position"
        )));
    }
    let parking_start = maximum_position.checked_add(1).ok_or_else(|| {
        DbErr::Custom("Playlist positions cannot be reordered safely".to_string())
    })?;
    let final_offset = entry_ids
        .len()
        .checked_sub(1)
        .map(i32::try_from)
        .transpose()
        .map_err(|_| DbErr::Custom("Playlist has too many entries".to_string()))?
        .unwrap_or(0);
    parking_start.checked_add(final_offset).ok_or_else(|| {
        DbErr::Custom("Playlist positions cannot be reordered safely".to_string())
    })?;

    // Park every row above the complete current range, then assign 0..N.
    // The unique position index remains valid after every individual update.
    // Reuse the snapshot already validated above: querying each entry again
    // in both passes would add 2N database round trips while holding the
    // write transaction.
    let mut current_by_id: HashMap<String, playlist_entry::Model> = current
        .into_iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect();
    let mut parked_entries = Vec::with_capacity(entry_ids.len());
    for (offset, entry_id) in entry_ids.iter().enumerate() {
        let offset = i32::try_from(offset)
            .map_err(|_| DbErr::Custom("Playlist has too many entries".to_string()))?;
        let mut entry: playlist_entry::ActiveModel = current_by_id
            .remove(entry_id)
            .ok_or_else(|| DbErr::RecordNotFound(format!("Entry {entry_id} not found")))?
            .into();
        entry.position = Set(parking_start + offset);
        parked_entries.push(entry.update(txn).await?);
    }
    debug_assert!(current_by_id.is_empty());

    for (position, entry) in parked_entries.into_iter().enumerate() {
        let position = i32::try_from(position)
            .map_err(|_| DbErr::Custom("Playlist has too many entries".to_string()))?;
        let mut entry: playlist_entry::ActiveModel = entry.into();
        entry.position = Set(position);
        entry.update(txn).await?;
    }

    Ok(())
}

fn normalize_fingerprint(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Convert library duration metadata into safe playlist match evidence.
///
/// Duration is optional during reconciliation. Corrupt negative values and
/// values outside the playlist schema's non-negative `i32` range must not
/// wrap, block exact-path matching, or make an otherwise unique fingerprint
/// appear authoritative.
fn valid_track_match_duration(track: &track::Model) -> Option<i32> {
    let duration_secs = track.duration_secs?;
    match i32::try_from(duration_secs) {
        Ok(value) if value >= 0 => Some(value),
        _ => {
            warn!(
                track_id = %track.id,
                duration_secs,
                "Omitting invalid track duration from playlist match evidence"
            );
            None
        }
    }
}

/// Get current time as RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, Database,
        DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    };
    use sea_orm_migration::MigratorTrait;

    use super::{
        recently_played_default_rules, top_25_most_played_default_rules, PlaylistEntryInput,
        PlaylistManager, StoredPlaylistEntry,
    };
    use crate::architecture::{MediaKey, SourceId, TrackId};
    use crate::db::entities::{playlist, playlist_entry, track};
    use crate::db::migration::Migrator;
    use crate::local::playlist_io::ImportedTrack;
    use crate::local::smart_rules;

    /// Open a fresh in-memory SQLite database with all migrations applied.
    ///
    /// SeaORM forces `max_connections(1)` for SQLite, so the single pooled
    /// connection keeps the in-memory schema alive for the whole test.
    async fn in_memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        Migrator::up(&db, None).await.expect("run migrations");
        db
    }

    async fn insert_entry(db: &DatabaseConnection, playlist_id: &str, id: &str, position: i32) {
        playlist_entry::ActiveModel {
            id: Set(id.to_string()),
            playlist_id: Set(playlist_id.to_string()),
            position: Set(position),
            source_id: Set(SourceId::local().to_string()),
            track_id: Set(None),
            local_track_id: Set(None),
            match_file_path: Set(None),
            match_title: Set("placeholder".to_string()),
            match_artist: Set("placeholder".to_string()),
            match_album: Set(String::new()),
            match_duration_secs: Set(None),
        }
        .insert(db)
        .await
        .expect("insert entry");
    }

    async fn insert_track(
        db: &DatabaseConnection,
        id: &str,
        file_path: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_secs: Option<i64>,
    ) -> track::Model {
        let model = track::Model {
            id: id.to_string(),
            file_path: file_path.to_string(),
            title: title.to_string(),
            artist_name: artist.to_string(),
            album_artist_name: None,
            album_title: album.to_string(),
            genre: None,
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            last_played_at_ms: None,
            rating: None,
            date_added: "2026-07-12T00:00:00Z".to_string(),
            date_modified: "2026-07-12T00:00:00Z".to_string(),
            file_size_bytes: None,
        };
        let active: track::ActiveModel = model.into();
        active.insert(db).await.expect("insert track")
    }

    async fn playlist_entries(
        db: &DatabaseConnection,
        playlist_id: &str,
    ) -> Vec<playlist_entry::Model> {
        playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .order_by_asc(playlist_entry::Column::Position)
            .all(db)
            .await
            .expect("load playlist entries")
    }

    #[test]
    fn playback_history_default_rules_have_canonical_serialized_forms() {
        let recently_played = serde_json::to_string(&recently_played_default_rules())
            .expect("serialize Recently Played defaults");
        assert_eq!(
            recently_played,
            r#"{"match_mode":"All","rules":[{"field":"LastPlayed","operator":{"IsInTheLast":{"amount":14,"unit":"Days"}},"value":{"Number":14}}],"limit":null,"sort_order":[{"field":"LastPlayed","direction":"Descending"},{"field":"TrackId","direction":"Ascending"}]}"#
        );

        let top_25 = serde_json::to_string(&top_25_most_played_default_rules())
            .expect("serialize Top 25 defaults");
        assert_eq!(
            top_25,
            r#"{"match_mode":"All","rules":[{"field":"PlayCount","operator":"GreaterThan","value":{"Number":0}}],"limit":{"value":25,"unit":"Items","selected_by":"MostPlayed"},"sort_order":[{"field":"PlayCount","direction":"Descending"},{"field":"LastPlayed","direction":"Descending"},{"field":"TrackId","direction":"Ascending"}]}"#
        );
    }

    #[test]
    fn stored_entry_decode_enforces_remote_byte_bound_without_leaking_identity() {
        // SQLite's length() reports characters, so the storage boundary still
        // enforces the architecture's byte ceiling when decoding a row.
        let oversized_secret = "é".repeat(3_000);
        let model = playlist_entry::Model {
            id: "remote-entry".to_string(),
            playlist_id: "playlist".to_string(),
            position: 0,
            source_id: SourceId::random().to_string(),
            track_id: Some(oversized_secret.clone()),
            local_track_id: None,
            match_title: "Title".to_string(),
            match_artist: "Artist".to_string(),
            match_album: "Album".to_string(),
            match_duration_secs: None,
            match_file_path: None,
        };

        let error = StoredPlaylistEntry::from_model(model)
            .expect_err("remote IDs over 4096 bytes must fail closed");
        assert!(!error.to_string().contains(&oversized_secret));
    }

    #[test]
    fn stored_entry_decode_rejects_unidentified_local_orphan() {
        let model = playlist_entry::Model {
            id: "unidentified-local-entry".to_string(),
            playlist_id: "playlist".to_string(),
            position: 0,
            source_id: SourceId::local().to_string(),
            track_id: None,
            local_track_id: None,
            match_title: " ".to_string(),
            match_artist: String::new(),
            match_album: String::new(),
            match_duration_secs: None,
            match_file_path: Some("  ".to_string()),
        };

        StoredPlaylistEntry::from_model(model)
            .expect_err("an unmatched local row needs path or title/artist evidence");
    }

    #[test]
    fn stored_entry_decode_rejects_negative_position() {
        let model = playlist_entry::Model {
            id: "negative-position-entry".to_string(),
            playlist_id: "playlist".to_string(),
            position: -1,
            source_id: SourceId::random().to_string(),
            track_id: Some("remote-track".to_string()),
            local_track_id: None,
            match_title: String::new(),
            match_artist: String::new(),
            match_album: String::new(),
            match_duration_secs: None,
            match_file_path: None,
        };

        StoredPlaylistEntry::from_model(model)
            .expect_err("typed storage must reject a negative occurrence position");
    }

    #[tokio::test]
    async fn fresh_seed_persists_canonical_history_rules_and_redundant_columns() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        manager.seed_defaults().await.expect("seed defaults");

        let recently_played = playlist::Entity::find()
            .filter(playlist::Column::Name.eq("Recently Played"))
            .one(&db)
            .await
            .expect("query Recently Played")
            .expect("Recently Played seed");
        assert_eq!(
            recently_played.smart_rules_json.as_deref(),
            Some(
                r#"{"match_mode":"All","rules":[{"field":"LastPlayed","operator":{"IsInTheLast":{"amount":14,"unit":"Days"}},"value":{"Number":14}}],"limit":null,"sort_order":[{"field":"LastPlayed","direction":"Descending"},{"field":"TrackId","direction":"Ascending"}]}"#
            )
        );
        assert!(!recently_played.limit_enabled);
        assert_eq!(recently_played.limit_value, None);
        assert_eq!(recently_played.limit_unit, None);
        assert_eq!(recently_played.limit_sort, None);
        assert_eq!(recently_played.match_mode, "all");
        assert!(recently_played.live_updating);

        let top_25 = playlist::Entity::find()
            .filter(playlist::Column::Name.eq("Top 25 Most Played"))
            .one(&db)
            .await
            .expect("query Top 25")
            .expect("Top 25 seed");
        assert_eq!(
            top_25.smart_rules_json.as_deref(),
            Some(
                r#"{"match_mode":"All","rules":[{"field":"PlayCount","operator":"GreaterThan","value":{"Number":0}}],"limit":{"value":25,"unit":"Items","selected_by":"MostPlayed"},"sort_order":[{"field":"PlayCount","direction":"Descending"},{"field":"LastPlayed","direction":"Descending"},{"field":"TrackId","direction":"Ascending"}]}"#
            )
        );
        assert!(top_25.limit_enabled);
        assert_eq!(top_25.limit_value, Some(25));
        assert_eq!(top_25.limit_unit.as_deref(), Some(r#""Items""#));
        assert_eq!(top_25.limit_sort.as_deref(), Some(r#""MostPlayed""#));
        assert_eq!(top_25.match_mode, "all");
        assert!(top_25.live_updating);
    }

    #[tokio::test]
    async fn seeded_history_playlists_reflect_committed_history_rows() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let older = insert_track(
            &db,
            "history-older",
            "/music/history-older.flac",
            "Older play",
            "Artist",
            "Album",
            Some(180),
        )
        .await;
        let newer = insert_track(
            &db,
            "history-newer",
            "/music/history-newer.flac",
            "Newer play",
            "Artist",
            "Album",
            Some(180),
        )
        .await;
        manager.seed_defaults().await.expect("seed defaults");

        let recently_played = playlist::Entity::find()
            .filter(playlist::Column::Name.eq("Recently Played"))
            .one(&db)
            .await
            .expect("query Recently Played")
            .expect("Recently Played seed");
        let top_25 = playlist::Entity::find()
            .filter(playlist::Column::Name.eq("Top 25 Most Played"))
            .one(&db)
            .await
            .expect("query Top 25")
            .expect("Top 25 seed");

        assert!(manager
            .evaluate_smart_playlist(&recently_played.id)
            .await
            .expect("evaluate empty Recently Played")
            .is_empty());
        assert!(manager
            .evaluate_smart_playlist(&top_25.id)
            .await
            .expect("evaluate empty Top 25")
            .is_empty());

        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut older_active: track::ActiveModel = older.into();
        older_active.play_count = Set(7);
        older_active.last_played_at_ms = Set(Some(now_ms - 2_000));
        older_active
            .update(&db)
            .await
            .expect("commit older playback history");
        let mut newer_active: track::ActiveModel = newer.into();
        newer_active.play_count = Set(2);
        newer_active.last_played_at_ms = Set(Some(now_ms - 1_000));
        newer_active
            .update(&db)
            .await
            .expect("commit newer playback history");

        let recent_ids: Vec<_> = manager
            .evaluate_smart_playlist(&recently_played.id)
            .await
            .expect("reevaluate Recently Played")
            .into_iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(recent_ids, ["history-newer", "history-older"]);

        let top_ids: Vec<_> = manager
            .evaluate_smart_playlist(&top_25.id)
            .await
            .expect("reevaluate Top 25")
            .into_iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(top_ids, ["history-older", "history-newer"]);
    }

    #[tokio::test]
    async fn smart_playlist_rating_rules_use_persisted_values_and_deterministic_membership() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());

        for (id, rating) in [
            ("b", Some(80)),
            ("a", Some(80)),
            ("low", Some(20)),
            ("none", None),
        ] {
            let track = insert_track(
                &db,
                id,
                &format!("/music/{id}.flac"),
                id,
                "Artist",
                "Album",
                Some(180),
            )
            .await;
            let mut active: track::ActiveModel = track.into();
            active.rating = Set(rating);
            active.update(&db).await.expect("persist test rating");
        }

        let playlist = manager
            .create_playlist("Highest rated", true)
            .await
            .expect("create smart playlist");
        let highest = smart_rules::SmartRules {
            match_mode: smart_rules::MatchMode::All,
            rules: vec![smart_rules::SmartRule {
                field: smart_rules::RuleField::Rating,
                operator: smart_rules::RuleOperator::IsRated,
                value: smart_rules::RuleValue::Number(1),
            }],
            limit: Some(smart_rules::SmartLimit {
                value: 2,
                unit: smart_rules::LimitUnit::Items,
                selected_by: smart_rules::LimitSort::HighestRated,
            }),
            sort_order: vec![smart_rules::SortCriterion {
                field: smart_rules::SortField::Rating,
                direction: smart_rules::SortDirection::Descending,
            }],
        };
        manager
            .set_smart_rules(&playlist.id, &highest)
            .await
            .expect("save rating rules");
        let highest_ids: Vec<_> = manager
            .evaluate_smart_playlist(&playlist.id)
            .await
            .expect("evaluate rating rules")
            .into_iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(highest_ids, ["a", "b"]);

        let unrated = smart_rules::SmartRules {
            match_mode: smart_rules::MatchMode::All,
            rules: vec![smart_rules::SmartRule {
                field: smart_rules::RuleField::Rating,
                operator: smart_rules::RuleOperator::IsUnrated,
                value: smart_rules::RuleValue::Number(1),
            }],
            limit: None,
            sort_order: Vec::new(),
        };
        manager
            .set_smart_rules(&playlist.id, &unrated)
            .await
            .expect("save unrated rule");
        let unrated_ids: Vec<_> = manager
            .evaluate_smart_playlist(&playlist.id)
            .await
            .expect("evaluate unrated rule")
            .into_iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(unrated_ids, ["none"]);
    }

    #[tokio::test]
    async fn import_commits_matched_unmatched_and_failed_counts_and_reconciles_path() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let existing = insert_track(
            &db,
            "existing",
            "/music/existing.flac",
            "Canonical Title",
            "Canonical Artist",
            "Canonical Album",
            Some(180),
        )
        .await;
        let imported = vec![
            ImportedTrack {
                title: "Wrong metadata".to_string(),
                artist: "Wrong artist".to_string(),
                album: String::new(),
                file_path: existing.file_path.clone(),
                duration_secs: None,
            },
            ImportedTrack {
                title: " canonical title ".to_string(),
                artist: "CANONICAL ARTIST".to_string(),
                album: "Canonical Album".to_string(),
                file_path: String::new(),
                duration_secs: Some(180),
            },
            ImportedTrack {
                title: String::new(),
                artist: String::new(),
                album: String::new(),
                file_path: "/music/available-later.flac".to_string(),
                duration_secs: Some(200),
            },
            ImportedTrack {
                title: "No artist".to_string(),
                artist: String::new(),
                album: String::new(),
                file_path: String::new(),
                duration_secs: None,
            },
            ImportedTrack {
                title: "Exact path must not bypass schema validation".to_string(),
                artist: "Artist".to_string(),
                album: String::new(),
                file_path: existing.file_path.clone(),
                duration_secs: Some(2_147_483_648_u64),
            },
        ];

        let result = manager
            .import_regular_playlist("Imported", &imported)
            .await
            .expect("commit playlist import");
        assert_eq!(result.counts.matched, 2);
        assert_eq!(result.counts.unmatched, 1);
        assert_eq!(result.counts.failed, 2);

        let before = playlist_entries(&db, &result.playlist.id).await;
        assert_eq!(before.len(), 3);
        assert_eq!(before[0].position, 0);
        assert_eq!(before[0].source_id, SourceId::local().to_string());
        assert_eq!(before[0].track_id.as_deref(), Some(existing.id.as_str()));
        assert_eq!(
            before[0].local_track_id.as_deref(),
            Some(existing.id.as_str())
        );
        assert_eq!(
            before[0].match_file_path.as_deref(),
            Some(existing.file_path.as_str())
        );
        assert_eq!(before[0].match_title, "canonical title");
        assert_eq!(before[1].position, 1);
        assert_eq!(before[1].track_id.as_deref(), Some(existing.id.as_str()));
        assert_eq!(
            before[1].local_track_id.as_deref(),
            Some(existing.id.as_str())
        );
        assert_eq!(before[1].match_file_path, None);
        assert_eq!(before[2].position, 2);
        assert_eq!(before[2].track_id, None);
        assert_eq!(before[2].local_track_id, None);
        assert_eq!(
            before[2].match_file_path.as_deref(),
            Some("/music/available-later.flac")
        );

        let later = insert_track(
            &db,
            "available-later",
            "/music/available-later.flac",
            "Metadata can differ",
            "Path wins",
            "Album",
            Some(200),
        )
        .await;
        assert_eq!(manager.reconcile_all().await.expect("reconcile path"), 1);
        let after = playlist_entries(&db, &result.playlist.id).await;
        assert_eq!(after[2].id, before[2].id);
        assert_eq!(after[2].position, before[2].position);
        assert_eq!(after[2].track_id.as_deref(), Some(later.id.as_str()));
        assert_eq!(after[2].local_track_id.as_deref(), Some(later.id.as_str()));
        assert_eq!(after[2].match_title, "metadata can differ");
        assert_eq!(after[2].match_artist, "path wins");
        assert_eq!(after[2].match_album, "album");
        assert_eq!(after[2].match_duration_secs, Some(200));
    }

    #[tokio::test]
    async fn playlist_import_never_mutates_app_owned_library_ratings() {
        let db = in_memory_db().await;
        let existing = insert_track(
            &db,
            "rated-existing",
            "/music/rated.flac",
            "Rated",
            "Artist",
            "Album",
            Some(180),
        )
        .await;
        let mut rated: track::ActiveModel = existing.clone().into();
        rated.rating = Set(Some(87));
        rated.update(&db).await.expect("seed app-owned rating");

        let imported = [ImportedTrack {
            title: "Conflicting external title".to_string(),
            artist: "Conflicting external artist".to_string(),
            album: String::new(),
            file_path: existing.file_path,
            duration_secs: None,
        }];
        let manager = PlaylistManager::new(db.clone());
        let result = manager
            .import_regular_playlist("Rating-neutral import", &imported)
            .await
            .expect("import playlist");
        assert_eq!(result.counts.matched, 1);

        let after = track::Entity::find_by_id("rated-existing")
            .one(&db)
            .await
            .expect("query rated track")
            .expect("rated track remains");
        assert_eq!(after.rating, Some(87));
    }

    #[tokio::test]
    async fn import_entry_failure_rolls_back_playlist_and_prior_entries() {
        let db = in_memory_db().await;
        db.execute_unprepared(
            "CREATE TRIGGER fail_second_import_entry
             BEFORE INSERT ON playlist_entries
             WHEN NEW.position = 1
             BEGIN
                 SELECT RAISE(ABORT, 'injected import failure');
             END",
        )
        .await
        .expect("install failure trigger");
        let manager = PlaylistManager::new(db.clone());
        let imported = vec![
            ImportedTrack {
                title: "One".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                file_path: String::new(),
                duration_secs: None,
            },
            ImportedTrack {
                title: "Two".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                file_path: String::new(),
                duration_secs: None,
            },
        ];

        assert!(manager
            .import_regular_playlist("Must roll back", &imported)
            .await
            .is_err());
        let playlists = playlist::Entity::find()
            .filter(playlist::Column::Name.eq("Must roll back"))
            .all(&db)
            .await
            .expect("query rolled-back playlist");
        assert!(playlists.is_empty());
        assert!(playlist_entry::Entity::find()
            .all(&db)
            .await
            .expect("query rolled-back entries")
            .is_empty());
    }

    #[tokio::test]
    async fn import_propagates_track_table_read_errors_without_creating_a_playlist() {
        let db = in_memory_db().await;
        db.execute_unprepared("DROP TABLE tracks")
            .await
            .expect("drop tracks table");
        let manager = PlaylistManager::new(db.clone());
        let imported = vec![ImportedTrack {
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            album: String::new(),
            file_path: String::new(),
            duration_secs: None,
        }];

        assert!(manager
            .import_regular_playlist("DB failure", &imported)
            .await
            .is_err());
        let playlists = playlist::Entity::find()
            .filter(playlist::Column::Name.eq("DB failure"))
            .all(&db)
            .await
            .expect("query playlists after failed import");
        assert!(playlists.is_empty());
    }

    #[tokio::test]
    async fn legacy_live_updating_false_still_reevaluates_and_is_canonicalized_on_save() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let created = manager
            .create_playlist("Legacy smart playlist", true)
            .await
            .expect("create smart playlist");
        let playlist_id = created.id.clone();

        // Older releases persisted this option in both the rules JSON and a
        // NOT NULL table column. It never changed evaluation semantics: smart
        // playlists are evaluated from the current track table on every load.
        let legacy_json = r#"{
            "match_mode":"All",
            "rules":[{
                "field":"Artist",
                "operator":"Contains",
                "value":{"Text":"Legacy Artist"}
            }],
            "limit":null,
            "live_updating":false,
            "sort_order":[]
        }"#;
        let mut legacy: playlist::ActiveModel = created.into();
        legacy.smart_rules_json = Set(Some(legacy_json.to_string()));
        legacy.live_updating = Set(false);
        legacy.update(&db).await.expect("persist legacy rules");

        insert_track(
            &db,
            "legacy-one",
            "/music/legacy-one.flac",
            "First",
            "Legacy Artist",
            "Album",
            Some(180),
        )
        .await;
        assert_eq!(
            manager
                .evaluate_smart_playlist(&playlist_id)
                .await
                .expect("evaluate legacy rules")
                .len(),
            1
        );

        insert_track(
            &db,
            "legacy-two",
            "/music/legacy-two.flac",
            "Second",
            "Legacy Artist",
            "Album",
            Some(200),
        )
        .await;
        assert_eq!(
            manager
                .evaluate_smart_playlist(&playlist_id)
                .await
                .expect("reevaluate legacy rules against current library")
                .len(),
            2
        );

        let rules = serde_json::from_str(legacy_json).expect("parse legacy rules JSON");
        manager
            .set_smart_rules(&playlist_id, &rules)
            .await
            .expect("save legacy rules");
        let saved = manager
            .get_playlist(&playlist_id)
            .await
            .expect("load saved playlist")
            .expect("saved playlist exists");
        assert!(saved.live_updating);
        assert!(!saved
            .smart_rules_json
            .expect("saved rules JSON")
            .contains("live_updating"));
    }

    #[tokio::test]
    async fn source_scoped_batch_preserves_order_duplicates_and_local_projection() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Mixed storage fixture", false)
            .await
            .expect("create playlist");
        let local = insert_track(
            &db,
            "shared-native-id",
            "/music/local.flac",
            "Local title",
            "Local artist",
            "Local album",
            Some(180),
        )
        .await;
        let remote_source = SourceId::random();
        let other_source = SourceId::random();
        let shared_remote_id = TrackId::remote("shared-native-id").expect("remote track ID");
        let inputs = vec![
            PlaylistEntryInput::new(
                MediaKey::new(remote_source, shared_remote_id.clone()),
                " Remote title ",
                "REMOTE ARTIST",
                "Remote album",
                Some(200),
            ),
            PlaylistEntryInput::local(&local).expect("local playlist input"),
            PlaylistEntryInput::new(
                MediaKey::new(remote_source, shared_remote_id.clone()),
                "Remote title",
                "Remote artist",
                "Remote album",
                Some(200),
            ),
            PlaylistEntryInput::new(
                MediaKey::new(other_source, shared_remote_id.clone()),
                "Other source title",
                "Other source artist",
                "Other source album",
                None,
            ),
        ];

        let inserted = manager
            .add_entries(&playlist.id, &inputs)
            .await
            .expect("append source-scoped entries");
        assert_eq!(
            inserted
                .iter()
                .map(|entry| entry.position)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_ne!(inserted[0].id, inserted[2].id);
        assert_eq!(inserted[0].media_key(), inserted[2].media_key());
        assert_ne!(inserted[0].media_key(), inserted[3].media_key());
        assert_eq!(inserted[0].source_id, remote_source);
        assert_eq!(inserted[0].local_track_id, None);
        assert_eq!(inserted[0].match_title, "remote title");
        assert_eq!(inserted[1].source_id, SourceId::local());
        assert_eq!(
            inserted[1].local_track_id.as_ref().map(TrackId::as_str),
            Some(local.id.as_str())
        );

        let stored = manager
            .get_playlist_entries(&playlist.id)
            .await
            .expect("load typed entries");
        assert_eq!(stored, inserted);
        let projected = manager
            .get_playlist_tracks(&playlist.id)
            .await
            .expect("load local compatibility projection");
        assert_eq!(
            projected
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            vec![local.id.as_str()]
        );

        // A remote occurrence waiting for live-session projection must never
        // be fingerprint-reconciled to a similarly named local row.
        assert_eq!(
            manager.reconcile_all().await.expect("reconcile local only"),
            0
        );
        let after = manager
            .get_playlist_entries(&playlist.id)
            .await
            .expect("reload source-scoped entries");
        assert_eq!(after[0].source_id, remote_source);
        assert_eq!(after[0].track_id.as_ref(), Some(&shared_remote_id));
        assert_eq!(after[0].local_track_id, None);

        manager
            .remove_entries(&playlist.id, &[inserted[0].id.clone()])
            .await
            .expect("remove one duplicate occurrence");
        let remaining = manager
            .get_playlist_entries(&playlist.id)
            .await
            .expect("reload after exact removal");
        assert_eq!(remaining.len(), 3);
        assert_eq!(
            remaining
                .iter()
                .filter(|entry| entry.media_key() == inserted[2].media_key())
                .count(),
            1
        );
        assert_eq!(
            remaining
                .iter()
                .map(|entry| entry.position)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[tokio::test]
    async fn aligned_playlist_load_retains_every_occurrence_and_only_joins_exact_local_rows() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Aligned mixed load", false)
            .await
            .expect("create playlist");
        let local = insert_track(
            &db,
            "shared-native-id",
            "/music/local.flac",
            "Local title",
            "Local artist",
            "Local album",
            Some(180),
        )
        .await;
        let vanished = insert_track(
            &db,
            "vanished-local-id",
            "/music/vanished.flac",
            "Vanished title",
            "Local artist",
            "Local album",
            Some(190),
        )
        .await;
        let first_remote_source = SourceId::random();
        let second_remote_source = SourceId::random();
        let shared_remote_id = TrackId::remote("shared-native-id").expect("remote track ID");
        let inserted = manager
            .add_entries(
                &playlist.id,
                &[
                    PlaylistEntryInput::new(
                        MediaKey::new(first_remote_source, shared_remote_id.clone()),
                        "First remote",
                        "Remote artist",
                        "Remote album",
                        Some(200),
                    ),
                    PlaylistEntryInput::local(&local).expect("local playlist input"),
                    PlaylistEntryInput::new(
                        MediaKey::new(second_remote_source, shared_remote_id),
                        "Second remote",
                        "Remote artist",
                        "Remote album",
                        Some(210),
                    ),
                    PlaylistEntryInput::local(&local).expect("duplicate local playlist input"),
                    PlaylistEntryInput::local(&vanished).expect("vanishing local playlist input"),
                ],
            )
            .await
            .expect("append mixed occurrences");

        track::Entity::delete_by_id(vanished.id.clone())
            .exec(&db)
            .await
            .expect("delete local row and clear its foreign-key cache");

        let loaded = manager
            .load_playlist_entries(&playlist.id)
            .await
            .expect("load aligned occurrences");
        assert_eq!(loaded.len(), inserted.len());
        assert_eq!(
            loaded
                .iter()
                .map(|entry| entry.stored.id.as_str())
                .collect::<Vec<_>>(),
            inserted
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            loaded
                .iter()
                .map(|entry| entry.stored.position)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert_eq!(loaded[0].local_track, None, "remote rows never join local");
        assert_eq!(loaded[2].local_track, None, "source identity isolates IDs");
        assert_eq!(
            loaded[1]
                .local_track
                .as_ref()
                .map(|track| track.id.as_str()),
            Some(local.id.as_str())
        );
        assert_eq!(
            loaded[3]
                .local_track
                .as_ref()
                .map(|track| track.id.as_str()),
            Some(local.id.as_str()),
            "duplicate occurrences remain independently aligned"
        );
        assert_ne!(loaded[1].stored.id, loaded[3].stored.id);
        assert_eq!(loaded[1].stored.media_key(), loaded[3].stored.media_key());
        assert_eq!(loaded[4].local_track, None);
        assert_eq!(
            loaded[4].stored.track_id.as_ref().map(TrackId::as_str),
            Some(vanished.id.as_str()),
            "local deletion preserves durable identity"
        );
        assert_eq!(loaded[4].stored.local_track_id, None);
    }

    #[tokio::test]
    async fn source_scoped_batch_rejects_invalid_input_without_partial_writes_or_id_leaks() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Atomic batch", false)
            .await
            .expect("create playlist");
        let oversized_secret = format!("private-track-id-{}", "x".repeat(4096));
        let remote_input = PlaylistEntryInput::new(
            MediaKey::new(
                SourceId::random(),
                TrackId::new(oversized_secret.clone()).expect("generic bounded track ID"),
            ),
            "Remote",
            "Artist",
            "Album",
            None,
        );
        let error = manager
            .add_entries(&playlist.id, &[remote_input])
            .await
            .expect_err("server-controlled ID ceiling must be enforced");
        assert!(!error.to_string().contains(&oversized_secret));
        assert!(playlist_entries(&db, &playlist.id).await.is_empty());

        let source_id = SourceId::random();
        let valid_first = PlaylistEntryInput::new(
            MediaKey::new(
                source_id,
                TrackId::remote("first-valid").expect("remote ID"),
            ),
            "First",
            "Artist",
            "Album",
            Some(180),
        );
        let invalid_second = PlaylistEntryInput::new(
            MediaKey::new(
                source_id,
                TrackId::remote("second-invalid").expect("remote ID"),
            ),
            "Second",
            "Artist",
            "Album",
            Some(u64::MAX),
        );
        assert!(manager
            .add_entries(&playlist.id, &[valid_first, invalid_second])
            .await
            .is_err());
        assert!(playlist_entries(&db, &playlist.id).await.is_empty());

        let missing_local = PlaylistEntryInput::new(
            MediaKey::new(
                SourceId::local(),
                TrackId::new("missing-private-local-id").expect("local track ID"),
            ),
            "Missing",
            "Artist",
            "Album",
            None,
        );
        let error = manager
            .add_entries(&playlist.id, &[missing_local])
            .await
            .expect_err("missing local FK target must reject the batch");
        assert!(!error.to_string().contains("missing-private-local-id"));
        assert!(playlist_entries(&db, &playlist.id).await.is_empty());

        let smart = manager
            .create_playlist("Smart", true)
            .await
            .expect("create smart playlist");
        let valid_remote = PlaylistEntryInput::new(
            MediaKey::new(
                SourceId::random(),
                TrackId::remote("valid-remote").expect("remote ID"),
            ),
            "Remote",
            "Artist",
            "Album",
            None,
        );
        assert!(manager
            .add_entries(&smart.id, &[valid_remote])
            .await
            .is_err());
        assert!(playlist_entries(&db, &smart.id).await.is_empty());
    }

    #[tokio::test]
    async fn add_uses_checked_positions_and_rolls_back_on_overflow() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Position overflow", false)
            .await
            .expect("create playlist");
        insert_entry(&db, &playlist.id, "maximum-position", i32::MAX).await;
        let track = insert_track(
            &db,
            "overflow-candidate",
            "/music/overflow-candidate.flac",
            "Candidate",
            "Artist",
            "Album",
            Some(180),
        )
        .await;

        assert!(manager.add_track(&playlist.id, &track).await.is_err());
        let entries = playlist_entries(&db, &playlist.id).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "maximum-position");
        assert_eq!(entries[0].position, i32::MAX);
    }

    #[tokio::test]
    async fn reorder_yields_unique_contiguous_positions() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());

        let playlist = manager
            .create_playlist("Test", false)
            .await
            .expect("create playlist");

        // Five entries at the natural 0..4 positions.
        let ids: Vec<String> = (0..5).map(|i| format!("entry-{i}")).collect();
        for (pos, id) in ids.iter().enumerate() {
            insert_entry(&db, &playlist.id, id, pos as i32).await;
        }

        // Moving the last entry to the front would, under a naive sequential
        // update, immediately collide with position 0 against the new UNIQUE
        // index — exercising the two-phase reorder path.
        let new_order = vec![
            ids[4].clone(),
            ids[2].clone(),
            ids[0].clone(),
            ids[3].clone(),
            ids[1].clone(),
        ];
        manager
            .reorder_entries(&playlist.id, &new_order)
            .await
            .expect("reorder must not violate UNIQUE(playlist_id, position)");

        let entries = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(&playlist.id))
            .order_by_asc(playlist_entry::Column::Position)
            .all(&db)
            .await
            .expect("load entries");

        // Positions must be exactly the 0..N permutation: no gaps, no dupes.
        let positions: Vec<i32> = entries.iter().map(|e| e.position).collect();
        assert_eq!(positions, (0..5).collect::<Vec<i32>>());

        // ...and the entries must follow the requested order.
        let ordered_ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ordered_ids, new_order);
    }

    #[tokio::test]
    async fn reorder_and_remove_require_exact_occurrence_ids_and_rollback_invalid_requests() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Exact mutations", false)
            .await
            .expect("create playlist");
        for (position, id) in ["first", "second", "third"].into_iter().enumerate() {
            insert_entry(&db, &playlist.id, id, position as i32).await;
        }
        let original_ids = vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ];

        assert!(manager
            .reorder_entries(
                &playlist.id,
                &[
                    "first".to_string(),
                    "first".to_string(),
                    "third".to_string()
                ],
            )
            .await
            .is_err());
        assert_eq!(
            playlist_entries(&db, &playlist.id)
                .await
                .into_iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            original_ids
        );

        assert!(manager
            .remove_entries(&playlist.id, &["second".to_string(), "second".to_string()])
            .await
            .is_err());
        assert!(manager
            .remove_entries(&playlist.id, &["not-in-this-playlist".to_string()])
            .await
            .is_err());
        assert_eq!(playlist_entries(&db, &playlist.id).await.len(), 3);

        manager
            .remove_entries(&playlist.id, &["second".to_string()])
            .await
            .expect("remove exact occurrence");
        let remaining = playlist_entries(&db, &playlist.id).await;
        assert_eq!(
            remaining
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "third"]
        );
        assert_eq!(
            remaining
                .iter()
                .map(|entry| entry.position)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[tokio::test]
    async fn rename_fallback_relinks_without_changing_playlist_entry_identity() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Rename fallback", false)
            .await
            .expect("create playlist");
        let original = insert_track(
            &db,
            "track-before-rename",
            "/music/before.flac",
            "Example Song",
            "Example Artist",
            "Example Album",
            Some(240),
        )
        .await;
        manager
            .add_track(&playlist.id, &original)
            .await
            .expect("add original track to playlist");
        let before = playlist_entries(&db, &playlist.id)
            .await
            .pop()
            .expect("playlist entry before rename");

        // The current watcher fallback for an unpaired rename is delete plus
        // insert. The FK preserves the playlist entry by nulling its link.
        track::Entity::delete_by_id(&original.id)
            .exec(&db)
            .await
            .expect("delete old track path");
        let orphan = playlist_entries(&db, &playlist.id)
            .await
            .pop()
            .expect("orphaned playlist entry");
        assert_eq!(orphan.track_id.as_deref(), Some(original.id.as_str()));
        assert_eq!(orphan.local_track_id, None);

        let replacement = insert_track(
            &db,
            "track-after-rename",
            "/music/after.flac",
            "Example Song",
            "Example Artist",
            "Example Album",
            Some(240),
        )
        .await;
        assert_eq!(manager.reconcile_all().await.expect("reconcile rename"), 1);

        let after = playlist_entries(&db, &playlist.id)
            .await
            .pop()
            .expect("relinked playlist entry");
        assert_eq!(after.id, before.id);
        assert_eq!(after.playlist_id, before.playlist_id);
        assert_eq!(after.position, before.position);
        assert_eq!(after.match_title, before.match_title);
        assert_eq!(after.match_artist, before.match_artist);
        assert_eq!(after.match_album, before.match_album);
        assert_eq!(after.match_duration_secs, before.match_duration_secs);
        assert_eq!(after.track_id.as_deref(), Some(replacement.id.as_str()));
        assert_eq!(
            after.local_track_id.as_deref(),
            Some(replacement.id.as_str())
        );
        assert_eq!(
            manager
                .reconcile_all()
                .await
                .expect("repeat reconciliation"),
            0
        );
    }

    #[tokio::test]
    async fn manual_entry_does_not_relink_a_different_track_at_a_reused_path() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Reused path", false)
            .await
            .expect("create playlist");
        let original = insert_track(
            &db,
            "original-track",
            "/music/reused.flac",
            "Original Song",
            "Original Artist",
            "Original Album",
            Some(180),
        )
        .await;
        manager
            .add_track(&playlist.id, &original)
            .await
            .expect("add original track to playlist");
        let linked = playlist_entries(&db, &playlist.id)
            .await
            .pop()
            .expect("linked playlist entry");
        assert_eq!(linked.match_file_path, None);

        track::Entity::delete_by_id(&original.id)
            .exec(&db)
            .await
            .expect("delete original track");
        insert_track(
            &db,
            "different-at-original-path",
            "/music/reused.flac",
            "Different Song",
            "Different Artist",
            "Different Album",
            Some(300),
        )
        .await;
        let relocated = insert_track(
            &db,
            "relocated-original",
            "/music/relocated.flac",
            "Original Song",
            "Original Artist",
            "Original Album",
            Some(180),
        )
        .await;

        assert_eq!(
            manager
                .reconcile_all()
                .await
                .expect("reconcile relocated fingerprint"),
            1
        );
        let relinked = playlist_entries(&db, &playlist.id)
            .await
            .pop()
            .expect("relinked playlist entry");
        assert_eq!(relinked.track_id.as_deref(), Some(relocated.id.as_str()));
        assert_eq!(
            relinked.local_track_id.as_deref(),
            Some(relocated.id.as_str())
        );
        assert_eq!(relinked.match_file_path, None);

        track::Entity::delete_by_id(&relocated.id)
            .exec(&db)
            .await
            .expect("delete relocated original");
        insert_track(
            &db,
            "different-at-relocated-path",
            "/music/relocated.flac",
            "Another Song",
            "Another Artist",
            "Another Album",
            Some(180),
        )
        .await;
        assert_eq!(
            manager
                .reconcile_all()
                .await
                .expect("reconcile second reused path"),
            0
        );
        let orphan = playlist_entries(&db, &playlist.id)
            .await
            .pop()
            .expect("preserved orphan");
        assert_eq!(orphan.track_id.as_deref(), Some(relocated.id.as_str()));
        assert_eq!(orphan.local_track_id, None);
        assert_eq!(orphan.match_title, "original song");
        assert_eq!(orphan.match_artist, "original artist");
        assert_eq!(orphan.match_album, "original album");
        assert_eq!(orphan.match_file_path, None);
    }

    #[tokio::test]
    async fn invalid_duration_evidence_is_omitted_and_cannot_block_path_reconciliation() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Invalid duration evidence", false)
            .await
            .expect("create playlist");
        let negative = insert_track(
            &db,
            "negative-duration",
            "/music/negative.flac",
            "Negative",
            "Artist",
            "Album",
            Some(-1),
        )
        .await;
        let overflowing = insert_track(
            &db,
            "overflowing-duration",
            "/music/overflowing.flac",
            "Overflowing",
            "Artist",
            "Album",
            Some(i64::from(i32::MAX) + 1),
        )
        .await;
        manager
            .add_track(&playlist.id, &negative)
            .await
            .expect("add negative-duration track");
        manager
            .add_track(&playlist.id, &overflowing)
            .await
            .expect("add overflowing-duration track");

        let linked = playlist_entries(&db, &playlist.id).await;
        assert_eq!(linked.len(), 2);
        assert!(linked
            .iter()
            .all(|entry| entry.match_duration_secs.is_none()));

        let imported = [ImportedTrack {
            title: overflowing.title.clone(),
            artist: overflowing.artist_name.clone(),
            album: overflowing.album_title.clone(),
            file_path: String::new(),
            duration_secs: Some(i32::MAX as u64),
        }];
        let import_result = manager
            .import_regular_playlist("Out-of-range candidate", &imported)
            .await
            .expect("import against out-of-range library duration");
        assert_eq!(import_result.counts.matched, 0);
        assert_eq!(import_result.counts.unmatched, 1);
        let imported_orphan = playlist_entries(&db, &import_result.playlist.id)
            .await
            .pop()
            .expect("preserved import with valid source duration");
        assert_eq!(imported_orphan.track_id, None);
        assert_eq!(imported_orphan.local_track_id, None);
        assert_eq!(imported_orphan.match_duration_secs, Some(i32::MAX));

        playlist_entry::ActiveModel {
            id: Set("corrupt-duration-orphan".to_string()),
            playlist_id: Set(playlist.id.clone()),
            position: Set(2),
            source_id: Set(SourceId::local().to_string()),
            track_id: Set(None),
            local_track_id: Set(None),
            match_file_path: Set(Some(overflowing.file_path.clone())),
            match_title: Set(String::new()),
            match_artist: Set(String::new()),
            match_album: Set(String::new()),
            match_duration_secs: Set(Some(-1)),
        }
        .insert(&db)
        .await
        .expect("insert orphan with corrupt duration evidence");

        assert_eq!(
            manager
                .reconcile_all()
                .await
                .expect("reconcile despite corrupt optional duration"),
            1
        );
        let reconciled = playlist_entries(&db, &playlist.id)
            .await
            .into_iter()
            .find(|entry| entry.id == "corrupt-duration-orphan")
            .expect("reconciled corrupt-duration entry");
        assert_eq!(
            reconciled.track_id.as_deref(),
            Some(overflowing.id.as_str())
        );
        assert_eq!(
            reconciled.local_track_id.as_deref(),
            Some(overflowing.id.as_str())
        );
        assert_eq!(
            reconciled.match_file_path.as_deref(),
            Some(overflowing.file_path.as_str())
        );
        assert_eq!(reconciled.match_duration_secs, None);
    }

    #[tokio::test]
    async fn full_rebuild_relinks_all_unique_tracks_and_preserves_order() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Full rebuild", false)
            .await
            .expect("create playlist");
        let first = insert_track(
            &db,
            "track-one-old",
            "/music/one-old.flac",
            "Song One",
            "Artist",
            "Album",
            Some(180),
        )
        .await;
        let second = insert_track(
            &db,
            "track-two-old",
            "/music/two-old.flac",
            "Song Two",
            "Artist",
            "Album",
            Some(200),
        )
        .await;
        manager
            .add_track(&playlist.id, &first)
            .await
            .expect("add first track");
        manager
            .add_track(&playlist.id, &second)
            .await
            .expect("add second track");
        let before = playlist_entries(&db, &playlist.id).await;

        track::Entity::delete_many()
            .exec(&db)
            .await
            .expect("clear library for rebuild");
        assert!(playlist_entries(&db, &playlist.id)
            .await
            .iter()
            .all(|entry| entry.local_track_id.is_none()));

        // Insert in reverse order to ensure matching is fingerprint-based,
        // not dependent on table or scan order.
        let second_new = insert_track(
            &db,
            "track-two-new",
            "/music/two-new.flac",
            "Song Two",
            "Artist",
            "Album",
            Some(200),
        )
        .await;
        let first_new = insert_track(
            &db,
            "track-one-new",
            "/music/one-new.flac",
            "Song One",
            "Artist",
            "Album",
            Some(180),
        )
        .await;

        assert_eq!(manager.reconcile_all().await.expect("reconcile rebuild"), 2);
        let after = playlist_entries(&db, &playlist.id).await;
        assert_eq!(
            after.iter().map(|entry| &entry.id).collect::<Vec<_>>(),
            before.iter().map(|entry| &entry.id).collect::<Vec<_>>()
        );
        assert_eq!(
            after.iter().map(|entry| entry.position).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(after[0].track_id.as_deref(), Some(first_new.id.as_str()));
        assert_eq!(after[1].track_id.as_deref(), Some(second_new.id.as_str()));
        assert_eq!(
            after[0].local_track_id.as_deref(),
            Some(first_new.id.as_str())
        );
        assert_eq!(
            after[1].local_track_id.as_deref(),
            Some(second_new.id.as_str())
        );
    }

    #[tokio::test]
    async fn reconciliation_leaves_ambiguous_fingerprint_matches_orphaned() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Ambiguous", false)
            .await
            .expect("create playlist");
        let original = insert_track(
            &db,
            "ambiguous-old",
            "/music/ambiguous-old.flac",
            "Duplicate",
            "Artist",
            "Album",
            Some(210),
        )
        .await;
        manager
            .add_track(&playlist.id, &original)
            .await
            .expect("add original track");
        track::Entity::delete_by_id(&original.id)
            .exec(&db)
            .await
            .expect("delete original track");
        insert_track(
            &db,
            "ambiguous-a",
            "/music/ambiguous-a.flac",
            "Duplicate",
            "Artist",
            "Album",
            Some(209),
        )
        .await;
        insert_track(
            &db,
            "ambiguous-b",
            "/music/ambiguous-b.flac",
            "Duplicate",
            "Artist",
            "Album",
            Some(211),
        )
        .await;

        assert_eq!(
            manager
                .reconcile_all()
                .await
                .expect("reconcile ambiguous entry"),
            0
        );
        let orphan = &playlist_entries(&db, &playlist.id).await[0];
        assert_eq!(orphan.track_id.as_deref(), Some(original.id.as_str()));
        assert_eq!(orphan.local_track_id, None);
    }

    #[tokio::test]
    async fn reconciliation_uses_duration_to_select_the_only_eligible_match() {
        let db = in_memory_db().await;
        let manager = PlaylistManager::new(db.clone());
        let playlist = manager
            .create_playlist("Duration", false)
            .await
            .expect("create playlist");
        let original = insert_track(
            &db,
            "duration-old",
            "/music/duration-old.flac",
            "Same Fingerprint",
            "Artist",
            "Album",
            Some(240),
        )
        .await;
        manager
            .add_track(&playlist.id, &original)
            .await
            .expect("add original track");
        track::Entity::delete_by_id(&original.id)
            .exec(&db)
            .await
            .expect("delete original track");
        let expected = insert_track(
            &db,
            "duration-near",
            "/music/duration-near.flac",
            "Same Fingerprint",
            "Artist",
            "Album",
            Some(242),
        )
        .await;
        insert_track(
            &db,
            "duration-far",
            "/music/duration-far.flac",
            "Same Fingerprint",
            "Artist",
            "Album",
            Some(260),
        )
        .await;

        assert_eq!(
            manager
                .reconcile_all()
                .await
                .expect("reconcile by duration"),
            1
        );
        assert_eq!(
            playlist_entries(&db, &playlist.id).await[0]
                .local_track_id
                .as_deref(),
            Some(expected.id.as_str())
        );
    }
}
