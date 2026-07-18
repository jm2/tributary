//! `MediaBackend` implementation for DAAP (iTunes Sharing) servers.
//!
//! All metadata is held in memory — nothing touches the local SQLite DB.
//! Login and catalogue loading are separate lifecycle stages. Cached tracks
//! retain only stable native identities; the central registry resolves those
//! against the exact adopted session immediately before media is consumed.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::media::{RemoteMediaResolver, ResolvedHttpRequest};
use crate::architecture::models::*;
use crate::architecture::{AdvertisedHttpRoute, TrackId};
use crate::source_lifecycle::{
    AdapterCloseFuture, CloseAuthority, FailureCategory, LifecycleAdapter,
};

use super::client::{DaapCatalogueScope, DaapClient};
use super::dmap;

// ---------------------------------------------------------------------------
// In-memory library cache
// ---------------------------------------------------------------------------

/// In-memory library cache populated from the DAAP server.
struct LibraryCache {
    scope: Option<DaapCatalogueScope>,
    tracks: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    /// Tributary UUID → index in `tracks`.
    track_by_uuid: HashMap<Uuid, usize>,
    /// Tributary UUID → DAAP item ID.
    track_to_daap_id: HashMap<Uuid, u32>,
    /// DAAP item ID → validated stream format for exact at-use resolution.
    format_by_daap_id: HashMap<u32, String>,
}

