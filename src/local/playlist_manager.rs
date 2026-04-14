//! Playlist manager — CRUD operations and track reconciliation.
//!
//! Manages regular and smart playlists. Regular playlists store track
//! references with fingerprint data for rediscovery after library rebuilds.
//! Smart playlists store rule configurations and evaluate dynamically.

use sea_orm::prelude::*;
use sea_orm::{ActiveValue::Set, Condition, QueryOrder};
use tracing::{debug, info};
use uuid::Uuid;

use super::smart_rules::{self, SmartRules};
use crate::db::entities::{playlist, playlist_entry, track};

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
        // Get next position.
        let max_pos = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .order_by_desc(playlist_entry::Column::Position)
            .one(&self.db)
            .await?
            .map(|e| e.position)
            .unwrap_or(-1);

        let entry = playlist_entry::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            playlist_id: Set(playlist_id.to_string()),
            position: Set(max_pos + 1),
            track_id: Set(Some(track.id.clone())),
            match_title: Set(track.title.to_lowercase().trim().to_string()),
            match_artist: Set(track.artist_name.to_lowercase().trim().to_string()),
            match_album: Set(track.album_title.to_lowercase().trim().to_string()),
            match_duration_secs: Set(track.duration_secs.map(|d| d as i32)),
        };
        entry.insert(&self.db).await?;
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
        for (pos, entry_id) in entry_ids.iter().enumerate() {
            let mut entry: playlist_entry::ActiveModel =
                playlist_entry::Entity::find_by_id(entry_id.clone())
                    .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
                    .one(&self.db)
                    .await?
                    .ok_or(DbErr::RecordNotFound(format!("Entry {entry_id} not found")))?
                    .into();

            entry.position = Set(pos as i32);
            entry.update(&self.db).await?;
        }
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

        let mut tracks = Vec::new();
        for entry in entries {
            if let Some(track_id) = &entry.track_id {
                if let Some(t) = track::Entity::find_by_id(track_id.clone())
                    .one(&self.db)
                    .await?
                {
                    tracks.push(t);
                }
            }
        }
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
        model.live_updating = Set(rules.live_updating);

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

        let rules_json = playlist.smart_rules_json.as_deref().unwrap_or("{}");
        let rules: SmartRules = serde_json::from_str(rules_json).unwrap_or(SmartRules {
            match_mode: smart_rules::MatchMode::All,
            rules: Vec::new(),
            limit: None,
            live_updating: true,
        });

        let all_tracks = track::Entity::find().all(&self.db).await?;
        let results = smart_rules::evaluate(&rules, &all_tracks);
        Ok(results)
    }

    // ── Track reconciliation ─────────────────────────────────────────

    /// Re-link orphaned playlist entries to newly-discovered tracks.
    ///
    /// Called after a library rebuild (FullSync). Finds entries with
    /// `track_id IS NULL` and attempts to match them against current
    /// tracks by `(title, artist, album)` fingerprint with optional
    /// duration tolerance (±2 seconds).
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

        let mut relinked = 0u32;

        for orphan in orphans {
            // Find candidate tracks matching the fingerprint.
            let mut condition = Condition::all()
                .add(
                    sea_orm::sea_query::Expr::expr(sea_orm::sea_query::Func::lower(
                        sea_orm::sea_query::Expr::col(track::Column::Title),
                    ))
                    .eq(&orphan.match_title),
                )
                .add(
                    sea_orm::sea_query::Expr::expr(sea_orm::sea_query::Func::lower(
                        sea_orm::sea_query::Expr::col(track::Column::ArtistName),
                    ))
                    .eq(&orphan.match_artist),
                )
                .add(
                    sea_orm::sea_query::Expr::expr(sea_orm::sea_query::Func::lower(
                        sea_orm::sea_query::Expr::col(track::Column::AlbumTitle),
                    ))
                    .eq(&orphan.match_album),
                );

            // If duration is available, allow ±2 second tolerance.
            if let Some(dur) = orphan.match_duration_secs {
                condition = condition
                    .add(track::Column::DurationSecs.gte((dur - 2) as i64))
                    .add(track::Column::DurationSecs.lte((dur + 2) as i64));
            }

            let candidates = track::Entity::find()
                .filter(condition)
                .all(&self.db)
                .await?;

            if let Some(best) = candidates.first() {
                let mut entry: playlist_entry::ActiveModel = orphan.into();
                entry.track_id = Set(Some(best.id.clone()));
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
}

/// Get current time as RFC3339 string.
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO date for now — we don't have chrono.
    format!("{secs}")
}
