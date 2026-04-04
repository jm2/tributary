//! `MediaBackend` implementation for Plex servers.
//!
//! Connects to a Plex instance, discovers music libraries, fetches
//! the full track/album/artist catalogue into an in-memory cache, and
//! exposes it through the unified `MediaBackend` trait.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::info;
use url::Url;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::models::*;

use super::api::{
    PlexAlbumsResponse, PlexArtistsResponse, PlexIdentityResponse, PlexSectionsResponse, PlexTrack,
    PlexTracksResponse,
};
use super::client::PlexClient;

// ── Discovery result ────────────────────────────────────────────────────

/// A music library discovered on the Plex server.
#[derive(Debug, Clone)]
pub struct MusicLibrary {
    /// Numeric section key used in subsequent API calls.
    pub key: String,
    /// Human-readable library name (e.g. "Music", "Vinyl Rips").
    pub name: String,
    /// Server-assigned UUID for this section, if available.
    pub uuid: Option<String>,
}

// ── In-memory cache ─────────────────────────────────────────────────────

struct LibraryCache {
    tracks: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    track_by_uuid: HashMap<Uuid, usize>,
    /// Plex rating key → UUID we generated.
    #[allow(dead_code)]
    plex_id_to_uuid: HashMap<String, Uuid>,
}

impl LibraryCache {
    fn empty() -> Self {
        Self {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            track_by_uuid: HashMap::new(),
            plex_id_to_uuid: HashMap::new(),
        }
    }
}

// ── Backend ─────────────────────────────────────────────────────────────

/// A Plex backend that implements [`MediaBackend`].
///
/// Create one with [`PlexBackend::connect`] (token) or
/// [`PlexBackend::from_client`] (pre-authenticated client).
pub struct PlexBackend {
    display_name: String,
    client: PlexClient,
    music_libraries: Vec<MusicLibrary>,
    cache: RwLock<LibraryCache>,
}

impl PlexBackend {
    /// Connect using a pre-existing auth token, then fetch the full library.
    pub async fn connect(name: &str, server_url: &str, auth_token: &str) -> BackendResult<Self> {
        let client = PlexClient::new(server_url, auth_token)?;
        Self::init(name, client).await
    }

    /// Build from a pre-authenticated `PlexClient` (e.g. after
    /// interactive login via `PlexClient::authenticate`).
    pub async fn from_client(name: &str, client: PlexClient) -> BackendResult<Self> {
        Self::init(name, client).await
    }

    /// Shared initialisation: identity check, discover, fetch library.
    async fn init(name: &str, client: PlexClient) -> BackendResult<Self> {
        let identity: PlexIdentityResponse = client.get("identity").await?;
        info!(
            server = %client.base_url(),
            version = ?identity.media_container.version,
            machine_id = ?identity.media_container.machine_identifier,
            "Plex identity OK"
        );

        let mut backend = Self {
            display_name: name.to_string(),
            client,
            music_libraries: Vec::new(),
            cache: RwLock::new(LibraryCache::empty()),
        };

        backend.music_libraries = backend.discover_music_libraries().await?;
        backend.refresh_library().await?;

        Ok(backend)
    }

    /// Discover music-only library sections.
    pub async fn discover_music_libraries(&self) -> BackendResult<Vec<MusicLibrary>> {
        let sections: PlexSectionsResponse = self.client.get("library/sections").await?;

        let music_libs: Vec<MusicLibrary> = sections
            .media_container
            .directory
            .into_iter()
            .filter(|dir| dir.section_type.eq_ignore_ascii_case("artist"))
            .map(|dir| MusicLibrary {
                key: dir.key,
                name: dir.title,
                uuid: dir.uuid,
            })
            .collect();

        info!(
            server = %self.display_name,
            total_sections = sections.media_container.size,
            music_libraries = music_libs.len(),
            "Plex music library discovery complete"
        );

        for lib in &music_libs {
            info!(
                section_key = %lib.key,
                library_name = %lib.name,
                "Found Plex music library"
            );
        }

        Ok(music_libs)
    }

