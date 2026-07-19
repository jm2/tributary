//! Production lifecycle service for every managed media source.
//!
//! Authenticated remotes, the built-in Radio-Browser adapter, mounted
//! removable media, and ephemeral OS-opened files share one source/session
//! authority. Catalogue rows and playback queues retain only
//! `(SourceId, TrackId)`, an optional `ViewOrigin`, and the non-secret epoch
//! that published them. Protected requests, public locators, retained
//! roots/files, credentials, leases, and adapter state stay behind this
//! boundary until media use.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::architecture::backend::{
    validate_catalogue_rating_capability, BackendResult, MediaBackend,
};
use crate::architecture::error::BackendError;
use crate::architecture::media::{
    MediaRequest, PublicHttpAuthority, PublicHttpEndpoint, RemoteMediaResolver,
    ResolvedHttpRequest, ResolvedPublicHttpRequest,
};
use crate::architecture::models::{RatingCapability, Track};
use crate::architecture::{MediaKey, SourceId, TrackId, ViewOrigin};
use crate::external_file::{ExternalFileCandidate, ExternalFileHint};
use crate::local::resolver::ResolvedFileMedia;
use crate::source_lifecycle::{
    AdapterCloseFuture, AdapterStream, AdapterTaskResult, CatalogueCommitAuthority,
    CatalogueCommitRequest, CloseAuthority, ConstructionCancellationPolicy, FailureCategory,
    LifecycleAdapter, LifecycleBaseline, LifecycleSnapshot, ProvenanceClaimId, RefreshLane,
    RefreshTaskResult, RetirementWaiter, ShutdownBarrier, SourceLifecycleRegistry,
    SourceProvenance,
};
use url::Url;

pub type CatalogueFuture =
    Pin<Box<dyn Future<Output = BackendResult<Vec<Track>>> + Send + 'static>>;
pub type ViewFuture = Pin<Box<dyn Future<Output = ViewLoadResult> + Send + 'static>>;
pub type StreamFuture =
    Pin<Box<dyn Future<Output = BackendResult<AdapterStream>> + Send + 'static>>;
type ArtworkFuture =
    Pin<Box<dyn Future<Output = BackendResult<Option<ResolvedHttpRequest>>> + Send + 'static>>;

/// At-use stream resolved through the centralized source authority.
///
/// Existing authenticated and public-radio requests retain their exact
/// `MediaRequest` behavior inside `Http`; filesystem adapters return a
/// path-free retained capability in `File`.
pub enum ResolvedSourceStream {
    Http(MediaRequest),
    File(ResolvedFileMedia),
}

/// Whether one managed adapter permits Tributary-owned regular-playlist
/// membership over its accepted source-wide catalogue.
///
/// This capability does not represent server-native playlist synchronization
/// and is never sufficient authority by itself. Registry operations also
/// require an exact active catalogue, session, generation, and track identity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RegularPlaylistCapability {
    #[default]
    Unsupported,
    SourceScopedEntries,
}

/// Transient identity of one exact accepted catalogue.
///
/// Guards are minted only by a successful registry lookup and must never be
/// persisted. A session replacement changes `session_epoch`; a same-session
/// catalogue replacement changes `catalogue_generation`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegularPlaylistCatalogueGuard {
    source_id: SourceId,
    session_epoch: u64,
    catalogue_generation: u64,
}

impl RegularPlaylistCatalogueGuard {
    pub const fn source_id(self) -> SourceId {
        self.source_id
    }

    pub const fn session_epoch(self) -> u64 {
        self.session_epoch
    }

    pub const fn catalogue_generation(self) -> u64 {
        self.catalogue_generation
    }
}

/// Fixed, non-sensitive reason that a persisted source-scoped identity cannot
/// currently be projected from a regular playlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegularPlaylistUnavailableReason {
    SourceUnavailable,
    UnsupportedSource,
    InvalidCatalogue,
    TrackMissing,
}

/// Closed failure returned by guarded regular-playlist media resolution.
/// Raw adapter errors, URLs, credentials, and native IDs never cross this
/// boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RegularPlaylistMediaError {
    #[error("regular playlist media authority is unavailable")]
    Unavailable,
    #[error("regular playlist media backend failed")]
    BackendFailure(FailureCategory),
}

/// One available regular-playlist track resolved from an exact live catalogue.
#[derive(Clone)]
pub struct RegularPlaylistTrack {
    media_key: MediaKey,
    guard: RegularPlaylistCatalogueGuard,
    metadata: RegularPlaylistTrackMetadata,
}

impl RegularPlaylistTrack {
    pub fn media_key(&self) -> &MediaKey {
        &self.media_key
    }

    pub const fn guard(&self) -> RegularPlaylistCatalogueGuard {
        self.guard
    }

    pub fn metadata(&self) -> &RegularPlaylistTrackMetadata {
        &self.metadata
    }

    /// Construct a registry-shaped available result for cross-module UI
    /// tests without making catalogue guards forgeable in production.
    #[cfg(test)]
    pub(crate) fn for_ui_test(
        media_key: MediaKey,
        session_epoch: u64,
        catalogue_generation: u64,
        track: &Track,
    ) -> Self {
        assert_ne!(media_key.source_id, SourceId::local());
        assert_ne!(session_epoch, 0);
        assert_ne!(catalogue_generation, 0);
        Self {
            guard: RegularPlaylistCatalogueGuard {
                source_id: media_key.source_id,
                session_epoch,
                catalogue_generation,
            },
            media_key,
            metadata: RegularPlaylistTrackMetadata::from_track(track),
        }
    }
}

/// Whitelisted display, sorting, rating, and history metadata for a regular
/// playlist row.
///
/// This is deliberately not a `Track` clone. Adding a new field to `Track`
/// cannot silently expose a locator, credential, or future private adapter
/// detail across the playlist authority boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct RegularPlaylistTrackMetadata {
    title: String,
    artist_name: String,
    album_artist_name: Option<String>,
    album_title: String,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    duration_secs: Option<u64>,
    composer: Option<String>,
    genre: Option<String>,
    year: Option<i32>,
    date_added: Option<chrono::DateTime<chrono::Utc>>,
    date_modified: Option<chrono::DateTime<chrono::Utc>>,
    bitrate_kbps: Option<u32>,
    sample_rate_hz: Option<u32>,
    format: Option<String>,
    play_count: Option<u32>,
    rating: crate::architecture::models::TrackRating,
    last_played: Option<chrono::DateTime<chrono::Utc>>,
}

impl RegularPlaylistTrackMetadata {
    fn from_track(track: &Track) -> Self {
        Self {
            title: track.title.clone(),
            artist_name: track.artist_name.clone(),
            album_artist_name: track.album_artist_name.clone(),
            album_title: track.album_title.clone(),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_secs: track.duration_secs,
            composer: track.composer.clone(),
            genre: track.genre.clone(),
            year: track.year,
            date_added: track.date_added,
            date_modified: track.date_modified,
            bitrate_kbps: track.bitrate_kbps,
            sample_rate_hz: track.sample_rate_hz,
            format: track.format.clone(),
            play_count: track.play_count,
            rating: track.rating,
            last_played: track.last_played,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn artist_name(&self) -> &str {
        &self.artist_name
    }

    pub fn album_artist_name(&self) -> Option<&str> {
        self.album_artist_name.as_deref()
    }

    pub fn album_title(&self) -> &str {
        &self.album_title
    }

    pub const fn track_number(&self) -> Option<u32> {
        self.track_number
    }

    pub const fn disc_number(&self) -> Option<u32> {
        self.disc_number
    }

    pub const fn duration_secs(&self) -> Option<u64> {
        self.duration_secs
    }

    pub fn composer(&self) -> Option<&str> {
        self.composer.as_deref()
    }

    pub fn genre(&self) -> Option<&str> {
        self.genre.as_deref()
    }

    pub const fn year(&self) -> Option<i32> {
        self.year
    }

    // Kept in the explicit catalogue whitelist for future source-aware sort
    // projections; Record B does not yet expose a Date Added column.
    #[allow(dead_code)]
    pub fn date_added(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.date_added
    }

    pub fn date_modified(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.date_modified
    }

    pub const fn bitrate_kbps(&self) -> Option<u32> {
        self.bitrate_kbps
    }

    pub const fn sample_rate_hz(&self) -> Option<u32> {
        self.sample_rate_hz
    }

    pub fn format(&self) -> Option<&str> {
        self.format.as_deref()
    }

    pub const fn play_count(&self) -> Option<u32> {
        self.play_count
    }

    pub const fn rating(&self) -> crate::architecture::models::TrackRating {
        self.rating
    }

    // Remote history is intentionally read-only. The value may cross this
    // sanitized boundary even though local-only history UI does not consume it.
    #[allow(dead_code)]
    pub fn last_played(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.last_played
    }
}

/// One unavailable regular-playlist identity. Its optional private guard lets
/// the registry distinguish a still-current missing/unsupported catalogue
/// observation from a replacement without exposing lifecycle internals.
#[derive(Clone)]
pub struct RegularPlaylistUnavailable {
    media_key: MediaKey,
    reason: RegularPlaylistUnavailableReason,
    observed_guard: Option<RegularPlaylistCatalogueGuard>,
}

impl RegularPlaylistUnavailable {
    pub fn media_key(&self) -> &MediaKey {
        &self.media_key
    }

    pub const fn reason(&self) -> RegularPlaylistUnavailableReason {
        self.reason
    }
}

/// Ordered result of resolving one persisted source-scoped playlist identity.
#[derive(Clone)]
pub enum RegularPlaylistTrackResolution {
    Available(Box<RegularPlaylistTrack>),
    Unavailable(RegularPlaylistUnavailable),
}

impl RegularPlaylistTrackResolution {
    pub fn media_key(&self) -> &MediaKey {
        match self {
            Self::Available(track) => track.media_key(),
            Self::Unavailable(track) => track.media_key(),
        }
    }
}

impl ResolvedSourceStream {
    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        match self {
            Self::Http(request) => request.is_active(),
            Self::File(media) => media.is_active(),
        }
    }
}

/// One exact public locator contribution owned by an accepted source view.
pub struct PublicStreamContribution {
    track_id: TrackId,
    endpoint: PublicHttpEndpoint,
}

impl PublicStreamContribution {
    pub fn new(track_id: TrackId, endpoint: Url) -> BackendResult<Self> {
        Ok(Self {
            track_id,
            endpoint: PublicHttpEndpoint::new(endpoint)?,
        })
    }
}

/// Immutable pathless tracks produced by one catalogue or named view load.
///
/// Public locator contributions are consumed into a private registry payload
/// before publication. Snapshot projections contain only `tracks`.
pub struct AcceptedView {
    tracks: Arc<Vec<Track>>,
    public_streams: HashMap<TrackId, PublicHttpEndpoint>,
}

impl AcceptedView {
    pub fn public_http(
        tracks: Arc<Vec<Track>>,
        contributions: Vec<PublicStreamContribution>,
    ) -> BackendResult<Self> {
        let mut track_ids = HashSet::with_capacity(tracks.len());
        for track in tracks.iter() {
            if track.file_path.is_some()
                || track.stream_url.is_some()
                || track.cover_art_url.is_some()
            {
                return Err(BackendError::Internal(anyhow::anyhow!(
                    "accepted public view contains a concrete media locator"
                )));
            }
            let track_id = track.native_track_id.clone().ok_or_else(|| {
                BackendError::Internal(anyhow::anyhow!(
                    "accepted public view track has no native identity"
                ))
            })?;
            if !track_ids.insert(track_id) {
                return Err(BackendError::Internal(anyhow::anyhow!(
                    "accepted public view contains duplicate track identity"
                )));
            }
        }

        let mut public_streams = HashMap::with_capacity(contributions.len());
        for contribution in contributions {
            if !track_ids.contains(&contribution.track_id) {
                return Err(BackendError::Internal(anyhow::anyhow!(
                    "accepted public view contains an orphan locator"
                )));
            }
            if public_streams
                .insert(contribution.track_id, contribution.endpoint)
                .is_some()
            {
                return Err(BackendError::Internal(anyhow::anyhow!(
                    "accepted public view contains duplicate locator ownership"
                )));
            }
        }
        if public_streams.len() != track_ids.len() {
            return Err(BackendError::Internal(anyhow::anyhow!(
                "accepted public view is missing a track locator"
            )));
        }

        Ok(Self {
            tracks,
            public_streams,
        })
    }

    fn published(tracks: Arc<Vec<Track>>) -> Self {
        Self {
            tracks,
            public_streams: HashMap::new(),
        }
    }

    pub fn tracks(&self) -> &[Track] {
        self.tracks.as_slice()
    }

    #[cfg(test)]
    pub fn tracks_arc(&self) -> Arc<Vec<Track>> {
        Arc::clone(&self.tracks)
    }
}

/// Closed adapter result for one cancellation-aware named view load.
pub enum ViewLoadResult {
    Loaded(AcceptedView),
    Failed(FailureCategory),
    Cancelled,
}

struct AcceptedSourcePayload {
    tracks: Arc<Vec<Track>>,
    public_streams: HashMap<TrackId, PublicHttpEndpoint>,
    regular_playlist_capability: RegularPlaylistCapability,
    regular_playlist_index: RegularPlaylistTrackIndex,
}

#[derive(Clone)]
enum RegularPlaylistTrackIndex {
    Unsupported,
    Invalid,
    Exact(Arc<HashMap<TrackId, usize>>),
}

impl AcceptedSourcePayload {
    fn from_view(view: AcceptedView) -> Self {
        Self {
            tracks: view.tracks,
            public_streams: view.public_streams,
            regular_playlist_capability: RegularPlaylistCapability::Unsupported,
            regular_playlist_index: RegularPlaylistTrackIndex::Unsupported,
        }
    }

    fn catalogue(tracks: Vec<Track>, capability: RegularPlaylistCapability) -> Self {
        let regular_playlist_index = match capability {
            RegularPlaylistCapability::Unsupported => RegularPlaylistTrackIndex::Unsupported,
            RegularPlaylistCapability::SourceScopedEntries => {
                let mut exact = HashMap::with_capacity(tracks.len());
                let valid = tracks.iter().enumerate().all(|(index, track)| {
                    track
                        .native_track_id
                        .as_ref()
                        .is_some_and(|track_id| exact.insert(track_id.clone(), index).is_none())
                });
                if valid {
                    RegularPlaylistTrackIndex::Exact(Arc::new(exact))
                } else {
                    RegularPlaylistTrackIndex::Invalid
                }
            }
        };
        Self {
            tracks: Arc::new(tracks),
            public_streams: HashMap::new(),
            regular_playlist_capability: capability,
            regular_playlist_index,
        }
    }

    fn published(&self) -> AcceptedView {
        AcceptedView::published(Arc::clone(&self.tracks))
    }

    fn regular_playlist_track(&self, track_id: &TrackId) -> Option<&Track> {
        if self.regular_playlist_capability != RegularPlaylistCapability::SourceScopedEntries {
            return None;
        }
        let RegularPlaylistTrackIndex::Exact(index) = &self.regular_playlist_index else {
            return None;
        };
        index
            .get(track_id)
            .and_then(|index| self.tracks.get(*index))
    }
}

/// Heterogeneous operational contract stored by one lifecycle registry.
pub trait ManagedSourceAdapter: LifecycleAdapter + Send + Sync {
    /// Explicit opt-in for Tributary-owned source-scoped regular playlists.
    /// Future adapters remain denied until their accepted catalogue and media
    /// resolver satisfy the same exact-ID authority contract.
    fn regular_playlist_capability(&self) -> RegularPlaylistCapability {
        RegularPlaylistCapability::Unsupported
    }

    /// Load the first complete catalogue after construction is staged.
    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture;

    /// Load one named view while observing exact generation cancellation.
    fn load_view(
        self: Arc<Self>,
        _view: ViewOrigin,
        _cancellation: crate::source_lifecycle::CancellationObserver,
    ) -> ViewFuture {
        Box::pin(async { ViewLoadResult::Failed(FailureCategory::Backend) })
    }

    fn resolve_stream(self: Arc<Self>, _track_id: TrackId) -> StreamFuture {
        Box::pin(async {
            Err(BackendError::Unsupported {
                operation: "stream resolution".to_string(),
            })
        })
    }

