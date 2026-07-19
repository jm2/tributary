//! Playlist manager — CRUD operations and track reconciliation.
//!
//! Manages regular and smart playlists. Regular playlists store track
//! references with fingerprint data for rediscovery after library rebuilds.
//! Smart playlists store rule configurations and evaluate dynamically.

use std::collections::HashMap;

use sea_orm::prelude::*;
use sea_orm::{ActiveValue::Set, QueryOrder, TransactionTrait};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::playlist_io::{ImportedTrack, ImportedTrackMatchIndex};
use super::smart_rules::{self, SmartRules};
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
                track_id: Set(track_id),
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
        // Wrap the next-position read and the insert in a transaction so the
        // two statements form a single atomic unit. (Fully preventing two
        // concurrent adds from claiming the same position would additionally
        // require a UNIQUE(playlist_id, position) index — see migrations.)
        let txn = self.db.begin().await?;

        // Get next position.
        let max_pos = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .order_by_desc(playlist_entry::Column::Position)
            .one(&txn)
            .await?
            .map(|e| e.position)
            .unwrap_or(-1);

        let entry = playlist_entry::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            playlist_id: Set(playlist_id.to_string()),
            position: Set(max_pos + 1),
            track_id: Set(Some(track.id.clone())),
            // A path is authoritative only when an imported playlist supplied
            // it as durable location evidence. A manually added entry follows
            // the song fingerprint after deletion/rename; otherwise a
            // different file later scanned at the reused path could silently
            // replace the user's original choice.
            match_file_path: Set(None),
            match_title: Set(track.title.to_lowercase().trim().to_string()),
            match_artist: Set(track.artist_name.to_lowercase().trim().to_string()),
            match_album: Set(track.album_title.to_lowercase().trim().to_string()),
            match_duration_secs: Set(valid_track_match_duration(track)),
        };
        entry.insert(&txn).await?;
        txn.commit().await?;
        debug!(playlist = %playlist_id, track = %track.title, "Track added to playlist");
        Ok(())
    }

    /// Remove an entry from a playlist.
    pub async fn remove_entry(&self, entry_id: &str) -> Result<(), DbErr> {
        playlist_entry::Entity::delete_by_id(entry_id.to_string())
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Reorder entries in a playlist. `entry_ids` is the new order.
    pub async fn reorder_entries(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), DbErr> {
        // The `UNIQUE(playlist_id, position)` index makes naive sequential
        // updates collide: assigning an entry its final position while another
        // entry still holds it is a transient duplicate the index rejects.
        //
        // Two phases inside one transaction avoid that. Phase 1 parks every
        // affected entry in a high, non-overlapping range; phase 2 assigns the
        // final `0..N` positions. Phase-1 values (>= `TEMP_OFFSET`) never
        // overlap phase-2 targets, so no statement ever produces a duplicate.
        // The whole thing is transactional, so a mid-way failure rolls back
        // cleanly instead of leaving a mix of old and new positions.
        const TEMP_OFFSET: i32 = 1_000_000;

        let txn = self.db.begin().await?;

        // Phase 1: park each entry at `index + TEMP_OFFSET`.
        for (pos, entry_id) in entry_ids.iter().enumerate() {
            let mut entry: playlist_entry::ActiveModel =
                playlist_entry::Entity::find_by_id(entry_id.clone())
                    .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
                    .one(&txn)
                    .await?
                    .ok_or(DbErr::RecordNotFound(format!("Entry {entry_id} not found")))?
                    .into();

            entry.position = Set(pos as i32 + TEMP_OFFSET);
            entry.update(&txn).await?;
        }

        // Phase 2: assign the final 0..N positions.
        for (pos, entry_id) in entry_ids.iter().enumerate() {
            let mut entry: playlist_entry::ActiveModel =
                playlist_entry::Entity::find_by_id(entry_id.clone())
                    .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
                    .one(&txn)
                    .await?
                    .ok_or(DbErr::RecordNotFound(format!("Entry {entry_id} not found")))?
                    .into();

            entry.position = Set(pos as i32);
            entry.update(&txn).await?;
        }

        txn.commit().await?;
        Ok(())
    }

    /// Get all matched tracks for a regular playlist (ordered by position).
    ///
    /// Returns only entries that have a valid `track_id` link. Unmatched
    /// entries (orphans from a library rebuild) are excluded.
    pub async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<track::Model>, DbErr> {
        let entries = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .filter(playlist_entry::Column::TrackId.is_not_null())
            .order_by_asc(playlist_entry::Column::Position)
            .all(&self.db)
            .await?;

        // Collect the linked track IDs in playlist order.
        let track_ids: Vec<String> = entries.iter().filter_map(|e| e.track_id.clone()).collect();
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
    /// `track_id IS NULL` and attempts to match them against current
    /// tracks by exact retained path first, then by a normalized
    /// `(title, artist, album)` fingerprint with optional duration tolerance.
    ///
    /// Returns the number of entries re-linked.
    pub async fn reconcile_all(&self) -> Result<u32, DbErr> {
        let orphans = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::TrackId.is_null())
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
                let track_id = best.id.clone();
                let match_title = normalize_fingerprint(&best.title);
                let match_artist = normalize_fingerprint(&best.artist_name);
                let match_album = normalize_fingerprint(&best.album_title);
                let match_duration_secs = valid_track_match_duration(best);
                let mut entry: playlist_entry::ActiveModel = orphan.into();
                entry.track_id = Set(Some(track_id));
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

    use super::{recently_played_default_rules, top_25_most_played_default_rules, PlaylistManager};
    use crate::db::entities::{playlist, playlist_entry, track};
    use crate::db::migration::Migrator;
    use crate::local::playlist_io::ImportedTrack;

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
            track_id: Set(None),
            match_file_path: Set(None),
            match_title: Set(String::new()),
            match_artist: Set(String::new()),
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
        assert_eq!(before[0].track_id.as_deref(), Some(existing.id.as_str()));
        assert_eq!(
            before[0].match_file_path.as_deref(),
            Some(existing.file_path.as_str())
        );
        assert_eq!(before[0].match_title, "canonical title");
        assert_eq!(before[1].position, 1);
        assert_eq!(before[1].track_id.as_deref(), Some(existing.id.as_str()));
        assert_eq!(before[1].match_file_path, None);
        assert_eq!(before[2].position, 2);
        assert_eq!(before[2].track_id, None);
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
        assert_eq!(after[2].match_title, "metadata can differ");
        assert_eq!(after[2].match_artist, "path wins");
        assert_eq!(after[2].match_album, "album");
        assert_eq!(after[2].match_duration_secs, Some(200));
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
        assert_eq!(orphan.track_id, None);

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
        assert_eq!(orphan.track_id, None);
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
        assert_eq!(imported_orphan.match_duration_secs, Some(i32::MAX));

        playlist_entry::ActiveModel {
            id: Set("corrupt-duration-orphan".to_string()),
            playlist_id: Set(playlist.id.clone()),
            position: Set(2),
            track_id: Set(None),
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
            .all(|entry| entry.track_id.is_none()));

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
        assert_eq!(playlist_entries(&db, &playlist.id).await[0].track_id, None);
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
                .track_id
                .as_deref(),
            Some(expected.id.as_str())
        );
    }
}