    /// Fetch the entire music library into the in-memory cache.
    async fn refresh_library(&self) -> BackendResult<()> {
        info!("Fetching Plex library...");

        let mut all_tracks = Vec::new();
        let mut all_albums = Vec::new();
        let mut all_artists = Vec::new();
        let mut track_by_uuid = HashMap::new();
        let mut plex_id_to_uuid = HashMap::new();

        for lib in &self.music_libraries {
            let section_endpoint = format!("library/sections/{}/all", lib.key);

            // ── Fetch tracks (type=10) ──────────────────────────────
            let tracks_resp: PlexTracksResponse = self
                .client
                .get_with_params(&section_endpoint, &[("type", "10")])
                .await?;

            for plex_track in &tracks_resp.media_container.metadata {
                let track_uuid = deterministic_uuid(&plex_track.rating_key);
                let artist_id = plex_track
                    .grandparent_rating_key
                    .as_deref()
                    .map(deterministic_uuid);
                let album_id = plex_track
                    .parent_rating_key
                    .as_deref()
                    .map(deterministic_uuid);

                let track =
                    plex_track_to_track(plex_track, track_uuid, artist_id, album_id, &self.client);

                let idx = all_tracks.len();
                track_by_uuid.insert(track_uuid, idx);
                plex_id_to_uuid.insert(plex_track.rating_key.clone(), track_uuid);
                all_tracks.push(track);
            }

            // ── Fetch albums (type=9) ───────────────────────────────
            let albums_resp: PlexAlbumsResponse = self
                .client
                .get_with_params(&section_endpoint, &[("type", "9")])
                .await?;

            for plex_album in &albums_resp.media_container.metadata {
                let album_uuid = deterministic_uuid(&plex_album.rating_key);
                let artist_id = plex_album
                    .parent_rating_key
                    .as_deref()
                    .map(deterministic_uuid);

                let cover_art_url = plex_album
                    .thumb
                    .as_deref()
                    .map(|t| self.client.thumb_url(t));

                let genre = plex_album.genre.first().and_then(|g| g.tag.clone());

                all_albums.push(Album {
                    id: album_uuid,
                    title: plex_album.title.clone().unwrap_or_default(),
                    artist_name: plex_album.parent_title.clone().unwrap_or_default(),
                    artist_id,
                    year: plex_album.year,
                    genre,
                    cover_art_url,
                    track_count: plex_album.leaf_count.unwrap_or(0),
                    total_duration_secs: plex_album.duration.map(|d| d / 1000),
                });
            }

            // ── Fetch artists (type=8) ──────────────────────────────
            let artists_resp: PlexArtistsResponse = self
                .client
                .get_with_params(&section_endpoint, &[("type", "8")])
                .await?;

            for plex_artist in &artists_resp.media_container.metadata {
                let artist_uuid = deterministic_uuid(&plex_artist.rating_key);

                let cover_art_url = plex_artist
                    .thumb
                    .as_deref()
                    .map(|t| self.client.thumb_url(t));

                // Count tracks and albums for this artist.
                let track_count = all_tracks
                    .iter()
                    .filter(|t| t.artist_id.as_ref() == Some(&artist_uuid))
                    .count() as u32;
                let album_count = all_albums
                    .iter()
                    .filter(|a| a.artist_id.as_ref() == Some(&artist_uuid))
                    .count() as u32;

                all_artists.push(Artist {
                    id: artist_uuid,
                    name: plex_artist.title.clone().unwrap_or_default(),
                    album_count,
                    track_count,
                    cover_art_url,
                });
            }
        }

        info!(
            artists = all_artists.len(),
            albums = all_albums.len(),
            tracks = all_tracks.len(),
            "Plex library loaded"
        );

        let mut cache = self.cache.write().await;
        *cache = LibraryCache {
            tracks: all_tracks,
            albums: all_albums,
            artists: all_artists,
            track_by_uuid,
            plex_id_to_uuid,
        };

        Ok(())
    }

    /// Return all tracks from the cache (for UI integration layer).
    pub async fn all_tracks(&self) -> Vec<Track> {
        self.cache.read().await.tracks.clone()
    }

    /// Return the music libraries discovered during init.
    pub fn music_libraries(&self) -> &[MusicLibrary] {
        &self.music_libraries
    }
}

// ── MediaBackend trait implementation ────────────────────────────────────

#[async_trait]
impl crate::architecture::MediaBackend for PlexBackend {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn backend_type(&self) -> &str {
        "plex"
    }

