//! `LocalBackend` — `MediaBackend` implementation for the local SQLite library.

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, Func};
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter,
    QueryOrder, QuerySelect,
};
use url::Url;
use uuid::Uuid;

use crate::architecture::backend::{BackendResult, MediaBackend};
use crate::architecture::error::BackendError;
use crate::architecture::models::*;
use crate::db::entities::track;

use super::engine::db_model_to_track;

/// Local filesystem backend backed by SQLite.
pub struct LocalBackend {
    db: DatabaseConnection,
}

impl LocalBackend {
    /// Create a new local backend with the given database connection.
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

/// One row of the `list_albums` GROUP BY aggregate.
///
/// The bare `artist_name`/`year`/`genre` columns take a representative value
/// from each `album_title` group (SQLite's documented "bare column" behaviour),
/// matching the previous Rust fold which kept the first row encountered.
#[derive(FromQueryResult)]
struct AlbumAgg {
    album_title: String,
    artist_name: String,
    year: Option<i32>,
    genre: Option<String>,
    track_count: i64,
    total_duration_secs: Option<i64>,
}

/// One row of the `list_artists` GROUP BY aggregate.
#[derive(FromQueryResult)]
struct ArtistAgg {
    artist_name: String,
    track_count: i64,
    album_count: i64,
}

/// The single row produced by the `get_stats` aggregate query.
#[derive(FromQueryResult, Default)]
struct StatsAgg {
    total_tracks: i64,
    total_duration_secs: Option<i64>,
    total_albums: i64,
    total_artists: i64,
}

#[async_trait]
impl MediaBackend for LocalBackend {
    fn name(&self) -> &str {
        "Local Filesystem"
    }

    fn backend_type(&self) -> &str {
        "local"
    }

    async fn ping(&self) -> BackendResult<()> {
        // Simple connectivity check: try to count tracks
        track::Entity::find()
            .one(&self.db)
            .await
            .map(|_| ())
            .map_err(|e| BackendError::ConnectionFailed {
                message: e.to_string(),
                source: Some(Box::new(e)),
            })
    }