    fn resolve_artwork(self: Arc<Self>, _track_id: TrackId) -> ArtworkFuture {
        Box::pin(async { Ok(None) })
    }
}

mod source_scoped_playlist_sealed {
    pub trait Adapter {}
}

fn source_scoped_playlist_capability<A>() -> RegularPlaylistCapability
where
    A: source_scoped_playlist_sealed::Adapter,
{
    RegularPlaylistCapability::SourceScopedEntries
}

macro_rules! standard_remote_adapter {
    ($adapter:ty, $regular_playlist_capability:expr) => {
        impl LifecycleAdapter for $adapter {
            fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
                Box::pin(async { Ok(()) })
            }
        }

        impl ManagedSourceAdapter for $adapter {
            fn regular_playlist_capability(&self) -> RegularPlaylistCapability {
                $regular_playlist_capability
            }

            fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
                Box::pin(
                    async move { crate::architecture::load_track_catalog(self.as_ref()).await },
                )
            }

            fn resolve_stream(self: Arc<Self>, track_id: TrackId) -> StreamFuture {
                Box::pin(async move {
                    RemoteMediaResolver::resolve_stream(self.as_ref(), &track_id)
                        .await
                        .map(|request| AdapterStream::ProtectedHttp(Box::new(request)))
                })
            }

            fn resolve_artwork(self: Arc<Self>, track_id: TrackId) -> ArtworkFuture {
                Box::pin(async move {
                    RemoteMediaResolver::resolve_artwork(self.as_ref(), &track_id).await
                })
            }
        }
    };
}

impl source_scoped_playlist_sealed::Adapter for crate::subsonic::SubsonicBackend {}
standard_remote_adapter!(
    crate::subsonic::SubsonicBackend,
    source_scoped_playlist_capability::<crate::subsonic::SubsonicBackend>()
);
// Plex's legacy auth token is a durable credential, not a revocable server
// session: its documented revocation mechanisms are account/device-wide, so
// Tributary has no safe per-adapter close authority. Constructors may therefore
// be aborted, while disconnect only revokes local media/session authority.
impl source_scoped_playlist_sealed::Adapter for crate::plex::PlexBackend {}
standard_remote_adapter!(
    crate::plex::PlexBackend,
    source_scoped_playlist_capability::<crate::plex::PlexBackend>()
);

impl LifecycleAdapter for crate::jellyfin::JellyfinBackend {
    fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
        Box::pin(async move {
            self.logout_owned_session()
                .await
                .map_err(|error| failure_category(&error))
        })
    }
}

impl ManagedSourceAdapter for crate::jellyfin::JellyfinBackend {
    fn regular_playlist_capability(&self) -> RegularPlaylistCapability {
        source_scoped_playlist_capability::<Self>()
    }

    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move {
            self.ensure_initialized().await?;
            crate::architecture::load_track_catalog(self.as_ref()).await
        })
    }

    fn resolve_stream(self: Arc<Self>, track_id: TrackId) -> StreamFuture {
        Box::pin(async move {
            RemoteMediaResolver::resolve_stream(self.as_ref(), &track_id)
                .await
                .map(|request| AdapterStream::ProtectedHttp(Box::new(request)))
        })
    }

    fn resolve_artwork(self: Arc<Self>, track_id: TrackId) -> ArtworkFuture {
        Box::pin(
            async move { RemoteMediaResolver::resolve_artwork(self.as_ref(), &track_id).await },
        )
    }
}

impl source_scoped_playlist_sealed::Adapter for crate::jellyfin::JellyfinBackend {}

mod sealed {
    pub trait AbortableSourceAdapter {}
}

/// Marker for constructors whose cancellation cannot strand lifecycle-owned,
/// individually closeable server state. DAAP and interactive Jellyfin login
/// deliberately cannot satisfy it. Plex's legacy durable credential has no
/// safe per-token close operation and is documented separately above.
pub trait AbortableSourceAdapter: ManagedSourceAdapter + sealed::AbortableSourceAdapter {}

macro_rules! abortable_remote_adapter {
    ($adapter:ty) => {
        impl sealed::AbortableSourceAdapter for $adapter {}
        impl AbortableSourceAdapter for $adapter {}
    };
}

abortable_remote_adapter!(crate::subsonic::SubsonicBackend);
abortable_remote_adapter!(crate::plex::PlexBackend);

/// Preserve DAAP's session-aware loader while applying the same catalogue
/// rating invariant as the standard dynamic-dispatch publication path.
fn validate_daap_initial_catalogue(
    tracks: Vec<Track>,
    capability: RatingCapability,
) -> BackendResult<Vec<Track>> {
    validate_catalogue_rating_capability(&tracks, capability)?;
    Ok(tracks)
}

impl ManagedSourceAdapter for crate::daap::DaapBackend {
    fn regular_playlist_capability(&self) -> RegularPlaylistCapability {
        source_scoped_playlist_capability::<Self>()
    }

    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move {
            let tracks = self.load_catalogue().await?;
            validate_daap_initial_catalogue(tracks, self.rating_capability())
        })
    }

    fn resolve_stream(self: Arc<Self>, track_id: TrackId) -> StreamFuture {
        Box::pin(async move {
            RemoteMediaResolver::resolve_stream(self.as_ref(), &track_id)
                .await
                .map(|request| AdapterStream::ProtectedHttp(Box::new(request)))
        })
    }

    fn resolve_artwork(self: Arc<Self>, track_id: TrackId) -> ArtworkFuture {
        Box::pin(
            async move { RemoteMediaResolver::resolve_artwork(self.as_ref(), &track_id).await },
        )
    }
}

impl source_scoped_playlist_sealed::Adapter for crate::daap::DaapBackend {}

struct BuiltInInstallation {
    _claim_id: ProvenanceClaimId,
    session_epoch: Option<u64>,
}

struct SourceRegistryInner {
    lifecycle: SourceLifecycleRegistry<dyn ManagedSourceAdapter, AcceptedSourcePayload>,
    runtime: tokio::runtime::Handle,
    built_ins: Mutex<HashMap<SourceId, BuiltInInstallation>>,
    external_sessions: Mutex<HashMap<SourceId, ProvenanceClaimId>>,
}

impl PublicHttpAuthority for SourceRegistryInner {
    fn is_current_public_stream(
        &self,
        source_id: SourceId,
        session_epoch: u64,
        winner_generation: u64,
        track_id: &TrackId,
    ) -> bool {
        self.lifecycle.is_current_latest_accepted_view(
            source_id,
            session_epoch,
            winner_generation,
            |payload| payload.public_streams.contains_key(track_id).then_some(()),
        )
    }
}

/// Cloneable application service around the centralized lifecycle authority.
#[derive(Clone)]
pub struct SourceRegistry {
    inner: Arc<SourceRegistryInner>,
}

/// Opaque authority retaining every remote session and catalogue selected for
/// one regular-playlist database commit.
///
/// The lifecycle permit is declared before the registry owner so it is
/// released first during drop. This prevents final-handle teardown from
/// waiting on a permit owned by the value currently being destroyed.
#[must_use = "regular-playlist commit authority must be retained through the database commit"]
pub struct RegularPlaylistCommitAuthority {
    #[allow(dead_code)] // Retention through Drop is the authority operation.
    authority: CatalogueCommitAuthority,
    _registry: Arc<SourceRegistryInner>,
}

impl RegularPlaylistCommitAuthority {
    #[cfg(test)]
    fn permit_count(&self) -> usize {
        self.authority.permit_count()
    }

    #[cfg(test)]
    fn revocation_started(&self) -> bool {
        self.authority.revocation_started()
    }
}

/// Pathless identity and catalogue projection for one admitted OS-opened
/// file. The registry retains all locator and provenance authority.
#[derive(Clone)]
pub struct ExternalFileSession {
    source_id: SourceId,
    track_id: TrackId,
    session_epoch: u64,
    track: Track,
    #[cfg(test)]
    close_probe: Arc<std::sync::atomic::AtomicUsize>,
}

impl ExternalFileSession {
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn track_id(&self) -> &TrackId {
        &self.track_id
    }

    pub const fn session_epoch(&self) -> u64 {
        self.session_epoch
    }

    pub fn track(&self) -> &Track {
        &self.track
    }

    #[cfg(test)]
    fn close_calls(&self) -> usize {
        self.close_probe.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl std::fmt::Debug for ExternalFileSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExternalFileSession")
            .field("source_id", &self.source_id)
            .field("track_id", &self.track_id)
            .field("session_epoch", &self.session_epoch)
            .finish_non_exhaustive()
    }
}

