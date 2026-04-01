//! `MediaBackend` implementation for Subsonic-compatible servers.
//!
//! All metadata is held in memory — nothing touches the local SQLite DB.
//! The full library is fetched during [`SubsonicBackend::connect`] and
//! cached for fast browsing.  Streaming URLs include authentication
//! tokens so GStreamer can fetch audio directly.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::info;
use url::Url;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::models::*;

use super::api::{AlbumEntry, ArtistEntry, SongEntry};
use super::client::SubsonicClient;

/// In-memory library cache populated from the Subsonic API.
struct LibraryCache {
    tracks: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    /// Subsonic song ID → index in `tracks`.
    track_by_uuid: HashMap<Uuid, usize>,
    /// Subsonic song ID (string) → Uuid we generated.
    #[allow(dead_code)]
    subsonic_id_to_uuid: HashMap<String, Uuid>,
}

impl LibraryCache {
    fn empty() -> Self {
        Self {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            track_by_uuid: HashMap::new(),
            subsonic_id_to_uuid: HashMap::new(),
        }
    }
}

/// A Subsonic/Navidrome/Airsonic backend that implements [`MediaBackend`].
///
/// Create one with [`SubsonicBackend::connect`], which authenticates and
/// fetches the full library into memory.
pub struct SubsonicBackend {
    display_name: String,
    client: SubsonicClient,
    cache: RwLock<LibraryCache>,
}

impl SubsonicBackend {
    /// Connect to a Subsonic server, authenticate, and fetch the full
    /// library into memory.
    ///
    /// # Arguments
    /// * `name` — display name for the sidebar (e.g. "Navidrome (home)")
    /// * `server_url` — base URL including scheme (e.g. `https://music.example.com`)
    /// * `username` / `password` — Subsonic credentials
    pub async fn connect(
        name: &str,
        server_url: &str,
        username: &str,
        password: &str,
    ) -> BackendResult<Self> {
        let client = SubsonicClient::new(server_url, username, password)?;

        // Verify connectivity.
        client.get("ping.view").await?;
        info!(server = %server_url, "Subsonic ping OK");

        let backend = Self {
            display_name: name.to_string(),
            client,
            cache: RwLock::new(LibraryCache::empty()),
        };

        backend.refresh_library().await?;

        Ok(backend)
    }

    /// Fetch the entire library from the server into the in-memory cache.
    async fn refresh_library(&self) -> BackendResult<()> {
        info!("Fetching Subsonic library...");

        // ── Artists ─────────────────────────────────────────────────
        let artists_resp = self.client.get("getArtists.view").await?;
        let api_artists: Vec<ArtistEntry> = artists_resp
            .response
            .artists
            .map(|w| w.index.into_iter().flat_map(|i| i.artist).collect())
            .unwrap_or_default();

        // ── Walk each artist → albums → songs ───────────────────────
        let mut all_tracks = Vec::new();
        let mut all_albums = Vec::new();
        let mut all_artists = Vec::new();
        let mut track_by_uuid = HashMap::new();
        let mut subsonic_id_to_uuid = HashMap::new();

        for api_artist in &api_artists {
            let artist_uuid = deterministic_uuid(&api_artist.id);

            // getArtist gives us albums for this artist.
            let artist_resp = self
                .client
                .get_with_params("getArtist.view", &[("id", &api_artist.id)])
                .await;

            let api_albums = match artist_resp {
                Ok(env) => env.response.artist.map(|a| a.album).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(
                        artist = %api_artist.name,
                        error = %e,
                        "Failed to fetch artist detail, skipping"
                    );
                    continue;
                }
            };

            let mut artist_track_count = 0u32;

            for api_album in &api_albums {
                let album_uuid = deterministic_uuid(&api_album.id);

                // getAlbum gives us songs.
                let album_resp = self
                    .client
                    .get_with_params("getAlbum.view", &[("id", &api_album.id)])
                    .await;

                let songs = match album_resp {
                    Ok(env) => env.response.album.map(|a| a.song).unwrap_or_default(),
                    Err(e) => {
                        tracing::warn!(
                            album = %api_album.name,
                            error = %e,
                            "Failed to fetch album detail, skipping"
                        );
                        continue;
                    }
                };

                for song in &songs {
                    let track_uuid = deterministic_uuid(&song.id);
                    let stream_url = self.client.stream_url(&song.id);

                    let track = song_to_track(
                        song,
                        track_uuid,
                        Some(artist_uuid),
                        Some(album_uuid),
                        stream_url,
                        song.cover_art
                            .as_deref()
                            .map(|id| self.client.cover_art_url(id)),
                    );

                    let idx = all_tracks.len();
                    track_by_uuid.insert(track_uuid, idx);
                    subsonic_id_to_uuid.insert(song.id.clone(), track_uuid);
                    all_tracks.push(track);
                }

                artist_track_count += songs.len() as u32;

                all_albums.push(album_entry_to_album(
                    api_album,
                    album_uuid,
                    Some(artist_uuid),
                    api_album
                        .cover_art
                        .as_deref()
                        .map(|id| self.client.cover_art_url(id)),
                ));
            }

            all_artists.push(Artist {
                id: artist_uuid,
                name: api_artist.name.clone(),
                album_count: api_albums.len() as u32,
                track_count: artist_track_count,
                cover_art_url: api_artist
                    .cover_art
                    .as_deref()
                    .map(|id| self.client.cover_art_url(id)),
            });
        }

