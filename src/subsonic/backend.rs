//! `MediaBackend` implementation for Subsonic-compatible servers.
//!
//! All metadata is held in memory — nothing touches the local SQLite DB.
//! The full library is fetched during [`SubsonicBackend::connect`] and
//! cached for fast browsing. Credentials remain in the retained backend and
//! are resolved into proxy-only requests at playback time.

use std::collections::HashMap;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::models::*;
use crate::architecture::{AdvertisedHttpRoute, RemoteMediaResolver, ResolvedHttpRequest};

use super::api::{AlbumEntry, ArtistEntry, SongEntry};
use super::client::SubsonicClient;

/// Maximum number of per-artist / per-album metadata fetches kept in
/// flight at once while loading the full library.  Bounds concurrency so a
/// large library does not open hundreds of simultaneous connections, while
/// still overlapping request latency for a large speed-up over the old
/// fully-sequential walk.
const FETCH_CONCURRENCY: usize = 8;

/// In-memory library cache populated from the Subsonic API.
#[allow(dead_code)]
struct LibraryCache {
    tracks: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    /// Application track UUID → Subsonic song ID.
    stream_locator_by_uuid: HashMap<Uuid, String>,
    /// Application track UUID → Subsonic cover-art ID.
    track_artwork_locator_by_uuid: HashMap<Uuid, String>,
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
            stream_locator_by_uuid: HashMap::new(),
            track_artwork_locator_by_uuid: HashMap::new(),
            subsonic_id_to_uuid: HashMap::new(),
        }
    }
}

/// A Subsonic/Navidrome/Airsonic backend that implements [`MediaBackend`].
///
/// Create one with [`SubsonicBackend::connect`], which authenticates and
/// fetches the full library into memory.
#[allow(dead_code)]
pub struct SubsonicBackend {
    display_name: String,
    client: SubsonicClient,
    cache: RwLock<LibraryCache>,
}

