//! `MediaBackend` implementation for Jellyfin servers.
//!
//! Connects to a Jellyfin instance, discovers music libraries, fetches
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

use super::api::{JellyfinItem, JellyfinItemsResponse, JellyfinViewsResponse};
use super::client::JellyfinClient;

// ── Constants ───────────────────────────────────────────────────────────

/// Page size for paginated item fetches.
const PAGE_SIZE: u32 = 500;

// ── Discovery result ────────────────────────────────────────────────────

/// A music library discovered on the Jellyfin server.
#[derive(Debug, Clone)]
pub struct MusicLibrary {
    /// Server-side library ID.
    pub id: String,
    /// Human-readable library name (e.g. "Music", "FLAC Collection").
    pub name: String,
}

// ── In-memory cache ─────────────────────────────────────────────────────

struct LibraryCache {
    tracks: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    track_by_uuid: HashMap<Uuid, usize>,
    /// Jellyfin item ID → UUID we generated.
    jellyfin_id_to_uuid: HashMap<String, Uuid>,
}

impl LibraryCache {
    fn empty() -> Self {
        Self {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            track_by_uuid: HashMap::new(),
            jellyfin_id_to_uuid: HashMap::new(),
        }
    }
}

// ── Backend ─────────────────────────────────────────────────────────────

/// A Jellyfin backend that implements [`MediaBackend`].
///
/// Create one with [`JellyfinBackend::connect`] (API key) or
/// [`JellyfinBackend::from_client`] (pre-authenticated client).
pub struct JellyfinBackend {
    display_name: String,
    client: JellyfinClient,
    music_libraries: Vec<MusicLibrary>,
    cache: RwLock<LibraryCache>,
}

impl JellyfinBackend {
    /// Connect using a pre-existing API key and user ID, then fetch
    /// the full library.
    pub async fn connect(
        name: &str,
        server_url: &str,
        api_key: &str,
        user_id: &str,
    ) -> BackendResult<Self> {
        let client = JellyfinClient::new(server_url, api_key, user_id)?;
        Self::init(name, client).await
    }

    /// Build from a pre-authenticated `JellyfinClient` (e.g. after
    /// interactive login via `JellyfinClient::authenticate`).
    pub async fn from_client(name: &str, client: JellyfinClient) -> BackendResult<Self> {
        Self::init(name, client).await
    }

