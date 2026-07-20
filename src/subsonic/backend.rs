//! `MediaBackend` implementation for Subsonic-compatible servers.
//!
//! All metadata is held in memory — nothing touches the local SQLite DB.
//! The full library is fetched during [`SubsonicBackend::connect`] and
//! cached for fast browsing. Credentials remain in the retained backend and
//! are resolved into proxy-only requests at playback time.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::models::*;
use crate::architecture::{
    AdvertisedHttpRoute, NativePlaylistId, RemoteMediaResolver, ResolvedHttpRequest,
    ServerPlaylistSnapshot, ServerPlaylistSummary, TrackId, MAX_SERVER_PLAYLISTS_PER_LIST,
    MAX_SERVER_PLAYLIST_ENTRIES,
};

use super::api::{AlbumEntry, ArtistEntry, SongEntry};
use super::client::SubsonicClient;

/// Maximum number of per-artist / per-album metadata fetches kept in
/// flight at once while loading the full library.  Bounds concurrency so a
/// large library does not open hundreds of simultaneous connections, while
/// still overlapping request latency for a large speed-up over the old
/// fully-sequential walk.
const FETCH_CONCURRENCY: usize = 8;

/// Playlist listing responses contain metadata only and should remain far
/// smaller than a full catalogue response.
const MAX_PLAYLIST_LIST_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// A detailed playlist may legitimately contain many ordered occurrences,
/// but it is still finite and receives a tighter ceiling than a full-library
/// response.
const MAX_PLAYLIST_DETAIL_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// In-memory library cache populated from the Subsonic API.
#[allow(dead_code)]
struct LibraryCache {
    tracks: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    /// Exact Subsonic song ID → stream locator.
    stream_locator_by_track_id: HashMap<TrackId, String>,
    /// Exact Subsonic song ID → cover-art ID.
    track_artwork_locator_by_track_id: HashMap<TrackId, String>,
}

