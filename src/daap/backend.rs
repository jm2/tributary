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
use crate::source_registry::PlaybackAttributionProfile;

// ---------------------------------------------------------------------------
// In-memory library cache
// ---------------------------------------------------------------------------

/// In-memory library cache populated from the DAAP server.
struct LibraryCache {
    scope: Option<DaapCatalogueScope>,
    tracks: Vec<Track>,
    /// DAAP item ID → server-declared stream format. The raw `Option` is
    /// preserved: `None` means the server never declared `asfm`, and the
    /// resolved representation must stay explicitly unknown instead of
    /// inheriting a guessed default (issue #255).
    format_by_daap_id: HashMap<u32, Option<String>>,
    /// DAAP item ID → Last.fm attribution profile derived from the raw
    /// accepted protocol row before display fallbacks were substituted.
    attribution_profiles: HashMap<TrackId, PlaybackAttributionProfile>,
}

impl LibraryCache {
    fn empty() -> Self {
        Self {
            scope: None,
            tracks: Vec::new(),
            format_by_daap_id: HashMap::new(),
            attribution_profiles: HashMap::new(),
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
    client: DaapClient,
    cache: RwLock<LibraryCache>,
}

impl DaapBackend {
    /// Login to a DAAP server and return the first close-capable adapter.
    ///
    /// # Arguments
    /// * `server_url` — base URL including scheme (e.g. `http://192.168.1.50:3689`)
    /// * `password` — optional share password
    pub async fn login(server_url: &str, password: Option<&str>) -> BackendResult<Self> {
        Self::login_with_route(server_url, password, None).await
    }

    /// Connect through a retained mDNS route without replacing the advertised
    /// hostname in the DAAP origin.
    pub(crate) async fn login_with_route(
        server_url: &str,
        password: Option<&str>,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
        let client = DaapClient::login_with_route(server_url, password, advertised_route).await?;
        Ok(Self {
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
        let mut format_by_daap_id = HashMap::new();
        let mut attribution_profiles = HashMap::new();

        for nodes in mlit_items {
            let nodes = nodes.as_slice();
            let Some(daap_id) = dmap::find_u32(nodes, b"miid") else {
                continue; // Skip items without an ID.
            };

            let raw_title = dmap::find_string(nodes, b"minm");
            let raw_artist_name = dmap::find_string(nodes, b"asar");
            let raw_album_title = dmap::find_string(nodes, b"asal");
            let title = raw_title.clone().unwrap_or_else(|| "Unknown".to_string());
            let artist_name = raw_artist_name
                .clone()
                .unwrap_or_else(|| "Unknown".to_string());
            let album_title = raw_album_title.clone().unwrap_or_default();
            let duration_ms = dmap::find_u32(nodes, b"astm");
            let track_number = dmap::find_u16(nodes, b"astn");
            let disc_number = dmap::find_u16(nodes, b"asdn");
            let genre = dmap::find_string(nodes, b"asgn");
            let year = dmap::find_u16(nodes, b"asyr");
            let format = dmap::find_string(nodes, b"asfm");
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

            let native_track_id = crate::architecture::TrackId::remote(daap_id.to_string()).ok();
            let track = Track {
                id: track_uuid,
                native_track_id: native_track_id.clone(),
                title,
                artist_name,
                album_artist_name: None,
                artist_id: Some(artist_uuid),
                album_title,
                album_id: Some(album_uuid),
                track_number: track_number.map(u32::from),
                disc_number: disc_number.map(u32::from),
                duration_secs,
                composer: None,
                genre,
                year: year.map(i32::from),
                file_path: None,
                stream_url: None,
                cover_art_url: None,
                date_added: None,
                date_modified,
                bitrate_kbps: bitrate.map(u32::from),
                sample_rate_hz: sample_rate,
                format,
                play_count: None,
                rating: TrackRating::unsupported(),
                last_played: None,
            };

            // Attribution provenance is frozen from the raw accepted row
            // before the display fallbacks above substitute any "Unknown",
            // so a synthesized fallback can never become attribution
            // authority.
            if let Some(track_id) = &native_track_id {
                if let Some(profile) = PlaybackAttributionProfile::from_remote_row(
                    raw_title,
                    raw_artist_name,
                    raw_album_title,
                    None,
                    track_number.map(u32::from),
                    duration_secs,
                ) {
                    attribution_profiles.insert(track_id.clone(), profile);
                }
            }
            all_tracks.push(track);
        }

        info!(tracks = all_tracks.len(), "DAAP library loaded");

        let mut cache = self.cache.write().await;
        *cache = LibraryCache {
            scope: Some(scope),
            tracks: all_tracks,
            format_by_daap_id,
            attribution_profiles,
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
        self.client
            .stream_request(scope, song_id, format.as_deref())
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

    /// Return the exact Last.fm attribution profile retained for one accepted
    /// catalogue row by its native identity.
    ///
    /// Profiles are derived from the raw protocol row during refresh, before
    /// display fallbacks are substituted, so a synthesized `"Unknown"` can
    /// never become attribution authority. The lookup is deliberately
    /// non-blocking: a contended refresh returns `None`, so Last.fm
    /// attribution fails closed instead of waiting on the lifecycle state
    /// lock that the registry holds while minting.
    pub(crate) fn catalogue_attribution_profile(
        &self,
        track_id: &TrackId,
    ) -> Option<PlaybackAttributionProfile> {
        let cache = self.cache.try_read().ok()?;
        cache.attribution_profiles.get(track_id).cloned()
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

/// Prove the attribution lookup fails closed while a refresh holds the cache
/// write lock, then succeeds once the cache is readable again. Lives beside
/// the cache field so the real-socket lifecycle test suite can exercise
/// contention without widening field visibility.
#[cfg(test)]
pub(super) async fn assert_attribution_fails_closed_while_cache_contended(
    backend: &DaapBackend,
    track_id: &TrackId,
) {
    let guard = backend.cache.write().await;
    assert!(
        backend.catalogue_attribution_profile(track_id).is_none(),
        "contended cache must refuse attribution instead of blocking"
    );
    drop(guard);
    assert!(
        backend.catalogue_attribution_profile(track_id).is_some(),
        "uncontended cache must serve the retained profile again"
    );
}

/// Admit one canonical DAAP item identity. A duplicate `miid` is ambiguous:
/// silently overwriting it could bind catalogue metadata to another row's
/// stream, so reject the complete candidate catalogue before publication.
fn remember_track_format(
    formats: &mut HashMap<u32, Option<String>>,
    daap_id: u32,
    format: Option<String>,
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
    async fn list_tracks(&self) -> BackendResult<Vec<Track>> {
        Ok(self.cache.read().await.tracks.clone())
    }

    fn rating_capability(&self) -> RatingCapability {
        RatingCapability::Unsupported
    }
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
    use crate::architecture::media::{MediaContainer, MediaRepresentation};

    /// A backend whose cache holds exactly what catalogue ingest would store
    /// for one track with the given server-declared `asfm` (`None` when the
    /// server never declared one). Resolution-site methods build requests
    /// offline; no network is involved.
    fn backend_with_declared_format(song_id: u32, format: Option<&str>) -> DaapBackend {
        let mut cache = LibraryCache::empty();
        cache.scope = Some(DaapCatalogueScope::for_adapter_tests());
        cache
            .format_by_daap_id
            .insert(song_id, format.map(str::to_string));
        DaapBackend {
            client: DaapClient::for_adapter_tests("http://198.51.100.10:3689/"),
            cache: RwLock::new(cache),
        }
    }

    #[tokio::test]
    async fn declared_allowlisted_format_resolves_to_the_matching_descriptor() {
        let backend = backend_with_declared_format(7, Some("flac"));
        let track_id = TrackId::remote("7").expect("track id");

        let request = backend
            .stream_request_for_native_id(&track_id)
            .await
            .expect("stream resolution");

        assert_eq!(
            request.representation(),
            MediaRepresentation::buffered(MediaContainer::Flac)
        );
        assert!(request
            .endpoint()
            .path()
            .ends_with("/databases/1/items/7.flac"));
    }

    #[tokio::test]
    async fn non_allowlisted_declared_format_resolves_to_explicit_unknown() {
        let backend = backend_with_declared_format(7, Some("shn"));
        let track_id = TrackId::remote("7").expect("track id");

        let request = backend
            .stream_request_for_native_id(&track_id)
            .await
            .expect("stream resolution");

        assert_eq!(
            request.representation(),
            MediaRepresentation::buffered_unknown()
        );
        assert!(request
            .endpoint()
            .path()
            .ends_with("/databases/1/items/7.shn"));
    }

    #[tokio::test]
    async fn absent_declared_format_stays_unknown_instead_of_guessing_mp3() {
        // The cache stores the raw `None` — the historical behaviour stored
        // a defaulted "mp3" here, which leaked `audio/mpeg` LOAD descriptors
        // and `.mp3` ticket suffixes onto media of unknown container.
        let backend = backend_with_declared_format(7, None);
        let track_id = TrackId::remote("7").expect("track id");

        let request = backend
            .stream_request_for_native_id(&track_id)
            .await
            .expect("stream resolution");

        let representation = request.representation();
        assert_eq!(representation, MediaRepresentation::buffered_unknown());
        assert_eq!(representation.content_type(), None);
        assert_eq!(representation.ticket_suffix(), None);
        // The legacy URL item hint is unchanged; the descriptor is not.
        assert!(request
            .endpoint()
            .path()
            .ends_with("/databases/1/items/7.mp3"));
    }

    #[test]
    fn duplicate_daap_item_identity_fails_closed_without_overwriting_first_row() {
        let mut formats = HashMap::new();
        remember_track_format(&mut formats, 7, Some("flac".to_string())).expect("first identity");

        let error = remember_track_format(&mut formats, 7, Some("mp3".to_string()))
            .expect_err("duplicate identity must reject the candidate catalogue");

        assert!(matches!(
            error,
            BackendError::ParseError {
                ref message,
                source: None
            } if message == "DAAP catalogue contains duplicate item identity"
        ));
        assert_eq!(formats.get(&7).and_then(Option::as_deref), Some("flac"));
    }
}
