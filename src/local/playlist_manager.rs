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
            match_title: Set(track.title.to_lowercase().trim().to_string()),
            match_artist: Set(track.artist_name.to_lowercase().trim().to_string()),
            match_album: Set(track.album_title.to_lowercase().trim().to_string()),
            match_duration_secs: Set(track.duration_secs.map(|d| d as i32)),
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
                live_updating: true,
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

        // Load the track table once and index it by normalized
        // (title, artist, album) fingerprint, so each orphan is resolved with
        // an in-memory lookup instead of a full-table-scan SQL query per
        // orphan (the SQL `lower(col)` predicate couldn't use the column
        // indexes). The keys are lowercased AND trimmed to match how the
        // `match_*` fields are stored in `add_track`.
        let all_tracks = track::Entity::find().all(&self.db).await?;
        let mut index: HashMap<(String, String, String), Vec<&track::Model>> = HashMap::new();
        for t in &all_tracks {
            let key = (
                t.title.to_lowercase().trim().to_string(),
                t.artist_name.to_lowercase().trim().to_string(),
                t.album_title.to_lowercase().trim().to_string(),
            );
            index.entry(key).or_default().push(t);
        }

        let mut relinked = 0u32;

        for orphan in orphans {
            // `match_*` are already lowercased + trimmed at insert time.
            let key = (
                orphan.match_title.clone(),
                orphan.match_artist.clone(),
                orphan.match_album.clone(),
            );
            let Some(candidates) = index.get(&key) else {
                continue;
            };

            // If duration was recorded, require a candidate within ±2s
            // (preserving the previous matching semantics); otherwise take
            // the first fingerprint match.
            let best = if let Some(dur) = orphan.match_duration_secs {
                candidates
                    .iter()
                    .find(|t| {
                        t.duration_secs
                            .is_some_and(|d| (d - i64::from(dur)).abs() <= 2)
                    })
                    .copied()
            } else {
                candidates.first().copied()
            };

            if let Some(best) = best {
                let track_id = best.id.clone();
                let mut entry: playlist_entry::ActiveModel = orphan.into();
                entry.track_id = Set(Some(track_id));
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
            live_updating: true,
            sort_order: vec![smart_rules::SortCriterion {
                field: smart_rules::SortField::DateAdded,
                direction: smart_rules::SortDirection::Descending,
            }],
        };
        let pl = self.create_playlist("Recently Added", true).await?;
        self.set_smart_rules(&pl.id, &rules_recently_added).await?;
        info!(id = %pl.id, "Seeded: Recently Added");
        created.push(pl);

        // 2. Recently Played — Date Modified in last 14 days AND Play Count > 0
        let rules_recently_played = smart_rules::SmartRules {
            match_mode: smart_rules::MatchMode::All,
            rules: vec![
                smart_rules::SmartRule {
                    field: smart_rules::RuleField::DateModified,
                    operator: smart_rules::RuleOperator::IsInTheLast {
                        amount: 14,
                        unit: smart_rules::DateUnit::Days,
                    },
                    value: smart_rules::RuleValue::Number(14),
                },
                smart_rules::SmartRule {
                    field: smart_rules::RuleField::PlayCount,
                    operator: smart_rules::RuleOperator::GreaterThan,
                    value: smart_rules::RuleValue::Number(0),
                },
            ],
            limit: None,
            live_updating: true,
            sort_order: vec![smart_rules::SortCriterion {
                field: smart_rules::SortField::DateModified,
                direction: smart_rules::SortDirection::Descending,
            }],
        };
        let pl = self.create_playlist("Recently Played", true).await?;
        self.set_smart_rules(&pl.id, &rules_recently_played).await?;
        info!(id = %pl.id, "Seeded: Recently Played");
        created.push(pl);

        // 3. Top 25 Most Played — Play Count > 0, limit 25, sort by Most Played
        let rules_top25 = smart_rules::SmartRules {
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
            live_updating: true,
            sort_order: vec![],
        };
        let pl = self.create_playlist("Top 25 Most Played", true).await?;
        self.set_smart_rules(&pl.id, &rules_top25).await?;
        info!(id = %pl.id, "Seeded: Top 25 Most Played");
        created.push(pl);

        info!(count = created.len(), "Default smart playlists seeded");
        Ok(created)
    }
}

/// Get current time as RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ColumnTrait, Database, DatabaseConnection, EntityTrait,
        QueryFilter, QueryOrder,
    };
    use sea_orm_migration::MigratorTrait;

    use super::PlaylistManager;
    use crate::db::entities::playlist_entry;
    use crate::db::migration::Migrator;

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
            match_title: Set(String::new()),
            match_artist: Set(String::new()),
            match_album: Set(String::new()),
            match_duration_secs: Set(None),
        }
        .insert(db)
        .await
        .expect("insert entry");
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
}
