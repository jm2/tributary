//! `MediaBackend` implementation for DAAP (iTunes Sharing) servers.
//!
//! All metadata is held in memory — nothing touches the local SQLite DB.
//! The full library is fetched during [`DaapBackend::connect`] and
//! cached for fast browsing.  Streaming URLs include the DAAP session-id
//! so GStreamer can fetch audio directly.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::info;
use url::Url;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::models::*;

use super::client::DaapClient;
use super::dmap;

// ---------------------------------------------------------------------------
// In-memory library cache
// ---------------------------------------------------------------------------

/// In-memory library cache populated from the DAAP server.
struct LibraryCache {
    tracks: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    /// Tributary UUID → index in `tracks`.
    track_by_uuid: HashMap<Uuid, usize>,
    /// DAAP item ID → Tributary UUID.
    #[allow(dead_code)]
    daap_id_to_uuid: HashMap<u32, Uuid>,
}

impl LibraryCache {
    fn empty() -> Self {
        Self {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            track_by_uuid: HashMap::new(),
            daap_id_to_uuid: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// DaapBackend
// ---------------------------------------------------------------------------

/// A DAAP backend that implements [`MediaBackend`].
///
/// Create one with [`DaapBackend::connect`], which performs the DAAP
/// handshake and fetches the full library into memory.
pub struct DaapBackend {
    display_name: String,
    client: DaapClient,
    cache: RwLock<LibraryCache>,
}

impl DaapBackend {
    /// Connect to a DAAP server, perform the handshake, and fetch the
    /// full library into memory.
    ///
    /// # Arguments
    /// * `name` — display name for the sidebar (e.g. "Living Room DAAP")
    /// * `server_url` — base URL including scheme (e.g. `http://192.168.1.50:3689`)
    /// * `password` — optional share password
    pub async fn connect(
        name: &str,
        server_url: &str,
        password: Option<&str>,
    ) -> BackendResult<Self> {
        let client = DaapClient::connect(server_url, password).await?;

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
        info!("Fetching DAAP library...");

        let mlit_items = self.client.fetch_tracks().await?;

        let mut all_tracks = Vec::new();
        let mut track_by_uuid = HashMap::new();
        let mut daap_id_to_uuid = HashMap::new();

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
            let bitrate = dmap::find_u16(nodes, b"asbr");
            let sample_rate = dmap::find_u32(nodes, b"assr");

            // DAAP date modified: seconds since 2001-01-01 00:00:00 UTC.
            // Convert to DateTime<Utc> by adding the DAAP epoch offset
            // (978307200 = seconds between Unix epoch and 2001-01-01).
            let date_modified = dmap::find_u32(nodes, b"asdm").and_then(|daap_secs| {
                chrono::DateTime::from_timestamp(i64::from(daap_secs) + 978_307_200, 0)
            });

            let track_uuid = deterministic_uuid(daap_id);
            let artist_uuid = deterministic_uuid_from_name(&artist_name);
            let album_uuid = deterministic_uuid_from_name(&album_title);

            let stream_url = self.client.stream_url(daap_id, &format);

            let duration_secs = duration_ms.map(|ms| u64::from(ms) / 1000);

            let track = Track {
                id: track_uuid,
                title,
                artist_name: artist_name.clone(),
                artist_id: Some(artist_uuid),
                album_title: album_title.clone(),
                album_id: Some(album_uuid),
                track_number: track_number.map(u32::from),
                disc_number: disc_number.map(u32::from),
                duration_secs,
                genre: genre.clone(),
                year: year.map(i32::from),
                file_path: None,
                stream_url: Some(stream_url),
                cover_art_url: None,
                date_added: None,
                date_modified,
                bitrate_kbps: bitrate.map(u32::from),
                sample_rate_hz: sample_rate,
                format: Some(format),
                play_count: None,
            };

            let idx = all_tracks.len();
            track_by_uuid.insert(track_uuid, idx);
            daap_id_to_uuid.insert(daap_id, track_uuid);
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
            tracks: all_tracks,
            albums: all_albums,
            artists: all_artists,
            track_by_uuid,
            daap_id_to_uuid,
        };

        Ok(())
    }

    /// Return all tracks from the cache as Tributary `Track` models.
    /// Used by the integration layer to send a RemoteSync to the UI.
    pub async fn all_tracks(&self) -> Vec<Track> {
        self.cache.read().await.tracks.clone()
    }

    /// Send a best-effort logout to the DAAP server.
    pub async fn disconnect(&self) {
        self.client.logout().await;
    }

    /// Build the logout URL for this session (used by the eject button).
    pub fn logout_url(&self) -> String {
        format!(
            "{}/logout?session-id={}",
            self.client.base_url().as_str().trim_end_matches('/'),
            self.client.session_id()
        )
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
        // Issue a lightweight server-info request to check connectivity.
        let url = format!(
            "{}/server-info",
            self.client.base_url().as_str().trim_end_matches('/')
        );
        let resp = self
            .client
            .http_clone()
            .get(&url)
            .send()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("DAAP ping failed: {e}"),
                source: Some(Box::new(e)),
            })?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP ping HTTP {}", resp.status()),
                source: None,
            });
        }

        Ok(())
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

    async fn get_cover_art(&self, _album_id: &Uuid) -> BackendResult<Option<Url>> {
        // DAAP cover art requires a separate endpoint; deferred to a future phase.
        Ok(None)
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

// ── Drop: best-effort logout ────────────────────────────────────────────

impl Drop for DaapBackend {
    fn drop(&mut self) {
        let url = format!(
            "{}/logout?session-id={}",
            self.client.base_url().as_str().trim_end_matches('/'),
            self.client.session_id()
        );
        let http = self.client.http_clone();
        // Guard against missing runtime — during process shutdown the
        // tokio runtime may already be dropped.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = http.get(&url).send().await;
            });
        }
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