impl SubsonicBackend {
    /// Connect to a Subsonic server, authenticate, and fetch the full
    /// library into memory.
    ///
    /// Authentication strategy:
    /// 1. Try **token auth** first (`t=md5(password+salt)` + `s=salt`).
    /// 2. If the server returns error code **41** ("token auth not
    ///    supported" — e.g. Nextcloud Music), automatically retry with
    ///    **hex-encoded plaintext** auth (`p=enc:<hex>`).
    /// 3. The plaintext fallback is **refused over plain HTTP** — only
    ///    HTTPS connections are permitted for this mode.
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
        Self::connect_with_route(name, server_url, username, password, None).await
    }

    /// Connect through an immutable address route supplied by discovery.
    pub async fn connect_with_route(
        name: &str,
        server_url: &str,
        username: &str,
        password: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
        let mut client = match advertised_route {
            Some(route) => {
                SubsonicClient::new_with_route(server_url, username, password, Some(route))?
            }
            None => SubsonicClient::new(server_url, username, password)?,
        };

        // Try token auth first (modern, recommended).
        match client.get("ping.view").await {
            Ok(_) => {
                info!(server = %server_url, "Subsonic ping OK (token auth)");
            }
            Err(BackendError::TokenAuthNotSupported { message }) => {
                // Server doesn't support token auth — fall back to
                // hex-encoded plaintext, but only over HTTPS.
                info!(
                    server = %server_url,
                    reason = %message,
                    "Token auth rejected, falling back to hex-encoded plaintext"
                );
                client.switch_to_plaintext_auth()?;
                client.get("ping.view").await?;
                info!(server = %server_url, "Subsonic ping OK (plaintext auth)");
            }
            Err(e) => return Err(e),
        }

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
        //
        // The library is fetched with *bounded concurrency* instead of a
        // fully serialized N+1 walk: per-artist `getArtist` calls run in
        // one bounded stream, then every album's `getAlbum` call runs in a
        // second bounded stream (at most `FETCH_CONCURRENCY` requests in
        // flight per phase).  Results come back unordered, are restored to
        // the original artist/album order, then assembled deterministically
        // so the resulting cache is identical to the old sequential walk.
        // Per-item failures keep the original log-and-skip semantics: a
        // failed `getArtist` drops that artist entirely, a failed `getAlbum`
        // drops just that album (the artist's `album_count` still counts it).

        // Phase 1 — fetch each artist's album list concurrently. Each future
        // owns a cheap `SubsonicClient` clone (reqwest's `Client` is
        // `Arc`-backed) and the artist id/name, so it borrows neither `self`
        // nor `api_artists` — keeping the spawned `refresh_library` future
        // `Send + 'static`.
        let client = self.client.clone();
        let artist_reqs: Vec<(usize, String, String)> = api_artists
            .iter()
            .enumerate()
            .map(|(idx, a)| (idx, a.id.clone(), a.name.clone()))
            .collect();
        let mut artist_albums: Vec<(usize, Option<Vec<AlbumEntry>>)> =
            futures::stream::iter(artist_reqs)
                .map(|(idx, id, name)| {
                    let client = client.clone();
                    async move {
                        let albums = match client
                            .get_with_params("getArtist.view", &[("id", &id)])
                            .await
                        {
                            Ok(env) => {
                                Some(env.response.artist.map(|a| a.album).unwrap_or_default())
                            }
                            Err(e) => {
                                tracing::warn!(
                                    artist = %name,
                                    error = %e,
                                    "Failed to fetch artist detail, skipping"
                                );
                                None
                            }
                        };
                        (idx, albums)
                    }
                })
                .buffer_unordered(FETCH_CONCURRENCY)
                .collect()
                .await;
        // Restore the original artist order — `buffer_unordered` yields as
        // each fetch completes, so positions now line up with `api_artists`.
        artist_albums.sort_by_key(|(idx, _)| *idx);

        // Phase 2 — fetch the songs for every album concurrently. Build an
        // owned (artist-pos, album-pos, id, name) descriptor list first so the
        // futures own their data (same `Send + 'static` reasoning as phase 1).
        let album_reqs: Vec<(usize, usize, String, String)> = artist_albums
            .iter()
            .enumerate()
            .filter_map(|(ai, (_, albums))| albums.as_ref().map(|al| (ai, al)))
            .flat_map(|(ai, albums)| {
                albums
                    .iter()
                    .enumerate()
                    .map(move |(bi, album)| (ai, bi, album.id.clone(), album.name.clone()))
            })
            .collect();
        let album_songs: Vec<(usize, usize, Option<Vec<SongEntry>>)> =
            futures::stream::iter(album_reqs)
                .map(|(ai, bi, id, name)| {
                    let client = client.clone();
                    async move {
                        let songs = match client
                            .get_with_params("getAlbum.view", &[("id", &id)])
                            .await
                        {
                            Ok(env) => Some(env.response.album.map(|a| a.song).unwrap_or_default()),
                            Err(e) => {
                                tracing::warn!(
                                    album = %name,
                                    error = %e,
                                    "Failed to fetch album detail, skipping"
                                );
                                None
                            }
                        };
                        (ai, bi, songs)
                    }
                })
                .buffer_unordered(FETCH_CONCURRENCY)
                .collect()
                .await;

        // Index the fetched songs by (artist position, album position).
        // Albums whose `getAlbum` failed are absent here and so are skipped
        // during assembly below.
        let mut songs_by_album: HashMap<(usize, usize), Vec<SongEntry>> = HashMap::new();
        for (ai, bi, songs) in album_songs {
            if let Some(songs) = songs {
                songs_by_album.insert((ai, bi), songs);
            }
        }

        // Phase 3 — assemble the cache deterministically in artist/album
        // order, mirroring the original sequential walk exactly.
        let mut all_tracks = Vec::new();
        let mut all_albums = Vec::new();
        let mut all_artists = Vec::new();
        let mut stream_locator_by_uuid = HashMap::new();
        let mut track_artwork_locator_by_uuid = HashMap::new();
        let mut subsonic_id_to_uuid = HashMap::new();

        for (ai, (_, albums)) in artist_albums.iter().enumerate() {
            // A failed `getArtist` drops the artist entirely.
            let Some(api_albums) = albums.as_ref() else {
                continue;
            };
            let api_artist = &api_artists[ai];
            let artist_uuid = deterministic_uuid(&api_artist.id);

            let mut artist_track_count = 0u32;

            for (bi, api_album) in api_albums.iter().enumerate() {
                // A failed `getAlbum` drops just this album.
                let Some(songs) = songs_by_album.get(&(ai, bi)) else {
                    continue;
                };
                let album_uuid = deterministic_uuid(&api_album.id);

                for song in songs {
                    let track_uuid = deterministic_uuid(&song.id);
                    let track =
                        song_to_track(song, track_uuid, Some(artist_uuid), Some(album_uuid));

                    stream_locator_by_uuid.insert(track_uuid, song.id.clone());
                    if let Some(cover_art_id) = &song.cover_art {
                        track_artwork_locator_by_uuid.insert(track_uuid, cover_art_id.clone());
                    }
                    subsonic_id_to_uuid.insert(song.id.clone(), track_uuid);
                    all_tracks.push(track);
                }

                artist_track_count += songs.len() as u32;

                all_albums.push(album_entry_to_album(
                    api_album,
                    album_uuid,
                    Some(artist_uuid),
                ));
            }

            all_artists.push(Artist {
                id: artist_uuid,
                name: api_artist.name.clone(),
                album_count: api_albums.len() as u32,
                track_count: artist_track_count,
                cover_art_url: None,
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
            stream_locator_by_uuid,
            track_artwork_locator_by_uuid,
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

        let results = SearchResults {
            artists: sr
                .artist
                .iter()
                .map(|a| Artist {
                    id: deterministic_uuid(&a.id),
                    name: a.name.clone(),
                    album_count: a.album_count.unwrap_or(0),
                    track_count: 0,
                    cover_art_url: None,
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
                    )
                })
                .collect(),
        };

        // Search results may include entities outside the initially loaded
        // catalogue. Retain their native locators before exposing the generic
        // models so selecting one can still resolve at playback time.
        let mut cache = self.cache.write().await;
        for song in &sr.song {
            let uuid = deterministic_uuid(&song.id);
            cache.stream_locator_by_uuid.insert(uuid, song.id.clone());
            if let Some(cover_art_id) = &song.cover_art {
                cache
                    .track_artwork_locator_by_uuid
                    .insert(uuid, cover_art_id.clone());
            } else {
                cache.track_artwork_locator_by_uuid.remove(&uuid);
            }
        }

        Ok(results)
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

#[async_trait]
impl RemoteMediaResolver for SubsonicBackend {
    async fn resolve_stream(&self, track_id: &Uuid) -> BackendResult<ResolvedHttpRequest> {
        let song_id = self
            .cache
            .read()
            .await
            .stream_locator_by_uuid
            .get(track_id)
            .cloned()
            .ok_or_else(|| BackendError::NotFound {
                entity_type: "track".into(),
                id: *track_id,
            })?;
        self.client.resolved_stream_request(&song_id)
    }

    async fn resolve_artwork(&self, track_id: &Uuid) -> BackendResult<Option<ResolvedHttpRequest>> {
        let cover_art_id = self
            .cache
            .read()
            .await
            .track_artwork_locator_by_uuid
            .get(track_id)
            .cloned();
        cover_art_id
            .as_deref()
            .map(|id| self.client.resolved_artwork_request(id))
            .transpose()
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
) -> Track {
    Track {
        id,
        title: song.title.clone().unwrap_or_else(|| "Unknown".into()),
        artist_name: song.artist.clone().unwrap_or_else(|| "Unknown".into()),
        album_artist_name: None,
        artist_id,
        album_title: song.album.clone().unwrap_or_default(),
        album_id,
        track_number: song.track,
        disc_number: song.disc_number,
        duration_secs: song.duration,
        composer: song
            .display_composer
            .as_ref()
            .or(song.composer.as_ref())
            .cloned(),
        genre: song.genre.clone(),
        year: song.year,
        file_path: None, // Remote — no local file
        stream_url: None,
        cover_art_url: None,
        date_added: None,
        date_modified: None,
        bitrate_kbps: song.bit_rate,
        sample_rate_hz: None,
        format: song.suffix.clone(),
        play_count: song.play_count,
    }
}

fn album_entry_to_album(entry: &AlbumEntry, id: Uuid, artist_id: Option<Uuid>) -> Album {
    Album {
        id,
        title: entry.name.clone(),
        artist_name: entry.artist.clone().unwrap_or_default(),
        artist_id,
        year: entry.year,
        genre: entry.genre.clone(),
        cover_art_url: None,
        track_count: entry.song_count.unwrap_or(0),
        total_duration_secs: entry.duration,
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use axum::extract::OriginalUri;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};

    use crate::architecture::MediaBackend as _;

    use super::*;

    async fn overlapping_id_fixture(OriginalUri(uri): OriginalUri) -> Response {
        let body = match uri.path() {
            "/rest/ping.view" => serde_json::json!({
                "subsonic-response": { "status": "ok" }
            }),
            "/rest/getArtists.view" => serde_json::json!({
                "subsonic-response": {
                    "status": "ok",
                    "artists": {
                        "index": [{
                            "artist": [{
                                "id": "shared-native-id",
                                "name": "Artist",
                                "coverArt": "full-artist-cover"
                            }]
                        }]
                    }
                }
            }),
            "/rest/getArtist.view" => serde_json::json!({
                "subsonic-response": {
                    "status": "ok",
                    "artist": {
                        "id": "shared-native-id",
                        "name": "Artist",
                        "album": [{
                            "id": "shared-native-id",
                            "name": "Album",
                            "coverArt": "full-album-cover"
                        }]
                    }
                }
            }),
            "/rest/getAlbum.view" => serde_json::json!({
                "subsonic-response": {
                    "status": "ok",
                    "album": {
                        "id": "shared-native-id",
                        "name": "Album",
                        "song": [{
                            "id": "shared-native-id",
                            "title": "Song",
                            "coverArt": "full-song-cover"
                        }]
                    }
                }
            }),
            "/rest/search3.view" => serde_json::json!({
                "subsonic-response": {
                    "status": "ok",
                    "searchResult3": {
                        "artist": [{
                            "id": "shared-native-id",
                            "name": "Artist",
                            "coverArt": "search-artist-cover"
                        }],
                        "album": [{
                            "id": "shared-native-id",
                            "name": "Album",
                            "coverArt": "search-album-cover"
                        }],
                        "song": [{
                            "id": "shared-native-id",
                            "title": "Song",
                            "coverArt": "search-song-cover"
                        }]
                    }
                }
            }),
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        Json(body).into_response()
    }

    fn resolved_media_id(request: &ResolvedHttpRequest) -> String {
        request
            .endpoint()
            .query_pairs()
            .find_map(|(key, value)| (key == "id").then(|| value.into_owned()))
            .expect("resolved Subsonic request carries a public media ID")
    }

    #[test]
    fn converted_models_do_not_expose_remote_credentials_or_urls() {
        let song: SongEntry = serde_json::from_value(serde_json::json!({
            "id": "song-id",
            "title": "Song",
            "coverArt": "cover-id"
        }))
        .unwrap();
        let album: AlbumEntry = serde_json::from_value(serde_json::json!({
            "id": "album-id",
            "name": "Album",
            "coverArt": "cover-id"
        }))
        .unwrap();

        let track = song_to_track(&song, Uuid::new_v4(), None, None);
        let album = album_entry_to_album(&album, Uuid::new_v4(), None);

        assert!(track.stream_url.is_none());
        assert!(track.cover_art_url.is_none());
        assert!(album.cover_art_url.is_none());
    }

    #[tokio::test]
    async fn track_artwork_survives_same_native_album_and_artist_ids() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind Subsonic fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(get(overlapping_id_fixture)),
            )
            .await
            .expect("serve Subsonic fixture");
        });

        let fixture_secret = Uuid::new_v4().to_string();
        let backend = SubsonicBackend::connect(
            "fixture",
            &format!("http://{address}"),
            "user",
            &fixture_secret,
        )
        .await
        .expect("connect to fixture");
        let shared_id = deterministic_uuid("shared-native-id");

        let initial = backend
            .resolve_artwork(&shared_id)
            .await
            .expect("resolve full-library artwork")
            .expect("full-library artwork");
        assert_eq!(resolved_media_id(&initial), "full-song-cover");

        let results = backend.search("Song", 10).await.expect("search fixture");
        assert_eq!(results.tracks.len(), 1);
        assert_eq!(results.albums.len(), 1);
        assert_eq!(results.artists.len(), 1);
        let searched = backend
            .resolve_artwork(&shared_id)
            .await
            .expect("resolve searched artwork")
            .expect("searched artwork");
        assert_eq!(resolved_media_id(&searched), "search-song-cover");

        server.abort();
    }
}