        info!(
            artists = all_artists.len(),
            albums = all_albums.len(),
            tracks = all_tracks.len(),
            "Subsonic library loaded"
        );

        let mut cache = self.cache.write().await;
        *cache = LibraryCache {
            tracks: all_tracks,
            albums: all_albums,
            artists: all_artists,
            track_by_uuid,
            subsonic_id_to_uuid,
        };

        Ok(())
    }

    /// Return all tracks from the cache as Tributary `Track` models.
    /// Used by the integration layer to send a FullSync to the UI.
    pub async fn all_tracks(&self) -> Vec<Track> {
        self.cache.read().await.tracks.clone()
    }
}

// ── MediaBackend trait implementation ────────────────────────────────────

#[async_trait]
impl crate::architecture::MediaBackend for SubsonicBackend {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn backend_type(&self) -> &str {
        "subsonic"
    }

    async fn ping(&self) -> BackendResult<()> {
        self.client.get("ping.view").await?;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> BackendResult<SearchResults> {
        let limit_str = limit.to_string();
        let env = self
            .client
            .get_with_params(
                "search3.view",
                &[
                    ("query", query),
                    ("artistCount", &limit_str),
                    ("albumCount", &limit_str),
                    ("songCount", &limit_str),
                ],
            )
            .await?;

        let sr = env
            .response
            .search_result3
            .unwrap_or_else(|| super::api::SearchResult3 {
                artist: Vec::new(),
                album: Vec::new(),
                song: Vec::new(),
            });

        Ok(SearchResults {
            artists: sr
                .artist
                .iter()
                .map(|a| Artist {
                    id: deterministic_uuid(&a.id),
                    name: a.name.clone(),
                    album_count: a.album_count.unwrap_or(0),
                    track_count: 0,
                    cover_art_url: a
                        .cover_art
                        .as_deref()
                        .map(|id| self.client.cover_art_url(id)),
                })
                .collect(),
            albums: sr
                .album
                .iter()
                .map(|a| {
                    album_entry_to_album(
                        a,
                        deterministic_uuid(&a.id),
                        a.artist_id.as_deref().map(deterministic_uuid),
                        a.cover_art
                            .as_deref()
                            .map(|id| self.client.cover_art_url(id)),
                    )
                })
                .collect(),
            tracks: sr
                .song
                .iter()
                .map(|s| {
                    let uuid = deterministic_uuid(&s.id);
                    song_to_track(
                        s,
                        uuid,
                        s.artist_id.as_deref().map(deterministic_uuid),
                        s.album_id.as_deref().map(deterministic_uuid),
                        self.client.stream_url(&s.id),
                        s.cover_art
                            .as_deref()
                            .map(|id| self.client.cover_art_url(id)),
                    )
                })
                .collect(),
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

/// Generate a deterministic UUID from a Subsonic string ID.
/// This ensures the same Subsonic entity always maps to the same UUID
/// across sessions without needing persistent storage.
fn deterministic_uuid(subsonic_id: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, subsonic_id.as_bytes())
}

fn song_to_track(
    song: &SongEntry,
    id: Uuid,
    artist_id: Option<Uuid>,
    album_id: Option<Uuid>,
    stream_url: Url,
    cover_art_url: Option<Url>,
) -> Track {
    Track {
        id,
        title: song.title.clone().unwrap_or_else(|| "Unknown".into()),
        artist_name: song.artist.clone().unwrap_or_else(|| "Unknown".into()),
        artist_id,
        album_title: song.album.clone().unwrap_or_default(),
        album_id,
        track_number: song.track,
        disc_number: song.disc_number,
        duration_secs: song.duration,
        genre: song.genre.clone(),
        year: song.year,
        file_path: None, // Remote — no local file
        stream_url: Some(stream_url),
        cover_art_url,
        date_added: None,
        date_modified: None,
        bitrate_kbps: song.bit_rate,
        sample_rate_hz: None,
        format: song.suffix.clone(),
        play_count: song.play_count,
    }
}

fn album_entry_to_album(
    entry: &AlbumEntry,
    id: Uuid,
    artist_id: Option<Uuid>,
    cover_art_url: Option<Url>,
) -> Album {
    Album {
        id,
        title: entry.name.clone(),
        artist_name: entry.artist.clone().unwrap_or_default(),
        artist_id,
        year: entry.year,
        genre: entry.genre.clone(),
        cover_art_url,
        track_count: entry.song_count.unwrap_or(0),
        total_duration_secs: entry.duration,
    }
}
