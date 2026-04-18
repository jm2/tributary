//! `LocalBackend` — `MediaBackend` implementation for the local SQLite library.

use async_trait::async_trait;
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter};
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
            .all(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?
            .iter()
            .take(limit)
            .map(db_model_to_track)
            .collect();

        Ok(SearchResults {
            tracks,
            albums: vec![],
            artists: vec![],
        })
    }

    async fn list_albums(&self, sort: SortField, order: SortOrder) -> BackendResult<Vec<Album>> {
        // Derive albums from track data using GROUP BY equivalent
        let all_tracks = track::Entity::find()
            .all(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?;

        let mut album_map = std::collections::BTreeMap::<String, Album>::new();
        for row in &all_tracks {
            let entry = album_map
                .entry(row.album_title.clone())
                .or_insert_with(|| Album {
                    id: Uuid::new_v4(),
                    title: row.album_title.clone(),
                    artist_name: row.artist_name.clone(),
                    artist_id: None,
                    year: row.year,
                    genre: row.genre.clone(),
                    cover_art_url: None,
                    track_count: 0,
                    total_duration_secs: Some(0),
                });
            entry.track_count += 1;
            if let Some(dur) = row.duration_secs {
                *entry.total_duration_secs.as_mut().unwrap() += dur as u64;
            }
        }

        let mut albums: Vec<Album> = album_map.into_values().collect();

        // Sort
        match sort {
            SortField::Title => albums.sort_by(|a, b| a.title.cmp(&b.title)),
            SortField::Artist => albums.sort_by(|a, b| a.artist_name.cmp(&b.artist_name)),
            SortField::Year => albums.sort_by(|a, b| a.year.cmp(&b.year)),
            _ => albums.sort_by(|a, b| a.title.cmp(&b.title)),
        }

        if matches!(order, SortOrder::Descending) {
            albums.reverse();
        }

        Ok(albums)
    }

    async fn list_artists(&self) -> BackendResult<Vec<Artist>> {
        let all_tracks = track::Entity::find()
            .all(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?;

        let mut artist_map = std::collections::BTreeMap::<String, Artist>::new();
        for row in &all_tracks {
            let entry = artist_map
                .entry(row.artist_name.clone())
                .or_insert_with(|| Artist {
                    id: Uuid::new_v4(),
                    name: row.artist_name.clone(),
                    album_count: 0,
                    track_count: 0,
                    cover_art_url: None,
                });
            entry.track_count += 1;
        }

        // Compute album counts
        let mut album_sets: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();
        for row in &all_tracks {
            album_sets
                .entry(row.artist_name.clone())
                .or_default()
                .insert(row.album_title.clone());
        }
        for (name, artist) in &mut artist_map {
            if let Some(albums) = album_sets.get(name) {
                artist.album_count = albums.len() as u32;
            }
        }

        Ok(artist_map.into_values().collect())
    }

    async fn get_album_tracks(&self, _album_id: &Uuid) -> BackendResult<Vec<Track>> {
        // For now, album_id isn't stored in DB.  This will be wired up
        // when we have a proper album registry; for now return empty.
        Ok(vec![])
    }

    async fn get_artist_tracks(&self, _artist_id: &Uuid) -> BackendResult<Vec<Track>> {
        Ok(vec![])
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

        Url::parse(&format!("file://{}", row.file_path))
            .map_err(|e| BackendError::Internal(e.into()))
    }

    async fn get_cover_art(&self, _album_id: &Uuid) -> BackendResult<Option<Url>> {
        // Cover art is extracted on-the-fly from embedded tags in window.rs
        // (update_album_art / extract_album_art_bytes) rather than through
        // this trait method.  Returns None — no separate cover art URL.
        Ok(None)
    }

    async fn get_stats(&self) -> BackendResult<LibraryStats> {
        let all = track::Entity::find()
            .all(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?;

        let total_tracks = all.len() as u64;
        let total_duration_secs: u64 = all
            .iter()
            .filter_map(|t| t.duration_secs)
            .map(|d| d as u64)
            .sum();

        let albums: std::collections::HashSet<&str> =
            all.iter().map(|t| t.album_title.as_str()).collect();
        let artists: std::collections::HashSet<&str> =
            all.iter().map(|t| t.artist_name.as_str()).collect();

        Ok(LibraryStats {
            total_tracks,
            total_albums: albums.len() as u64,
            total_artists: artists.len() as u64,
            total_duration_secs,
        })
    }
}