impl LibraryCache {
    fn empty() -> Self {
        Self {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            stream_locator_by_track_id: HashMap::new(),
            track_artwork_locator_by_track_id: HashMap::new(),
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

    /// List the current user's server-native playlists.
    ///
    /// This is a pull-only metadata operation. The returned identifiers are
    /// opaque and bounded, and duplicate playlist identifiers make the whole
    /// response invalid rather than introducing ambiguous synchronization
    /// authority.
    pub(crate) async fn list_server_playlists(&self) -> BackendResult<Vec<ServerPlaylistSummary>> {
        let envelope = self
            .client
            .get_with_params_bounded(
                "getPlaylists.view",
                &[],
                MAX_PLAYLIST_LIST_BODY_BYTES,
                "server-playlist-list",
            )
            .await?;
        let playlists = envelope
            .response
            .playlists
            .ok_or_else(|| {
                invalid_playlist_response(
                    "server playlist listing was missing its playlists object",
                )
            })?
            .playlist;
        if playlists.len() > MAX_SERVER_PLAYLISTS_PER_LIST {
            return Err(invalid_playlist_response(
                "server playlist listing exceeded the supported item count",
            ));
        }

        let mut seen = HashSet::with_capacity(playlists.len());
        let mut summaries = Vec::with_capacity(playlists.len());
        for playlist in playlists {
            let native_id = NativePlaylistId::new(playlist.id).map_err(|_| {
                invalid_playlist_response(
                    "server playlist listing contained an invalid playlist identifier",
                )
            })?;
            if !seen.insert(native_id.clone()) {
                return Err(invalid_playlist_response(
                    "server playlist listing contained a duplicate playlist identifier",
                ));
            }
            summaries.push(
                ServerPlaylistSummary::new(
                    native_id,
                    playlist.name,
                    playlist.owner,
                    playlist.song_count,
                )
                .map_err(|_| {
                    invalid_playlist_response(
                        "server playlist listing contained invalid presentation metadata",
                    )
                })?,
            );
        }
        Ok(summaries)
    }

    /// Fetch one exact server-native playlist snapshot.
    ///
    /// The detail endpoint's ordered `entry` array is authoritative. Its
    /// optional `songCount` is retained only as a hint, so a concurrently
    /// changing server cannot make a complete valid snapshot fail solely due
    /// to a stale advertised count.
    pub(crate) async fn get_server_playlist(
        &self,
        native_id: &NativePlaylistId,
    ) -> BackendResult<ServerPlaylistSnapshot> {
        let envelope = self
            .client
            .get_with_params_bounded(
                "getPlaylist.view",
                &[("id", native_id.as_str())],
                MAX_PLAYLIST_DETAIL_BODY_BYTES,
                "server-playlist-detail",
            )
            .await?;
        let playlist = envelope.response.playlist.ok_or_else(|| {
            invalid_playlist_response("server playlist detail was missing its playlist object")
        })?;
        let returned_id = NativePlaylistId::new(playlist.id).map_err(|_| {
            invalid_playlist_response(
                "server playlist detail contained an invalid playlist identifier",
            )
        })?;
        if &returned_id != native_id {
            return Err(invalid_playlist_response(
                "server playlist detail did not match the requested playlist",
            ));
        }
        if playlist.entry.len() > MAX_SERVER_PLAYLIST_ENTRIES {
            return Err(invalid_playlist_response(
                "server playlist detail exceeded the supported entry count",
            ));
        }

        let mut track_ids = Vec::with_capacity(playlist.entry.len());
        for entry in playlist.entry {
            track_ids.push(TrackId::remote(entry.id).map_err(|_| {
                invalid_playlist_response(
                    "server playlist detail contained an invalid track identifier",
                )
            })?);
        }

        ServerPlaylistSnapshot::new(
            returned_id,
            playlist.name,
            playlist.owner,
            playlist.song_count,
            track_ids,
        )
        .map_err(|_| {
            invalid_playlist_response(
                "server playlist detail contained invalid presentation metadata",
            )
        })
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
        let mut stream_locator_by_track_id = HashMap::new();
        let mut track_artwork_locator_by_track_id = HashMap::new();
        let mut skipped_invalid_track_ids = 0usize;

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
                    let Ok(track_id) = TrackId::remote(song.id.clone()) else {
                        skipped_invalid_track_ids += 1;
                        continue;
                    };
                    let track_uuid = deterministic_uuid(&song.id);
                    let track = song_to_track(
                        song,
                        track_id.clone(),
                        track_uuid,
                        Some(artist_uuid),
                        Some(album_uuid),
                    );

                    stream_locator_by_track_id.insert(track_id.clone(), song.id.clone());
                    if let Some(cover_art_id) = &song.cover_art {
                        track_artwork_locator_by_track_id.insert(track_id, cover_art_id.clone());
                    }
                    all_tracks.push(track);
                    artist_track_count += 1;
                }

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
            skipped_invalid_track_ids,
            "Subsonic library loaded"
        );

        let mut cache = self.cache.write().await;
        *cache = LibraryCache {
            tracks: all_tracks,
            albums: all_albums,
            artists: all_artists,
            stream_locator_by_track_id,
            track_artwork_locator_by_track_id,
        };

        Ok(())
    }
}