    async fn search(&self, query: &str, limit: usize) -> BackendResult<SearchResults> {
        let tracks: Vec<Track> = track::Entity::find()
            .filter(
                Condition::any()
                    .add(track::Column::Title.contains(query))
                    .add(track::Column::ArtistName.contains(query))
                    .add(track::Column::AlbumTitle.contains(query)),
            )
            // Bound the result set in SQL rather than materialising every
            // matching row and truncating in Rust.
            .limit(limit as u64)
            .all(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?
            .iter()
            .map(db_model_to_track)
            .collect();

        Ok(SearchResults {
            tracks,
            albums: vec![],
            artists: vec![],
        })
    }

    async fn list_albums(&self, sort: SortField, order: SortOrder) -> BackendResult<Vec<Album>> {
        // Derive albums by letting SQLite GROUP BY album_title and aggregate,
        // rather than loading the whole tracks table and folding in Rust.
        let rows = track::Entity::find()
            .select_only()
            .column(track::Column::AlbumTitle)
            .column(track::Column::ArtistName)
            .column(track::Column::Year)
            .column(track::Column::Genre)
            .column_as(track::Column::Id.count(), "track_count")
            .column_as(track::Column::DurationSecs.sum(), "total_duration_secs")
            .group_by(track::Column::AlbumTitle)
            // Reproduce the previous BTreeMap<album_title, _> iteration order so
            // the stable sort below breaks ties identically.
            .order_by_asc(track::Column::AlbumTitle)
            .into_model::<AlbumAgg>()
            .all(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?;

        let mut albums: Vec<Album> = rows
            .into_iter()
            .map(|r| Album {
                id: Uuid::new_v4(),
                title: r.album_title,
                artist_name: r.artist_name,
                artist_id: None,
                year: r.year,
                genre: r.genre,
                cover_art_url: None,
                track_count: r.track_count as u32,
                // The previous fold seeded the total at Some(0) and added each
                // non-null duration; SQL SUM is NULL for an all-null album, so
                // coalesce back to 0 to keep the Some(0) semantics.
                total_duration_secs: Some(r.total_duration_secs.unwrap_or(0) as u64),
            })
            .collect();

        // Sort
        match sort {
            SortField::Title => albums.sort_by(|a, b| a.title.cmp(&b.title)),
            SortField::Artist => albums.sort_by(|a, b| a.artist_name.cmp(&b.artist_name)),
            SortField::Year => albums.sort_by_key(|a| a.year),
            _ => albums.sort_by(|a, b| a.title.cmp(&b.title)),
        }

        if matches!(order, SortOrder::Descending) {
            albums.reverse();
        }

        Ok(albums)
    }

    async fn list_artists(&self) -> BackendResult<Vec<Artist>> {
        // Aggregate per-artist tallies in SQLite: COUNT(*) tracks and
        // COUNT(DISTINCT album_title) albums, replicating the previous two-pass
        // Rust fold (track tally + per-artist album HashSet) exactly.
        let rows = track::Entity::find()
            .select_only()
            .column(track::Column::ArtistName)
            .column_as(track::Column::Id.count(), "track_count")
            .column_as(
                Expr::expr(Func::count_distinct(Expr::col(track::Column::AlbumTitle))),
                "album_count",
            )
            .group_by(track::Column::ArtistName)
            // Preserve the previous BTreeMap<artist_name, _> ascending order.
            .order_by_asc(track::Column::ArtistName)
            .into_model::<ArtistAgg>()
            .all(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?;

        Ok(rows
            .into_iter()
            .map(|r| Artist {
                id: Uuid::new_v4(),
                name: r.artist_name,
                album_count: r.album_count as u32,
                track_count: r.track_count as u32,
                cover_art_url: None,
            })
            .collect())
    }

    async fn get_album_tracks(&self, _album_id: &Uuid) -> BackendResult<Vec<Track>> {
        // The local backend derives albums from track rows on the fly
        // and assigns ephemeral UUIDs in `list_albums()`. Until album
        // identity is persisted, lookup-by-album-id has no stable key
        // to query against; callers should filter the local track list
        // by album_title themselves rather than rely on this method.
        Err(BackendError::Unsupported {
            operation: "LocalBackend::get_album_tracks (album IDs are not persisted)".into(),
        })
    }

    async fn get_artist_tracks(&self, _artist_id: &Uuid) -> BackendResult<Vec<Track>> {
        // Same shape as get_album_tracks above: artist IDs are
        // synthesised per-call and not persisted, so a stable
        // by-id lookup is not implementable today.
        Err(BackendError::Unsupported {
            operation: "LocalBackend::get_artist_tracks (artist IDs are not persisted)".into(),
        })
    }

    async fn get_stream_url(&self, track_id: &Uuid) -> BackendResult<Url> {
        let id_str = track_id.to_string();
        let row = track::Entity::find_by_id(&id_str)
            .one(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?
            .ok_or_else(|| BackendError::NotFound {
                entity_type: "Track".to_string(),
                id: *track_id,
            })?;

        // Build the file URL via from_file_path so reserved characters in
        // the path ('#', '?', spaces, …) are percent-encoded correctly,
        // rather than string-concatenating a "file://…" URL that mis-parses.
        Url::from_file_path(&row.file_path).map_err(|()| {
            BackendError::Internal(anyhow::anyhow!(
                "Invalid file path for stream URL: {}",
                row.file_path
            ))
        })
    }

    async fn get_cover_art(&self, _album_id: &Uuid) -> BackendResult<Option<Url>> {
        // Cover art is extracted on-the-fly from embedded tags in window.rs
        // (update_album_art / extract_album_art_bytes) rather than through
        // this trait method.  Returns None — no separate cover art URL.
        Ok(None)
    }

    async fn get_stats(&self) -> BackendResult<LibraryStats> {
        // Compute every statistic in a single aggregate query instead of
        // materialising the whole tracks table and folding/HashSet-ing in Rust.
        // An aggregate query with no GROUP BY always yields exactly one row;
        // `unwrap_or_default` is purely defensive.
        let stats = track::Entity::find()
            .select_only()
            .column_as(track::Column::Id.count(), "total_tracks")
            .column_as(track::Column::DurationSecs.sum(), "total_duration_secs")
            .column_as(
                Expr::expr(Func::count_distinct(Expr::col(track::Column::AlbumTitle))),
                "total_albums",
            )
            .column_as(
                Expr::expr(Func::count_distinct(Expr::col(track::Column::ArtistName))),
                "total_artists",
            )
            .into_model::<StatsAgg>()
            .one(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?
            .unwrap_or_default();

        Ok(LibraryStats {
            total_tracks: stats.total_tracks as u64,
            total_albums: stats.total_albums as u64,
            total_artists: stats.total_artists as u64,
            // SUM is NULL on an empty table; the previous fold summed to 0.
            total_duration_secs: stats.total_duration_secs.unwrap_or(0) as u64,
        })
    }
}