impl LibraryCache {
    fn empty() -> Self {
        Self {
            scope: None,
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            track_by_uuid: HashMap::new(),
            track_to_daap_id: HashMap::new(),
            format_by_daap_id: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// DaapBackend
// ---------------------------------------------------------------------------

/// A DAAP backend that implements [`MediaBackend`].
///
/// Login and catalogue loading are deliberately separate so a server-side
/// session enters the central lifecycle registry immediately after `mlid` is
/// parsed and before update/database/items work begins.
pub struct DaapBackend {
    display_name: String,
    client: DaapClient,
    cache: RwLock<LibraryCache>,
}

impl DaapBackend {
    /// Login to a DAAP server and return the first close-capable adapter.
    ///
    /// # Arguments
    /// * `name` — display name for the sidebar (e.g. "Living Room DAAP")
    /// * `server_url` — base URL including scheme (e.g. `http://192.168.1.50:3689`)
    /// * `password` — optional share password
    pub async fn login(
        name: &str,
        server_url: &str,
        password: Option<&str>,
    ) -> BackendResult<Self> {
        Self::login_with_route(name, server_url, password, None).await
    }

    /// Connect through a retained mDNS route without replacing the advertised
    /// hostname in the DAAP origin.
    pub(crate) async fn login_with_route(
        name: &str,
        server_url: &str,
        password: Option<&str>,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
        let client = DaapClient::login_with_route(server_url, password, advertised_route).await?;
        Ok(Self {
            display_name: name.to_string(),
            client,
            cache: RwLock::new(LibraryCache::empty()),
        })
    }

    /// Discover and fetch the entire catalogue after lifecycle staging.
    pub(crate) async fn load_catalogue(&self) -> BackendResult<Vec<Track>> {
        info!("Fetching DAAP library...");

        let scope = self.client.discover_catalogue_scope().await?;
        let mlit_items = self.client.fetch_tracks(scope).await?;

        let mut all_tracks = Vec::new();
        let mut track_by_uuid = HashMap::new();
        let mut track_to_daap_id = HashMap::new();
        let mut format_by_daap_id = HashMap::new();

        // Aggregation maps for artists and albums.
        // Key: name (lowercased for dedup), Value: (display_name, metadata).
        let mut artist_map: HashMap<String, ArtistAgg> = HashMap::new();
        let mut album_map: HashMap<(String, String), AlbumAgg> = HashMap::new();

        for nodes in &mlit_items {
            let Some(daap_id) = dmap::find_u32(nodes, b"miid") else {
                continue; // Skip items without an ID.
            };

            let title = dmap::find_string(nodes, b"minm").unwrap_or_else(|| "Unknown".to_string());
            let artist_name =
                dmap::find_string(nodes, b"asar").unwrap_or_else(|| "Unknown".to_string());
            let album_title = dmap::find_string(nodes, b"asal").unwrap_or_default();
            let duration_ms = dmap::find_u32(nodes, b"astm");
            let track_number = dmap::find_u16(nodes, b"astn");
            let disc_number = dmap::find_u16(nodes, b"asdn");
            let genre = dmap::find_string(nodes, b"asgn");
            let year = dmap::find_u16(nodes, b"asyr");
            let format = dmap::find_string(nodes, b"asfm").unwrap_or_else(|| "mp3".to_string());
            remember_track_format(&mut format_by_daap_id, daap_id, format.clone())?;
            let bitrate = dmap::find_u16(nodes, b"asbr");
            let sample_rate = dmap::find_u32(nodes, b"assr");

            // DAAP date modified: most real-world DAAP servers (forked-daapd,
            // OwnTone, etc.) send `asdm` as a standard Unix timestamp
            // (seconds since 1970-01-01), NOT seconds since the DAAP epoch
            // (2001-01-01).  Treat it as a plain Unix timestamp.
            let date_modified = dmap::find_u32(nodes, b"asdm")
                .and_then(|unix_secs| chrono::DateTime::from_timestamp(i64::from(unix_secs), 0));

            let track_uuid = deterministic_uuid(daap_id);
            let artist_uuid = deterministic_uuid_from_name(&artist_name);
            let album_uuid = deterministic_uuid_from_name(&album_title);

            let duration_secs = duration_ms.map(|ms| u64::from(ms) / 1000);

            let track = Track {
                id: track_uuid,
                native_track_id: crate::architecture::TrackId::remote(daap_id.to_string()).ok(),
                title,
                artist_name: artist_name.clone(),
                album_artist_name: None,
                artist_id: Some(artist_uuid),
                album_title: album_title.clone(),
                album_id: Some(album_uuid),
                track_number: track_number.map(u32::from),
                disc_number: disc_number.map(u32::from),
                duration_secs,
                composer: None,
                genre: genre.clone(),
                year: year.map(i32::from),
                file_path: None,
                stream_url: None,
                cover_art_url: None,
                date_added: None,
                date_modified,
                bitrate_kbps: bitrate.map(u32::from),
                sample_rate_hz: sample_rate,
                format: Some(format.clone()),
                play_count: None,
            };

            let idx = all_tracks.len();
            track_by_uuid.insert(track_uuid, idx);
            track_to_daap_id.insert(track_uuid, daap_id);
            all_tracks.push(track);

            // ── Aggregate artist ────────────────────────────────────
            let artist_key = artist_name.to_lowercase();
            let agg = artist_map.entry(artist_key).or_insert_with(|| ArtistAgg {
                display_name: artist_name.clone(),
                uuid: artist_uuid,
                album_names: std::collections::HashSet::new(),
                track_count: 0,
            });
            agg.track_count += 1;
            if !album_title.is_empty() {
                agg.album_names.insert(album_title.to_lowercase());
            }

            // ── Aggregate album ─────────────────────────────────────
            let album_key = (album_title.to_lowercase(), artist_name.to_lowercase());
            let album_agg = album_map.entry(album_key).or_insert_with(|| AlbumAgg {
                display_title: album_title,
                display_artist: artist_name,
                uuid: album_uuid,
                artist_uuid,
                year: year.map(i32::from),
                genre,
                track_count: 0,
                total_duration_secs: 0,
            });
            album_agg.track_count += 1;
            album_agg.total_duration_secs += duration_secs.unwrap_or(0);
        }

        // ── Build Artist models ─────────────────────────────────────
        let all_artists: Vec<Artist> = artist_map
            .into_values()
            .map(|agg| Artist {
                id: agg.uuid,
                name: agg.display_name,
                album_count: agg.album_names.len() as u32,
                track_count: agg.track_count,
                cover_art_url: None,
            })
            .collect();

        // ── Build Album models ──────────────────────────────────────
        let all_albums: Vec<Album> = album_map
            .into_values()
            .map(|agg| Album {
                id: agg.uuid,
                title: agg.display_title,
                artist_name: agg.display_artist,
                artist_id: Some(agg.artist_uuid),
                year: agg.year,
                genre: agg.genre,
                cover_art_url: None,
                track_count: agg.track_count,
                total_duration_secs: Some(agg.total_duration_secs),
            })
            .collect();

        info!(
            tracks = all_tracks.len(),
            albums = all_albums.len(),
            artists = all_artists.len(),
            "DAAP library loaded"
        );

        let mut cache = self.cache.write().await;
        *cache = LibraryCache {
            scope: Some(scope),
            tracks: all_tracks,
            albums: all_albums,
            artists: all_artists,
            track_by_uuid,
            track_to_daap_id,
            format_by_daap_id,
        };

        Ok(cache.tracks.clone())
    }

    async fn stream_request_for_native_id(
        &self,
        track_id: &TrackId,
    ) -> BackendResult<ResolvedHttpRequest> {
        let song_id = parse_daap_track_id(track_id)?;
        let cache = self.cache.read().await;
        let scope = cache.scope.ok_or_else(unavailable_catalogue)?;
        let format = cache
            .format_by_daap_id
            .get(&song_id)
            .ok_or_else(unavailable_catalogue)?;
        self.client.stream_request(scope, song_id, format)
    }

    async fn artwork_request_for_native_id(
        &self,
        track_id: &TrackId,
    ) -> BackendResult<ResolvedHttpRequest> {
        let song_id = parse_daap_track_id(track_id)?;
        let cache = self.cache.read().await;
        let scope = cache.scope.ok_or_else(unavailable_catalogue)?;
        if !cache.format_by_daap_id.contains_key(&song_id) {
            return Err(unavailable_catalogue());
        }
        self.client.cover_art_request(scope, song_id)
    }

    /// Resolve a cached application track ID for DAAP lifecycle tests.
    /// Production playback instead carries pathless `(SourceId, TrackId,
    /// session epoch)` identity through the central lifecycle registry.
    #[cfg(test)]
    pub(super) async fn stream_request_for_track(
        &self,
        track_id: &Uuid,
    ) -> BackendResult<ResolvedHttpRequest> {
        let cache = self.cache.read().await;
        let song_id =
            cache
                .track_to_daap_id
                .get(track_id)
                .ok_or_else(|| BackendError::NotFound {
                    entity_type: "track".into(),
                    id: *track_id,
                })?;
        let idx = cache.track_by_uuid[track_id];
        let format = cache.tracks[idx].format.as_deref().unwrap_or("mp3");
        let scope = cache.scope.ok_or_else(unavailable_catalogue)?;
        self.client.stream_request(scope, *song_id, format)
    }
}

fn parse_daap_track_id(track_id: &TrackId) -> BackendResult<u32> {
    let song_id = track_id
        .as_str()
        .parse::<u32>()
        .map_err(|_| unavailable_catalogue())?;
    if song_id.to_string() != track_id.as_str() {
        return Err(unavailable_catalogue());
    }
    Ok(song_id)
}

fn unavailable_catalogue() -> BackendError {
    BackendError::ConnectionFailed {
        message: "DAAP track is unavailable in the active catalogue".to_string(),
        source: None,
    }
}

/// Admit one canonical DAAP item identity. A duplicate `miid` is ambiguous:
/// silently overwriting it could bind catalogue metadata to another row's
/// stream, so reject the complete candidate catalogue before publication.
fn remember_track_format(
    formats: &mut HashMap<u32, String>,
    daap_id: u32,
    format: String,
) -> BackendResult<()> {
    if formats.contains_key(&daap_id) {
        return Err(BackendError::ParseError {
            message: "DAAP catalogue contains duplicate item identity".to_string(),
            source: None,
        });
    }
    formats.insert(daap_id, format);
    Ok(())
}

#[async_trait]
impl RemoteMediaResolver for DaapBackend {
    async fn resolve_stream(&self, track_id: &TrackId) -> BackendResult<ResolvedHttpRequest> {
        self.stream_request_for_native_id(track_id).await
    }

    async fn resolve_artwork(
        &self,
        track_id: &TrackId,
    ) -> BackendResult<Option<ResolvedHttpRequest>> {
        self.artwork_request_for_native_id(track_id).await.map(Some)
    }
}

impl LifecycleAdapter for DaapBackend {
    fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
        Box::pin(async move {
            self.client.logout().await;
            Ok::<(), FailureCategory>(())
        })
    }
}

// ── MediaBackend trait implementation ────────────────────────────────────

#[async_trait]
impl crate::architecture::MediaBackend for DaapBackend {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn backend_type(&self) -> &str {
        "daap"
    }

    async fn ping(&self) -> BackendResult<()> {
        self.client.ping().await
    }

    async fn search(&self, query: &str, limit: usize) -> BackendResult<SearchResults> {
        let cache = self.cache.read().await;
        let q = query.to_lowercase();

        let tracks: Vec<Track> = cache
            .tracks
            .iter()
            .filter(|t| {
                t.title.to_lowercase().contains(&q)
                    || t.artist_name.to_lowercase().contains(&q)
                    || t.album_title.to_lowercase().contains(&q)
            })
            .take(limit)
            .cloned()
            .collect();

        let albums: Vec<Album> = cache
            .albums
            .iter()
            .filter(|a| {
                a.title.to_lowercase().contains(&q) || a.artist_name.to_lowercase().contains(&q)
            })
            .take(limit)
            .cloned()
            .collect();

        let artists: Vec<Artist> = cache
            .artists
            .iter()
            .filter(|a| a.name.to_lowercase().contains(&q))
            .take(limit)
            .cloned()
            .collect();

        Ok(SearchResults {
            tracks,
            albums,
            artists,
        })
    }

    async fn list_tracks(&self) -> BackendResult<Vec<Track>> {
        Ok(self.cache.read().await.tracks.clone())
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

// ── Aggregation helpers ─────────────────────────────────────────────────

/// Temporary aggregation state for building `Artist` models.
struct ArtistAgg {
    display_name: String,
    uuid: Uuid,
    album_names: std::collections::HashSet<String>,
    track_count: u32,
}

/// Temporary aggregation state for building `Album` models.
struct AlbumAgg {
    display_title: String,
    display_artist: String,
    uuid: Uuid,
    artist_uuid: Uuid,
    year: Option<i32>,
    genre: Option<String>,
    track_count: u32,
    total_duration_secs: u64,
}

// ── UUID helpers ────────────────────────────────────────────────────────

/// Generate a deterministic UUID from a DAAP numeric item ID.
fn deterministic_uuid(daap_id: u32) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("daap:item:{daap_id}").as_bytes(),
    )
}

/// Generate a deterministic UUID from a name string (for artists/albums).
fn deterministic_uuid_from_name(name: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("daap:name:{name}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_daap_item_identity_fails_closed_without_overwriting_first_row() {
        let mut formats = HashMap::new();
        remember_track_format(&mut formats, 7, "flac".to_string()).expect("first identity");

        let error = remember_track_format(&mut formats, 7, "mp3".to_string())
            .expect_err("duplicate identity must reject the candidate catalogue");

        assert!(matches!(
            error,
            BackendError::ParseError {
                ref message,
                source: None
            } if message == "DAAP catalogue contains duplicate item identity"
        ));
        assert_eq!(formats.get(&7).map(String::as_str), Some("flac"));
    }
}