fn invalid_playlist_response(message: &'static str) -> BackendError {
    BackendError::ParseError {
        message: message.to_string(),
        source: None,
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
                .filter_map(|s| {
                    let track_id = TrackId::remote(s.id.clone()).ok()?;
                    let uuid = deterministic_uuid(&s.id);
                    Some(song_to_track(
                        s,
                        track_id,
                        uuid,
                        s.artist_id.as_deref().map(deterministic_uuid),
                        s.album_id.as_deref().map(deterministic_uuid),
                    ))
                })
                .collect(),
        };

        // Search results may include entities outside the initially loaded
        // catalogue. Retain their native locators before exposing the generic
        // models so selecting one can still resolve at playback time.
        let mut cache = self.cache.write().await;
        for song in &sr.song {
            let Ok(track_id) = TrackId::remote(song.id.clone()) else {
                continue;
            };
            cache
                .stream_locator_by_track_id
                .insert(track_id.clone(), song.id.clone());
            if let Some(cover_art_id) = &song.cover_art {
                cache
                    .track_artwork_locator_by_track_id
                    .insert(track_id, cover_art_id.clone());
            } else {
                cache.track_artwork_locator_by_track_id.remove(&track_id);
            }
        }

        Ok(results)
    }

    async fn list_tracks(&self) -> BackendResult<Vec<Track>> {
        Ok(self.cache.read().await.tracks.clone())
    }

    fn rating_capability(&self) -> RatingCapability {
        RatingCapability::ReadOnly
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
    async fn resolve_stream(&self, track_id: &TrackId) -> BackendResult<ResolvedHttpRequest> {
        let song_id = self
            .cache
            .read()
            .await
            .stream_locator_by_track_id
            .get(track_id)
            .cloned()
            .ok_or_else(|| BackendError::NotFound {
                entity_type: "track".into(),
                id: deterministic_uuid(track_id.as_str()),
            })?;
        self.client.resolved_stream_request(&song_id)
    }

    async fn resolve_artwork(
        &self,
        track_id: &TrackId,
    ) -> BackendResult<Option<ResolvedHttpRequest>> {
        let cover_art_id = self
            .cache
            .read()
            .await
            .track_artwork_locator_by_track_id
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
    track_id: TrackId,
    id: Uuid,
    artist_id: Option<Uuid>,
    album_id: Option<Uuid>,
) -> Track {
    Track {
        id,
        native_track_id: Some(track_id),
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
        rating: TrackRating::read_only(song.user_rating.and_then(Rating::from_five_star_scale)),
        last_played: None,
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
    use axum::http::StatusCode;
    use md5::{Digest as _, Md5};

    use crate::architecture::MediaBackend as _;
    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};

    use super::*;

    fn resolved_media_id(request: &ResolvedHttpRequest) -> String {
        request
            .endpoint()
            .query_pairs()
            .find_map(|(key, value)| (key == "id").then(|| value.into_owned()))
            .expect("resolved Subsonic request carries a public media ID")
    }

    fn empty_catalogue_routes(prefix: &str) -> Vec<MockRoute> {
        vec![
            MockRoute::get(format!("{prefix}/rest/ping.view")).reply(MockResponse::json(
                serde_json::json!({"subsonic-response": {"status": "ok"}}),
            )),
            MockRoute::get(format!("{prefix}/rest/getArtists.view")).reply(MockResponse::json(
                serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "artists": {"index": []}
                    }
                }),
            )),
        ]
    }

    async fn connect_empty_catalogue(
        service: &MockHttpService,
        prefix: &str,
        username: &str,
        password: &str,
    ) -> SubsonicBackend {
        SubsonicBackend::connect(
            "fixture",
            &format!("{}{prefix}", service.base_url()),
            username,
            password,
        )
        .await
        .expect("connect to empty Subsonic fixture")
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

        let track = song_to_track(
            &song,
            TrackId::remote("song-id").expect("track ID"),
            Uuid::new_v4(),
            None,
            None,
        );
        let album = album_entry_to_album(&album, Uuid::new_v4(), None);

        assert!(track.stream_url.is_none());
        assert!(track.cover_art_url.is_none());
        assert!(album.cover_art_url.is_none());
    }

    #[tokio::test]
    async fn track_artwork_survives_same_native_album_and_artist_ids() {
        let service = MockHttpService::start(vec![
            MockRoute::get("/rest/ping.view").reply(MockResponse::json(serde_json::json!({
                "subsonic-response": { "status": "ok" }
            }))),
            MockRoute::get("/rest/getArtists.view").reply(MockResponse::json(serde_json::json!({
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
            }))),
            MockRoute::get("/rest/getArtist.view")
                .with_query("id", "shared-native-id")
                .reply(MockResponse::json(serde_json::json!({
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
                }))),
            MockRoute::get("/rest/getAlbum.view")
                .with_query("id", "shared-native-id")
                .reply(MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "album": {
                            "id": "shared-native-id",
                            "name": "Album",
                            "song": [{
                                "id": "shared-native-id",
                                "title": "Song",
                                "userRating": 4,
                                "coverArt": "full-song-cover"
                            }]
                        }
                    }
                }))),
            MockRoute::get("/rest/search3.view")
                .with_query("query", "Song")
                .reply(MockResponse::json(serde_json::json!({
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
                                "userRating": 3,
                                "coverArt": "search-song-cover"
                            }]
                        }
                    }
                }))),
        ])
        .await;

        let fixture_secret = Uuid::new_v4().to_string();
        let backend =
            SubsonicBackend::connect("fixture", &service.base_url(), "user", &fixture_secret)
                .await
                .expect("connect to fixture");
        let shared_id = TrackId::remote("shared-native-id").expect("track ID");
        assert_eq!(backend.rating_capability(), RatingCapability::ReadOnly);
        let published = crate::architecture::load_track_catalog(&backend)
            .await
            .expect("catalogue rating capabilities agree");
        assert_eq!(
            published[0].rating,
            TrackRating::read_only(Some(Rating::new(80).unwrap()))
        );

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
        assert_eq!(
            results.tracks[0].rating,
            TrackRating::read_only(Some(Rating::new(60).unwrap()))
        );
        let searched = backend
            .resolve_artwork(&shared_id)
            .await
            .expect("resolve searched artwork")
            .expect("searched artwork");
        assert_eq!(resolved_media_id(&searched), "search-song-cover");

        let requests = service.requests();
        assert_eq!(requests.len(), 5);
        for request in requests {
            let query = request
                .uri
                .query()
                .map(|query| {
                    url::form_urlencoded::parse(query.as_bytes())
                        .into_owned()
                        .collect::<HashMap<_, _>>()
                })
                .expect("Subsonic fixture request query");
            assert_eq!(query.get("u").map(String::as_str), Some("user"));
            assert_eq!(query.get("v").map(String::as_str), Some("1.16.1"));
            assert_eq!(query.get("c").map(String::as_str), Some("Tributary"));
            assert_eq!(query.get("f").map(String::as_str), Some("json"));
            assert!(!query.contains_key("p"));
            let salt = query.get("s").expect("token-auth salt");
            let expected_token = Md5::digest(format!("{fixture_secret}{salt}")).iter().fold(
                String::new(),
                |mut token, byte| {
                    use std::fmt::Write as _;
                    let _ = write!(token, "{byte:02x}");
                    token
                },
            );
            assert_eq!(query.get("t"), Some(&expected_token));
            assert!(request.body.is_empty());
        }
        service.finish().await;
    }

    #[test]
    fn subsonic_user_rating_is_exact_read_only_five_star_data() {
        for (native, expected) in [
            (None, None),
            (Some(-1), None),
            (Some(0), None),
            (Some(1), Some(20)),
            (Some(5), Some(100)),
            (Some(6), None),
        ] {
            let song: SongEntry = serde_json::from_value(serde_json::json!({
                "id": "song-id",
                "userRating": native
            }))
            .unwrap();
            let track = song_to_track(
                &song,
                TrackId::remote("song-id").unwrap(),
                Uuid::new_v4(),
                None,
                None,
            );
            assert_eq!(track.rating.capability(), RatingCapability::ReadOnly);
            assert_eq!(
                track.rating.value().map(Rating::value),
                expected,
                "native Subsonic rating {native:?}"
            );
        }
    }

    #[tokio::test]
    async fn rejected_token_auth_stops_before_catalogue_fetch() {
        let password = Uuid::new_v4().to_string();
        let service = MockHttpService::start(vec![MockRoute::get("/rest/ping.view").reply(
            MockResponse::json(serde_json::json!({
                "subsonic-response": {
                    "status": "failed",
                    "error": {"code": 40, "message": password.clone()}
                }
            })),
        )])
        .await;
        let error = SubsonicBackend::connect("fixture", &service.base_url(), "user", &password)
            .await
            .err()
            .expect("fixture authentication must fail");

        assert!(matches!(error, BackendError::AuthenticationFailed { .. }));
        assert!(!error.to_string().contains(&password));
        assert_eq!(service.requests().len(), 1);
        service.finish().await;
    }

    #[tokio::test]
    async fn prefixed_catalogue_keeps_healthy_items_after_bounded_partial_failures() {
        let service = MockHttpService::start(vec![
            MockRoute::get("/gateway/rest/ping.view").reply(MockResponse::json(
                serde_json::json!({"subsonic-response": {"status": "ok"}}),
            )),
            MockRoute::get("/gateway/rest/getArtists.view").reply(MockResponse::json(
                serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "artists": {"index": [{"artist": [
                            {"id": "healthy-artist", "name": "Healthy Artist"},
                            {"id": "failed-artist", "name": "Failed Artist"}
                        ]}]}
                    }
                }),
            )),
            MockRoute::get("/gateway/rest/getArtist.view")
                .with_query("id", "healthy-artist")
                .reply(MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "artist": {
                            "id": "healthy-artist",
                            "name": "Healthy Artist",
                            "album": [
                                {"id": "healthy-album", "name": "Healthy Album"},
                                {"id": "failed-album", "name": "Failed Album"}
                            ]
                        }
                    }
                }))),
            MockRoute::get("/gateway/rest/getArtist.view")
                .with_query("id", "failed-artist")
                .reply(MockResponse::status(StatusCode::SERVICE_UNAVAILABLE)),
            MockRoute::get("/gateway/rest/getAlbum.view")
                .with_query("id", "healthy-album")
                .reply(MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "album": {
                            "id": "healthy-album",
                            "name": "Healthy Album",
                            "song": [{
                                "id": "healthy-track",
                                "title": "Healthy Track",
                                "artist": "Healthy Artist",
                                "album": "Healthy Album"
                            }]
                        }
                    }
                }))),
            MockRoute::get("/gateway/rest/getAlbum.view")
                .with_query("id", "failed-album")
                .reply(MockResponse::status(StatusCode::BAD_GATEWAY)),
        ])
        .await;
        let password = Uuid::new_v4().to_string();
        let backend = SubsonicBackend::connect(
            "fixture",
            &format!("{}/gateway/", service.base_url()),
            "user",
            &password,
        )
        .await
        .expect("partial failures must retain the healthy catalogue subset");

        let cache = backend.cache.read().await;
        assert_eq!(cache.tracks.len(), 1);
        assert_eq!(cache.tracks[0].title, "Healthy Track");
        assert_eq!(cache.albums.len(), 1);
        assert_eq!(cache.albums[0].title, "Healthy Album");
        assert_eq!(cache.artists.len(), 1);
        assert_eq!(cache.artists[0].name, "Healthy Artist");
        assert_eq!(cache.artists[0].album_count, 2);
        let track_id = cache.tracks[0]
            .native_track_id
            .clone()
            .expect("fixture track retains its native ID");
        assert_eq!(track_id.as_str(), "healthy-track");
        drop(cache);
        assert_eq!(
            backend
                .resolve_stream(&track_id)
                .await
                .expect("resolve healthy stream")
                .endpoint()
                .path(),
            "/gateway/rest/stream.view"
        );
        assert_eq!(service.requests().len(), 6);
        service.finish().await;
    }

    #[tokio::test]
    async fn native_playlist_http_preserves_prefix_auth_order_duplicates_and_count_hints() {
        const PREFIX: &str = "/proxy";
        let mut routes = empty_catalogue_routes(PREFIX);
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylists.view")).replies([
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": {}
                    }
                })),
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": {"playlist": [
                            {
                                "id": "playlist-one",
                                "name": "One",
                                "owner": "fixture-owner",
                                "songCount": 999
                            },
                            {"id": "playlist-two", "name": "Two", "songCount": 0}
                        ]}
                    }
                })),
            ]),
        );
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylist.view"))
                .with_query("id", "playlist-one")
                .reply(MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlist": {
                            "id": "playlist-one",
                            "name": "One",
                            "owner": "fixture-owner",
                            "songCount": 999,
                            "entry": [
                                {"id": "track-b"},
                                {"id": "track-a"},
                                {"id": "track-b"}
                            ]
                        }
                    }
                }))),
        );
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylist.view"))
                .with_query("id", "playlist-two")
                .replies([
                    MockResponse::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "ok",
                            "playlist": {"id": "playlist-two"}
                        }
                    })),
                    MockResponse::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "ok",
                            "playlist": {"id": "playlist-two", "entry": []}
                        }
                    })),
                ]),
        );
        let service = MockHttpService::start(routes).await;
        let username = Uuid::new_v4().to_string();
        let password = Uuid::new_v4().to_string();
        let backend = connect_empty_catalogue(&service, PREFIX, &username, &password).await;

        assert!(backend
            .list_server_playlists()
            .await
            .expect("empty listing")
            .is_empty());
        let summaries = backend
            .list_server_playlists()
            .await
            .expect("playlist listing");
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].native_id().as_str(), "playlist-one");
        assert_eq!(summaries[0].name(), Some("One"));
        assert_eq!(summaries[0].owner(), Some("fixture-owner"));
        assert_eq!(summaries[0].advertised_track_count(), Some(999));
        assert_eq!(summaries[1].native_id().as_str(), "playlist-two");

        let snapshot = backend
            .get_server_playlist(summaries[0].native_id())
            .await
            .expect("playlist detail");
        assert_eq!(snapshot.native_id(), summaries[0].native_id());
        assert_eq!(snapshot.advertised_track_count(), Some(999));
        assert_eq!(
            snapshot
                .track_ids()
                .iter()
                .map(TrackId::as_str)
                .collect::<Vec<_>>(),
            ["track-b", "track-a", "track-b"]
        );
        for _ in 0..2 {
            assert!(backend
                .get_server_playlist(summaries[1].native_id())
                .await
                .expect("empty playlist detail")
                .track_ids()
                .is_empty());
        }

        let requests = service.requests();
        assert_eq!(requests.len(), 7);
        for request in &requests {
            assert!(request.uri.path().starts_with(PREFIX));
            let query = request
                .uri
                .query()
                .map(|query| {
                    url::form_urlencoded::parse(query.as_bytes())
                        .into_owned()
                        .collect::<HashMap<_, _>>()
                })
                .expect("authenticated Subsonic query");
            assert_eq!(query.get("u"), Some(&username));
            assert!(query.contains_key("t"));
            assert!(query.contains_key("s"));
            assert_eq!(query.get("v").map(String::as_str), Some("1.16.1"));
            assert_eq!(query.get("c").map(String::as_str), Some("Tributary"));
            assert_eq!(query.get("f").map(String::as_str), Some("json"));
            assert!(!query.contains_key("p"));
        }
        service.finish().await;
    }

    #[tokio::test]
    async fn native_playlist_listing_requires_an_explicit_wrapper() {
        const PREFIX: &str = "/wrapper";
        let response_secret = Uuid::new_v4().to_string();
        let mut routes = empty_catalogue_routes(PREFIX);
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylists.view")).replies([
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "unrelated": response_secret.clone()
                    }
                })),
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": null,
                        "unrelated": response_secret.clone()
                    }
                })),
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": {}
                    }
                })),
            ]),
        );
        let service = MockHttpService::start(routes).await;
        let password = Uuid::new_v4().to_string();
        let backend = connect_empty_catalogue(&service, PREFIX, "user", &password).await;

        for _ in 0..2 {
            let error = backend
                .list_server_playlists()
                .await
                .expect_err("absent or null wrapper must fail");
            assert!(matches!(
                error,
                BackendError::ParseError { source: None, .. }
            ));
            let rendered = error.to_string();
            assert!(rendered.contains("missing its playlists object"));
            assert!(!rendered.contains(&response_secret));
            assert!(!rendered.contains(&password));
        }
        assert!(backend
            .list_server_playlists()
            .await
            .expect("explicit empty wrapper")
            .is_empty());
        service.finish().await;
    }

    #[tokio::test]
    async fn native_playlist_identifiers_fail_closed_without_echoing_server_content() {
        const PREFIX: &str = "/native-id";
        let oversized_playlist_id = format!("playlist-secret-{}", "x".repeat(4 * 1024));
        let mismatched_playlist_id = "mismatched-secret-playlist";
        let oversized_track_id = format!("track-secret-{}", "x".repeat(4 * 1024));
        let mut routes = empty_catalogue_routes(PREFIX);
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylists.view")).replies([
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": {"playlist": [{"id": "", "name": "bad"}]}
                    }
                })),
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": {"playlist": [{
                            "id": oversized_playlist_id.clone(),
                            "name": "bad"
                        }]}
                    }
                })),
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": {"playlist": [
                            {"id": "duplicate-secret", "name": "first"},
                            {"id": "duplicate-secret", "name": "second"}
                        ]}
                    }
                })),
            ]),
        );
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylist.view"))
                .with_query("id", "expected-playlist")
                .replies([
                    MockResponse::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "ok",
                            "playlist": {"id": mismatched_playlist_id, "entry": []}
                        }
                    })),
                    MockResponse::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "ok",
                            "playlist": {
                                "id": "expected-playlist",
                                "entry": [{"id": ""}]
                            }
                        }
                    })),
                    MockResponse::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "ok",
                            "playlist": {
                                "id": "expected-playlist",
                                "entry": [{"id": oversized_track_id.clone()}]
                            }
                        }
                    })),
                ]),
        );
        let service = MockHttpService::start(routes).await;
        let username = Uuid::new_v4().to_string();
        let password = Uuid::new_v4().to_string();
        let backend = connect_empty_catalogue(&service, PREFIX, &username, &password).await;
        let expected = NativePlaylistId::new("expected-playlist").unwrap();

        let mut rendered_errors = Vec::new();
        for _ in 0..3 {
            rendered_errors.push(
                backend
                    .list_server_playlists()
                    .await
                    .expect_err("invalid listing must fail all-or-none")
                    .to_string(),
            );
        }
        for _ in 0..3 {
            rendered_errors.push(
                backend
                    .get_server_playlist(&expected)
                    .await
                    .expect_err("invalid detail must fail all-or-none")
                    .to_string(),
            );
        }
        for rendered in rendered_errors {
            for secret in [
                oversized_playlist_id.as_str(),
                mismatched_playlist_id,
                "duplicate-secret",
                oversized_track_id.as_str(),
                username.as_str(),
                password.as_str(),
            ] {
                assert!(!rendered.contains(secret), "error exposed server content");
            }
        }
        service.finish().await;
    }

    #[tokio::test]
    async fn native_playlist_body_and_item_count_limits_are_enforced() {
        const PREFIX: &str = "/bounds";
        let maximum_summaries = (0..MAX_SERVER_PLAYLISTS_PER_LIST)
            .map(|index| serde_json::json!({"id": format!("playlist-{index}")}))
            .collect::<Vec<_>>();
        let too_many_summaries = (0..=MAX_SERVER_PLAYLISTS_PER_LIST)
            .map(|index| serde_json::json!({"id": format!("playlist-{index}")}))
            .collect::<Vec<_>>();
        let maximum_entries = (0..MAX_SERVER_PLAYLIST_ENTRIES)
            .map(|index| serde_json::json!({"id": format!("track-{index}")}))
            .collect::<Vec<_>>();
        let too_many_entries = (0..=MAX_SERVER_PLAYLIST_ENTRIES)
            .map(|index| serde_json::json!({"id": format!("track-{index}")}))
            .collect::<Vec<_>>();
        let mut routes = empty_catalogue_routes(PREFIX);
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylists.view")).replies([
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": {"playlist": maximum_summaries}
                    }
                })),
                MockResponse::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "playlists": {"playlist": too_many_summaries}
                    }
                })),
                MockResponse::text(
                    "x".repeat(usize::try_from(MAX_PLAYLIST_LIST_BODY_BYTES).unwrap() + 1),
                ),
            ]),
        );
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylist.view"))
                .with_query("id", "bounded-playlist")
                .replies([
                    MockResponse::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "ok",
                            "playlist": {
                                "id": "bounded-playlist",
                                "entry": maximum_entries
                            }
                        }
                    })),
                    MockResponse::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "ok",
                            "playlist": {
                                "id": "bounded-playlist",
                                "entry": too_many_entries
                            }
                        }
                    })),
                    MockResponse::text(
                        "x".repeat(usize::try_from(MAX_PLAYLIST_DETAIL_BODY_BYTES).unwrap() + 1),
                    ),
                ]),
        );
        let service = MockHttpService::start(routes).await;
        let password = Uuid::new_v4().to_string();
        let backend = connect_empty_catalogue(&service, PREFIX, "user", &password).await;
        let playlist_id = NativePlaylistId::new("bounded-playlist").unwrap();

        let maximum_listing = backend
            .list_server_playlists()
            .await
            .expect("listing at exact item cap");
        assert_eq!(maximum_listing.len(), MAX_SERVER_PLAYLISTS_PER_LIST);
        assert_eq!(maximum_listing[0].native_id().as_str(), "playlist-0");
        assert_eq!(
            maximum_listing[MAX_SERVER_PLAYLISTS_PER_LIST - 1]
                .native_id()
                .as_str(),
            "playlist-9999"
        );
        let maximum_detail = backend
            .get_server_playlist(&playlist_id)
            .await
            .expect("detail at exact item cap");
        assert_eq!(
            maximum_detail.track_ids().len(),
            MAX_SERVER_PLAYLIST_ENTRIES
        );
        assert_eq!(maximum_detail.track_ids()[0].as_str(), "track-0");
        assert_eq!(
            maximum_detail.track_ids()[MAX_SERVER_PLAYLIST_ENTRIES - 1].as_str(),
            "track-99999"
        );
        let listing_count = backend
            .list_server_playlists()
            .await
            .expect_err("listing item bound");
        assert!(listing_count.to_string().contains("supported item count"));
        let detail_count = backend
            .get_server_playlist(&playlist_id)
            .await
            .expect_err("detail item bound");
        assert!(detail_count.to_string().contains("supported entry count"));
        let listing_body = backend
            .list_server_playlists()
            .await
            .expect_err("listing body bound");
        assert!(listing_body.to_string().contains("response body too large"));
        let detail_body = backend
            .get_server_playlist(&playlist_id)
            .await
            .expect_err("detail body bound");
        assert!(detail_body.to_string().contains("response body too large"));
        for error in [listing_count, detail_count, listing_body, detail_body] {
            assert!(!error.to_string().contains(&password));
        }
        service.finish().await;
    }

    #[tokio::test]
    async fn native_playlist_api_failures_discard_server_messages() {
        const PREFIX: &str = "/failure";
        let server_message = Uuid::new_v4().to_string();
        let mut routes = empty_catalogue_routes(PREFIX);
        routes.push(
            MockRoute::get(format!("{PREFIX}/rest/getPlaylists.view")).reply(MockResponse::json(
                serde_json::json!({
                    "subsonic-response": {
                        "status": "failed",
                        "error": {"code": 70, "message": server_message.clone()}
                    }
                }),
            )),
        );
        let service = MockHttpService::start(routes).await;
        let password = Uuid::new_v4().to_string();
        let backend = connect_empty_catalogue(&service, PREFIX, "user", &password).await;

        let error = backend
            .list_server_playlists()
            .await
            .expect_err("failed API envelope");
        assert!(matches!(error, BackendError::ConnectionFailed { .. }));
        let rendered = error.to_string();
        assert!(rendered.contains("Subsonic API error 70"));
        assert!(!rendered.contains(&server_message));
        assert!(!rendered.contains(&password));
        service.finish().await;
    }
}