    /// Shared initialisation: ping, discover, fetch library.
    async fn init(name: &str, client: JellyfinClient) -> BackendResult<Self> {
        client.get_text("System/Ping").await?;
        info!(server = %client.base_url(), "Jellyfin ping OK");

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

    /// Discover music-only libraries.
    pub async fn discover_music_libraries(&self) -> BackendResult<Vec<MusicLibrary>> {
        let endpoint = format!("Users/{}/Views", self.client.user_id());
        let views: JellyfinViewsResponse = self.client.get(&endpoint).await?;

        let music_libs: Vec<MusicLibrary> = views
            .items
            .into_iter()
            .filter(|item| {
                item.collection_type
                    .as_deref()
                    .map(|ct| ct.eq_ignore_ascii_case("music"))
                    .unwrap_or(false)
            })
            .map(|item| MusicLibrary {
                id: item.id,
                name: item.name,
            })
            .collect();

        info!(
            server = %self.display_name,
            total_views = views.total_record_count,
            music_libraries = music_libs.len(),
            "Jellyfin music library discovery complete"
        );

        for lib in &music_libs {
            info!(library_id = %lib.id, library_name = %lib.name, "Found Jellyfin music library");
        }

        Ok(music_libs)
    }

    /// Fetch the entire music library into the in-memory cache.
    async fn refresh_library(&self) -> BackendResult<()> {
        info!("Fetching Jellyfin library...");

        let user_id = self.client.user_id().to_string();
        let items_endpoint = format!("Users/{user_id}/Items");

        let mut all_tracks = Vec::new();
        let mut all_albums = Vec::new();
        let mut all_artists = Vec::new();
        let mut track_by_uuid = HashMap::new();
        let mut jellyfin_id_to_uuid = HashMap::new();

        for lib in &self.music_libraries {
            // ── Fetch tracks ────────────────────────────────────────
            let tracks = self
                .fetch_all_items(
                    &items_endpoint,
                    &lib.id,
                    "Audio",
                    "MediaSources,Genres,UserData,DateCreated",
                )
                .await?;

            for item in &tracks {
                let track_uuid = deterministic_uuid(&item.id);
                let artist_id = item.artist_items.first().map(|a| deterministic_uuid(&a.id));
                let album_id = item.album_id.as_deref().map(deterministic_uuid);

                let stream_url = self.client.stream_url(&item.id);

                let cover_art_url = item
                    .album_id
                    .as_deref()
                    .map(|aid| self.client.image_url(aid));

                let track = jellyfin_item_to_track(
                    item,
                    track_uuid,
                    artist_id,
                    album_id,
                    stream_url,
                    cover_art_url,
                );

                let idx = all_tracks.len();
                track_by_uuid.insert(track_uuid, idx);
                jellyfin_id_to_uuid.insert(item.id.clone(), track_uuid);
                all_tracks.push(track);
            }

            // ── Fetch albums ────────────────────────────────────────
            let albums = self
                .fetch_all_items(&items_endpoint, &lib.id, "MusicAlbum", "Genres")
                .await?;

            for item in &albums {
                let album_uuid = deterministic_uuid(&item.id);
                let artist_id = item.artist_items.first().map(|a| deterministic_uuid(&a.id));

                let cover_art_url = if item
                    .image_tags
                    .as_ref()
                    .and_then(|v| v.get("Primary"))
                    .is_some()
                {
                    Some(self.client.image_url(&item.id))
                } else {
                    None
                };

                all_albums.push(Album {
                    id: album_uuid,
                    title: item.name.clone().unwrap_or_default(),
                    artist_name: item.album_artist.clone().unwrap_or_default(),
                    artist_id,
                    year: item.production_year,
                    genre: item.genres.first().cloned(),
                    cover_art_url,
                    track_count: item.child_count.unwrap_or(0),
                    total_duration_secs: item.run_time_ticks.map(|t| t / 10_000_000),
                });
            }

            // ── Fetch artists ───────────────────────────────────────
            let artists = self
                .fetch_all_items(&items_endpoint, &lib.id, "MusicArtist", "")
                .await?;

            for item in &artists {
                let artist_uuid = deterministic_uuid(&item.id);

                let cover_art_url = if item
                    .image_tags
                    .as_ref()
                    .and_then(|v| v.get("Primary"))
                    .is_some()
                {
                    Some(self.client.image_url(&item.id))
                } else {
                    None
                };

                // Count tracks and albums for this artist from what we've already fetched.
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
                    name: item.name.clone().unwrap_or_default(),
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
            "Jellyfin library loaded"
        );

        let mut cache = self.cache.write().await;
        *cache = LibraryCache {
            tracks: all_tracks,
            albums: all_albums,
            artists: all_artists,
            track_by_uuid,
            jellyfin_id_to_uuid,
        };

        Ok(())
    }

    /// Fetch all items of a given type from a library, handling pagination.
    async fn fetch_all_items(
        &self,
        endpoint: &str,
        parent_id: &str,
        include_item_types: &str,
        fields: &str,
    ) -> BackendResult<Vec<JellyfinItem>> {
        let mut all_items = Vec::new();
        let mut start_index: u32 = 0;

        loop {
            let start_str = start_index.to_string();
            let limit_str = PAGE_SIZE.to_string();

            let mut params = vec![
                ("ParentId", parent_id),
                ("IncludeItemTypes", include_item_types),
                ("Recursive", "true"),
                ("StartIndex", &start_str),
                ("Limit", &limit_str),
            ];

            if !fields.is_empty() {
                params.push(("Fields", fields));
            }

            let resp: JellyfinItemsResponse =
                self.client.get_with_params(endpoint, &params).await?;

            let page_count = resp.items.len() as u32;
            all_items.extend(resp.items);

            if start_index + page_count >= resp.total_record_count {
                break;
            }
            start_index += page_count;
        }

        Ok(all_items)
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
impl crate::architecture::MediaBackend for JellyfinBackend {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn backend_type(&self) -> &str {
        "jellyfin"
    }

    async fn ping(&self) -> BackendResult<()> {
        self.client.get_text("System/Ping").await?;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> BackendResult<SearchResults> {
        let user_id = self.client.user_id().to_string();
        let endpoint = format!("Users/{user_id}/Items");
        let limit_str = limit.to_string();

        let resp: JellyfinItemsResponse = self
            .client
            .get_with_params(
                &endpoint,
                &[
                    ("SearchTerm", query),
                    ("IncludeItemTypes", "Audio,MusicAlbum,MusicArtist"),
                    ("Recursive", "true"),
                    ("Limit", &limit_str),
                    ("Fields", "MediaSources,Genres,UserData"),
                ],
            )
            .await?;

        let mut tracks = Vec::new();
        let mut albums = Vec::new();
        let mut artists = Vec::new();

        for item in &resp.items {
            match item.item_type.as_deref() {
                Some("Audio") => {
                    let uuid = deterministic_uuid(&item.id);
                    let artist_id = item.artist_items.first().map(|a| deterministic_uuid(&a.id));
                    let album_id = item.album_id.as_deref().map(deterministic_uuid);
                    let stream_url = self.client.stream_url(&item.id);
                    let cover_art_url = item
                        .album_id
                        .as_deref()
                        .map(|aid| self.client.image_url(aid));
                    tracks.push(jellyfin_item_to_track(
                        item,
                        uuid,
                        artist_id,
                        album_id,
                        stream_url,
                        cover_art_url,
                    ));
                }
                Some("MusicAlbum") => {
                    let uuid = deterministic_uuid(&item.id);
                    let artist_id = item.artist_items.first().map(|a| deterministic_uuid(&a.id));
                    albums.push(Album {
                        id: uuid,
                        title: item.name.clone().unwrap_or_default(),
                        artist_name: item.album_artist.clone().unwrap_or_default(),
                        artist_id,
                        year: item.production_year,
                        genre: item.genres.first().cloned(),
                        cover_art_url: Some(self.client.image_url(&item.id)),
                        track_count: item.child_count.unwrap_or(0),
                        total_duration_secs: item.run_time_ticks.map(|t| t / 10_000_000),
                    });
                }
                Some("MusicArtist") => {
                    let uuid = deterministic_uuid(&item.id);
                    artists.push(Artist {
                        id: uuid,
                        name: item.name.clone().unwrap_or_default(),
                        album_count: item.album_count.unwrap_or(0),
                        track_count: 0,
                        cover_art_url: Some(self.client.image_url(&item.id)),
                    });
                }
                _ => {}
            }
        }

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

/// Generate a deterministic UUID from a Jellyfin string ID.
fn deterministic_uuid(jellyfin_id: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, jellyfin_id.as_bytes())
}

fn jellyfin_item_to_track(
    item: &JellyfinItem,
    id: Uuid,
    artist_id: Option<Uuid>,
    album_id: Option<Uuid>,
    stream_url: Url,
    cover_art_url: Option<Url>,
) -> Track {
    // Extract bitrate and sample rate from media sources.
    let (bitrate_kbps, sample_rate_hz) = item
        .media_sources
        .first()
        .map(|ms| {
            let bitrate = ms.bitrate.map(|b| b / 1000); // bps → kbps
            let sample_rate = ms
                .media_streams
                .iter()
                .find(|s| s.stream_type.as_deref() == Some("Audio"))
                .and_then(|s| s.sample_rate);
            (bitrate, sample_rate)
        })
        .unwrap_or((None, None));

    Track {
        id,
        title: item.name.clone().unwrap_or_else(|| "Unknown".into()),
        artist_name: item
            .artist_items
            .first()
            .map(|a| a.name.clone())
            .or_else(|| item.album_artist.clone())
            .unwrap_or_else(|| "Unknown".into()),
        album_artist_name: item.album_artist.clone(),
        artist_id,
        album_title: item.album.clone().unwrap_or_default(),
        album_id,
        track_number: item.index_number,
        disc_number: item.parent_index_number,
        duration_secs: item.run_time_ticks.map(|t| t / 10_000_000),
        genre: item.genres.first().cloned(),
        year: item.production_year,
        file_path: None,
        stream_url: Some(stream_url),
        cover_art_url,
        date_added: None,
        date_modified: item.date_created.as_deref().and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        }),
        bitrate_kbps,
        sample_rate_hz,
        format: item.container.clone(),
        play_count: item.user_data.as_ref().and_then(|ud| ud.play_count),
    }
}