impl SourceRegistry {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        let lifecycle = SourceLifecycleRegistry::new(runtime.clone());
        let source_id = SourceId::radio_browser();
        let claim_id = lifecycle
            .claim_provenance(source_id, SourceProvenance::BuiltIn)
            .expect("new lifecycle registry admits its built-in source");
        let mut built_ins = HashMap::new();
        built_ins.insert(
            source_id,
            BuiltInInstallation {
                _claim_id: claim_id,
                session_epoch: None,
            },
        );
        Self {
            inner: Arc::new(SourceRegistryInner {
                lifecycle,
                runtime,
                built_ins: Mutex::new(built_ins),
                external_sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Admit one already-open OS-delivered regular file as an independent,
    /// hidden, ephemeral lifecycle source.
    ///
    /// Exact-handle parsing and all validation finish before random identity
    /// is minted. The returned track is pathless and carries the exact epoch
    /// required by [`Self::resolve_stream`]. This synchronous function is
    /// `Send`-compatible and intended to run on a bounded blocking worker.
    #[cfg(test)]
    fn adopt_external_file(
        &self,
        file: File,
        hint: ExternalFileHint,
    ) -> BackendResult<ExternalFileSession> {
        self.adopt_external_file_if_current(file, hint, || true)
    }

    /// Admit one exact candidate only if its delivery still owns playback at
    /// the source-publication boundary.
    ///
    /// Validation and parsing happen before this predicate is evaluated. The
    /// predicate then runs while the same gate that serializes registry
    /// shutdown is held, before identity is minted or an adapter is created.
    pub(crate) fn adopt_external_file_if_current<IsCurrent>(
        &self,
        file: File,
        hint: ExternalFileHint,
        is_current: IsCurrent,
    ) -> BackendResult<ExternalFileSession>
    where
        IsCurrent: FnOnce() -> bool,
    {
        self.adopt_external_file_inner(file, hint, || {}, || {}, is_current)
    }

    fn adopt_external_file_inner<OnValidated, OnGate, IsCurrent>(
        &self,
        file: File,
        hint: ExternalFileHint,
        on_validated: OnValidated,
        on_gate_acquired: OnGate,
        is_current: IsCurrent,
    ) -> BackendResult<ExternalFileSession>
    where
        OnValidated: FnOnce(),
        OnGate: FnOnce(),
        IsCurrent: FnOnce() -> bool,
    {
        let candidate = ExternalFileCandidate::validate(file, hint)?;
        on_validated();

        // Publication and explicit shutdown share this lock. Validation and
        // parsing stay outside it; only the bounded lifecycle transaction is
        // serialized.
        let mut external_sessions = lock(&self.inner.external_sessions);
        on_gate_acquired();
        if self.inner.lifecycle.is_shutting_down() || !is_current() {
            return Err(closed_external_admission_error());
        }
        let adapter = candidate.into_adapter();
        let source_id = adapter.source_id();
        let track_id = adapter.track_id().clone();
        let track = adapter.track().clone();
        let regular_playlist_capability = adapter.regular_playlist_capability();
        #[cfg(test)]
        let close_probe = adapter.close_probe();
        let claim_id = self
            .inner
            .lifecycle
            .claim_provenance(source_id, SourceProvenance::External)
            .ok_or_else(closed_external_admission_error)?;
        let adopted = self.inner.lifecycle.adopt_stateless_session(
            source_id,
            Box::new(adapter),
            AcceptedSourcePayload::catalogue(vec![track.clone()], regular_playlist_capability),
        );
        let Some((_, session_epoch)) = adopted else {
            let _ = self.inner.lifecycle.release_provenance(source_id, claim_id);
            self.inner
                .lifecycle
                .schedule_prune_after_current_retirement(source_id);
            return Err(closed_external_admission_error());
        };

        let replaced = external_sessions.insert(source_id, claim_id);
        debug_assert!(
            replaced.is_none(),
            "random external source identity is unique"
        );
        Ok(ExternalFileSession {
            source_id,
            track_id,
            session_epoch,
            track,
            #[cfg(test)]
            close_probe,
        })
    }

    #[cfg(test)]
    fn adopt_external_file_with_gate_hook<OnGate>(
        &self,
        file: File,
        hint: ExternalFileHint,
        on_gate_acquired: OnGate,
    ) -> BackendResult<ExternalFileSession>
    where
        OnGate: FnOnce(),
    {
        self.adopt_external_file_inner(file, hint, || {}, on_gate_acquired, || true)
    }

    #[cfg(test)]
    fn adopt_external_file_with_validation_hook<OnValidated>(
        &self,
        file: File,
        hint: ExternalFileHint,
        on_validated: OnValidated,
    ) -> BackendResult<ExternalFileSession>
    where
        OnValidated: FnOnce(),
    {
        self.adopt_external_file_inner(file, hint, on_validated, || {}, || true)
    }

    /// Revoke and retire one exact external-file session.
    ///
    /// Claim removal is serialized before lifecycle teardown, making repeated
    /// stop/EOS/failure/shutdown hooks harmless. Hidden baseline rows are not
    /// owners and must not call this method merely because they are hidden.
    pub fn retire_external(&self, source_id: SourceId) -> Option<RetirementWaiter> {
        let claim_id = lock(&self.inner.external_sessions).remove(&source_id)?;
        let waiter = self.inner.lifecycle.disconnect(source_id);
        let _ = self.inner.lifecycle.release_provenance(source_id, claim_id);
        self.inner
            .lifecycle
            .schedule_prune_after_current_retirement(source_id);
        waiter
    }

    pub fn subscribe_invalidations(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.lifecycle.subscribe_invalidations()
    }

    pub fn claim_provenance(
        &self,
        source_id: SourceId,
        provenance: SourceProvenance,
    ) -> Option<ProvenanceClaimId> {
        self.inner.lifecycle.claim_provenance(source_id, provenance)
    }

    pub fn release_provenance(&self, source_id: SourceId, claim_id: ProvenanceClaimId) -> bool {
        if !self.inner.lifecycle.release_provenance(source_id, claim_id) {
            return false;
        }
        self.inner
            .lifecycle
            .schedule_prune_after_current_retirement(source_id);
        true
    }

    pub fn snapshot(&self, source_id: SourceId) -> Option<LifecycleSnapshot<AcceptedView>> {
        self.inner
            .lifecycle
            .snapshot(source_id)
            .map(public_snapshot)
    }

    /// Spawn one standard remote constructor under the only permitted
    /// abortable policy. The generation is minted synchronously before any
    /// network future can be queued.
    pub fn connect_standard<A, OnGeneration, Authenticate, AuthenticateFuture>(
        &self,
        source_id: SourceId,
        on_generation: OnGeneration,
        authenticate: Authenticate,
    ) -> Option<u64>
    where
        A: AbortableSourceAdapter + 'static,
        OnGeneration: FnOnce(u64),
        Authenticate: FnOnce() -> AuthenticateFuture + Send + 'static,
        AuthenticateFuture: Future<Output = BackendResult<A>> + Send + 'static,
    {
        self.spawn_connect(
            source_id,
            ConstructionCancellationPolicy::Abortable,
            on_generation,
            authenticate,
        )
    }

    /// Connect with a pre-existing Jellyfin API key. No server-side session
    /// is minted during construction, so cancellation may abort the future.
    pub fn connect_jellyfin_api_key<OnGeneration, Authenticate, AuthenticateFuture>(
        &self,
        source_id: SourceId,
        on_generation: OnGeneration,
        authenticate: Authenticate,
    ) -> Option<u64>
    where
        OnGeneration: FnOnce(u64),
        Authenticate: FnOnce() -> AuthenticateFuture + Send + 'static,
        AuthenticateFuture:
            Future<Output = BackendResult<crate::jellyfin::JellyfinBackend>> + Send + 'static,
    {
        self.spawn_connect(
            source_id,
            ConstructionCancellationPolicy::Abortable,
            on_generation,
            authenticate,
        )
    }

    /// Connect through AuthenticateByName. The constructor may mint a
    /// revocable Jellyfin session token, so cancellation must let it finish
    /// and transfer the synchronously staged adapter into exact logout.
    pub fn connect_jellyfin_session<OnGeneration, Authenticate, AuthenticateFuture>(
        &self,
        source_id: SourceId,
        on_generation: OnGeneration,
        authenticate: Authenticate,
    ) -> Option<u64>
    where
        OnGeneration: FnOnce(u64),
        Authenticate: FnOnce() -> AuthenticateFuture + Send + 'static,
        AuthenticateFuture:
            Future<Output = BackendResult<crate::jellyfin::JellyfinBackend>> + Send + 'static,
    {
        self.spawn_connect(
            source_id,
            ConstructionCancellationPolicy::FinishConstruction,
            on_generation,
            authenticate,
        )
    }

    /// Spawn a DAAP login under protected FinishConstruction. The login
    /// future returns immediately after `mlid`; update/database/items begin
    /// only in the registry-staged catalogue closure.
    pub fn connect_daap<OnGeneration, Authenticate, AuthenticateFuture>(
        &self,
        source_id: SourceId,
        on_generation: OnGeneration,
        authenticate: Authenticate,
    ) -> Option<u64>
    where
        OnGeneration: FnOnce(u64),
        Authenticate: FnOnce() -> AuthenticateFuture + Send + 'static,
        AuthenticateFuture:
            Future<Output = BackendResult<crate::daap::DaapBackend>> + Send + 'static,
    {
        self.spawn_connect(
            source_id,
            ConstructionCancellationPolicy::FinishConstruction,
            on_generation,
            authenticate,
        )
    }

    /// Scan and connect one exact mounted removable source under lifecycle
    /// cancellation and epoch ownership.
    ///
    /// Filesystem walking and tag parsing run on Tokio's blocking pool. The
    /// construction task must finish cooperatively so registry shutdown can
    /// join it, but the cancellation observer lets a removed or relocated
    /// mount stop between filesystem operations without publishing failure.
    pub fn connect_removable<OnGeneration>(
        &self,
        source_id: SourceId,
        mount_root: std::path::PathBuf,
        on_generation: OnGeneration,
    ) -> Option<u64>
    where
        OnGeneration: FnOnce(u64),
    {
        if source_id == SourceId::radio_browser() {
            return None;
        }
        let owner = self.inner.lifecycle.begin_connect(source_id)?;
        let generation = owner.generation();
        on_generation(generation);
        let blocking_runtime = self.inner.runtime.clone();
        let adapter_runtime = self.inner.runtime.clone();
        owner.spawn_staged(
            ConstructionCancellationPolicy::FinishConstruction,
            move |cancellation| async move {
                let worker_cancellation = cancellation.clone();
                let result = blocking_runtime
                    .spawn_blocking(move || {
                        crate::removable::RemovableMediaAdapter::scan(
                            source_id,
                            mount_root,
                            &worker_cancellation,
                            adapter_runtime,
                        )
                    })
                    .await;
                match result {
                    Ok(Ok(Some(adapter))) => constructed_adapter(adapter),
                    Ok(Ok(None)) => AdapterTaskResult::Cancelled,
                    Ok(Err(error)) => AdapterTaskResult::Failed(failure_category(&error)),
                    Err(_) => AdapterTaskResult::Failed(FailureCategory::Backend),
                }
            },
            move |adapter, cancellation| async move {
                if cancellation.is_cancelled() {
                    return RefreshTaskResult::Cancelled;
                }
                let regular_playlist_capability = adapter.regular_playlist_capability();
                match adapter.load_initial_catalogue().await {
                    Ok(tracks) => RefreshTaskResult::Refreshed(AcceptedSourcePayload::catalogue(
                        tracks,
                        regular_playlist_capability,
                    )),
                    Err(error) => RefreshTaskResult::Failed(failure_category(&error)),
                }
            },
        );
        Some(generation)
    }

    fn spawn_connect<A, OnGeneration, Authenticate, AuthenticateFuture>(
        &self,
        source_id: SourceId,
        policy: ConstructionCancellationPolicy,
        on_generation: OnGeneration,
        authenticate: Authenticate,
    ) -> Option<u64>
    where
        A: ManagedSourceAdapter + 'static,
        OnGeneration: FnOnce(u64),
        Authenticate: FnOnce() -> AuthenticateFuture + Send + 'static,
        AuthenticateFuture: Future<Output = BackendResult<A>> + Send + 'static,
    {
        if source_id == SourceId::radio_browser() {
            return None;
        }
        let owner = self.inner.lifecycle.begin_connect(source_id)?;
        let generation = owner.generation();
        on_generation(generation);
        owner.spawn_staged(
            policy,
            move |cancellation| async move {
                if cancellation.is_cancelled() {
                    return AdapterTaskResult::Cancelled;
                }
                match authenticate().await {
                    Ok(adapter) => constructed_adapter(adapter),
                    Err(error) => AdapterTaskResult::Failed(failure_category(&error)),
                }
            },
            move |adapter, cancellation| async move {
                if cancellation.is_cancelled() {
                    return RefreshTaskResult::Cancelled;
                }
                let regular_playlist_capability = adapter.regular_playlist_capability();
                match adapter.load_initial_catalogue().await {
                    Ok(tracks) => RefreshTaskResult::Refreshed(AcceptedSourcePayload::catalogue(
                        tracks,
                        regular_playlist_capability,
                    )),
                    Err(error) => RefreshTaskResult::Failed(failure_category(&error)),
                }
            },
        );
        Some(generation)
    }

    /// Ensure the one stateless Radio-Browser session exists, then start a
    /// cancellable refresh for an exact radio view.
    ///
    /// The concrete adapter is constructed outside the installation mutex.
    /// Concurrent first clicks may construct an unused stateless candidate,
    /// but exactly one candidate is adopted and no caller factory or callback
    /// executes while installation is serialized.
    pub fn refresh_builtin_radio_view(&self, view: ViewOrigin) -> Option<u64> {
        let source_id = SourceId::radio_browser();
        if self.current_builtin_epoch(source_id).is_none() {
            let candidate = crate::radio::adapter::RadioBrowserAdapter::new()
                .map(|adapter| Box::new(adapter) as Box<dyn ManagedSourceAdapter>);
            self.install_builtin_candidate(source_id, candidate)?;
        }

        self.refresh_view(source_id, view)
    }

    fn current_builtin_epoch(&self, source_id: SourceId) -> Option<u64> {
        let mut built_ins = lock(&self.inner.built_ins);
        let installation = built_ins.get_mut(&source_id)?;
        let current = installation
            .session_epoch
            .filter(|epoch| self.inner.lifecycle.active_session_epoch(source_id) == Some(*epoch));
        if current.is_none() {
            installation.session_epoch = None;
        }
        current
    }

    fn install_builtin_candidate(
        &self,
        source_id: SourceId,
        candidate: BackendResult<Box<dyn ManagedSourceAdapter>>,
    ) -> Option<u64> {
        let mut built_ins = lock(&self.inner.built_ins);
        let installation = built_ins.get_mut(&source_id)?;
        if let Some(current) = installation
            .session_epoch
            .filter(|epoch| self.inner.lifecycle.active_session_epoch(source_id) == Some(*epoch))
        {
            return Some(current);
        }
        installation.session_epoch = None;
        let adapter = match candidate {
            Ok(adapter) => adapter,
            Err(error) => {
                self.inner
                    .lifecycle
                    .fail_stateless_session(source_id, failure_category(&error));
                return None;
            }
        };
        let regular_playlist_capability = adapter.regular_playlist_capability();
        let (_, session_epoch) = self.inner.lifecycle.adopt_stateless_session(
            source_id,
            adapter,
            AcceptedSourcePayload::catalogue(Vec::new(), regular_playlist_capability),
        )?;
        installation.session_epoch = Some(session_epoch);
        Some(session_epoch)
    }

    /// Start or supersede one exact named-view refresh.
    pub fn refresh_view(&self, source_id: SourceId, view: ViewOrigin) -> Option<u64> {
        let owner = self
            .inner
            .lifecycle
            .begin_refresh(source_id, RefreshLane::View(view.clone()))?;
        let generation = owner.generation();
        owner.spawn(move |session, cancellation| async move {
            match session.adapter().load_view(view, cancellation).await {
                ViewLoadResult::Loaded(view) => {
                    RefreshTaskResult::Refreshed(AcceptedSourcePayload::from_view(view))
                }
                ViewLoadResult::Failed(category) => RefreshTaskResult::Failed(category),
                ViewLoadResult::Cancelled => RefreshTaskResult::Cancelled,
            }
        });
        Some(generation)
    }

    #[cfg(test)]
    pub fn remove_view(&self, source_id: SourceId, view: &ViewOrigin) -> bool {
        self.inner.lifecycle.remove_view(source_id, view)
    }

    pub fn disconnect(&self, source_id: SourceId) -> Option<RetirementWaiter> {
        self.inner.lifecycle.disconnect(source_id)
    }

    pub fn shutdown(&self) -> ShutdownBarrier {
        // Serialize gate closure with external claim/adoption publication.
        // Once shutdown owns this lock, a candidate can neither publish a
        // post-shutdown claim nor leave stale explicit-retirement ownership.
        let mut external_sessions = lock(&self.inner.external_sessions);
        external_sessions.clear();
        let barrier = self.inner.lifecycle.shutdown();
        drop(external_sessions);
        barrier
    }

    #[cfg(test)]
    pub fn is_shutting_down(&self) -> bool {
        self.inner.lifecycle.is_shutting_down()
    }

    /// Validate the exact accepted catalogue at the GTK publication boundary.
    #[cfg(test)]
    pub fn is_current_catalogue(
        &self,
        source_id: SourceId,
        generation: u64,
        session_epoch: u64,
    ) -> bool {
        self.inner
            .lifecycle
            .is_current_catalogue(source_id, generation, session_epoch)
    }

    #[cfg(test)]
    pub fn has_session_epoch(&self, source_id: SourceId, session_epoch: u64) -> bool {
        self.inner.lifecycle.active_session_epoch(source_id) == Some(session_epoch)
    }

    pub fn snapshot_all(&self) -> LifecycleBaseline<AcceptedView> {
        let baseline = self.inner.lifecycle.snapshot_all();
        LifecycleBaseline {
            revision: baseline.revision,
            shutting_down: baseline.shutting_down,
            sources: baseline
                .sources
                .into_iter()
                .map(|(source_id, snapshot)| (source_id, public_snapshot(snapshot)))
                .collect(),
        }
    }

    /// Resolve persisted source-scoped identities in their input order.
    ///
    /// Every requested occurrence receives exactly one result. Sources are
    /// observed independently under the lifecycle lock, while metadata copies
    /// and ordering work happen outside it. Similar metadata, another source's
    /// matching native ID, and endpoint identity are never considered.
    pub fn resolve_regular_playlist_tracks(
        &self,
        media_keys: &[MediaKey],
    ) -> Vec<RegularPlaylistTrackResolution> {
        let mut by_source: HashMap<SourceId, Vec<(usize, TrackId)>> = HashMap::new();
        for (position, media_key) in media_keys.iter().enumerate() {
            by_source
                .entry(media_key.source_id)
                .or_default()
                .push((position, media_key.track_id.clone()));
        }

        let mut results: Vec<Option<RegularPlaylistTrackResolution>> = vec![None; media_keys.len()];
        for (source_id, occurrences) in by_source {
            let accepted =
                self.inner
                    .lifecycle
                    .resolve_current_accepted_catalogue(source_id, |payload| {
                        Some((
                            payload.regular_playlist_capability,
                            payload.regular_playlist_index.clone(),
                            Arc::clone(&payload.tracks),
                        ))
                    });

            let Some(accepted) = accepted else {
                for (position, track_id) in occurrences {
                    results[position] = Some(RegularPlaylistTrackResolution::Unavailable(
                        RegularPlaylistUnavailable {
                            media_key: MediaKey::new(source_id, track_id),
                            reason: RegularPlaylistUnavailableReason::SourceUnavailable,
                            observed_guard: None,
                        },
                    ));
                }
                continue;
            };

            let guard = RegularPlaylistCatalogueGuard {
                source_id,
                session_epoch: accepted.session_epoch,
                catalogue_generation: accepted.generation,
            };
            let (capability, index, tracks) = accepted.value;
            for (position, track_id) in occurrences {
                let media_key = MediaKey::new(source_id, track_id.clone());
                let resolution = match (&capability, &index) {
                    (RegularPlaylistCapability::Unsupported, _) => {
                        RegularPlaylistTrackResolution::Unavailable(RegularPlaylistUnavailable {
                            media_key,
                            reason: RegularPlaylistUnavailableReason::UnsupportedSource,
                            observed_guard: Some(guard),
                        })
                    }
                    (
                        RegularPlaylistCapability::SourceScopedEntries,
                        RegularPlaylistTrackIndex::Invalid,
                    ) => RegularPlaylistTrackResolution::Unavailable(RegularPlaylistUnavailable {
                        media_key,
                        reason: RegularPlaylistUnavailableReason::InvalidCatalogue,
                        observed_guard: Some(guard),
                    }),
                    (
                        RegularPlaylistCapability::SourceScopedEntries,
                        RegularPlaylistTrackIndex::Exact(index),
                    ) => {
                        if let Some(track) = index
                            .get(&track_id)
                            .and_then(|track_index| tracks.get(*track_index))
                        {
                            RegularPlaylistTrackResolution::Available(Box::new(
                                RegularPlaylistTrack {
                                    media_key,
                                    guard,
                                    metadata: RegularPlaylistTrackMetadata::from_track(track),
                                },
                            ))
                        } else {
                            RegularPlaylistTrackResolution::Unavailable(
                                RegularPlaylistUnavailable {
                                    media_key,
                                    reason: RegularPlaylistUnavailableReason::TrackMissing,
                                    observed_guard: Some(guard),
                                },
                            )
                        }
                    }
                    // Constructor invariants keep these combinations
                    // unreachable. Treat any future drift as invalid rather
                    // than granting playlist authority.
                    _ => RegularPlaylistTrackResolution::Unavailable(RegularPlaylistUnavailable {
                        media_key,
                        reason: RegularPlaylistUnavailableReason::InvalidCatalogue,
                        observed_guard: Some(guard),
                    }),
                };
                results[position] = Some(resolution);
            }
        }

        results
            .into_iter()
            .map(|result| result.expect("every playlist occurrence is resolved exactly once"))
            .collect()
    }

    /// Recheck that an ordered lookup result still describes the current
    /// source/catalogue authority. Metadata is immutable within a guard, so
    /// only exact identity, guard, and closed availability state are compared.
    pub fn are_regular_playlist_tracks_current(
        &self,
        resolutions: &[RegularPlaylistTrackResolution],
    ) -> bool {
        let media_keys: Vec<_> = resolutions
            .iter()
            .map(|resolution| resolution.media_key().clone())
            .collect();
        let current = self.resolve_regular_playlist_tracks(&media_keys);
        resolutions
            .iter()
            .zip(current.iter())
            .all(|(observed, current)| regular_playlist_authority_eq(observed, current))
    }

    /// Acquire commit-scoped authority for an ordered remote playlist plan.
    ///
    /// Every occurrence must be an available exact catalogue member. The
    /// lifecycle registry deduplicates repeated tracks and guards, validates
    /// the complete batch in one locked observation, and admits in-flight
    /// session/catalogue permits before returning. The opaque result must be
    /// retained until the database transaction has committed.
    pub fn acquire_regular_playlist_commit_authority(
        &self,
        resolutions: &[RegularPlaylistTrackResolution],
    ) -> Option<RegularPlaylistCommitAuthority> {
        let mut requests = Vec::with_capacity(resolutions.len());
        for resolution in resolutions {
            let RegularPlaylistTrackResolution::Available(track) = resolution else {
                return None;
            };
            let guard = track.guard();
            if track.media_key.source_id != guard.source_id {
                return None;
            }
            requests.push(CatalogueCommitRequest {
                source_id: guard.source_id,
                catalogue_generation: guard.catalogue_generation,
                session_epoch: guard.session_epoch,
                selected: track.media_key.track_id.clone(),
            });
        }

        let authority = self
            .inner
            .lifecycle
            .acquire_catalogue_commit_authority(&requests, |payload, track_id| {
                payload.regular_playlist_track(track_id).is_some()
            })?;
        Some(RegularPlaylistCommitAuthority {
            authority,
            _registry: Arc::clone(&self.inner),
        })
    }

    /// Resolve one stream only through the exact catalogue guard that minted
    /// the playlist row. The returned protected request carries the accepted
    /// catalogue's revocable lease, so a same-session catalogue replacement
    /// also invalidates a request after resolution but before consumption.
    pub async fn resolve_regular_playlist_stream(
        &self,
        guard: RegularPlaylistCatalogueGuard,
        track_id: TrackId,
    ) -> Result<ResolvedSourceStream, RegularPlaylistMediaError> {
        let selected_track_id = track_id.clone();
        let (stream, accepted_authority, ()) = self
            .inner
            .lifecycle
            .resolve_catalogue_stream(
                guard.source_id,
                guard.catalogue_generation,
                guard.session_epoch,
                move |payload| {
                    payload
                        .regular_playlist_track(&selected_track_id)
                        .map(|_| ())
                },
                move |adapter| async move { adapter.resolve_stream(track_id).await },
            )
            .await
            .map_err(regular_playlist_media_error)?;
        match stream {
            AdapterStream::ProtectedHttp(request) => Ok(ResolvedSourceStream::Http(
                MediaRequest::ProtectedHttp(Box::new((*request).with_lease(accepted_authority))),
            )),
            AdapterStream::File(_) => Err(RegularPlaylistMediaError::Unavailable),
        }
    }

    /// Artwork counterpart of [`Self::resolve_regular_playlist_stream`] with
    /// the same exact pre/post guard checks and catalogue-generation lease.
    pub async fn resolve_regular_playlist_artwork(
        &self,
        guard: RegularPlaylistCatalogueGuard,
        track_id: TrackId,
    ) -> Result<Option<ResolvedHttpRequest>, RegularPlaylistMediaError> {
        let selected_track_id = track_id.clone();
        let (request, accepted_authority, ()) = self
            .inner
            .lifecycle
            .resolve_catalogue_optional_http(
                guard.source_id,
                guard.catalogue_generation,
                guard.session_epoch,
                move |payload| {
                    payload
                        .regular_playlist_track(&selected_track_id)
                        .map(|_| ())
                },
                move |adapter| async move { adapter.resolve_artwork(track_id).await },
            )
            .await
            .map_err(regular_playlist_media_error)?;
        Ok(request.map(|request| request.with_lease(accepted_authority)))
    }

    pub async fn resolve_stream(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        track_id: TrackId,
    ) -> BackendResult<ResolvedSourceStream> {
        if let Some(resolved) = self.inner.lifecycle.resolve_latest_accepted_view(
            source_id,
            expected_session_epoch,
            |payload| payload.public_streams.get(&track_id).cloned(),
        ) {
            let authority: Arc<dyn PublicHttpAuthority> = self.inner.clone();
            let authority = Arc::downgrade(&authority);
            return Ok(ResolvedSourceStream::Http(MediaRequest::PublicHttp(
                ResolvedPublicHttpRequest::new(
                    resolved.value,
                    resolved.authority,
                    authority,
                    source_id,
                    track_id,
                    resolved.session_epoch,
                    resolved.generation,
                ),
            )));
        }

        let stream = self
            .inner
            .lifecycle
            .resolve_stream(
                source_id,
                expected_session_epoch,
                move |adapter| async move { adapter.resolve_stream(track_id).await },
            )
            .await?;
        Ok(match stream {
            AdapterStream::ProtectedHttp(request) => {
                ResolvedSourceStream::Http(MediaRequest::ProtectedHttp(request))
            }
            AdapterStream::File(media) => ResolvedSourceStream::File(media),
        })
    }

    pub async fn resolve_artwork(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        track_id: TrackId,
    ) -> BackendResult<Option<ResolvedHttpRequest>> {
        self.inner
            .lifecycle
            .resolve_optional_http(
                source_id,
                expected_session_epoch,
                move |adapter| async move { adapter.resolve_artwork(track_id).await },
            )
            .await
    }
}

fn regular_playlist_authority_eq(
    observed: &RegularPlaylistTrackResolution,
    current: &RegularPlaylistTrackResolution,
) -> bool {
    match (observed, current) {
        (
            RegularPlaylistTrackResolution::Available(observed),
            RegularPlaylistTrackResolution::Available(current),
        ) => observed.media_key == current.media_key && observed.guard == current.guard,
        (
            RegularPlaylistTrackResolution::Unavailable(observed),
            RegularPlaylistTrackResolution::Unavailable(current),
        ) => {
            observed.media_key == current.media_key
                && observed.reason == current.reason
                && observed.observed_guard == current.observed_guard
        }
        _ => false,
    }
}

fn regular_playlist_media_error(
    error: crate::source_lifecycle::CatalogueMediaResolveError,
) -> RegularPlaylistMediaError {
    match error {
        crate::source_lifecycle::CatalogueMediaResolveError::Unavailable => {
            RegularPlaylistMediaError::Unavailable
        }
        crate::source_lifecycle::CatalogueMediaResolveError::Backend(error) => {
            RegularPlaylistMediaError::BackendFailure(failure_category(&error))
        }
    }
}

/// Convert one concrete backend into the task result accepted by the
/// heterogeneous lifecycle registry.
fn constructed_adapter<A>(adapter: A) -> AdapterTaskResult<dyn ManagedSourceAdapter>
where
    A: ManagedSourceAdapter + 'static,
{
    AdapterTaskResult::Constructed(Box::new(adapter))
}

fn public_snapshot(
    snapshot: LifecycleSnapshot<AcceptedSourcePayload>,
) -> LifecycleSnapshot<AcceptedView> {
    let LifecycleSnapshot {
        revision,
        state,
        session_epoch,
        provenance,
        visibility,
        retention,
        catalogue,
        views,
        failure,
        refresh_failures,
        pending_connect,
        pending_refreshes,
        pending_retirements,
    } = snapshot;
    LifecycleSnapshot {
        revision,
        state,
        session_epoch,
        provenance,
        visibility,
        retention,
        catalogue: catalogue.map(public_accepted_snapshot),
        views: views
            .into_iter()
            .map(|(view, snapshot)| (view, public_accepted_snapshot(snapshot)))
            .collect(),
        failure,
        refresh_failures,
        pending_connect,
        pending_refreshes,
        pending_retirements,
    }
}

fn public_accepted_snapshot(
    snapshot: crate::source_lifecycle::AcceptedSnapshot<AcceptedSourcePayload>,
) -> crate::source_lifecycle::AcceptedSnapshot<AcceptedView> {
    snapshot.map(AcceptedSourcePayload::published)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn closed_external_admission_error() -> BackendError {
    BackendError::Internal(anyhow::anyhow!("external media admission is unavailable"))
}

/// Reduce backend-specific errors to the closed categories retained by the
/// lifecycle registry. No backend error chain crosses this boundary.
pub fn failure_category(error: &BackendError) -> FailureCategory {
    match error {
        BackendError::AuthenticationFailed { .. } => FailureCategory::AuthenticationRejected,
        BackendError::ConnectionFailed { .. } | BackendError::Io(_) => FailureCategory::Connection,
        BackendError::Timeout { .. } => FailureCategory::Timeout,
        BackendError::ParseError { .. } => FailureCategory::InvalidResponse,
        BackendError::TokenAuthNotSupported { .. } => FailureCategory::UnsupportedAuthentication,
        BackendError::NotFound { .. } => FailureCategory::UnavailableOrPermission,
        BackendError::Unsupported { .. } | BackendError::Internal(_) => FailureCategory::Backend,
    }
}

/// GTK-owned bookkeeping for the exact provenance claims minted by real
/// Saved, Environment, and Discovery publishers.
///
/// This map owns only opaque claim tokens. Session, cancellation, media, and
/// retirement authority remain exclusively in [`SourceRegistry`].
#[derive(Clone, Default)]
pub struct ProvenanceClaims {
    claims: Rc<RefCell<ProvenanceClaimMap>>,
}

type ProvenanceClaimKey = (SourceId, SourceProvenance, String);
type ProvenanceClaimMap = HashMap<ProvenanceClaimKey, ProvenanceClaimId>;

impl ProvenanceClaims {
    pub fn ensure(
        &self,
        registry: &SourceRegistry,
        source_id: SourceId,
        provenance: SourceProvenance,
        publisher: impl Into<String>,
    ) -> bool {
        let key = (source_id, provenance, publisher.into());
        if self.claims.borrow().contains_key(&key) {
            return true;
        }
        let Some(claim) = registry.claim_provenance(source_id, provenance) else {
            return false;
        };
        self.claims.borrow_mut().insert(key, claim);
        true
    }

    pub fn release(
        &self,
        registry: &SourceRegistry,
        source_id: SourceId,
        provenance: SourceProvenance,
        publisher: &str,
    ) -> bool {
        let key = (source_id, provenance, publisher.to_string());
        let Some(claim) = self.claims.borrow().get(&key).copied() else {
            return false;
        };
        if !registry.release_provenance(source_id, claim) {
            return false;
        }
        self.claims.borrow_mut().remove(&key);
        true
    }

    #[cfg(test)]
    pub fn contains(
        &self,
        source_id: SourceId,
        provenance: SourceProvenance,
        publisher: &str,
    ) -> bool {
        self.claims
            .borrow()
            .contains_key(&(source_id, provenance, publisher.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use axum::http::{Method, StatusCode};
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use tokio::runtime::Handle;
    use tokio::sync::{oneshot, watch};
    use tokio::time::{timeout, Duration};
    use url::Url;
    use uuid::Uuid;

    use crate::architecture::models::{
        Album, Artist, LibraryStats, SearchResults, SortField, SortOrder,
    };
    use crate::db::migration::Migrator;
    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};
    use crate::local::playlist_manager::{
        PlaylistEntryAddOutcome, PlaylistEntryInput, PlaylistManager,
    };

    use super::*;

    struct FakeProbe {
        close_calls: AtomicUsize,
        stream_calls: AtomicUsize,
        artwork_calls: AtomicUsize,
        close_release: watch::Sender<bool>,
        stream_release: watch::Sender<bool>,
        view_specs: Mutex<HashMap<ViewOrigin, VecDeque<ViewSpec>>>,
    }

    struct ViewSpec {
        delay: Duration,
        endpoint: Url,
    }

    impl FakeProbe {
        fn new(close_released: bool) -> Arc<Self> {
            let (close_release, _receiver) = watch::channel(close_released);
            let (stream_release, _receiver) = watch::channel(true);
            Arc::new(Self {
                close_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
                artwork_calls: AtomicUsize::new(0),
                close_release,
                stream_release,
                view_specs: Mutex::new(HashMap::new()),
            })
        }

        fn queue_public_view(&self, view: ViewOrigin, endpoint: &str, delay: Duration) {
            lock(&self.view_specs)
                .entry(view)
                .or_default()
                .push_back(ViewSpec {
                    delay,
                    endpoint: Url::parse(endpoint).expect("fixture URL"),
                });
        }

        fn adapter(self: &Arc<Self>, label: &'static str) -> FakeAdapter {
            FakeAdapter {
                label,
                probe: Arc::clone(self),
                close_release: self.close_release.subscribe(),
                catalogue: Vec::new(),
                regular_playlist_capability: RegularPlaylistCapability::Unsupported,
                stream_failure: None,
                artwork_available: false,
            }
        }

        fn playlist_adapter(
            self: &Arc<Self>,
            label: &'static str,
            catalogue: Vec<Track>,
        ) -> FakeAdapter {
            FakeAdapter {
                label,
                probe: Arc::clone(self),
                close_release: self.close_release.subscribe(),
                catalogue,
                regular_playlist_capability: RegularPlaylistCapability::SourceScopedEntries,
                stream_failure: None,
                artwork_available: true,
            }
        }

        async fn wait_for_close_calls(&self, expected: usize) {
            timeout(Duration::from_secs(2), async {
                while self.close_calls.load(Ordering::Acquire) < expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("adapter close started");
        }
    }

    struct FakeAdapter {
        label: &'static str,
        probe: Arc<FakeProbe>,
        close_release: watch::Receiver<bool>,
        catalogue: Vec<Track>,
        regular_playlist_capability: RegularPlaylistCapability,
        stream_failure: Option<String>,
        artwork_available: bool,
    }

    #[async_trait]
    impl MediaBackend for FakeAdapter {
        fn name(&self) -> &str {
            self.label
        }

        fn backend_type(&self) -> &str {
            "test"
        }

        async fn ping(&self) -> BackendResult<()> {
            Ok(())
        }

        async fn search(&self, _query: &str, _limit: usize) -> BackendResult<SearchResults> {
            Ok(SearchResults::default())
        }

        async fn list_tracks(&self) -> BackendResult<Vec<Track>> {
            Ok(Vec::new())
        }

        async fn list_albums(
            &self,
            _sort: SortField,
            _order: SortOrder,
        ) -> BackendResult<Vec<Album>> {
            Ok(Vec::new())
        }

        async fn list_artists(&self) -> BackendResult<Vec<Artist>> {
            Ok(Vec::new())
        }

        async fn get_album_tracks(&self, _album_id: &Uuid) -> BackendResult<Vec<Track>> {
            Ok(Vec::new())
        }

        async fn get_artist_tracks(&self, _artist_id: &Uuid) -> BackendResult<Vec<Track>> {
            Ok(Vec::new())
        }

        async fn get_stats(&self) -> BackendResult<LibraryStats> {
            Ok(LibraryStats::default())
        }
    }

    #[async_trait]
    impl RemoteMediaResolver for FakeAdapter {
        async fn resolve_stream(&self, track_id: &TrackId) -> BackendResult<ResolvedHttpRequest> {
            self.probe.stream_calls.fetch_add(1, Ordering::AcqRel);
            let mut release = self.probe.stream_release.subscribe();
            while !*release.borrow_and_update() {
                if release.changed().await.is_err() {
                    break;
                }
            }
            if let Some(message) = &self.stream_failure {
                return Err(BackendError::ConnectionFailed {
                    message: message.clone(),
                    source: None,
                });
            }
            ResolvedHttpRequest::new(
                Url::parse(&format!(
                    "https://media.invalid/{}/{}",
                    self.label,
                    track_id.as_str()
                ))
                .expect("fixture URL"),
            )
        }

        async fn resolve_artwork(
            &self,
            track_id: &TrackId,
        ) -> BackendResult<Option<ResolvedHttpRequest>> {
            self.probe.artwork_calls.fetch_add(1, Ordering::AcqRel);
            if self.artwork_available {
                ResolvedHttpRequest::new(
                    Url::parse(&format!(
                        "https://art.invalid/{}/{}",
                        self.label,
                        track_id.as_str()
                    ))
                    .expect("fixture URL"),
                )
                .map(Some)
            } else {
                Ok(None)
            }
        }
    }

    impl LifecycleAdapter for FakeAdapter {
        fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
            self.probe.close_calls.fetch_add(1, Ordering::AcqRel);
            let mut release = self.close_release.clone();
            Box::pin(async move {
                while !*release.borrow_and_update() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
                Ok(())
            })
        }
    }

    impl ManagedSourceAdapter for FakeAdapter {
        fn regular_playlist_capability(&self) -> RegularPlaylistCapability {
            self.regular_playlist_capability
        }

        fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
            Box::pin(async move { Ok(self.catalogue.clone()) })
        }

        fn resolve_stream(self: Arc<Self>, track_id: TrackId) -> StreamFuture {
            Box::pin(async move {
                RemoteMediaResolver::resolve_stream(self.as_ref(), &track_id)
                    .await
                    .map(|request| AdapterStream::ProtectedHttp(Box::new(request)))
            })
        }

        fn resolve_artwork(self: Arc<Self>, track_id: TrackId) -> ArtworkFuture {
            Box::pin(
                async move { RemoteMediaResolver::resolve_artwork(self.as_ref(), &track_id).await },
            )
        }

        fn load_view(
            self: Arc<Self>,
            view: ViewOrigin,
            mut cancellation: crate::source_lifecycle::CancellationObserver,
        ) -> ViewFuture {
            let spec = lock(&self.probe.view_specs)
                .get_mut(&view)
                .and_then(VecDeque::pop_front);
            Box::pin(async move {
                let Some(spec) = spec else {
                    return ViewLoadResult::Failed(FailureCategory::Backend);
                };
                tokio::select! {
                    () = tokio::time::sleep(spec.delay) => {}
                    () = cancellation.cancelled() => return ViewLoadResult::Cancelled,
                }
                let track_id = TrackId::remote("shared-station").expect("track ID");
                let track = fixture_track(track_id.clone());
                match PublicStreamContribution::new(track_id, spec.endpoint).and_then(
                    |contribution| {
                        AcceptedView::public_http(Arc::new(vec![track]), vec![contribution])
                    },
                ) {
                    Ok(view) => ViewLoadResult::Loaded(view),
                    Err(_) => ViewLoadResult::Failed(FailureCategory::Backend),
                }
            })
        }
    }

    impl sealed::AbortableSourceAdapter for FakeAdapter {}
    impl AbortableSourceAdapter for FakeAdapter {}

    fn registry() -> SourceRegistry {
        SourceRegistry::new(Handle::current())
    }

    async fn playlist_manager_fixture(name: &str) -> (PlaylistManager, String) {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        Migrator::up(&db, None).await.expect("run migrations");
        let manager = PlaylistManager::new(db);
        let playlist = manager
            .create_playlist(name, false)
            .await
            .expect("create regular playlist");
        (manager, playlist.id)
    }

    fn minimal_wav_bytes(sample: u8) -> Vec<u8> {
        let data_size = 1_u32;
        let mut bytes = Vec::with_capacity(45);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.push(sample);
        bytes
    }

    fn external_fixture(directory: &tempfile::TempDir, sample: u8) -> (File, ExternalFileHint) {
        let path = directory.path().join("fixture.wav");
        std::fs::write(&path, minimal_wav_bytes(sample)).expect("write external WAV");
        (
            File::open(path).expect("open external WAV"),
            ExternalFileHint::new("fixture.wav", Some("wav")).expect("safe external hint"),
        )
    }

    fn read_file_stream(stream: &ResolvedSourceStream) -> Vec<u8> {
        let ResolvedSourceStream::File(media) = stream else {
            panic!("fixture expected retained file media");
        };
        let mut file = media
            .try_clone_file()
            .expect("clone retained external file");
        file.seek(SeekFrom::Start(0))
            .expect("rewind retained external file");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .expect("read retained external file");
        bytes
    }

    fn fixture_track(track_id: TrackId) -> Track {
        Track {
            id: Uuid::new_v4(),
            native_track_id: Some(track_id),
            title: "Station".to_string(),
            artist_name: "Radio".to_string(),
            album_artist_name: None,
            artist_id: None,
            album_title: "Internet Radio".to_string(),
            album_id: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            composer: None,
            genre: None,
            year: None,
            file_path: None,
            stream_url: None,
            cover_art_url: None,
            date_added: None,
            date_modified: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: None,
            rating: crate::architecture::models::TrackRating::unsupported(),
            last_played: None,
        }
    }

    #[test]
    fn daap_initial_catalogue_rejects_rating_capability_mismatch() {
        let mut track = fixture_track(TrackId::remote("daap-track").unwrap());
        track.rating = crate::architecture::models::TrackRating::read_only(None);

        let error = validate_daap_initial_catalogue(
            vec![track],
            crate::architecture::models::RatingCapability::Unsupported,
        )
        .expect_err("DAAP publication must reject per-track rating capability drift");

        assert!(matches!(error, BackendError::Internal(_)));
    }

    #[test]
    fn authenticated_adapter_playlist_capability_declarations_are_explicit() {
        fn assert_source_scoped<A>()
        where
            A: source_scoped_playlist_sealed::Adapter,
        {
            assert_eq!(
                source_scoped_playlist_capability::<A>(),
                RegularPlaylistCapability::SourceScopedEntries
            );
        }

        assert_source_scoped::<crate::subsonic::SubsonicBackend>();
        assert_source_scoped::<crate::jellyfin::JellyfinBackend>();
        assert_source_scoped::<crate::plex::PlexBackend>();
        assert_source_scoped::<crate::daap::DaapBackend>();
    }

    async fn wait_for_catalogue(registry: &SourceRegistry, source_id: SourceId) -> (u64, u64) {
        timeout(Duration::from_secs(2), async {
            loop {
                if let Some(catalogue) = registry
                    .snapshot(source_id)
                    .and_then(|snapshot| snapshot.catalogue)
                {
                    return (catalogue.generation, catalogue.session_epoch);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("catalogue accepted")
    }

    async fn wait_for_view(
        registry: &SourceRegistry,
        source_id: SourceId,
        view: &ViewOrigin,
        generation: u64,
    ) -> u64 {
        timeout(Duration::from_secs(2), async {
            loop {
                if let Some(accepted) = registry
                    .snapshot(source_id)
                    .and_then(|snapshot| snapshot.views.get(view).cloned())
                    .filter(|accepted| accepted.generation == generation)
                {
                    return accepted.session_epoch;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("view accepted")
    }

    async fn connect_view_fixture(
        registry: &SourceRegistry,
        probe: &Arc<FakeProbe>,
    ) -> (SourceId, u64) {
        let source_id = SourceId::random();
        registry
            .claim_provenance(source_id, SourceProvenance::Saved)
            .expect("saved claim");
        let adapter_probe = Arc::clone(probe);
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(adapter_probe.adapter("view")) },
            )
            .expect("fixture connection admitted");
        let (_, session_epoch) = wait_for_catalogue(registry, source_id).await;
        (source_id, session_epoch)
    }

    async fn connect_playlist_fixture(
        registry: &SourceRegistry,
        source_id: SourceId,
        adapter: FakeAdapter,
    ) -> (u64, u64) {
        registry
            .claim_provenance(source_id, SourceProvenance::Saved)
            .expect("saved claim");
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(adapter) },
            )
            .expect("playlist fixture admitted");
        wait_for_catalogue(registry, source_id).await
    }

    fn available(resolution: &RegularPlaylistTrackResolution) -> &RegularPlaylistTrack {
        let RegularPlaylistTrackResolution::Available(track) = resolution else {
            panic!("expected available regular-playlist track");
        };
        track
    }

    fn unavailable_reason(
        resolution: &RegularPlaylistTrackResolution,
    ) -> RegularPlaylistUnavailableReason {
        let RegularPlaylistTrackResolution::Unavailable(track) = resolution else {
            panic!("expected unavailable regular-playlist track");
        };
        track.reason()
    }

    async fn refresh_playlist_catalogue(
        registry: &SourceRegistry,
        source_id: SourceId,
        tracks: Vec<Track>,
        capability: RegularPlaylistCapability,
    ) -> u64 {
        let owner = registry
            .inner
            .lifecycle
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("catalogue refresh admitted");
        let generation = owner.generation();
        owner.spawn(move |_session, _cancellation| async move {
            RefreshTaskResult::Refreshed(AcceptedSourcePayload::catalogue(tracks, capability))
        });
        timeout(Duration::from_secs(2), async {
            loop {
                if registry
                    .inner
                    .lifecycle
                    .snapshot(source_id)
                    .and_then(|snapshot| snapshot.catalogue)
                    .is_some_and(|catalogue| catalogue.generation == generation)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("catalogue refresh accepted");
        generation
    }

    fn consume_public(request: ResolvedSourceStream) -> BackendResult<Url> {
        match request {
            ResolvedSourceStream::Http(MediaRequest::PublicHttp(request)) => request.consume(),
            ResolvedSourceStream::Http(MediaRequest::ProtectedHttp(_))
            | ResolvedSourceStream::File(_) => panic!("fixture expected public media"),
        }
    }

    async fn wait_until_pruned(registry: &SourceRegistry, source_id: SourceId) {
        timeout(Duration::from_secs(2), async {
            while registry.snapshot(source_id).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("source pruned after final retirement");
    }

    async fn wait_for_request_count(service: &MockHttpService, expected: usize) {
        timeout(Duration::from_secs(2), async {
            while service.requests().len() < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fixture received request");
    }

    fn protected_request_is_active(request: &ResolvedSourceStream) -> bool {
        match request {
            ResolvedSourceStream::Http(MediaRequest::ProtectedHttp(request)) => request.is_active(),
            ResolvedSourceStream::Http(MediaRequest::PublicHttp(_))
            | ResolvedSourceStream::File(_) => panic!("fixture expected protected media"),
        }
    }

    #[test]
    fn every_backend_error_maps_to_a_closed_failure_category() {
        let cases = [
            (
                BackendError::AuthenticationFailed {
                    message: "secret-free".into(),
                },
                FailureCategory::AuthenticationRejected,
            ),
            (
                BackendError::ConnectionFailed {
                    message: "offline".into(),
                    source: None,
                },
                FailureCategory::Connection,
            ),
            (
                BackendError::Io(std::io::Error::other("offline")),
                FailureCategory::Connection,
            ),
            (
                BackendError::Timeout { duration_secs: 1 },
                FailureCategory::Timeout,
            ),
            (
                BackendError::ParseError {
                    message: "invalid".into(),
                    source: None,
                },
                FailureCategory::InvalidResponse,
            ),
            (
                BackendError::TokenAuthNotSupported {
                    message: "unsupported".into(),
                },
                FailureCategory::UnsupportedAuthentication,
            ),
            (
                BackendError::NotFound {
                    entity_type: "track".into(),
                    id: Uuid::nil(),
                },
                FailureCategory::UnavailableOrPermission,
            ),
            (
                BackendError::Unsupported {
                    operation: "fixture".into(),
                },
                FailureCategory::Backend,
            ),
            (
                BackendError::Internal(anyhow::anyhow!("fixture")),
                FailureCategory::Backend,
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(failure_category(&error), expected);
        }
    }

    #[test]
    fn public_view_requires_one_pathless_locator_for_each_exact_track() {
        let track_id = TrackId::remote("station-one").expect("track ID");
        let track = fixture_track(track_id.clone());
        let contribution = PublicStreamContribution::new(
            track_id.clone(),
            Url::parse("https://radio.invalid/live?mount=one").unwrap(),
        )
        .expect("public locator");
        assert!(
            AcceptedView::public_http(Arc::new(vec![track.clone()]), vec![contribution]).is_ok()
        );

        assert!(AcceptedView::public_http(Arc::new(vec![track.clone()]), Vec::new()).is_err());

        let duplicate = vec![
            PublicStreamContribution::new(
                track_id.clone(),
                Url::parse("https://radio.invalid/one").unwrap(),
            )
            .unwrap(),
            PublicStreamContribution::new(
                track_id.clone(),
                Url::parse("https://radio.invalid/two").unwrap(),
            )
            .unwrap(),
        ];
        assert!(AcceptedView::public_http(Arc::new(vec![track.clone()]), duplicate).is_err());

        let mut concrete = track;
        concrete.stream_url = Some(Url::parse("https://radio.invalid/escaped").unwrap());
        let contribution = PublicStreamContribution::new(
            track_id,
            Url::parse("https://radio.invalid/one").unwrap(),
        )
        .unwrap();
        assert!(AcceptedView::public_http(Arc::new(vec![concrete]), vec![contribution]).is_err());
    }

    #[tokio::test]
    async fn regular_playlist_lookup_is_ordered_source_scoped_and_metadata_is_whitelisted() {
        let registry = registry();
        let shared_id = TrackId::remote("shared-native-id").expect("track ID");
        let other_id = TrackId::remote("other-native-id").expect("track ID");
        let missing_id = TrackId::remote("missing-native-id").expect("track ID");
        let secret = format!("secret-locator-{}", Uuid::new_v4());

        let mut first = fixture_track(shared_id.clone());
        first.title = "First source".to_string();
        first.file_path = Some(format!("/private/{secret}"));
        first.stream_url = Some(
            Url::parse(&format!("https://stream.invalid/audio?token={secret}"))
                .expect("fixture URL"),
        );
        first.cover_art_url = Some(
            Url::parse(&format!("https://art.invalid/cover?token={secret}")).expect("fixture URL"),
        );
        let mut other = fixture_track(other_id.clone());
        other.title = "Other track".to_string();
        let first_source = SourceId::random();
        let first_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            first_source,
            first_probe.playlist_adapter("first", vec![first, other]),
        )
        .await;

        let mut second = fixture_track(shared_id.clone());
        second.title = "Second source".to_string();
        let second_source = SourceId::random();
        let second_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            second_source,
            second_probe.playlist_adapter("second", vec![second]),
        )
        .await;

        let keys = vec![
            MediaKey::new(first_source, other_id),
            MediaKey::new(second_source, shared_id.clone()),
            MediaKey::new(first_source, shared_id.clone()),
            MediaKey::new(first_source, shared_id),
            MediaKey::new(first_source, missing_id),
        ];
        let resolved = registry.resolve_regular_playlist_tracks(&keys);
        assert_eq!(resolved.len(), keys.len());
        assert_eq!(available(&resolved[0]).metadata().title(), "Other track");
        assert_eq!(available(&resolved[1]).metadata().title(), "Second source");
        assert_eq!(available(&resolved[2]).metadata().title(), "First source");
        assert_eq!(available(&resolved[3]).metadata().title(), "First source");
        assert_eq!(available(&resolved[2]).media_key(), &keys[2]);
        assert_eq!(available(&resolved[3]).media_key(), &keys[3]);
        assert_eq!(
            unavailable_reason(&resolved[4]),
            RegularPlaylistUnavailableReason::TrackMissing
        );
        assert!(registry.are_regular_playlist_tracks_current(&resolved));

        let metadata_debug = format!("{:?}", available(&resolved[2]).metadata());
        assert!(!metadata_debug.contains(&secret));
        assert!(!metadata_debug.contains("stream_url"));
        assert!(!metadata_debug.contains("cover_art_url"));
        assert!(!metadata_debug.contains("file_path"));

        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn playlist_commit_authority_blocks_catalogue_publish_and_rejects_stale_guard() {
        let registry = registry();
        let source_id = SourceId::random();
        let track_id = TrackId::remote("commit-guarded-track").expect("track ID");
        let probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.playlist_adapter("commit-refresh", vec![fixture_track(track_id.clone())]),
        )
        .await;

        let key = MediaKey::new(source_id, track_id.clone());
        let resolved = registry.resolve_regular_playlist_tracks(&[key.clone(), key]);
        let authority = registry
            .acquire_regular_playlist_commit_authority(&resolved)
            .expect("current duplicate occurrences are admitted");
        assert_eq!(
            authority.permit_count(),
            2,
            "one unique guard owns one session and one catalogue permit"
        );

        let mut invalidations = registry.subscribe_invalidations();
        let owner = registry
            .inner
            .lifecycle
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("catalogue refresh admitted");
        let generation = owner.generation();
        let (started_tx, started_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let refreshed_track_id = track_id.clone();
        owner.spawn(move |_session, _cancellation| async move {
            let _ = started_tx.send(());
            let _ = finish_rx.await;
            let mut track = fixture_track(refreshed_track_id);
            track.title = "Replacement catalogue".to_string();
            RefreshTaskResult::Refreshed(AcceptedSourcePayload::catalogue(
                vec![track],
                RegularPlaylistCapability::SourceScopedEntries,
            ))
        });
        started_rx.await.expect("refresh task started");
        let _ = invalidations.borrow_and_update();
        finish_tx.send(()).expect("refresh task remains alive");

        timeout(Duration::from_secs(2), async {
            while !authority.revocation_started() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("catalogue replacement reached revocation");
        assert!(
            !invalidations
                .has_changed()
                .expect("invalidation sender alive"),
            "replacement cannot publish while commit authority is retained"
        );

        drop(authority);
        timeout(Duration::from_secs(2), invalidations.changed())
            .await
            .expect("replacement publishes after permit release")
            .expect("invalidation sender alive");
        timeout(Duration::from_secs(2), async {
            loop {
                if registry
                    .snapshot(source_id)
                    .and_then(|snapshot| snapshot.catalogue)
                    .is_some_and(|catalogue| catalogue.generation == generation)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement catalogue accepted");
        assert!(registry
            .acquire_regular_playlist_commit_authority(&resolved)
            .is_none());

        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn playlist_final_authority_rejection_rolls_back_after_disconnect_wins() {
        let registry = registry();
        let source_id = SourceId::random();
        let track_id = TrackId::remote("commit-stale-track").expect("track ID");
        let probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.playlist_adapter("commit-stale", vec![fixture_track(track_id.clone())]),
        )
        .await;

        let media_key = MediaKey::new(source_id, track_id);
        let resolved = registry.resolve_regular_playlist_tracks(std::slice::from_ref(&media_key));
        let (manager, playlist_id) = playlist_manager_fixture("Rejected guarded commit").await;
        let input = PlaylistEntryInput::new(
            media_key,
            available(&resolved[0]).metadata().title(),
            available(&resolved[0]).metadata().artist_name(),
            available(&resolved[0]).metadata().album_title(),
            available(&resolved[0]).metadata().duration_secs(),
        );

        let disconnect_waiter = Arc::new(Mutex::new(None));
        let waiter_slot = Arc::clone(&disconnect_waiter);
        let outcome = manager
            .add_entries_if_authorized(&playlist_id, &[input], || {
                let waiter = registry.disconnect(source_id).expect("disconnect admitted");
                *lock(&waiter_slot) = Some(waiter);
                registry.acquire_regular_playlist_commit_authority(&resolved)
            })
            .await
            .expect("stale authority is a typed rejection");
        assert_eq!(outcome, PlaylistEntryAddOutcome::Rejected);
        assert!(manager
            .get_playlist_entries(&playlist_id)
            .await
            .expect("load rolled-back playlist")
            .is_empty());
        let waiter = lock(&disconnect_waiter)
            .take()
            .expect("disconnect waiter retained");
        waiter.wait().await;

        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn playlist_commit_authority_blocks_disconnect_publish_and_rejects_stale_guard() {
        let registry = registry();
        let source_id = SourceId::random();
        let track_id = TrackId::remote("commit-disconnect-track").expect("track ID");
        let probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.playlist_adapter("commit-disconnect", vec![fixture_track(track_id.clone())]),
        )
        .await;

        let media_key = MediaKey::new(source_id, track_id);
        let resolved = registry.resolve_regular_playlist_tracks(std::slice::from_ref(&media_key));
        let (manager, playlist_id) = playlist_manager_fixture("Guarded commit").await;
        let input = PlaylistEntryInput::new(
            media_key,
            available(&resolved[0]).metadata().title(),
            available(&resolved[0]).metadata().artist_name(),
            available(&resolved[0]).metadata().album_title(),
            available(&resolved[0]).metadata().duration_secs(),
        );
        let mut invalidations = registry.subscribe_invalidations();
        let _ = invalidations.borrow_and_update();
        let (disconnect_tx, disconnect_rx) = std::sync::mpsc::sync_channel(1);
        let outcome = manager
            .add_entries_if_authorized(&playlist_id, &[input], || {
                let authority = registry
                    .acquire_regular_playlist_commit_authority(&resolved)
                    .expect("current occurrence is admitted at the commit boundary");
                let disconnecting = registry.clone();
                let _disconnect_worker = std::thread::spawn(move || {
                    let waiter = disconnecting
                        .disconnect(source_id)
                        .expect("disconnect admitted");
                    disconnect_tx
                        .send(waiter)
                        .expect("disconnect receiver remains alive");
                });

                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !authority.revocation_started() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "disconnect reached revocation"
                    );
                    std::thread::yield_now();
                }
                assert!(
                    !invalidations
                        .has_changed()
                        .expect("invalidation sender alive"),
                    "disconnect cannot publish before the database commit"
                );
                Some(authority)
            })
            .await
            .expect("guarded append has no database failure");
        assert!(matches!(outcome, PlaylistEntryAddOutcome::Committed(_)));
        assert_eq!(
            manager
                .get_playlist_entries(&playlist_id)
                .await
                .expect("load committed occurrence")
                .len(),
            1
        );

        let waiter = disconnect_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("disconnect returns after commit releases authority");
        timeout(Duration::from_secs(2), invalidations.changed())
            .await
            .expect("disconnect publishes after commit")
            .expect("invalidation sender alive");
        assert!(registry
            .acquire_regular_playlist_commit_authority(&resolved)
            .is_none());
        waiter.wait().await;

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn regular_playlist_default_deny_and_invalid_catalogues_fail_closed() {
        let registry = registry();
        let track_id = TrackId::remote("track").expect("track ID");

        let unsupported_source = SourceId::random();
        let unsupported_probe = FakeProbe::new(true);
        let mut unsupported = unsupported_probe.adapter("unsupported");
        unsupported.catalogue = vec![fixture_track(track_id.clone())];
        connect_playlist_fixture(&registry, unsupported_source, unsupported).await;
        let unsupported = registry.resolve_regular_playlist_tracks(&[MediaKey::new(
            unsupported_source,
            track_id.clone(),
        )]);
        assert_eq!(
            unavailable_reason(&unsupported[0]),
            RegularPlaylistUnavailableReason::UnsupportedSource
        );
        assert_eq!(unsupported_probe.stream_calls.load(Ordering::Acquire), 0);

        let duplicate_source = SourceId::random();
        let duplicate_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            duplicate_source,
            duplicate_probe.playlist_adapter(
                "duplicate",
                vec![
                    fixture_track(track_id.clone()),
                    fixture_track(track_id.clone()),
                ],
            ),
        )
        .await;
        assert_eq!(
            registry
                .snapshot(duplicate_source)
                .and_then(|snapshot| snapshot.catalogue)
                .expect("general duplicate catalogue remains published")
                .value
                .tracks()
                .len(),
            2
        );
        let duplicate = registry
            .resolve_regular_playlist_tracks(&[MediaKey::new(duplicate_source, track_id.clone())]);
        assert_eq!(
            unavailable_reason(&duplicate[0]),
            RegularPlaylistUnavailableReason::InvalidCatalogue
        );

        let missing_identity_source = SourceId::random();
        let missing_identity_probe = FakeProbe::new(true);
        let mut missing_identity = fixture_track(track_id.clone());
        missing_identity.native_track_id = None;
        connect_playlist_fixture(
            &registry,
            missing_identity_source,
            missing_identity_probe.playlist_adapter("missing", vec![missing_identity]),
        )
        .await;
        let invalid = registry
            .resolve_regular_playlist_tracks(&[MediaKey::new(missing_identity_source, track_id)]);
        assert_eq!(
            unavailable_reason(&invalid[0]),
            RegularPlaylistUnavailableReason::InvalidCatalogue
        );

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn guarded_playlist_media_rechecks_generation_and_retains_exact_authority() {
        let registry = registry();
        let source_id = SourceId::random();
        let track_id = TrackId::remote("guarded-track").expect("track ID");
        let probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.playlist_adapter("guarded", vec![fixture_track(track_id.clone())]),
        )
        .await;
        let initial =
            registry.resolve_regular_playlist_tracks(&[MediaKey::new(source_id, track_id.clone())]);
        let guard = available(&initial[0]).guard();
        assert_eq!(guard.source_id(), source_id);

        let stream = registry
            .resolve_regular_playlist_stream(guard, track_id.clone())
            .await
            .expect("guarded stream");
        assert!(protected_request_is_active(&stream));
        let artwork = registry
            .resolve_regular_playlist_artwork(guard, track_id.clone())
            .await
            .expect("guarded artwork")
            .expect("fixture artwork");
        assert!(artwork.is_active());
        let parked = registry
            .inner
            .lifecycle
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("parked payload snapshot");

        let mut refreshed_track = fixture_track(track_id.clone());
        refreshed_track.title = "Refreshed".to_string();
        let refreshed_generation = refresh_playlist_catalogue(
            &registry,
            source_id,
            vec![refreshed_track],
            RegularPlaylistCapability::SourceScopedEntries,
        )
        .await;
        assert_ne!(refreshed_generation, guard.catalogue_generation());
        assert!(!protected_request_is_active(&stream));
        assert!(!artwork.is_active());
        assert_eq!(parked.value.tracks[0].title, "Station");

        let calls_before_stale = probe.stream_calls.load(Ordering::Acquire);
        let stale = registry
            .resolve_regular_playlist_stream(guard, track_id.clone())
            .await;
        assert!(matches!(stale, Err(RegularPlaylistMediaError::Unavailable)));
        assert_eq!(
            probe.stream_calls.load(Ordering::Acquire),
            calls_before_stale
        );
        assert!(!registry.are_regular_playlist_tracks_current(&initial));

        let current =
            registry.resolve_regular_playlist_tracks(&[MediaKey::new(source_id, track_id.clone())]);
        assert_eq!(available(&current[0]).metadata().title(), "Refreshed");
        assert!(registry.are_regular_playlist_tracks_current(&current));

        probe.stream_release.send_replace(false);
        let current_guard = available(&current[0]).guard();
        let delayed_registry = registry.clone();
        let delayed_track = track_id.clone();
        let calls_before_delayed = probe.stream_calls.load(Ordering::Acquire);
        let delayed = tokio::spawn(async move {
            delayed_registry
                .resolve_regular_playlist_stream(current_guard, delayed_track)
                .await
        });
        timeout(Duration::from_secs(2), async {
            while probe.stream_calls.load(Ordering::Acquire) == calls_before_delayed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("adapter resolution started");
        refresh_playlist_catalogue(
            &registry,
            source_id,
            vec![fixture_track(track_id)],
            RegularPlaylistCapability::SourceScopedEntries,
        )
        .await;
        probe.stream_release.send_replace(true);
        assert!(matches!(
            delayed.await.expect("resolution task"),
            Err(RegularPlaylistMediaError::Unavailable)
        ));

        let before_disconnect = registry.resolve_regular_playlist_tracks(&[MediaKey::new(
            source_id,
            TrackId::remote("guarded-track").expect("track ID"),
        )]);
        let disconnect_guard = available(&before_disconnect[0]).guard();
        let disconnect_track = available(&before_disconnect[0])
            .media_key()
            .track_id
            .clone();
        let disconnect_stream = registry
            .resolve_regular_playlist_stream(disconnect_guard, disconnect_track.clone())
            .await
            .expect("pre-disconnect stream");
        let disconnect_art = registry
            .resolve_regular_playlist_artwork(disconnect_guard, disconnect_track.clone())
            .await
            .expect("pre-disconnect artwork")
            .expect("fixture artwork");
        let parked_disconnect = registry
            .inner
            .lifecycle
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("parked disconnect snapshot");
        let calls_before_disconnect = probe.stream_calls.load(Ordering::Acquire);
        let waiter = registry.disconnect(source_id).expect("disconnect source");
        assert!(!protected_request_is_active(&disconnect_stream));
        assert!(!disconnect_art.is_active());
        assert_eq!(parked_disconnect.value.tracks.len(), 1);
        let disconnected = registry
            .resolve_regular_playlist_tracks(&[MediaKey::new(source_id, disconnect_track.clone())]);
        assert_eq!(disconnected.len(), 1);
        assert_eq!(
            unavailable_reason(&disconnected[0]),
            RegularPlaylistUnavailableReason::SourceUnavailable
        );
        assert!(matches!(
            registry
                .resolve_regular_playlist_stream(disconnect_guard, disconnect_track)
                .await,
            Err(RegularPlaylistMediaError::Unavailable)
        ));
        assert_eq!(
            probe.stream_calls.load(Ordering::Acquire),
            calls_before_disconnect
        );
        waiter.wait().await;

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn guarded_playlist_media_rejects_replacement_without_calling_successor() {
        let registry = registry();
        let source_id = SourceId::random();
        let track_id = TrackId::remote("replacement-track").expect("track ID");
        let predecessor = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            predecessor.playlist_adapter("predecessor", vec![fixture_track(track_id.clone())]),
        )
        .await;
        let initial =
            registry.resolve_regular_playlist_tracks(&[MediaKey::new(source_id, track_id.clone())]);
        let old_guard = available(&initial[0]).guard();
        let old_stream = registry
            .resolve_regular_playlist_stream(old_guard, track_id.clone())
            .await
            .expect("predecessor stream");
        let old_art = registry
            .resolve_regular_playlist_artwork(old_guard, track_id.clone())
            .await
            .expect("predecessor artwork")
            .expect("fixture artwork");
        let parked = registry
            .inner
            .lifecycle
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("parked predecessor snapshot");

        let successor = FakeProbe::new(true);
        let successor_for_connect = Arc::clone(&successor);
        let successor_track = track_id.clone();
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move {
                    Ok(successor_for_connect
                        .playlist_adapter("successor", vec![fixture_track(successor_track)]))
                },
            )
            .expect("replacement admitted");
        timeout(Duration::from_secs(2), async {
            loop {
                let current = registry
                    .resolve_regular_playlist_tracks(&[MediaKey::new(source_id, track_id.clone())]);
                if matches!(&current[0], RegularPlaylistTrackResolution::Available(track)
                    if track.guard().session_epoch() != old_guard.session_epoch())
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("successor catalogue accepted");
        assert!(!protected_request_is_active(&old_stream));
        assert!(!old_art.is_active());
        assert_eq!(parked.value.tracks.len(), 1);
        assert!(matches!(
            registry
                .resolve_regular_playlist_stream(old_guard, track_id.clone())
                .await,
            Err(RegularPlaylistMediaError::Unavailable)
        ));
        assert_eq!(successor.stream_calls.load(Ordering::Acquire), 0);

        let fresh =
            registry.resolve_regular_playlist_tracks(&[MediaKey::new(source_id, track_id.clone())]);
        registry
            .resolve_regular_playlist_stream(available(&fresh[0]).guard(), track_id)
            .await
            .expect("fresh successor guard");
        assert_eq!(successor.stream_calls.load(Ordering::Acquire), 1);
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn guarded_playlist_media_does_not_retain_final_registry_authority() {
        let registry = registry();
        let source_id = SourceId::random();
        let track_id = TrackId::remote("last-handle-track").expect("track ID");
        let probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.playlist_adapter("last-handle", vec![fixture_track(track_id.clone())]),
        )
        .await;
        let resolved =
            registry.resolve_regular_playlist_tracks(&[MediaKey::new(source_id, track_id.clone())]);
        let guard = available(&resolved[0]).guard();
        let stream = registry
            .resolve_regular_playlist_stream(guard, track_id.clone())
            .await
            .expect("guarded stream");
        let artwork = registry
            .resolve_regular_playlist_artwork(guard, track_id)
            .await
            .expect("guarded artwork")
            .expect("fixture artwork");
        let parked = registry
            .inner
            .lifecycle
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("parked catalogue snapshot");
        assert!(protected_request_is_active(&stream));
        assert!(artwork.is_active());

        drop(registry);

        assert!(!protected_request_is_active(&stream));
        assert!(!artwork.is_active());
        assert_eq!(parked.value.tracks.len(), 1);
        probe.wait_for_close_calls(1).await;
    }

    #[tokio::test]
    async fn guarded_playlist_media_closes_raw_adapter_errors() {
        let registry = registry();
        let source_id = SourceId::random();
        let track_id = TrackId::remote("secret-track-id").expect("track ID");
        let probe = FakeProbe::new(true);
        let secret = format!("https://user:password@private.invalid/{}", Uuid::new_v4());
        let mut adapter = probe.playlist_adapter("failure", vec![fixture_track(track_id.clone())]);
        adapter.stream_failure = Some(secret.clone());
        connect_playlist_fixture(&registry, source_id, adapter).await;
        let resolution =
            registry.resolve_regular_playlist_tracks(&[MediaKey::new(source_id, track_id.clone())]);
        let result = registry
            .resolve_regular_playlist_stream(available(&resolution[0]).guard(), track_id)
            .await;
        let Err(error) = result else {
            panic!("adapter failure must be closed");
        };
        assert_eq!(
            error,
            RegularPlaylistMediaError::BackendFailure(FailureCategory::Connection)
        );
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains("password"));
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn repeated_concurrent_builtin_refreshes_share_one_session_epoch() {
        let registry = registry();
        let source_id = SourceId::radio_browser();
        let adopted = FakeProbe::new(true);
        let unused = FakeProbe::new(true);
        let first_epoch = registry
            .install_builtin_candidate(
                source_id,
                Ok(Box::new(adopted.adapter("builtin")) as Box<dyn ManagedSourceAdapter>),
            )
            .expect("built-in adopted");
        let repeated_epoch = registry
            .install_builtin_candidate(
                source_id,
                Ok(Box::new(unused.adapter("unused")) as Box<dyn ManagedSourceAdapter>),
            )
            .expect("existing built-in retained");
        assert_eq!(first_epoch, repeated_epoch);
        let radio_playlist = registry.resolve_regular_playlist_tracks(&[MediaKey::new(
            source_id,
            TrackId::remote("shared-station").expect("track ID"),
        )]);
        assert_eq!(
            unavailable_reason(&radio_playlist[0]),
            RegularPlaylistUnavailableReason::UnsupportedSource
        );

        let top_clicked = ViewOrigin::radio("top-clicked").expect("view");
        let top_voted = ViewOrigin::radio("top-voted").expect("view");
        adopted.queue_public_view(
            top_clicked.clone(),
            "https://radio.invalid/top-clicked?stream=1",
            Duration::from_millis(20),
        );
        adopted.queue_public_view(
            top_voted.clone(),
            "https://radio.invalid/top-voted?stream=1",
            Duration::from_millis(20),
        );
        let clicked_generation = registry
            .refresh_builtin_radio_view(top_clicked.clone())
            .expect("top-clicked refresh");
        let voted_generation = registry
            .refresh_builtin_radio_view(top_voted.clone())
            .expect("top-voted refresh");
        let clicked_epoch =
            wait_for_view(&registry, source_id, &top_clicked, clicked_generation).await;
        let voted_epoch = wait_for_view(&registry, source_id, &top_voted, voted_generation).await;
        assert_eq!(clicked_epoch, first_epoch);
        assert_eq!(voted_epoch, first_epoch);
        assert_eq!(unused.close_calls.load(Ordering::Acquire), 0);

        registry.shutdown().wait().await;
        assert_eq!(adopted.close_calls.load(Ordering::Acquire), 1);
        assert_eq!(unused.close_calls.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn builtin_constructor_failure_is_retained_without_a_session_epoch() {
        let registry = registry();
        let source_id = SourceId::radio_browser();
        assert!(registry
            .install_builtin_candidate(
                source_id,
                Err(BackendError::Internal(anyhow::anyhow!(
                    "redacted fixture failure"
                ))),
            )
            .is_none());
        let failed = registry.snapshot(source_id).expect("built-in row retained");
        assert!(failed.session_epoch.is_none());
        assert_eq!(failed.state, crate::source_lifecycle::SourceState::Failed);
        assert_eq!(
            failed.failure.expect("connect failure").failure.operation(),
            crate::source_lifecycle::FailureOperation::Connect
        );

        let recovered = FakeProbe::new(true);
        assert!(registry
            .install_builtin_candidate(
                source_id,
                Ok(Box::new(recovered.adapter("recovered")) as Box<dyn ManagedSourceAdapter>),
            )
            .is_some());
        let ready = registry
            .snapshot(source_id)
            .expect("built-in row recovered");
        assert!(ready.session_epoch.is_some());
        assert!(ready.failure.is_none());
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn generation_callback_runs_before_constructor_future_is_polled() {
        let registry = registry();
        let source_id = SourceId::random();
        registry
            .claim_provenance(source_id, SourceProvenance::Saved)
            .expect("saved claim");
        let seen_generation = Arc::new(AtomicU64::new(0));
        let future_observer = Arc::clone(&seen_generation);
        let callback_observer = Arc::clone(&seen_generation);
        let probe = FakeProbe::new(true);

        let returned_generation = registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                move |generation| callback_observer.store(generation, Ordering::Release),
                move || async move {
                    assert_ne!(
                        future_observer.load(Ordering::Acquire),
                        0,
                        "constructor was polled before generation publication"
                    );
                    Ok(probe.adapter("ordered"))
                },
            )
            .expect("connection admitted");

        assert_eq!(seen_generation.load(Ordering::Acquire), returned_generation);
        let (generation, epoch) = wait_for_catalogue(&registry, source_id).await;
        assert_eq!(generation, returned_generation);
        assert!(registry.is_current_catalogue(source_id, generation, epoch));

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn exact_publishers_are_idempotent_and_release_independently() {
        let registry = registry();
        let source_id = SourceId::random();
        let claims = ProvenanceClaims::default();

        assert!(claims.ensure(&registry, source_id, SourceProvenance::Saved, "saved:a"));
        assert!(claims.ensure(&registry, source_id, SourceProvenance::Saved, "saved:a"));
        assert!(claims.ensure(&registry, source_id, SourceProvenance::Saved, "saved:b"));
        assert!(claims.ensure(
            &registry,
            source_id,
            SourceProvenance::Discovery,
            "discovery:one"
        ));
        let snapshot = registry.snapshot(source_id).expect("claimed source");
        assert_eq!(snapshot.provenance.claim_count(SourceProvenance::Saved), 2);
        assert_eq!(
            snapshot.provenance.claim_count(SourceProvenance::Discovery),
            1
        );

        assert!(claims.release(&registry, source_id, SourceProvenance::Saved, "saved:a"));
        assert!(!claims.contains(source_id, SourceProvenance::Saved, "saved:a"));
        assert!(claims.contains(source_id, SourceProvenance::Saved, "saved:b"));
        assert_eq!(
            registry
                .snapshot(source_id)
                .expect("source retained")
                .provenance
                .claim_count(SourceProvenance::Saved),
            1
        );
        assert!(!claims.release(&registry, source_id, SourceProvenance::Saved, "saved:a"));
        assert!(claims.release(&registry, source_id, SourceProvenance::Saved, "saved:b"));
        assert!(claims.release(
            &registry,
            source_id,
            SourceProvenance::Discovery,
            "discovery:one"
        ));

        wait_until_pruned(&registry, source_id).await;
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn replacement_revokes_old_media_and_final_claim_auto_prunes() {
        let registry = registry();
        let source_id = SourceId::random();
        let claims = ProvenanceClaims::default();
        assert!(claims.ensure(&registry, source_id, SourceProvenance::Saved, "saved"));
        let predecessor = FakeProbe::new(true);
        let predecessor_for_connect = Arc::clone(&predecessor);
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(predecessor_for_connect.adapter("predecessor")) },
            )
            .expect("predecessor admitted");
        let (_, predecessor_epoch) = wait_for_catalogue(&registry, source_id).await;
        let track_id = TrackId::remote("track").expect("track ID");
        let predecessor_request = registry
            .resolve_stream(source_id, predecessor_epoch, track_id.clone())
            .await
            .expect("predecessor media");
        assert!(protected_request_is_active(&predecessor_request));

        let successor = FakeProbe::new(true);
        let successor_for_connect = Arc::clone(&successor);
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(successor_for_connect.adapter("successor")) },
            )
            .expect("replacement admitted");
        let (successor_generation, successor_epoch) = timeout(Duration::from_secs(2), async {
            loop {
                let current = wait_for_catalogue(&registry, source_id).await;
                if current.1 != predecessor_epoch {
                    return current;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("successor catalogue accepted");

        assert!(!protected_request_is_active(&predecessor_request));
        assert!(registry
            .resolve_stream(source_id, predecessor_epoch, track_id.clone())
            .await
            .is_err());
        assert_eq!(successor.stream_calls.load(Ordering::Acquire), 0);
        let successor_request = registry
            .resolve_stream(source_id, successor_epoch, track_id)
            .await
            .expect("successor media");
        assert!(protected_request_is_active(&successor_request));
        assert!(registry.has_session_epoch(source_id, successor_epoch));
        assert!(registry.is_current_catalogue(source_id, successor_generation, successor_epoch));

        assert!(claims.release(&registry, source_id, SourceProvenance::Saved, "saved"));
        wait_until_pruned(&registry, source_id).await;
        predecessor.wait_for_close_calls(1).await;
        successor.wait_for_close_calls(1).await;
        assert!(!protected_request_is_active(&successor_request));
        assert_eq!(predecessor.close_calls.load(Ordering::Acquire), 1);
        assert_eq!(successor.close_calls.load(Ordering::Acquire), 1);

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn greatest_cross_view_generation_wins_and_is_rechecked_at_consume() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let (source_id, session_epoch) = connect_view_fixture(&registry, &probe).await;
        let older = ViewOrigin::radio("older").expect("view");
        let newer = ViewOrigin::radio("newer").expect("view");
        let newest = ViewOrigin::radio("newest").expect("view");

        probe.queue_public_view(
            older.clone(),
            "https://radio.invalid/older?format=aac",
            Duration::from_millis(75),
        );
        probe.queue_public_view(
            newer.clone(),
            "https://radio.invalid/newer?format=opus",
            Duration::ZERO,
        );
        let older_generation = registry
            .refresh_view(source_id, older.clone())
            .expect("older refresh");
        let newer_generation = registry
            .refresh_view(source_id, newer.clone())
            .expect("newer refresh");
        assert!(newer_generation > older_generation);
        wait_for_view(&registry, source_id, &newer, newer_generation).await;
        wait_for_view(&registry, source_id, &older, older_generation).await;

        let track_id = TrackId::remote("shared-station").expect("track ID");
        let pending = registry
            .resolve_stream(source_id, session_epoch, track_id.clone())
            .await
            .expect("newer public locator");
        assert!(pending.is_active());

        probe.queue_public_view(
            newest.clone(),
            "https://radio.invalid/newest?format=flac",
            Duration::ZERO,
        );
        let newest_generation = registry
            .refresh_view(source_id, newest.clone())
            .expect("newest refresh");
        wait_for_view(&registry, source_id, &newest, newest_generation).await;
        assert!(
            consume_public(pending).is_err(),
            "a newer overlapping view must invalidate the prior winner"
        );

        let endpoint = consume_public(
            registry
                .resolve_stream(source_id, session_epoch, track_id)
                .await
                .expect("newest public locator"),
        )
        .expect("winner remains current");
        assert_eq!(
            endpoint.as_str(),
            "https://radio.invalid/newest?format=flac"
        );
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn same_view_replacement_revokes_its_pending_public_request() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let (source_id, session_epoch) = connect_view_fixture(&registry, &probe).await;
        let view = ViewOrigin::radio("same-view").expect("view");
        let track_id = TrackId::remote("shared-station").expect("track ID");

        probe.queue_public_view(
            view.clone(),
            "https://radio.invalid/first?quality=high",
            Duration::ZERO,
        );
        let first_generation = registry
            .refresh_view(source_id, view.clone())
            .expect("first refresh");
        wait_for_view(&registry, source_id, &view, first_generation).await;
        let first = registry
            .resolve_stream(source_id, session_epoch, track_id.clone())
            .await
            .expect("first locator");

        probe.queue_public_view(
            view.clone(),
            "https://radio.invalid/second?quality=low",
            Duration::ZERO,
        );
        let second_generation = registry
            .refresh_view(source_id, view.clone())
            .expect("second refresh");
        wait_for_view(&registry, source_id, &view, second_generation).await;
        assert!(consume_public(first).is_err());
        let second = consume_public(
            registry
                .resolve_stream(source_id, session_epoch, track_id)
                .await
                .expect("second locator"),
        )
        .expect("second locator current");
        assert_eq!(second.as_str(), "https://radio.invalid/second?quality=low");
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn view_removal_and_disconnect_revoke_public_requests() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let (source_id, session_epoch) = connect_view_fixture(&registry, &probe).await;
        let fallback_view = ViewOrigin::radio("fallback-view").expect("view");
        let removed_view = ViewOrigin::radio("removable-view").expect("view");
        let track_id = TrackId::remote("shared-station").expect("track ID");

        probe.queue_public_view(
            fallback_view.clone(),
            "https://radio.invalid/fallback?token=ordinary-query",
            Duration::ZERO,
        );
        let fallback_generation = registry
            .refresh_view(source_id, fallback_view.clone())
            .expect("fallback view refresh");
        wait_for_view(&registry, source_id, &fallback_view, fallback_generation).await;

        probe.queue_public_view(
            removed_view.clone(),
            "https://radio.invalid/removal?token=ordinary-query",
            Duration::ZERO,
        );
        let generation = registry
            .refresh_view(source_id, removed_view.clone())
            .expect("view refresh");
        wait_for_view(&registry, source_id, &removed_view, generation).await;
        let removed = registry
            .resolve_stream(source_id, session_epoch, track_id.clone())
            .await
            .expect("removal locator");
        assert!(registry.remove_view(source_id, &removed_view));
        assert!(consume_public(removed).is_err());
        let fallback = consume_public(
            registry
                .resolve_stream(source_id, session_epoch, track_id.clone())
                .await
                .expect("fallback locator"),
        )
        .expect("remaining view becomes authoritative");
        assert_eq!(
            fallback.as_str(),
            "https://radio.invalid/fallback?token=ordinary-query"
        );

        let disconnected = registry
            .resolve_stream(source_id, session_epoch, track_id)
            .await
            .expect("disconnect locator");
        registry.disconnect(source_id).expect("disconnect admitted");
        assert!(consume_public(disconnected).is_err());
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn pending_public_request_does_not_retain_the_last_registry_handle() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let (source_id, session_epoch) = connect_view_fixture(&registry, &probe).await;
        let view = ViewOrigin::radio("last-drop").expect("view");
        let track_id = TrackId::remote("shared-station").expect("track ID");
        probe.queue_public_view(
            view.clone(),
            "https://radio.invalid/last-drop?stream=1",
            Duration::ZERO,
        );
        let generation = registry
            .refresh_view(source_id, view.clone())
            .expect("view refresh");
        wait_for_view(&registry, source_id, &view, generation).await;
        let pending = registry
            .resolve_stream(source_id, session_epoch, track_id)
            .await
            .expect("pending locator");

        drop(registry);
        probe.wait_for_close_calls(1).await;
        assert!(consume_public(pending).is_err());
        assert_eq!(probe.close_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn shutdown_closes_the_gate_and_joins_held_retirement() {
        let registry = registry();
        let source_id = SourceId::random();
        registry
            .claim_provenance(source_id, SourceProvenance::Saved)
            .expect("saved claim");
        let held = FakeProbe::new(false);
        let held_for_connect = Arc::clone(&held);
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(held_for_connect.adapter("held")) },
            )
            .expect("connection admitted");
        wait_for_catalogue(&registry, source_id).await;

        let barrier = registry.shutdown();
        held.wait_for_close_calls(1).await;
        assert!(registry.is_shutting_down());
        assert!(registry.snapshot_all().shutting_down);
        assert!(!barrier.is_complete());
        assert!(registry
            .claim_provenance(SourceId::random(), SourceProvenance::Saved)
            .is_none());
        let callback_called = Arc::new(AtomicBool::new(false));
        let callback_observer = Arc::clone(&callback_called);
        let constructor_polled = Arc::new(AtomicBool::new(false));
        let constructor_observer = Arc::clone(&constructor_polled);
        assert!(registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                move |_| callback_observer.store(true, Ordering::Release),
                move || async move {
                    constructor_observer.store(true, Ordering::Release);
                    Ok(FakeProbe::new(true).adapter("rejected"))
                },
            )
            .is_none());
        assert!(!callback_called.load(Ordering::Acquire));
        assert!(!constructor_polled.load(Ordering::Acquire));

        held.close_release.send_replace(true);
        timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("shutdown joined held close");
        assert_eq!(held.close_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn cancelled_interactive_jellyfin_login_finishes_then_logs_out_without_catalogue_io() {
        let token = Uuid::new_v4().to_string();
        let service = MockHttpService::start(vec![
            MockRoute::new(Method::POST, "/Users/AuthenticateByName").reply(
                MockResponse::json(serde_json::json!({
                    "User": { "Id": "user-id", "Name": "Fixture" },
                    "AccessToken": token,
                }))
                .with_delay(Duration::from_millis(150)),
            ),
            MockRoute::new(Method::POST, "/Sessions/Logout")
                .reply(MockResponse::status(StatusCode::NO_CONTENT)),
        ])
        .await;
        let registry = registry();
        let source_id = SourceId::random();
        let claim = registry
            .claim_provenance(source_id, SourceProvenance::Saved)
            .expect("saved claim");
        let server_url = service.base_url();
        let password = Uuid::new_v4().to_string();
        registry
            .connect_jellyfin_session(
                source_id,
                |_| {},
                move || async move {
                    let client = crate::jellyfin::client::JellyfinClient::authenticate(
                        &server_url,
                        "fixture-user",
                        &password,
                    )
                    .await?;
                    Ok(crate::jellyfin::JellyfinBackend::stage_authenticated(
                        "fixture", client,
                    ))
                },
            )
            .expect("interactive login admitted");

        wait_for_request_count(&service, 1).await;
        assert!(registry.release_provenance(source_id, claim));
        wait_for_request_count(&service, 2).await;
        wait_until_pruned(&registry, source_id).await;
        registry.shutdown().wait().await;

        let requests = service.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].uri.path(), "/Users/AuthenticateByName");
        assert_eq!(requests[1].uri.path(), "/Sessions/Logout");
        service.finish().await;
    }

    #[tokio::test]
    async fn cancelled_jellyfin_api_key_staging_never_logs_out_durable_credential() {
        let service = MockHttpService::start(vec![MockRoute::get("/System/Ping")
            .reply(MockResponse::text("Jellyfin Server").with_delay(Duration::from_millis(150)))])
        .await;
        let registry = registry();
        let source_id = SourceId::random();
        let claim = registry
            .claim_provenance(source_id, SourceProvenance::Saved)
            .expect("saved claim");
        let server_url = service.base_url();
        registry
            .connect_jellyfin_api_key(
                source_id,
                |_| {},
                move || async move {
                    let client = crate::jellyfin::client::JellyfinClient::new(
                        &server_url,
                        "durable-api-key",
                        "user-id",
                    )?;
                    Ok(crate::jellyfin::JellyfinBackend::stage_authenticated(
                        "fixture", client,
                    ))
                },
            )
            .expect("API-key staging admitted");

        wait_for_request_count(&service, 1).await;
        assert!(registry.release_provenance(source_id, claim));
        wait_until_pruned(&registry, source_id).await;
        registry.shutdown().wait().await;

        let requests = service.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].uri.path(), "/System/Ping");
        service.finish().await;
    }

    #[tokio::test]
    async fn removable_connect_publishes_pathless_epoch_and_resolves_only_accepted_tracks() {
        let registry = registry();
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let path = mount.path().join("accepted.wav");
        let expected = minimal_wav_bytes(0x80);
        std::fs::write(&path, &expected).expect("write accepted removable WAV");
        let source_id =
            SourceId::removable("registry:test:pathless").expect("removable source identity");
        let claim = registry
            .claim_provenance(source_id, SourceProvenance::Removable)
            .expect("claim removable source");
        let observed_generation = Arc::new(AtomicU64::new(0));
        let callback_generation = Arc::clone(&observed_generation);
        let generation = registry
            .connect_removable(source_id, mount.path().to_path_buf(), move |generation| {
                callback_generation.store(generation, Ordering::Release);
            })
            .expect("removable connection admitted");
        assert_eq!(observed_generation.load(Ordering::Acquire), generation);

        let (accepted_generation, session_epoch) = wait_for_catalogue(&registry, source_id).await;
        assert_eq!(accepted_generation, generation);
        let snapshot = registry.snapshot(source_id).expect("removable snapshot");
        assert_eq!(snapshot.state, crate::source_lifecycle::SourceState::Ready);
        assert_eq!(snapshot.session_epoch, Some(session_epoch));
        assert!(snapshot.provenance.contains(SourceProvenance::Removable));
        assert_eq!(
            snapshot.visibility,
            crate::source_lifecycle::SourceVisibility::Visible
        );
        let catalogue = snapshot.catalogue.expect("accepted removable catalogue");
        assert_eq!(catalogue.value.tracks().len(), 1);
        let published = &catalogue.value.tracks()[0];
        assert!(published.file_path.is_none());
        assert!(published.stream_url.is_none());
        assert!(published.cover_art_url.is_none());
        let track_id = published
            .native_track_id
            .clone()
            .expect("removable native identity");
        assert_eq!(
            track_id
                .removable_relative_path()
                .expect("decode accepted identity"),
            std::path::PathBuf::from("accepted.wav")
        );
        let playlist =
            registry.resolve_regular_playlist_tracks(&[MediaKey::new(source_id, track_id.clone())]);
        assert_eq!(
            unavailable_reason(&playlist[0]),
            RegularPlaylistUnavailableReason::UnsupportedSource
        );

        let appeared_later = mount.path().join("appeared-later.wav");
        std::fs::write(&appeared_later, minimal_wav_bytes(0x40))
            .expect("write unlisted removable WAV");
        let unlisted_id = TrackId::removable_relative(mount.path(), &appeared_later)
            .expect("unlisted relative identity");
        assert!(registry
            .resolve_stream(source_id, session_epoch, unlisted_id)
            .await
            .is_err());

        let resolved = registry
            .resolve_stream(source_id, session_epoch, track_id)
            .await
            .expect("resolve accepted removable media");
        assert_eq!(read_file_stream(&resolved), expected);

        assert!(registry.release_provenance(source_id, claim));
        wait_until_pruned(&registry, source_id).await;
        assert!(!resolved.is_active());
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn removable_reconnect_mints_a_new_epoch_and_revokes_the_predecessor() {
        let registry = registry();
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let path = mount.path().join("same.wav");
        let expected = minimal_wav_bytes(0x80);
        std::fs::write(&path, &expected).expect("write removable WAV");
        let source_id =
            SourceId::removable("registry:test:reconnect").expect("removable source identity");
        let claim = registry
            .claim_provenance(source_id, SourceProvenance::Removable)
            .expect("claim removable source");

        registry
            .connect_removable(source_id, mount.path().to_path_buf(), |_| {})
            .expect("initial removable connection admitted");
        let (_, predecessor_epoch) = wait_for_catalogue(&registry, source_id).await;
        let predecessor_track = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .and_then(|catalogue| catalogue.value.tracks().first().cloned())
            .and_then(|track| track.native_track_id)
            .expect("predecessor track identity");
        let predecessor = registry
            .resolve_stream(source_id, predecessor_epoch, predecessor_track.clone())
            .await
            .expect("resolve predecessor removable media");
        assert!(predecessor.is_active());

        registry
            .disconnect(source_id)
            .expect("disconnect predecessor")
            .wait()
            .await;
        assert!(!predecessor.is_active());
        let ResolvedSourceStream::File(predecessor_media) = &predecessor else {
            panic!("fixture expected removable file media");
        };
        assert_eq!(
            predecessor_media
                .try_clone_file()
                .expect_err("disconnect revokes predecessor file authority")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let dormant = registry
            .snapshot(source_id)
            .expect("claimed source retained");
        assert_eq!(dormant.state, crate::source_lifecycle::SourceState::Dormant);
        assert!(dormant.catalogue.is_none());

        registry
            .connect_removable(source_id, mount.path().to_path_buf(), |_| {})
            .expect("successor removable connection admitted");
        let (_, successor_epoch) = wait_for_catalogue(&registry, source_id).await;
        assert_ne!(successor_epoch, predecessor_epoch);
        let successor_track = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .and_then(|catalogue| catalogue.value.tracks().first().cloned())
            .and_then(|track| track.native_track_id)
            .expect("successor track identity");
        assert_eq!(successor_track, predecessor_track);
        assert!(registry
            .resolve_stream(source_id, predecessor_epoch, successor_track.clone())
            .await
            .is_err());
        let successor = registry
            .resolve_stream(source_id, successor_epoch, successor_track)
            .await
            .expect("resolve successor removable media");
        assert_eq!(read_file_stream(&successor), expected);

        assert!(registry.release_provenance(source_id, claim));
        wait_until_pruned(&registry, source_id).await;
        assert!(!successor.is_active());
        registry.shutdown().wait().await;
    }

    #[test]
    fn removable_disconnect_cancels_a_queued_blocking_scan_without_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("single-blocking-thread runtime");
        runtime.block_on(async {
            let registry = registry();
            let mount = tempfile::tempdir().expect("temporary removable mount");
            std::fs::write(mount.path().join("queued.wav"), minimal_wav_bytes(0x80))
                .expect("write queued removable WAV");
            let source_id = SourceId::removable("registry:test:cancel-queued")
                .expect("removable source identity");
            let claim = registry
                .claim_provenance(source_id, SourceProvenance::Removable)
                .expect("claim removable source");

            let (blocker_started_tx, blocker_started_rx) = oneshot::channel();
            let (release_blocker_tx, release_blocker_rx) = std::sync::mpsc::channel();
            let blocker = Handle::current().spawn_blocking(move || {
                let _ = blocker_started_tx.send(());
                release_blocker_rx.recv().expect("release blocking pool");
            });
            blocker_started_rx.await.expect("blocking pool occupied");

            registry
                .connect_removable(source_id, mount.path().to_path_buf(), |_| {})
                .expect("queued removable connection admitted");
            // Let the tracked constructor reach `spawn_blocking`; the only
            // blocking slot is held above, so its cooperative scan is queued.
            tokio::task::yield_now().await;
            let waiter = registry
                .disconnect(source_id)
                .expect("disconnect queued removable scan");
            assert!(!waiter.is_complete());
            release_blocker_tx.send(()).expect("release blocking pool");
            blocker.await.expect("blocking pool fixture");
            waiter.wait().await;

            let snapshot = registry
                .snapshot(source_id)
                .expect("claimed source retained");
            assert_eq!(
                snapshot.state,
                crate::source_lifecycle::SourceState::Dormant
            );
            assert!(snapshot.catalogue.is_none());
            assert!(snapshot.failure.is_none());
            assert!(snapshot.pending_connect.is_none());
            assert!(registry.release_provenance(source_id, claim));
            wait_until_pruned(&registry, source_id).await;
            registry.shutdown().wait().await;
        });
    }

    #[test]
    fn removable_shutdown_joins_a_queued_scan_and_rejects_later_connect() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("single-blocking-thread runtime");
        runtime.block_on(async {
            let registry = registry();
            let mount = tempfile::tempdir().expect("temporary removable mount");
            std::fs::write(mount.path().join("queued.wav"), minimal_wav_bytes(0x80))
                .expect("write queued removable WAV");
            let source_id = SourceId::removable("registry:test:shutdown-queued")
                .expect("removable source identity");
            registry
                .claim_provenance(source_id, SourceProvenance::Removable)
                .expect("claim removable source");

            let (blocker_started_tx, blocker_started_rx) = oneshot::channel();
            let (release_blocker_tx, release_blocker_rx) = std::sync::mpsc::channel();
            let blocker = Handle::current().spawn_blocking(move || {
                let _ = blocker_started_tx.send(());
                release_blocker_rx.recv().expect("release blocking pool");
            });
            blocker_started_rx.await.expect("blocking pool occupied");

            registry
                .connect_removable(source_id, mount.path().to_path_buf(), |_| {})
                .expect("queued removable connection admitted");
            tokio::task::yield_now().await;
            let barrier = registry.shutdown();
            assert!(!barrier.is_complete());
            assert!(registry
                .connect_removable(source_id, mount.path().to_path_buf(), |_| {})
                .is_none());

            release_blocker_tx.send(()).expect("release blocking pool");
            blocker.await.expect("blocking pool fixture");
            timeout(Duration::from_secs(2), barrier.wait())
                .await
                .expect("shutdown joins queued removable scan");
            assert!(registry.is_shutting_down());
        });
    }

    #[tokio::test]
    async fn external_admission_is_pathless_exact_epoch_and_exact_track() {
        let registry = registry();
        let directory = tempfile::tempdir().expect("external fixture directory");
        let expected = minimal_wav_bytes(128);
        let (file, hint) = external_fixture(&directory, 128);
        let session = registry
            .adopt_external_file(file, hint)
            .expect("admit external file");

        let snapshot = registry
            .snapshot(session.source_id())
            .expect("external lifecycle snapshot");
        assert_eq!(
            snapshot.visibility,
            crate::source_lifecycle::SourceVisibility::Hidden
        );
        assert_eq!(
            snapshot.retention,
            crate::source_lifecycle::Retention::Ephemeral
        );
        assert_eq!(snapshot.session_epoch, Some(session.session_epoch()));
        assert!(snapshot.provenance.contains(SourceProvenance::External));
        let catalogue = snapshot.catalogue.expect("external catalogue");
        assert_eq!(catalogue.value.tracks().len(), 1);
        let published = &catalogue.value.tracks()[0];
        assert_eq!(published.native_track_id.as_ref(), Some(session.track_id()));
        assert!(published.file_path.is_none());
        assert!(published.stream_url.is_none());
        assert!(published.cover_art_url.is_none());
        let playlist = registry.resolve_regular_playlist_tracks(&[MediaKey::new(
            session.source_id(),
            session.track_id().clone(),
        )]);
        assert_eq!(
            unavailable_reason(&playlist[0]),
            RegularPlaylistUnavailableReason::UnsupportedSource
        );

        let wrong_track = TrackId::external();
        assert!(registry
            .resolve_stream(session.source_id(), session.session_epoch(), wrong_track)
            .await
            .is_err());
        assert!(registry
            .resolve_stream(
                session.source_id(),
                session.session_epoch().wrapping_add(1),
                session.track_id().clone(),
            )
            .await
            .is_err());
        let resolved = registry
            .resolve_stream(
                session.source_id(),
                session.session_epoch(),
                session.track_id().clone(),
            )
            .await
            .expect("resolve exact external identity");
        assert_eq!(read_file_stream(&resolved), expected);

        registry
            .retire_external(session.source_id())
            .expect("retire external session")
            .wait()
            .await;
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn repeated_external_opens_are_independent_and_retire_exactly_once() {
        let registry = registry();
        let directory = tempfile::tempdir().expect("external fixture directory");
        let path = directory.path().join("fixture.wav");
        std::fs::write(&path, minimal_wav_bytes(192)).expect("write shared external WAV");
        let first = registry
            .adopt_external_file(
                File::open(&path).expect("open first external handle"),
                ExternalFileHint::new("fixture.wav", Some("wav")).expect("first hint"),
            )
            .expect("admit first external session");
        let second = registry
            .adopt_external_file(
                File::open(&path).expect("open second external handle"),
                ExternalFileHint::new("fixture.wav", Some("wav")).expect("second hint"),
            )
            .expect("admit second external session");
        assert_ne!(first.source_id(), second.source_id());
        assert_ne!(first.track_id(), second.track_id());

        let pending = registry
            .resolve_stream(
                first.source_id(),
                first.session_epoch(),
                first.track_id().clone(),
            )
            .await
            .expect("resolve first session");
        let first_waiter = registry
            .retire_external(first.source_id())
            .expect("first retirement admitted");
        assert!(registry.retire_external(first.source_id()).is_none());
        first_waiter.wait().await;
        assert_eq!(first.close_calls(), 1);
        assert!(!pending.is_active());
        let ResolvedSourceStream::File(media) = &pending else {
            panic!("fixture expected retained file media");
        };
        let error = media
            .try_clone_file()
            .expect_err("retirement revokes future handle clones");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

        registry
            .retire_external(second.source_id())
            .expect("second retirement admitted")
            .wait()
            .await;
        assert_eq!(second.close_calls(), 1);
        wait_until_pruned(&registry, first.source_id()).await;
        wait_until_pruned(&registry, second.source_id()).await;
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn superseded_external_delivery_never_publishes_a_source() {
        let registry = registry();
        let directory = tempfile::tempdir().expect("external fixture directory");
        let (file, hint) = external_fixture(&directory, 48);
        let current = AtomicBool::new(true);

        let result = registry.adopt_external_file_inner(
            file,
            hint,
            || current.store(false, Ordering::Release),
            || {},
            || current.load(Ordering::Acquire),
        );

        assert!(result.is_err());
        assert!(lock(&registry.inner.external_sessions).is_empty());
        assert!(registry
            .snapshot_all()
            .sources
            .into_iter()
            .all(|(_, snapshot)| !snapshot.provenance.contains(SourceProvenance::External)));
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn shutdown_serializes_with_external_publication_and_owns_retirement() {
        let registry = registry();
        let directory = tempfile::tempdir().expect("external fixture directory");
        let (file, hint) = external_fixture(&directory, 96);
        let (gate_acquired_tx, gate_acquired_rx) = oneshot::channel();
        let (release_adoption_tx, release_adoption_rx) = oneshot::channel();
        let adoption_registry = registry.clone();
        let adoption = tokio::task::spawn_blocking(move || {
            adoption_registry.adopt_external_file_with_gate_hook(file, hint, move || {
                let _ = gate_acquired_tx.send(());
                release_adoption_rx
                    .blocking_recv()
                    .expect("release external adoption");
            })
        });
        gate_acquired_rx.await.expect("adoption owns gate");

        let (shutdown_started_tx, shutdown_started_rx) = oneshot::channel();
        let shutdown_registry = registry.clone();
        let shutdown = tokio::task::spawn_blocking(move || {
            let _ = shutdown_started_tx.send(());
            shutdown_registry.shutdown()
        });
        shutdown_started_rx.await.expect("shutdown started");
        release_adoption_tx.send(()).expect("finish adoption");

        let session = adoption
            .await
            .expect("adoption task")
            .expect("adoption publishes before waiting shutdown");
        let barrier = shutdown.await.expect("shutdown task");
        barrier.wait().await;
        assert_eq!(session.close_calls(), 1);
        assert!(registry.retire_external(session.source_id()).is_none());
        assert!(registry
            .resolve_stream(
                session.source_id(),
                session.session_epoch(),
                session.track_id().clone(),
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn shutdown_winning_before_publication_drops_only_a_non_adapter_candidate() {
        let registry = registry();
        let directory = tempfile::tempdir().expect("external fixture directory");
        let (file, hint) = external_fixture(&directory, 32);
        let (validated_tx, validated_rx) = oneshot::channel();
        let (release_candidate_tx, release_candidate_rx) = oneshot::channel();
        let adoption_registry = registry.clone();
        let adoption = tokio::task::spawn_blocking(move || {
            adoption_registry.adopt_external_file_with_validation_hook(file, hint, move || {
                let _ = validated_tx.send(());
                release_candidate_rx
                    .blocking_recv()
                    .expect("release validated candidate");
            })
        });
        validated_rx.await.expect("exact file validated");

        let barrier = registry.shutdown();
        barrier.wait().await;
        release_candidate_tx
            .send(())
            .expect("release candidate after shutdown");
        assert!(adoption.await.expect("adoption task").is_err());
        assert!(lock(&registry.inner.external_sessions).is_empty());
        assert!(registry
            .snapshot_all()
            .sources
            .into_iter()
            .all(|(_, snapshot)| !snapshot.provenance.contains(SourceProvenance::External)));
    }
}