    async fn ping(&self) -> BackendResult<()> {
        let _: PlexIdentityResponse = self.client.get("identity").await?;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> BackendResult<SearchResults> {
        // Plex search: filter the in-memory cache (the /hubs/search
        // endpoint is complex and not all Plex servers support it well).
        let cache = self.cache.read().await;
        let query_lower = query.to_lowercase();

        let tracks: Vec<Track> = cache
            .tracks
            .iter()
            .filter(|t| {
                t.title.to_lowercase().contains(&query_lower)
                    || t.artist_name.to_lowercase().contains(&query_lower)
                    || t.album_title.to_lowercase().contains(&query_lower)
            })
            .take(limit)
            .cloned()
            .collect();

        let albums: Vec<Album> = cache
            .albums
            .iter()
            .filter(|a| {
                a.title.to_lowercase().contains(&query_lower)
                    || a.artist_name.to_lowercase().contains(&query_lower)
            })
            .take(limit)
            .cloned()
            .collect();

        let artists: Vec<Artist> = cache
            .artists
            .iter()
            .filter(|a| a.name.to_lowercase().contains(&query_lower))
            .take(limit)
            .cloned()
            .collect();

        Ok(SearchResults {
            tracks,
            albums,
            artists,
        })
    }

    async fn list_albums(&self, sort: SortField, order: SortOrder) -> BackendResult<Vec<Album>> {
        let cache = self.cache.read().await;
        let mut albums = cache.albums.clone();

        albums.sort_by(|a, b| {
            let cmp = match sort {
                SortField::Title => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
                SortField::Artist => a
                    .artist_name
                    .to_lowercase()
                    .cmp(&b.artist_name.to_lowercase()),
                SortField::Year => a.year.cmp(&b.year),
                _ => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
            };
            match order {
                SortOrder::Ascending => cmp,
                SortOrder::Descending => cmp.reverse(),
            }
        });

        Ok(albums)
    }

    async fn list_artists(&self) -> BackendResult<Vec<Artist>> {
        Ok(self.cache.read().await.artists.clone())
    }

    async fn get_album_tracks(&self, album_id: &Uuid) -> BackendResult<Vec<Track>> {
        let cache = self.cache.read().await;
        Ok(cache
            .tracks
            .iter()
            .filter(|t| t.album_id.as_ref() == Some(album_id))
            .cloned()
            .collect())
    }

    async fn get_artist_tracks(&self, artist_id: &Uuid) -> BackendResult<Vec<Track>> {
        let cache = self.cache.read().await;
        Ok(cache
            .tracks
            .iter()
            .filter(|t| t.artist_id.as_ref() == Some(artist_id))
            .cloned()
            .collect())
    }

    async fn get_stream_url(&self, track_id: &Uuid) -> BackendResult<Url> {
        let cache = self.cache.read().await;
        let idx = cache
            .track_by_uuid
            .get(track_id)
            .ok_or_else(|| BackendError::NotFound {
                entity_type: "track".into(),
                id: *track_id,
            })?;
        let track = &cache.tracks[*idx];
        track.stream_url.clone().ok_or_else(|| {
            BackendError::Internal(anyhow::anyhow!("Track {} has no stream URL", track_id))
        })
    }

    async fn get_cover_art(&self, album_id: &Uuid) -> BackendResult<Option<Url>> {
        let cache = self.cache.read().await;
        let album = cache.albums.iter().find(|a| a.id == *album_id);
        Ok(album.and_then(|a| a.cover_art_url.clone()))
    }

    async fn get_stats(&self) -> BackendResult<LibraryStats> {
        let cache = self.cache.read().await;
        let total_duration: u64 = cache.tracks.iter().filter_map(|t| t.duration_secs).sum();

        Ok(LibraryStats {
            total_tracks: cache.tracks.len() as u64,
            total_albums: cache.albums.len() as u64,
            total_artists: cache.artists.len() as u64,
            total_duration_secs: total_duration,
        })
    }
}

// ── Conversion helpers ──────────────────────────────────────────────────

/// Generate a deterministic UUID from a Plex rating key.
fn deterministic_uuid(plex_id: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, plex_id.as_bytes())
}

fn plex_track_to_track(
    plex: &PlexTrack,
    id: Uuid,
    artist_id: Option<Uuid>,
    album_id: Option<Uuid>,
    client: &PlexClient,
) -> Track {
    // Extract stream URL from the first media part.
    let stream_url = plex
        .media
        .first()
        .and_then(|m| m.part.first())
        .and_then(|p| p.key.as_deref())
        .map(|key| client.stream_url(key));

    let cover_art_url = plex.thumb.as_deref().map(|t| client.thumb_url(t));

    let bitrate_kbps = plex.media.first().and_then(|m| m.bitrate);
    let format = plex
        .media
        .first()
        .and_then(|m| m.audio_codec.clone().or_else(|| m.container.clone()));

    Track {
        id,
        title: plex.title.clone().unwrap_or_else(|| "Unknown".into()),
        artist_name: plex
            .grandparent_title
            .clone()
            .unwrap_or_else(|| "Unknown".into()),
        artist_id,
        album_title: plex.parent_title.clone().unwrap_or_default(),
        album_id,
        track_number: plex.index,
        disc_number: plex.parent_index,
        duration_secs: plex.duration.map(|d| d / 1000), // ms → s
        genre: None, // Plex tracks don't carry genre directly; albums do.
        year: plex.year,
        file_path: None,
        stream_url,
        cover_art_url,
        date_added: None,
        date_modified: plex
            .updated_at
            .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0)),
        bitrate_kbps,
        sample_rate_hz: None, // Not available in Plex track metadata.
        format,
        play_count: plex.view_count,
    }
}
