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
use crate::architecture::{
    MediaKey, NativePlaylistId, ServerPlaylistSnapshot, ServerPlaylistSummary, SourceId, TrackId,
    ViewOrigin,
};
use crate::external_file::{ExternalFileCandidate, ExternalFileHint};
use crate::local::resolver::ResolvedFileMedia;
use crate::source_lifecycle::{
    AdapterCloseFuture, AdapterStream, AdapterTaskResult, CatalogueCommitAuthority,
    CatalogueCommitRequest, CloseAuthority, ConstructionCancellationPolicy, FailureCategory,
    LifecycleAdapter, LifecycleBaseline, LifecycleSnapshot, ProvenanceClaimId, RefreshLane,
    RefreshTaskResult, RetirementWaiter, SessionCommitAuthority, SessionOperationError,
    SessionOperationReceipt, ShutdownBarrier, SourceLifecycleRegistry, SourceProvenance,
};
use url::Url;

pub type CatalogueFuture =
    Pin<Box<dyn Future<Output = BackendResult<Vec<Track>>> + Send + 'static>>;
pub type ViewFuture = Pin<Box<dyn Future<Output = ViewLoadResult> + Send + 'static>>;
pub type StreamFuture =
    Pin<Box<dyn Future<Output = BackendResult<AdapterStream>> + Send + 'static>>;
type ArtworkFuture =
    Pin<Box<dyn Future<Output = BackendResult<Option<ResolvedHttpRequest>>> + Send + 'static>>;
// Record C intentionally stops at an internally tested authority foundation;
// Record D is the first non-test caller of the server-playlist surface.
#[cfg_attr(not(test), allow(dead_code))]
pub type ServerPlaylistListFuture =
    Pin<Box<dyn Future<Output = BackendResult<Vec<ServerPlaylistSummary>>> + Send + 'static>>;
#[cfg_attr(not(test), allow(dead_code))]
pub type ServerPlaylistSnapshotFuture =
    Pin<Box<dyn Future<Output = BackendResult<ServerPlaylistSnapshot>> + Send + 'static>>;

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

/// Whether an adopted adapter can read server-owned playlist snapshots.
///
/// This is deliberately independent from [`RegularPlaylistCapability`]. A
/// server-native snapshot can preserve an exact track ID that the current
/// catalogue does not publish, but it never grants playlist membership or
/// playback authority by itself.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum ServerPlaylistCapability {
    #[default]
    Unsupported,
    PullSnapshots,
}

/// One complete server-playlist listing bound to its exact successful source
/// session.
///
/// The receipt stays private and retains no lease or adapter. Callers can
/// derive only a presence selection or exact absence evidence; they cannot
/// extract an epoch or forge a commit admission from list contents.
#[cfg_attr(not(test), allow(dead_code))]
pub struct ServerPlaylistListing {
    source_id: SourceId,
    receipt: SessionOperationReceipt<dyn ManagedSourceAdapter>,
    playlists: Vec<ServerPlaylistSummary>,
    playlist_ids: HashSet<NativePlaylistId>,
}

/// Unforgeable in-process identity for one exact successful server-playlist
/// read result.
///
/// This deliberately carries no source, native playlist, session, or adapter
/// data. Pointer identity binds a commit authority back to the exact pull or
/// absence evidence whose lifecycle receipt admitted it, without exposing
/// any of those values at the persistence boundary.
#[derive(Clone)]
struct ServerPlaylistCommitBinding(Arc<()>);

impl ServerPlaylistCommitBinding {
    fn fresh() -> Self {
        Self(Arc::new(()))
    }

    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl ServerPlaylistListing {
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn playlists(&self) -> &[ServerPlaylistSummary] {
        &self.playlists
    }

    /// Select one exact native identity only when the complete list contains
    /// it. Similar names, contents, casing, and whitespace never match.
    pub fn select(&self, native_id: &NativePlaylistId) -> Option<ServerPlaylistSelection> {
        self.playlist_ids
            .contains(native_id)
            .then(|| ServerPlaylistSelection {
                source_id: self.source_id,
                native_id: native_id.clone(),
                receipt: self.receipt.clone(),
            })
    }

    /// Mint exact absence evidence only from this successful complete list.
    /// Detail failures and partial/malformed list responses cannot construct
    /// this type.
    pub fn prove_absent(
        &self,
        native_id: &NativePlaylistId,
    ) -> Option<ServerPlaylistAbsenceEvidence> {
        (!self.playlist_ids.contains(native_id)).then(|| ServerPlaylistAbsenceEvidence {
            source_id: self.source_id,
            native_id: native_id.clone(),
            receipt: self.receipt.clone(),
            commit_binding: ServerPlaylistCommitBinding::fresh(),
        })
    }
}

/// Opaque proof that one exact native playlist was present in a successful
/// complete listing. It is consumed by the detail request and has no public
/// identity, epoch, adapter, or authority accessors.
#[cfg_attr(not(test), allow(dead_code))]
pub struct ServerPlaylistSelection {
    source_id: SourceId,
    native_id: NativePlaylistId,
    receipt: SessionOperationReceipt<dyn ManagedSourceAdapter>,
}

/// One successfully fetched detail snapshot with an exact-session receipt for
/// final persistence admission.
#[cfg_attr(not(test), allow(dead_code))]
pub struct ServerPlaylistPull {
    source_id: SourceId,
    snapshot: ServerPlaylistSnapshot,
    receipt: SessionOperationReceipt<dyn ManagedSourceAdapter>,
    commit_binding: ServerPlaylistCommitBinding,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ServerPlaylistPull {
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn native_id(&self) -> &NativePlaylistId {
        self.snapshot.native_id()
    }

    pub fn snapshot(&self) -> &ServerPlaylistSnapshot {
        &self.snapshot
    }

    pub(crate) fn accepts_commit_authority(
        &self,
        authority: &ServerPlaylistCommitAuthority,
    ) -> bool {
        self.commit_binding.matches(&authority.commit_binding)
    }
}

/// Exact native identity absent from one successful complete list.
///
/// This type deliberately has no `Debug` implementation: its typed native ID
/// is exposed only to the persistence boundary that must compare and retain
/// it, never to diagnostics or commit authority.
#[cfg_attr(not(test), allow(dead_code))]
pub struct ServerPlaylistAbsenceEvidence {
    source_id: SourceId,
    native_id: NativePlaylistId,
    receipt: SessionOperationReceipt<dyn ManagedSourceAdapter>,
    commit_binding: ServerPlaylistCommitBinding,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ServerPlaylistAbsenceEvidence {
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn native_id(&self) -> &NativePlaylistId {
        &self.native_id
    }

    pub(crate) fn accepts_commit_authority(
        &self,
        authority: &ServerPlaylistCommitAuthority,
    ) -> bool {
        self.commit_binding.matches(&authority.commit_binding)
    }
}

/// Closed result for server-native playlist reads.
///
/// Raw adapter errors, response bodies, credentials, locators, and native
/// identities never cross this boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum ServerPlaylistError {
    #[error("server playlists are unsupported for this source")]
    UnsupportedSource,
    #[error("server playlist authority is unavailable")]
    Unavailable,
    #[error("server playlist backend failed")]
    BackendFailure(FailureCategory),
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

    /// Explicit opt-in for pull-only server-native playlist snapshots.
    /// Implementations remain denied unless their protocol adapter is
    /// reviewed for bounded exact native IDs and read-only request semantics.
    #[cfg_attr(not(test), allow(dead_code))]
    fn server_playlist_capability(&self) -> ServerPlaylistCapability {
        ServerPlaylistCapability::Unsupported
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn list_server_playlists(self: Arc<Self>) -> ServerPlaylistListFuture {
        Box::pin(async {
            Err(BackendError::Unsupported {
                operation: "server playlist listing".to_string(),
            })
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn get_server_playlist(
        self: Arc<Self>,
        _native_id: NativePlaylistId,
    ) -> ServerPlaylistSnapshotFuture {
        Box::pin(async {
            Err(BackendError::Unsupported {
                operation: "server playlist snapshot".to_string(),
            })
        })
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

#[cfg_attr(not(test), allow(dead_code))]
mod server_playlist_sealed {
    pub trait Adapter {}
}

#[cfg_attr(not(test), allow(dead_code))]
fn server_playlist_capability<A>() -> ServerPlaylistCapability
where
    A: server_playlist_sealed::Adapter,
{
    ServerPlaylistCapability::PullSnapshots
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
impl server_playlist_sealed::Adapter for crate::subsonic::SubsonicBackend {}

// Subsonic is deliberately spelled out rather than inheriting the standard
// remote macro: it is the only shipping adapter reviewed and opted in for
// pull-only server-native playlist snapshots.
impl LifecycleAdapter for crate::subsonic::SubsonicBackend {
    fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
        Box::pin(async { Ok(()) })
    }
}

impl ManagedSourceAdapter for crate::subsonic::SubsonicBackend {
    fn regular_playlist_capability(&self) -> RegularPlaylistCapability {
        source_scoped_playlist_capability::<Self>()
    }

    fn server_playlist_capability(&self) -> ServerPlaylistCapability {
        server_playlist_capability::<Self>()
    }

    fn list_server_playlists(self: Arc<Self>) -> ServerPlaylistListFuture {
        Box::pin(async move { Self::list_server_playlists(self.as_ref()).await })
    }

    fn get_server_playlist(
        self: Arc<Self>,
        native_id: NativePlaylistId,
    ) -> ServerPlaylistSnapshotFuture {
        Box::pin(async move { Self::get_server_playlist(self.as_ref(), &native_id).await })
    }

    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move { crate::architecture::load_track_catalog(self.as_ref()).await })
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

/// Opaque authority retaining one exact server-playlist source session
/// through a database commit.
///
/// This deliberately retains no native playlist identity and no catalogue
/// authority. Its opaque binding can only be matched against the exact pull
/// or absence evidence that minted it, so unrelated current-session authority
/// cannot admit stale or mismatched persistence input. Server playlist entries
/// may remain durable while absent from the accepted music catalogue. The
/// lifecycle permit is declared before the registry owner so final-handle
/// teardown cannot wait on a permit owned by the value being destroyed.
#[must_use = "server-playlist commit authority must be retained through the database commit"]
#[cfg_attr(not(test), allow(dead_code))]
pub struct ServerPlaylistCommitAuthority {
    #[allow(dead_code)] // Retention through Drop is the authority operation.
    authority: SessionCommitAuthority,
    commit_binding: ServerPlaylistCommitBinding,
    _registry: Arc<SourceRegistryInner>,
}

impl ServerPlaylistCommitAuthority {
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

    /// Inspect the server-playlist capability of the exact current session.
    ///
    /// This is an in-memory capability gate only: it performs no adapter or
    /// network operation and exposes neither the adapter nor its session
    /// epoch. `None` means no exact active session is currently available.
    pub(crate) fn current_server_playlist_session(
        &self,
        source_id: SourceId,
    ) -> Option<(u64, ServerPlaylistCapability)> {
        self.inner
            .lifecycle
            .inspect_active_session(source_id, ManagedSourceAdapter::server_playlist_capability)
    }

    pub(crate) fn current_server_playlist_capability(
        &self,
        source_id: SourceId,
    ) -> Option<ServerPlaylistCapability> {
        self.current_server_playlist_session(source_id)
            .map(|(_, capability)| capability)
    }

    /// List server-owned playlists through one exact active source session.
    ///
    /// Capability defaults to denied and is checked on the adapter captured
    /// with the session epoch. A replacement, disconnect, retirement, or
    /// shutdown completed while the request is in flight discards either its
    /// value or its backend error and reports unavailable authority.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn list_server_playlists(
        &self,
        source_id: SourceId,
    ) -> Result<ServerPlaylistListing, ServerPlaylistError> {
        let session_epoch = self
            .inner
            .lifecycle
            .active_session_epoch(source_id)
            .ok_or(ServerPlaylistError::Unavailable)?;
        self.list_server_playlists_for_session(source_id, session_epoch)
            .await
    }

    /// List server-owned playlists only through the exact observed session.
    ///
    /// Reconnect scheduling captures an atomic lifecycle baseline before its
    /// task runs. Requiring that baseline's epoch here prevents delayed work
    /// for a predecessor from silently adopting a successor session.
    pub(crate) async fn list_server_playlists_for_session(
        &self,
        source_id: SourceId,
        session_epoch: u64,
    ) -> Result<ServerPlaylistListing, ServerPlaylistError> {
        let (playlists, receipt) = self
            .inner
            .lifecycle
            .run_exact_session_operation(source_id, session_epoch, move |adapter| async move {
                if adapter.server_playlist_capability() != ServerPlaylistCapability::PullSnapshots {
                    return Err(BackendError::Unsupported {
                        operation: "server playlist listing".to_string(),
                    });
                }
                adapter.list_server_playlists().await
            })
            .await
            .map_err(server_playlist_error)?;

        let playlist_ids = playlists
            .iter()
            .map(|playlist| playlist.native_id().clone())
            .collect();
        Ok(ServerPlaylistListing {
            source_id,
            receipt,
            playlists,
            playlist_ids,
        })
    }

    /// Fetch one complete server-owned playlist selected from a successful
    /// complete listing.
    ///
    /// The returned ordered native IDs are not compared with the accepted
    /// catalogue: endpoint membership is import/synchronization input, while
    /// regular-playlist projection and playback retain their independent
    /// catalogue authority checks.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn get_server_playlist(
        &self,
        selection: ServerPlaylistSelection,
    ) -> Result<ServerPlaylistPull, ServerPlaylistError> {
        let source_id = selection.source_id;
        let native_id = selection.native_id;
        let expected_id = native_id.clone();
        let (snapshot, receipt) = self
            .inner
            .lifecycle
            .run_receipted_session_operation(&selection.receipt, move |adapter| async move {
                if adapter.server_playlist_capability() != ServerPlaylistCapability::PullSnapshots {
                    return Err(BackendError::Unsupported {
                        operation: "server playlist snapshot".to_string(),
                    });
                }
                let snapshot = adapter.get_server_playlist(native_id).await?;
                if snapshot.native_id() != &expected_id {
                    return Err(BackendError::ParseError {
                        message: "server playlist snapshot identity mismatch".to_string(),
                        source: None,
                    });
                }
                Ok(snapshot)
            })
            .await
            .map_err(server_playlist_error)?;
        Ok(ServerPlaylistPull {
            source_id,
            snapshot,
            receipt,
            commit_binding: ServerPlaylistCommitBinding::fresh(),
        })
    }

    /// Acquire commit-scoped authority for the exact session that returned a
    /// successful detail snapshot. The pull remains the sole carrier of its
    /// native identity; the returned authority contains only a session permit
    /// and registry owner.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn acquire_server_playlist_pull_commit_authority(
        &self,
        pull: &ServerPlaylistPull,
    ) -> Option<ServerPlaylistCommitAuthority> {
        self.acquire_server_playlist_commit_authority(&pull.receipt, &pull.commit_binding)
    }

    /// Acquire commit-scoped authority for exact absence proven by a
    /// successful complete listing. A detail/backend failure has no evidence
    /// type and therefore cannot call this boundary.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn acquire_server_playlist_absence_commit_authority(
        &self,
        evidence: &ServerPlaylistAbsenceEvidence,
    ) -> Option<ServerPlaylistCommitAuthority> {
        self.acquire_server_playlist_commit_authority(&evidence.receipt, &evidence.commit_binding)
    }

    fn acquire_server_playlist_commit_authority(
        &self,
        receipt: &SessionOperationReceipt<dyn ManagedSourceAdapter>,
        commit_binding: &ServerPlaylistCommitBinding,
    ) -> Option<ServerPlaylistCommitAuthority> {
        let authority =
            self.inner
                .lifecycle
                .acquire_session_commit_authority_if(receipt, |adapter| {
                    adapter.server_playlist_capability() == ServerPlaylistCapability::PullSnapshots
                })?;
        Some(ServerPlaylistCommitAuthority {
            authority,
            commit_binding: commit_binding.clone(),
            _registry: Arc::clone(&self.inner),
        })
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

#[cfg_attr(not(test), allow(dead_code))]
fn server_playlist_error(error: SessionOperationError) -> ServerPlaylistError {
    match error {
        SessionOperationError::Unavailable => ServerPlaylistError::Unavailable,
        SessionOperationError::Backend(BackendError::Unsupported { .. }) => {
            ServerPlaylistError::UnsupportedSource
        }
        SessionOperationError::Backend(error) => {
            ServerPlaylistError::BackendFailure(failure_category(&error))
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
    use std::sync::{mpsc, Arc, Mutex};

    use async_trait::async_trait;
    use axum::http::{Method, StatusCode};
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use tokio::runtime::Handle;
    use tokio::sync::{oneshot, watch};
    use tokio::time::{timeout, Duration};
    use tokio_util::sync::CancellationToken;
    use url::Url;
    use uuid::Uuid;

    use crate::architecture::models::{
        Album, Artist, LibraryStats, SearchResults, SortField, SortOrder,
    };
    use crate::db::migration::Migrator;
    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};
    use crate::local::playlist_manager::{
        PlaylistEntryAddOutcome, PlaylistEntryInput, PlaylistManager, ServerPlaylistCreateOutcome,
        ServerPlaylistImportOutcome, ServerPlaylistLocalState, ServerPlaylistMissingOutcome,
        ServerPlaylistPullOutcome, ServerPlaylistPullPolicy, ServerPlaylistRemoteState,
    };
    use crate::local::server_playlist_browser::{
        run_server_playlist_browser, server_playlist_browser_channel, ServerPlaylistBrowseOutcome,
        ServerPlaylistBrowserActionOutcome, ServerPlaylistBrowserRequestStatus,
        MAX_SERVER_PLAYLIST_BROWSER_ACTIONS,
    };
    use crate::local::server_playlist_runtime::{
        run_server_playlist_reconnect_observer, ServerPlaylistOperationOutcome,
        ServerPlaylistOperations,
    };

    use super::*;

    struct FakeProbe {
        close_calls: AtomicUsize,
        stream_calls: AtomicUsize,
        artwork_calls: AtomicUsize,
        server_playlist_list_calls: AtomicUsize,
        server_playlist_snapshot_calls: AtomicUsize,
        server_playlist_capability_enabled: AtomicBool,
        close_release: watch::Sender<bool>,
        stream_release: watch::Sender<bool>,
        server_playlist_release: watch::Sender<bool>,
        server_playlist_snapshot_release: watch::Sender<bool>,
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
            let (server_playlist_release, _receiver) = watch::channel(true);
            let (server_playlist_snapshot_release, _receiver) = watch::channel(true);
            Arc::new(Self {
                close_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
                artwork_calls: AtomicUsize::new(0),
                server_playlist_list_calls: AtomicUsize::new(0),
                server_playlist_snapshot_calls: AtomicUsize::new(0),
                server_playlist_capability_enabled: AtomicBool::new(true),
                close_release,
                stream_release,
                server_playlist_release,
                server_playlist_snapshot_release,
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
                server_playlist_capability: ServerPlaylistCapability::Unsupported,
                server_playlists: Vec::new(),
                server_playlist_snapshot: None,
                server_playlist_snapshots: HashMap::new(),
                server_playlist_list_failure: None,
                server_playlist_snapshot_failure: None,
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
                server_playlist_capability: ServerPlaylistCapability::Unsupported,
                server_playlists: Vec::new(),
                server_playlist_snapshot: None,
                server_playlist_snapshots: HashMap::new(),
                server_playlist_list_failure: None,
                server_playlist_snapshot_failure: None,
                stream_failure: None,
                artwork_available: true,
            }
        }

        fn server_playlist_adapter(self: &Arc<Self>, label: &'static str) -> FakeAdapter {
            let native_id = NativePlaylistId::new("native-playlist").expect("playlist ID");
            let repeated = TrackId::remote("not-in-catalogue").expect("track ID");
            let summary = ServerPlaylistSummary::new(
                native_id.clone(),
                Some("Server list".to_string()),
                Some("fixture owner".to_string()),
                Some(2),
            )
            .expect("playlist summary");
            let snapshot = ServerPlaylistSnapshot::new(
                native_id,
                Some("Server list".to_string()),
                Some("fixture owner".to_string()),
                Some(2),
                vec![repeated.clone(), repeated],
            )
            .expect("playlist snapshot");
            FakeAdapter {
                label,
                probe: Arc::clone(self),
                close_release: self.close_release.subscribe(),
                catalogue: Vec::new(),
                regular_playlist_capability: RegularPlaylistCapability::SourceScopedEntries,
                server_playlist_capability: ServerPlaylistCapability::PullSnapshots,
                server_playlists: vec![summary],
                server_playlist_snapshot: Some(snapshot),
                server_playlist_snapshots: HashMap::new(),
                server_playlist_list_failure: None,
                server_playlist_snapshot_failure: None,
                stream_failure: None,
                artwork_available: true,
            }
        }

        fn server_playlist_adapter_with_snapshots(
            self: &Arc<Self>,
            label: &'static str,
            snapshots: Vec<ServerPlaylistSnapshot>,
        ) -> FakeAdapter {
            let server_playlists = snapshots
                .iter()
                .map(|snapshot| {
                    ServerPlaylistSummary::new(
                        snapshot.native_id().clone(),
                        snapshot.name().map(str::to_string),
                        snapshot.owner().map(str::to_string),
                        snapshot.advertised_track_count(),
                    )
                    .expect("snapshot metadata remains valid as a summary")
                })
                .collect::<Vec<_>>();
            let server_playlist_snapshots = snapshots
                .into_iter()
                .map(|snapshot| (snapshot.native_id().clone(), snapshot))
                .collect::<HashMap<_, _>>();
            assert_eq!(
                server_playlist_snapshots.len(),
                server_playlists.len(),
                "fixture server playlist identities must be unique"
            );

            let mut adapter = self.server_playlist_adapter(label);
            adapter.server_playlists = server_playlists;
            adapter.server_playlist_snapshot = None;
            adapter.server_playlist_snapshots = server_playlist_snapshots;
            adapter
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
        server_playlist_capability: ServerPlaylistCapability,
        server_playlists: Vec<ServerPlaylistSummary>,
        server_playlist_snapshot: Option<ServerPlaylistSnapshot>,
        server_playlist_snapshots: HashMap<NativePlaylistId, ServerPlaylistSnapshot>,
        server_playlist_list_failure: Option<String>,
        server_playlist_snapshot_failure: Option<String>,
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

        fn server_playlist_capability(&self) -> ServerPlaylistCapability {
            if self.server_playlist_capability == ServerPlaylistCapability::PullSnapshots
                && !self
                    .probe
                    .server_playlist_capability_enabled
                    .load(Ordering::Acquire)
            {
                ServerPlaylistCapability::Unsupported
            } else {
                self.server_playlist_capability
            }
        }

        fn list_server_playlists(self: Arc<Self>) -> ServerPlaylistListFuture {
            self.probe
                .server_playlist_list_calls
                .fetch_add(1, Ordering::AcqRel);
            let mut release = self.probe.server_playlist_release.subscribe();
            let playlists = self.server_playlists.clone();
            let failure = self.server_playlist_list_failure.clone();
            Box::pin(async move {
                while !*release.borrow_and_update() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
                if let Some(message) = failure {
                    return Err(BackendError::ConnectionFailed {
                        message,
                        source: None,
                    });
                }
                Ok(playlists)
            })
        }

        fn get_server_playlist(
            self: Arc<Self>,
            native_id: NativePlaylistId,
        ) -> ServerPlaylistSnapshotFuture {
            self.probe
                .server_playlist_snapshot_calls
                .fetch_add(1, Ordering::AcqRel);
            let mut release = self.probe.server_playlist_release.subscribe();
            let mut snapshot_release = self.probe.server_playlist_snapshot_release.subscribe();
            let snapshot = self
                .server_playlist_snapshots
                .get(&native_id)
                .cloned()
                .or_else(|| self.server_playlist_snapshot.clone());
            let failure = self.server_playlist_snapshot_failure.clone();
            Box::pin(async move {
                while !*release.borrow_and_update() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
                while !*snapshot_release.borrow_and_update() {
                    if snapshot_release.changed().await.is_err() {
                        break;
                    }
                }
                if let Some(message) = failure {
                    return Err(BackendError::ConnectionFailed {
                        message,
                        source: None,
                    });
                }
                snapshot.ok_or_else(|| BackendError::ParseError {
                    message: "fixture server playlist snapshot missing".to_string(),
                    source: None,
                })
            })
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
            .create_regular_playlist(name)
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

        fn assert_server_playlist_pull<A>()
        where
            A: server_playlist_sealed::Adapter,
        {
            assert_eq!(
                server_playlist_capability::<A>(),
                ServerPlaylistCapability::PullSnapshots
            );
        }

        // Only Subsonic implements the sealed server-playlist marker. The
        // trait default keeps Jellyfin, Plex, DAAP, radio, removable, and
        // external adapters denied.
        assert_server_playlist_pull::<crate::subsonic::SubsonicBackend>();
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

    fn browser_playlist_snapshot(
        native_id: &str,
        name: &str,
        track_id: &str,
    ) -> ServerPlaylistSnapshot {
        ServerPlaylistSnapshot::new(
            NativePlaylistId::new(native_id).expect("browser native playlist ID"),
            Some(name.to_string()),
            Some("browser fixture owner".to_string()),
            Some(1),
            vec![TrackId::remote(track_id).expect("browser track ID")],
        )
        .expect("browser playlist snapshot")
    }

    async fn wait_for_server_playlist_snapshot_calls(probe: &FakeProbe, expected: usize) {
        timeout(Duration::from_secs(2), async {
            while probe.server_playlist_snapshot_calls.load(Ordering::Acquire) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected server-playlist detail calls");
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

    #[tokio::test]
    async fn server_playlist_reads_default_deny_and_do_not_grant_catalogue_authority() {
        let registry = registry();

        let unsupported_probe = FakeProbe::new(true);
        let unsupported_source = SourceId::random();
        connect_playlist_fixture(
            &registry,
            unsupported_source,
            unsupported_probe.adapter("unsupported"),
        )
        .await;
        assert!(matches!(
            registry.list_server_playlists(unsupported_source).await,
            Err(ServerPlaylistError::UnsupportedSource)
        ));
        assert_eq!(
            unsupported_probe
                .server_playlist_list_calls
                .load(Ordering::Acquire),
            0,
            "default-denied adapters must not receive a playlist request"
        );

        let supported_probe = FakeProbe::new(true);
        let supported_source = SourceId::random();
        connect_playlist_fixture(
            &registry,
            supported_source,
            supported_probe.server_playlist_adapter("server-playlists"),
        )
        .await;

        let listing = registry
            .list_server_playlists(supported_source)
            .await
            .expect("server playlist listing");
        assert_eq!(listing.source_id(), supported_source);
        assert_eq!(listing.playlists().len(), 1);
        let native_id = listing.playlists()[0].native_id().clone();
        assert!(listing.prove_absent(&native_id).is_none());
        let case_distinct = NativePlaylistId::new("Native-playlist").expect("distinct ID");
        let absence = listing
            .prove_absent(&case_distinct)
            .expect("exact identity comparison preserves case");
        assert_eq!(absence.source_id(), supported_source);
        assert_eq!(absence.native_id(), &case_distinct);
        assert!(listing.select(&case_distinct).is_none());
        let selection = listing
            .select(&native_id)
            .expect("listed native identity is selectable");
        let pull = registry
            .get_server_playlist(selection)
            .await
            .expect("server playlist snapshot");
        let snapshot = pull.snapshot();
        assert_eq!(pull.source_id(), supported_source);
        assert_eq!(pull.native_id(), &native_id);
        assert_eq!(snapshot.track_ids().len(), 2);
        assert_eq!(snapshot.track_ids()[0], snapshot.track_ids()[1]);

        let endpoint_identity = MediaKey::new(supported_source, snapshot.track_ids()[0].clone());
        assert_eq!(
            unavailable_reason(&registry.resolve_regular_playlist_tracks(&[endpoint_identity])[0]),
            RegularPlaylistUnavailableReason::TrackMissing,
            "native playlist membership must not become catalogue/playback authority"
        );
        assert_eq!(
            supported_probe
                .server_playlist_list_calls
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            supported_probe
                .server_playlist_snapshot_calls
                .load(Ordering::Acquire),
            1
        );

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn server_playlist_receipts_close_in_flight_and_successor_session_races() {
        let registry = registry();
        let predecessor_probe = FakeProbe::new(true);
        let source_id = SourceId::random();
        connect_playlist_fixture(
            &registry,
            source_id,
            predecessor_probe.server_playlist_adapter("predecessor"),
        )
        .await;
        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("predecessor listing");
        let predecessor_epoch = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.session_epoch)
            .expect("predecessor epoch");
        let native_id = listing.playlists()[0].native_id().clone();
        let delayed_selection = listing.select(&native_id).expect("predecessor selection");
        let stale_selection = listing
            .select(&native_id)
            .expect("second predecessor selection");

        predecessor_probe
            .server_playlist_release
            .send_replace(false);
        let delayed_registry = registry.clone();
        let delayed = tokio::spawn(async move {
            delayed_registry
                .get_server_playlist(delayed_selection)
                .await
        });
        timeout(Duration::from_secs(2), async {
            while predecessor_probe
                .server_playlist_snapshot_calls
                .load(Ordering::Acquire)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("snapshot request started");

        let disconnect = registry.disconnect(source_id).expect("disconnect source");
        predecessor_probe.server_playlist_release.send_replace(true);
        assert!(matches!(
            delayed.await.expect("snapshot task"),
            Err(ServerPlaylistError::Unavailable)
        ));
        disconnect.wait().await;

        let successor_probe = FakeProbe::new(true);
        let successor_adapter = successor_probe.server_playlist_adapter("successor");
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(successor_adapter) },
            )
            .expect("successor connection admitted");
        let _ = wait_for_catalogue(&registry, source_id).await;

        assert!(matches!(
            registry
                .list_server_playlists_for_session(source_id, predecessor_epoch)
                .await,
            Err(ServerPlaylistError::Unavailable)
        ));
        assert_eq!(
            successor_probe
                .server_playlist_list_calls
                .load(Ordering::Acquire),
            0,
            "a delayed predecessor reconnect must not list through its successor"
        );

        assert!(matches!(
            registry.get_server_playlist(stale_selection).await,
            Err(ServerPlaylistError::Unavailable)
        ));
        assert_eq!(
            successor_probe
                .server_playlist_snapshot_calls
                .load(Ordering::Acquire),
            0,
            "an old guard must be rejected before successor adapter work"
        );

        successor_probe.server_playlist_release.send_replace(false);
        let delayed_registry = registry.clone();
        let delayed_listing =
            tokio::spawn(async move { delayed_registry.list_server_playlists(source_id).await });
        timeout(Duration::from_secs(2), async {
            while successor_probe
                .server_playlist_list_calls
                .load(Ordering::Acquire)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("listing request started");
        let successor_disconnect = registry
            .disconnect(source_id)
            .expect("disconnect successor");
        successor_probe.server_playlist_release.send_replace(true);
        assert!(matches!(
            delayed_listing.await.expect("listing task"),
            Err(ServerPlaylistError::Unavailable)
        ));
        successor_disconnect.wait().await;

        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_pull_commit_authority_is_session_only_and_blocks_disconnect() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let source_id = SourceId::random();
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.server_playlist_adapter("commit-session"),
        )
        .await;

        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("complete listing");
        let native_id = listing.playlists()[0].native_id().clone();
        let selection = listing.select(&native_id).expect("present selection");
        let pull = registry
            .get_server_playlist(selection)
            .await
            .expect("detail pull");
        let authority = registry
            .acquire_server_playlist_pull_commit_authority(&pull)
            .expect("current pull admitted at commit boundary");

        // Server-native persistence is independent from the music catalogue.
        // Replacing the same-session catalogue must neither reject nor wait on
        // this session-only permit.
        refresh_playlist_catalogue(
            &registry,
            source_id,
            Vec::new(),
            RegularPlaylistCapability::Unsupported,
        )
        .await;

        let disconnecting = registry.clone();
        let (waiter_tx, waiter_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let waiter = disconnecting
                .disconnect(source_id)
                .expect("disconnect admitted");
            waiter_tx
                .send(waiter)
                .expect("disconnect waiter receiver alive");
        });
        timeout(Duration::from_secs(2), async {
            while !authority.revocation_started() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect reached session revocation");
        assert!(matches!(
            waiter_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        drop(authority);
        let waiter = waiter_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("disconnect resumes after commit authority release");
        worker.join().expect("disconnect worker");
        waiter.wait().await;
        assert!(registry
            .acquire_server_playlist_pull_commit_authority(&pull)
            .is_none());

        let successor_probe = FakeProbe::new(true);
        let successor = successor_probe.server_playlist_adapter("commit-successor");
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(successor) },
            )
            .expect("successor connection admitted");
        let _ = wait_for_catalogue(&registry, source_id).await;
        assert!(
            registry
                .acquire_server_playlist_pull_commit_authority(&pull)
                .is_none(),
            "a predecessor pull cannot gain authority from its successor session"
        );

        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_commit_authority_orders_replacement_publication() {
        let registry = registry();
        let predecessor_probe = FakeProbe::new(true);
        let source_id = SourceId::random();
        connect_playlist_fixture(
            &registry,
            source_id,
            predecessor_probe.server_playlist_adapter("replacement-predecessor"),
        )
        .await;

        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("predecessor listing");
        let native_id = listing.playlists()[0].native_id().clone();
        let pull = registry
            .get_server_playlist(listing.select(&native_id).expect("present selection"))
            .await
            .expect("predecessor pull");
        let authority = registry
            .acquire_server_playlist_pull_commit_authority(&pull)
            .expect("predecessor commit admitted");

        let mut invalidations = registry.subscribe_invalidations();
        let _ = invalidations.borrow_and_update();
        let successor_probe = FakeProbe::new(true);
        let successor = successor_probe.server_playlist_adapter("replacement-successor");
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(successor) },
            )
            .expect("replacement admitted");
        timeout(Duration::from_secs(2), async {
            while !authority.revocation_started() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement reached predecessor revocation");

        // Clear ConnectStarted/Connecting changes that necessarily precede
        // construction. SessionAdopted and successor catalogue publication
        // are ordered after the retained permit.
        let _ = invalidations.borrow_and_update();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert!(
            !invalidations
                .has_changed()
                .expect("invalidation sender alive"),
            "successor publication cannot pass retained commit authority"
        );

        drop(authority);
        timeout(Duration::from_secs(2), invalidations.changed())
            .await
            .expect("successor publishes after authority release")
            .expect("invalidation sender alive");
        let _ = wait_for_catalogue(&registry, source_id).await;
        assert!(
            registry
                .acquire_server_playlist_pull_commit_authority(&pull)
                .is_none(),
            "the predecessor pull is stale once replacement publishes"
        );

        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_import_rolls_back_stale_and_orders_an_admitted_commit() {
        let registry = registry();
        let predecessor_probe = FakeProbe::new(true);
        let source_id = SourceId::random();
        connect_playlist_fixture(
            &registry,
            source_id,
            predecessor_probe.server_playlist_adapter("import-predecessor"),
        )
        .await;
        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("predecessor listing");
        let native_id = listing.playlists()[0].native_id().clone();
        let stale_pull = registry
            .get_server_playlist(listing.select(&native_id).expect("present selection"))
            .await
            .expect("predecessor pull");

        let (manager, _baseline_id) = playlist_manager_fixture("Import baseline").await;
        let baseline_count = manager
            .list_playlists()
            .await
            .expect("list baseline playlists")
            .len();
        let stale_waiter = Arc::new(Mutex::new(None));
        let stale_waiter_slot = Arc::clone(&stale_waiter);
        let rejected = manager
            .import_server_playlist_copy_if_authorized(&stale_pull, "Fallback", || {
                let waiter = registry
                    .disconnect(source_id)
                    .expect("disconnect before final admission");
                *lock(&stale_waiter_slot) = Some(waiter);
                registry.acquire_server_playlist_pull_commit_authority(&stale_pull)
            })
            .await
            .expect("stale import is a typed rejection");
        assert!(matches!(rejected, ServerPlaylistImportOutcome::Rejected));
        assert_eq!(
            manager
                .list_playlists()
                .await
                .expect("list rolled-back playlists")
                .len(),
            baseline_count,
            "stale authority rolls the staged playlist and every entry back"
        );
        let waiter = lock(&stale_waiter).take().expect("stale disconnect waiter");
        waiter.wait().await;

        let successor_probe = FakeProbe::new(true);
        let successor = successor_probe.server_playlist_adapter("import-successor");
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(successor) },
            )
            .expect("successor connection admitted");
        let _ = wait_for_catalogue(&registry, source_id).await;
        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("successor listing");
        let current_id = listing.playlists()[0].native_id().clone();
        let current_pull = registry
            .get_server_playlist(listing.select(&current_id).expect("successor selection"))
            .await
            .expect("successor pull");

        let (waiter_tx, waiter_rx) = mpsc::sync_channel(1);
        let committed = manager
            .import_server_playlist_copy_if_authorized(&current_pull, "Fallback", || {
                let authority = registry
                    .acquire_server_playlist_pull_commit_authority(&current_pull)
                    .expect("current pull admitted");
                let disconnecting = registry.clone();
                let _worker = std::thread::spawn(move || {
                    let waiter = disconnecting
                        .disconnect(source_id)
                        .expect("disconnect after admission");
                    waiter_tx
                        .send(waiter)
                        .expect("disconnect waiter receiver alive");
                });
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !authority.revocation_started() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "disconnect reached retained session authority"
                    );
                    std::thread::yield_now();
                }
                Some(authority)
            })
            .await
            .expect("admitted import commits");
        let ServerPlaylistImportOutcome::Committed(copy) = committed else {
            panic!("current import must commit");
        };
        let waiter = waiter_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("disconnect resumes after database commit");
        waiter.wait().await;
        assert_eq!(copy.entry_count(), 2);
        assert_eq!(
            manager
                .get_playlist_entries(copy.playlist_id())
                .await
                .expect("load detached imported entries")
                .len(),
            2
        );
        assert!(manager
            .get_server_playlist_link(copy.playlist_id())
            .await
            .expect("load detached link state")
            .is_none());
        assert_eq!(
            manager
                .list_playlists()
                .await
                .expect("list committed playlists")
                .len(),
            baseline_count + 1
        );

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn server_playlist_persistence_requires_authority_from_the_exact_read_result() {
        let registry = registry();
        let source_id = SourceId::random();
        let probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.server_playlist_adapter("binding-primary"),
        )
        .await;

        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("list primary server playlists");
        let native_id = listing.playlists()[0].native_id().clone();
        let first_pull = registry
            .get_server_playlist(
                listing
                    .select(&native_id)
                    .expect("select first exact playlist"),
            )
            .await
            .expect("fetch first exact pull");
        let unrelated_pull = registry
            .get_server_playlist(
                listing
                    .select(&native_id)
                    .expect("select same playlist a second time"),
            )
            .await
            .expect("fetch unrelated same-session pull");

        let other_source_id = SourceId::random();
        let other_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            other_source_id,
            other_probe.server_playlist_adapter("binding-other-source"),
        )
        .await;
        let other_listing = registry
            .list_server_playlists(other_source_id)
            .await
            .expect("list other source playlists");
        let other_native_id = other_listing.playlists()[0].native_id().clone();
        let other_source_pull = registry
            .get_server_playlist(
                other_listing
                    .select(&other_native_id)
                    .expect("select other source playlist"),
            )
            .await
            .expect("fetch other source pull");

        let (manager, _baseline_id) = playlist_manager_fixture("Binding baseline").await;
        let baseline_count = manager
            .list_playlists()
            .await
            .expect("list baseline playlists")
            .len();

        assert!(matches!(
            manager
                .import_server_playlist_copy_if_authorized(&first_pull, "Fallback", || {
                    registry.acquire_server_playlist_pull_commit_authority(&unrelated_pull)
                })
                .await
                .expect("mismatched import authority is a typed rejection"),
            ServerPlaylistImportOutcome::Rejected
        ));
        assert!(matches!(
            manager
                .create_server_playlist_mirror_if_authorized(&first_pull, "Fallback", || {
                    registry.acquire_server_playlist_pull_commit_authority(&unrelated_pull)
                })
                .await
                .expect("mismatched mirror authority is a typed rejection"),
            ServerPlaylistCreateOutcome::Rejected
        ));
        assert!(matches!(
            manager
                .import_server_playlist_copy_if_authorized(&first_pull, "Fallback", || {
                    registry.acquire_server_playlist_pull_commit_authority(&other_source_pull)
                })
                .await
                .expect("other-source authority is a typed rejection"),
            ServerPlaylistImportOutcome::Rejected
        ));
        assert_eq!(
            manager
                .list_playlists()
                .await
                .expect("list after rejected creates")
                .len(),
            baseline_count,
            "every mismatched create must roll its staged rows back"
        );

        let created = manager
            .create_server_playlist_mirror_if_authorized(&first_pull, "Fallback", || {
                registry.acquire_server_playlist_pull_commit_authority(&first_pull)
            })
            .await
            .expect("exactly bound mirror authority");
        let ServerPlaylistCreateOutcome::Committed { copy, link } = created else {
            panic!("authority from the exact pull must commit");
        };
        let update_ticket = manager
            .prepare_server_playlist_sync(copy.playlist_id())
            .await
            .expect("prepare bound pull")
            .expect("mirror remains linked")
            .into_parts()
            .1;
        assert!(matches!(
            manager
                .apply_server_playlist_pull_if_authorized(
                    update_ticket,
                    &first_pull,
                    ServerPlaylistPullPolicy::ReplaceLocal,
                    || registry.acquire_server_playlist_pull_commit_authority(&unrelated_pull),
                )
                .await
                .expect("mismatched sync authority is a typed rejection"),
            ServerPlaylistPullOutcome::Rejected
        ));
        assert_eq!(
            manager
                .get_server_playlist_link(copy.playlist_id())
                .await
                .expect("load link after rejected sync"),
            Some(link.clone()),
            "mismatched sync authority must roll the staged revision back"
        );

        registry
            .disconnect(source_id)
            .expect("disconnect primary source")
            .wait()
            .await;
        let successor_probe = FakeProbe::new(true);
        let successor = successor_probe.server_playlist_adapter("binding-successor");
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(successor) },
            )
            .expect("connect primary successor");
        let _ = wait_for_catalogue(&registry, source_id).await;
        let successor_listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("list successor playlists");
        let successor_pull = registry
            .get_server_playlist(
                successor_listing
                    .select(&native_id)
                    .expect("select successor playlist"),
            )
            .await
            .expect("fetch successor pull");
        assert!(matches!(
            manager
                .import_server_playlist_copy_if_authorized(&first_pull, "Fallback", || {
                    registry.acquire_server_playlist_pull_commit_authority(&successor_pull)
                })
                .await
                .expect("stale pull paired with current authority is rejected"),
            ServerPlaylistImportOutcome::Rejected
        ));
        assert_eq!(
            manager
                .list_playlists()
                .await
                .expect("list after rejected stale import")
                .len(),
            baseline_count + 1
        );

        registry
            .disconnect(source_id)
            .expect("disconnect successor")
            .wait()
            .await;
        let absent_probe = FakeProbe::new(true);
        let mut absent_adapter = absent_probe.server_playlist_adapter("binding-absence");
        absent_adapter.server_playlists.clear();
        absent_adapter.server_playlist_snapshot = None;
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(absent_adapter) },
            )
            .expect("connect absence session");
        let _ = wait_for_catalogue(&registry, source_id).await;
        let empty_listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("complete empty listing");
        let exact_absence = empty_listing
            .prove_absent(&native_id)
            .expect("prove exact linked identity absent");
        let unrelated_absent_id =
            NativePlaylistId::new("unrelated-absent-playlist").expect("unrelated native ID");
        let unrelated_absence = empty_listing
            .prove_absent(&unrelated_absent_id)
            .expect("prove unrelated identity absent");
        let absence_ticket = manager
            .prepare_server_playlist_sync(copy.playlist_id())
            .await
            .expect("prepare absence")
            .expect("mirror remains linked")
            .into_parts()
            .1;
        assert!(matches!(
            manager
                .mark_server_playlist_missing_if_authorized(absence_ticket, &exact_absence, || {
                    registry.acquire_server_playlist_absence_commit_authority(&unrelated_absence)
                },)
                .await
                .expect("mismatched absence authority is a typed rejection"),
            ServerPlaylistMissingOutcome::Rejected
        ));
        assert_eq!(
            manager
                .get_server_playlist_link(copy.playlist_id())
                .await
                .expect("load link after rejected absence"),
            Some(link),
            "mismatched absence authority must roll the state change back"
        );

        let exact_ticket = manager
            .prepare_server_playlist_sync(copy.playlist_id())
            .await
            .expect("prepare exact absence")
            .expect("mirror remains linked")
            .into_parts()
            .1;
        assert!(matches!(
            manager
                .mark_server_playlist_missing_if_authorized(exact_ticket, &exact_absence, || {
                    registry.acquire_server_playlist_absence_commit_authority(&exact_absence)
                },)
                .await
                .expect("exactly bound absence authority"),
            ServerPlaylistMissingOutcome::Marked(_)
        ));

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn server_playlist_commit_revalidates_capability_and_rolls_back_if_withdrawn() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let source_id = SourceId::random();
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.server_playlist_adapter("capability-withdrawal"),
        )
        .await;
        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("listing while pull capability is advertised");
        let native_id = listing.playlists()[0].native_id().clone();
        let pull = registry
            .get_server_playlist(
                listing
                    .select(&native_id)
                    .expect("select exact advertised playlist"),
            )
            .await
            .expect("detail while pull capability is advertised");
        assert_eq!(
            probe.server_playlist_snapshot_calls.load(Ordering::Acquire),
            1
        );

        let (manager, _baseline_id) = playlist_manager_fixture("Capability baseline").await;
        let baseline_count = manager
            .list_playlists()
            .await
            .expect("list baseline playlists")
            .len();
        let rejected = manager
            .create_server_playlist_mirror_if_authorized(&pull, "Fallback", || {
                // The manager invokes final admission only after staging the
                // playlist, entries, and link in its transaction.
                probe
                    .server_playlist_capability_enabled
                    .store(false, Ordering::Release);
                let authority = registry.acquire_server_playlist_pull_commit_authority(&pull);
                assert!(
                    authority.is_none(),
                    "final admission must revalidate the exact adapter capability"
                );
                authority
            })
            .await
            .expect("capability withdrawal is a typed rejection");
        assert!(matches!(rejected, ServerPlaylistCreateOutcome::Rejected));
        assert_eq!(
            manager
                .list_playlists()
                .await
                .expect("list playlists after rejected commit")
                .len(),
            baseline_count,
            "rejected authority rolls back the staged mirror and entries"
        );
        assert!(manager
            .list_server_playlist_links(source_id)
            .await
            .expect("list links after rejected commit")
            .is_empty());

        // Nothing else about the receipt or lifecycle changed: restoring the
        // fake's capability makes the same exact-session receipt admissible.
        probe
            .server_playlist_capability_enabled
            .store(true, Ordering::Release);
        assert!(registry
            .acquire_server_playlist_pull_commit_authority(&pull)
            .is_some());

        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_public_pull_sync_and_absence_flow_is_end_to_end_authorized() {
        let registry = registry();
        let source_id = SourceId::random();
        let initial_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            initial_probe.server_playlist_adapter("sync-initial"),
        )
        .await;

        let initial_listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("list initial server playlists");
        let native_id = initial_listing.playlists()[0].native_id().clone();
        let initial_pull = registry
            .get_server_playlist(
                initial_listing
                    .select(&native_id)
                    .expect("select exact initial playlist"),
            )
            .await
            .expect("fetch initial server playlist");

        let (manager, _baseline_id) = playlist_manager_fixture("Sync baseline").await;
        let created = manager
            .create_server_playlist_mirror_if_authorized(&initial_pull, "Fallback", || {
                registry.acquire_server_playlist_pull_commit_authority(&initial_pull)
            })
            .await
            .expect("create authorized pull-only mirror");
        let ServerPlaylistCreateOutcome::Committed { copy, link } = created else {
            panic!("current initial pull must create a mirror");
        };
        assert_eq!(copy.name(), "Server list");
        assert_eq!(copy.entry_count(), 2);
        assert_eq!(link.source_id, source_id);
        assert_eq!(link.native_playlist_id, native_id);
        assert_eq!(link.local_state, ServerPlaylistLocalState::Clean);
        assert_eq!(link.remote_state, ServerPlaylistRemoteState::Present);
        assert_eq!(link.state_revision, 0);
        let initial_entries = manager
            .get_playlist_entries(copy.playlist_id())
            .await
            .expect("load initial mirror entries");
        assert_eq!(initial_entries.len(), 2);
        assert_eq!(initial_entries[0].position, 0);
        assert_eq!(initial_entries[1].position, 1);
        assert_eq!(initial_entries[0].source_id, source_id);
        assert_eq!(initial_entries[1].source_id, source_id);
        assert_eq!(initial_entries[0].track_id, initial_entries[1].track_id);

        // Capture the persisted revision before starting the next network
        // operation. The completion must consume this exact ticket rather
        // than loading a newer link revision after the request finishes.
        let update_preparation = manager
            .prepare_server_playlist_sync(copy.playlist_id())
            .await
            .expect("prepare update pull")
            .expect("mirror remains linked");
        assert_eq!(update_preparation.ticket().state_revision(), 0);
        registry
            .disconnect(source_id)
            .expect("disconnect initial server session")
            .wait()
            .await;

        let updated_probe = FakeProbe::new(true);
        let mut updated_adapter = updated_probe.server_playlist_adapter("sync-updated");
        let first = TrackId::remote("updated-first").expect("first updated track ID");
        let second = TrackId::remote("updated-second").expect("second updated track ID");
        updated_adapter.server_playlists = vec![ServerPlaylistSummary::new(
            native_id.clone(),
            Some("Updated server list".to_string()),
            Some("fixture owner".to_string()),
            Some(3),
        )
        .expect("updated playlist summary")];
        updated_adapter.server_playlist_snapshot = Some(
            ServerPlaylistSnapshot::new(
                native_id.clone(),
                Some("Updated server list".to_string()),
                Some("fixture owner".to_string()),
                Some(3),
                vec![first.clone(), second.clone(), first.clone()],
            )
            .expect("updated playlist snapshot"),
        );
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(updated_adapter) },
            )
            .expect("updated server session admitted");
        let _ = wait_for_catalogue(&registry, source_id).await;
        let updated_listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("list updated server playlists");
        let updated_pull = registry
            .get_server_playlist(
                updated_listing
                    .select(&native_id)
                    .expect("select exact updated playlist"),
            )
            .await
            .expect("fetch updated server playlist");

        let (waiter_tx, waiter_rx) = mpsc::sync_channel(1);
        let applied = manager
            .apply_server_playlist_pull_if_authorized(
                update_preparation.into_parts().1,
                &updated_pull,
                ServerPlaylistPullPolicy::ReplaceLocal,
                || {
                    let authority = registry
                        .acquire_server_playlist_pull_commit_authority(&updated_pull)
                        .expect("updated pull remains current at final admission");
                    let disconnecting = registry.clone();
                    let _worker = std::thread::spawn(move || {
                        let waiter = disconnecting
                            .disconnect(source_id)
                            .expect("disconnect updated server session");
                        waiter_tx
                            .send(waiter)
                            .expect("disconnect waiter receiver alive");
                    });
                    let deadline = std::time::Instant::now() + Duration::from_secs(2);
                    while !authority.revocation_started() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "disconnect reaches retained commit authority"
                        );
                        std::thread::yield_now();
                    }
                    Some(authority)
                },
            )
            .await
            .expect("apply authorized updated pull");
        let ServerPlaylistPullOutcome::Applied {
            copy: updated_copy,
            link: updated_link,
        } = applied
        else {
            panic!("updated public pull must apply");
        };
        let waiter = waiter_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("disconnect resumes after pull commit");
        waiter.wait().await;
        assert_eq!(updated_copy.playlist_id(), copy.playlist_id());
        assert_eq!(updated_copy.name(), "Updated server list");
        assert_eq!(updated_copy.entry_count(), 3);
        assert_eq!(updated_link.local_state, ServerPlaylistLocalState::Clean);
        assert_eq!(
            updated_link.remote_state,
            ServerPlaylistRemoteState::Present
        );
        assert_eq!(updated_link.state_revision, 1);
        let updated_entries = manager
            .get_playlist_entries(copy.playlist_id())
            .await
            .expect("load updated mirror entries");
        assert_eq!(
            updated_entries
                .iter()
                .map(|entry| (entry.position, entry.source_id, entry.track_id.clone()))
                .collect::<Vec<_>>(),
            vec![
                (0, source_id, Some(first.clone())),
                (1, source_id, Some(second)),
                (2, source_id, Some(first)),
            ]
        );

        let absence_preparation = manager
            .prepare_server_playlist_sync(copy.playlist_id())
            .await
            .expect("prepare absence listing")
            .expect("updated mirror remains linked");
        assert_eq!(absence_preparation.ticket().state_revision(), 1);
        let absent_probe = FakeProbe::new(true);
        let mut absent_adapter = absent_probe.server_playlist_adapter("sync-absent");
        absent_adapter.server_playlists.clear();
        absent_adapter.server_playlist_snapshot = None;
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(absent_adapter) },
            )
            .expect("absence server session admitted");
        let _ = wait_for_catalogue(&registry, source_id).await;
        let empty_listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("complete empty server listing");
        assert!(empty_listing.playlists().is_empty());
        let absence = empty_listing
            .prove_absent(&native_id)
            .expect("seal exact complete-list absence evidence");
        let missing = manager
            .mark_server_playlist_missing_if_authorized(
                absence_preparation.into_parts().1,
                &absence,
                || registry.acquire_server_playlist_absence_commit_authority(&absence),
            )
            .await
            .expect("persist authorized absence");
        let ServerPlaylistMissingOutcome::Marked(missing_link) = missing else {
            panic!("complete current absence must mark the mirror missing");
        };
        assert_eq!(missing_link.playlist_id, copy.playlist_id());
        assert_eq!(missing_link.source_id, source_id);
        assert_eq!(missing_link.native_playlist_id, native_id);
        assert_eq!(missing_link.local_state, ServerPlaylistLocalState::Clean);
        assert_eq!(
            missing_link.remote_state,
            ServerPlaylistRemoteState::Missing
        );
        assert_eq!(missing_link.state_revision, 2);
        assert_eq!(
            manager
                .get_playlist_entries(copy.playlist_id())
                .await
                .expect("absence preserves exact local mirror entries"),
            updated_entries
        );

        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reconnect_runtime_skips_server_listing_when_no_mirrors_are_linked() {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect empty-mirror database");
        Migrator::up(&database, None)
            .await
            .expect("migrate empty-mirror database");
        let registry = registry();
        let source_id = SourceId::random();
        let probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.server_playlist_adapter("empty-mirror-runtime"),
        )
        .await;
        let session_epoch = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.session_epoch)
            .expect("connected empty-mirror session epoch");
        assert_eq!(probe.server_playlist_list_calls.load(Ordering::Acquire), 0);

        let (refresh, _refresh_rx) =
            crate::local::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let operations = ServerPlaylistOperations::new(
            database,
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
        );
        let stamp = coordinator
            .reserve_request_stamp()
            .expect("reserve empty-mirror reconnect stamp");
        let fanout_stamp = stamp.clone();
        let (completed_tx, completed_rx) = oneshot::channel();
        assert_eq!(
            coordinator.begin_if_not_newer(
                crate::server_playlist_coordinator::ServerPlaylistOperationKey::source(source_id),
                &stamp,
                move |context| async move {
                    operations
                        .run_reconnect_sweep_for_test(
                            source_id,
                            session_epoch,
                            fanout_stamp,
                            context,
                        )
                        .await;
                    completed_tx
                        .send(())
                        .expect("report empty sweep completion");
                },
            ),
            crate::server_playlist_coordinator::ServerPlaylistRequestStatus::Queued
        );
        completed_rx.await.expect("empty reconnect sweep finishes");
        assert_eq!(
            probe.server_playlist_list_calls.load(Ordering::Acquire),
            0,
            "a source with no linked mirrors must not issue a server-playlist listing"
        );

        coordinator.close();
        coordinator_shutdown
            .shutdown()
            .await
            .expect("empty-mirror coordinator drains");
        refresh.close();
        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reconnect_runtime_applies_exact_presence_reports_failures_and_marks_only_proven_absence(
    ) {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect runtime database");
        Migrator::up(&database, None)
            .await
            .expect("migrate runtime database");
        let manager = PlaylistManager::new(database.clone());
        let registry = registry();
        let source_id = SourceId::random();

        let initial_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            initial_probe.server_playlist_adapter("runtime-initial"),
        )
        .await;
        let initial_listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("list initial runtime playlist");
        let native_id = initial_listing.playlists()[0].native_id().clone();
        let initial_pull = registry
            .get_server_playlist(
                initial_listing
                    .select(&native_id)
                    .expect("select initial runtime playlist"),
            )
            .await
            .expect("fetch initial runtime playlist");
        let created = manager
            .create_server_playlist_mirror_if_authorized(&initial_pull, "Fallback", || {
                registry.acquire_server_playlist_pull_commit_authority(&initial_pull)
            })
            .await
            .expect("create runtime mirror");
        let ServerPlaylistCreateOutcome::Committed { copy, .. } = created else {
            panic!("initial runtime pull creates one mirror");
        };
        let playlist_id = copy.playlist_id().to_string();
        registry
            .disconnect(source_id)
            .expect("disconnect initial runtime session")
            .wait()
            .await;

        let invalidations = registry.subscribe_invalidations();
        let (refresh, refresh_rx) =
            crate::local::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (snapshot_tx, snapshot_rx) = async_channel::bounded(4);
        let publisher = tokio::spawn(
            crate::local::playlist_sidebar::run_playlist_sidebar_publisher(
                database.clone(),
                refresh_rx,
                snapshot_tx,
            ),
        );
        let initial_sidebar = snapshot_rx
            .recv()
            .await
            .expect("initial runtime sidebar snapshot");
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let operations = ServerPlaylistOperations::new(
            database.clone(),
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
        );
        let observer_shutdown = tokio_util::sync::CancellationToken::new();
        let observer = tokio::spawn(run_server_playlist_reconnect_observer(
            operations.clone(),
            invalidations,
            observer_shutdown.clone(),
        ));

        let updated_probe = FakeProbe::new(true);
        let mut updated = updated_probe.server_playlist_adapter("runtime-updated");
        let first = TrackId::remote("runtime-updated-first").expect("updated first track");
        let second = TrackId::remote("runtime-updated-second").expect("updated second track");
        updated.server_playlists = vec![ServerPlaylistSummary::new(
            native_id.clone(),
            Some("Runtime updated".to_string()),
            None,
            Some(3),
        )
        .expect("updated runtime summary")];
        updated.server_playlist_snapshot = Some(
            ServerPlaylistSnapshot::new(
                native_id.clone(),
                Some("Runtime updated".to_string()),
                None,
                Some(3),
                vec![first.clone(), second.clone(), first.clone()],
            )
            .expect("updated runtime snapshot"),
        );
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(updated) },
            )
            .expect("connect updated runtime session");
        let _ = wait_for_catalogue(&registry, source_id).await;

        timeout(Duration::from_secs(2), async {
            loop {
                let link = manager
                    .get_server_playlist_link(&playlist_id)
                    .await
                    .expect("load automatically updated link")
                    .expect("runtime mirror remains linked");
                if link.last_synced_name == "Runtime updated" && link.state_revision == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconnect sweep applies exact present snapshot");
        let updated_sidebar = snapshot_rx
            .recv()
            .await
            .expect("runtime update publishes sidebar snapshot");
        assert!(updated_sidebar.revision() > initial_sidebar.revision());
        assert_eq!(
            manager
                .get_playlist_entries(&playlist_id)
                .await
                .expect("load runtime-updated entries")
                .into_iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            vec![Some(first.clone()), Some(second), Some(first)]
        );

        registry
            .disconnect(source_id)
            .expect("disconnect updated runtime session")
            .wait()
            .await;
        let failing_probe = FakeProbe::new(true);
        let mut failing = failing_probe.server_playlist_adapter("runtime-detail-failure");
        let secret = format!("runtime-secret-{}", Uuid::new_v4());
        failing.server_playlist_snapshot_failure = Some(secret.clone());
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(failing) },
            )
            .expect("connect failing runtime session");
        let _ = wait_for_catalogue(&registry, source_id).await;
        let before_failure = manager
            .get_server_playlist_link(&playlist_id)
            .await
            .expect("load link before detail failure")
            .expect("runtime mirror remains linked before failure");
        let submission = operations.sync_now(playlist_id.clone());
        assert_eq!(
            submission.status(),
            crate::server_playlist_coordinator::ServerPlaylistRequestStatus::Queued
        );
        let rendered = format!("{submission:?}");
        assert!(!rendered.contains(&playlist_id));
        assert!(!rendered.contains(native_id.as_str()));
        assert!(!rendered.contains(&secret));
        assert_eq!(
            submission.completion().await,
            ServerPlaylistOperationOutcome::Unavailable
        );
        assert_eq!(
            manager
                .get_server_playlist_link(&playlist_id)
                .await
                .expect("load link after detail failure")
                .expect("detail failure retains link"),
            before_failure,
            "detail failure cannot be reclassified as absence"
        );

        registry
            .disconnect(source_id)
            .expect("disconnect failing runtime session")
            .wait()
            .await;
        let absent_probe = FakeProbe::new(true);
        let mut absent = absent_probe.server_playlist_adapter("runtime-absent");
        absent.server_playlists.clear();
        absent.server_playlist_snapshot = None;
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(absent) },
            )
            .expect("connect absent runtime session");
        let _ = wait_for_catalogue(&registry, source_id).await;
        timeout(Duration::from_secs(2), async {
            loop {
                let link = manager
                    .get_server_playlist_link(&playlist_id)
                    .await
                    .expect("load automatically missing link")
                    .expect("missing runtime mirror remains linked");
                if link.remote_state == ServerPlaylistRemoteState::Missing
                    && link.state_revision == 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("complete-list absence marks runtime mirror missing");
        let missing_sidebar = snapshot_rx
            .recv()
            .await
            .expect("runtime absence publishes sidebar snapshot");
        assert!(missing_sidebar.revision() > updated_sidebar.revision());

        observer_shutdown.cancel();
        observer.await.expect("reconnect observer exits cleanly");
        coordinator.close();
        coordinator_shutdown
            .shutdown()
            .await
            .expect("coordinator drains runtime operations");
        refresh.close();
        publisher.await.expect("sidebar publisher exits cleanly");
        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reconnect_runtime_holds_the_ninth_detail_until_one_of_eight_operations_finishes() {
        const FANOUT_CAP: usize = 8;
        const MIRROR_COUNT: usize = FANOUT_CAP + 1;

        fn snapshots(label: &str) -> Vec<ServerPlaylistSnapshot> {
            (0..MIRROR_COUNT)
                .map(|index| {
                    ServerPlaylistSnapshot::new(
                        NativePlaylistId::new(format!("fanout-native-{index}"))
                            .expect("fanout native playlist ID"),
                        Some(format!("Fanout {label} {index}")),
                        None,
                        Some(1),
                        vec![TrackId::remote(format!("fanout-{label}-track-{index}"))
                            .expect("fanout track ID")],
                    )
                    .expect("fanout server playlist snapshot")
                })
                .collect()
        }

        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect fanout database");
        Migrator::up(&database, None)
            .await
            .expect("migrate fanout database");
        let manager = PlaylistManager::new(database.clone());
        let registry = registry();
        let source_id = SourceId::random();

        let initial_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            initial_probe
                .server_playlist_adapter_with_snapshots("fanout-initial", snapshots("initial")),
        )
        .await;
        let initial_listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("list initial fanout playlists");
        let native_ids = initial_listing
            .playlists()
            .iter()
            .map(|summary| summary.native_id().clone())
            .collect::<Vec<_>>();
        assert_eq!(native_ids.len(), MIRROR_COUNT);

        let mut playlist_ids = Vec::with_capacity(MIRROR_COUNT);
        for native_id in native_ids {
            let pull = registry
                .get_server_playlist(
                    initial_listing
                        .select(&native_id)
                        .expect("select exact initial fanout playlist"),
                )
                .await
                .expect("fetch exact initial fanout playlist");
            let created = manager
                .create_server_playlist_mirror_if_authorized(&pull, "Fallback", || {
                    registry.acquire_server_playlist_pull_commit_authority(&pull)
                })
                .await
                .expect("create initial fanout mirror");
            let ServerPlaylistCreateOutcome::Committed { copy, .. } = created else {
                panic!("each exact fanout identity creates one mirror");
            };
            playlist_ids.push(copy.playlist_id().to_string());
        }
        registry
            .disconnect(source_id)
            .expect("disconnect initial fanout session")
            .wait()
            .await;

        let invalidations = registry.subscribe_invalidations();
        let (refresh, _refresh_rx) =
            crate::local::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let operations = ServerPlaylistOperations::new(
            database,
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
        );
        let observer_shutdown = tokio_util::sync::CancellationToken::new();
        let observer = tokio::spawn(run_server_playlist_reconnect_observer(
            operations,
            invalidations,
            observer_shutdown.clone(),
        ));

        let successor_probe = FakeProbe::new(true);
        successor_probe
            .server_playlist_snapshot_release
            .send_replace(false);
        let successor = successor_probe
            .server_playlist_adapter_with_snapshots("fanout-successor", snapshots("updated"));
        registry
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(successor) },
            )
            .expect("successor fanout connection admitted");
        let _ = wait_for_catalogue(&registry, source_id).await;

        timeout(Duration::from_secs(2), async {
            while successor_probe
                .server_playlist_snapshot_calls
                .load(Ordering::Acquire)
                < FANOUT_CAP
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first eight fanout details start");
        assert_eq!(
            successor_probe
                .server_playlist_snapshot_calls
                .load(Ordering::Acquire),
            FANOUT_CAP,
            "the reconnect sweep must not cross its detail/commit fanout cap"
        );
        assert!(
            timeout(Duration::from_millis(50), async {
                while successor_probe
                    .server_playlist_snapshot_calls
                    .load(Ordering::Acquire)
                    < MIRROR_COUNT
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "the ninth detail started while all eight fanout slots were blocked"
        );
        assert_eq!(
            successor_probe
                .server_playlist_list_calls
                .load(Ordering::Acquire),
            1,
            "one reconnect sweep must share one complete listing"
        );

        successor_probe
            .server_playlist_snapshot_release
            .send_replace(true);
        timeout(Duration::from_secs(2), async {
            loop {
                let mut all_committed = successor_probe
                    .server_playlist_snapshot_calls
                    .load(Ordering::Acquire)
                    == MIRROR_COUNT;
                for (index, playlist_id) in playlist_ids.iter().enumerate() {
                    let link = manager
                        .get_server_playlist_link(playlist_id)
                        .await
                        .expect("load fanout link after release")
                        .expect("fanout mirror remains linked");
                    all_committed &= link.state_revision == 1
                        && link.last_synced_name == format!("Fanout updated {index}");
                }
                if all_committed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all nine exact-ID fanout operations commit after release");
        assert_eq!(
            successor_probe
                .server_playlist_list_calls
                .load(Ordering::Acquire),
            1
        );

        observer_shutdown.cancel();
        observer.await.expect("fanout observer exits cleanly");
        coordinator.close();
        coordinator_shutdown
            .shutdown()
            .await
            .expect("coordinator drains fanout operations");
        refresh.close();
        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn complete_list_absence_authority_is_exact_stale_safe_and_registry_bound() {
        let first = registry();
        let second = registry();
        let source_id = SourceId::random();

        let first_probe = FakeProbe::new(true);
        let mut empty_adapter = first_probe.server_playlist_adapter("empty-list");
        empty_adapter.server_playlists.clear();
        connect_playlist_fixture(&first, source_id, empty_adapter).await;

        let second_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &second,
            source_id,
            second_probe.server_playlist_adapter("same-epoch-other-registry"),
        )
        .await;
        assert_eq!(
            first.inner.lifecycle.active_session_epoch(source_id),
            second.inner.lifecycle.active_session_epoch(source_id),
            "both independent registries deliberately exercise the same first epoch"
        );

        let listing = first
            .list_server_playlists(source_id)
            .await
            .expect("explicit complete empty list");
        assert!(listing.playlists().is_empty());
        let secret = format!("native-secret-{}", Uuid::new_v4());
        let native_id = NativePlaylistId::new(secret.clone()).expect("native identity");
        let evidence = listing
            .prove_absent(&native_id)
            .expect("complete empty list proves exact absence");
        assert_eq!(evidence.source_id(), source_id);
        assert_eq!(evidence.native_id().as_str(), secret.as_str());
        assert!(!format!("{:?}", evidence.native_id()).contains(&secret));

        let current = first
            .acquire_server_playlist_absence_commit_authority(&evidence)
            .expect("current complete-list evidence admitted");
        drop(current);
        assert!(
            second
                .acquire_server_playlist_absence_commit_authority(&evidence)
                .is_none(),
            "another registry with the same source and first epoch cannot reuse evidence"
        );

        first
            .disconnect(source_id)
            .expect("disconnect first registry")
            .wait()
            .await;
        assert!(first
            .acquire_server_playlist_absence_commit_authority(&evidence)
            .is_none());

        let successor_probe = FakeProbe::new(true);
        let successor = successor_probe.server_playlist_adapter("absence-successor");
        first
            .connect_standard::<FakeAdapter, _, _, _>(
                source_id,
                |_| {},
                move || async move { Ok(successor) },
            )
            .expect("successor connection admitted");
        let _ = wait_for_catalogue(&first, source_id).await;
        assert!(
            first
                .acquire_server_playlist_absence_commit_authority(&evidence)
                .is_none(),
            "predecessor absence evidence cannot authorize successor state"
        );

        first.shutdown().wait().await;
        second.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_absence_commit_authority_blocks_shutdown_until_release() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let source_id = SourceId::random();
        let mut adapter = probe.server_playlist_adapter("shutdown-absence");
        adapter.server_playlists.clear();
        connect_playlist_fixture(&registry, source_id, adapter).await;

        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("complete empty list");
        let native_id = NativePlaylistId::new("shutdown-absent").expect("native identity");
        let evidence = listing.prove_absent(&native_id).expect("absence evidence");
        let authority = registry
            .acquire_server_playlist_absence_commit_authority(&evidence)
            .expect("current evidence admitted");

        let shutting_down = registry.clone();
        let (barrier_tx, barrier_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let barrier = shutting_down.shutdown();
            barrier_tx
                .send(barrier)
                .expect("shutdown barrier receiver alive");
        });
        timeout(Duration::from_secs(2), async {
            while !authority.revocation_started() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown reached session revocation");
        assert!(matches!(
            barrier_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        drop(authority);
        let barrier = barrier_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown resumes after authority release");
        worker.join().expect("shutdown worker");
        assert!(registry
            .acquire_server_playlist_absence_commit_authority(&evidence)
            .is_none());
        barrier.wait().await;
    }

    #[tokio::test]
    async fn server_playlist_backend_failures_are_sanitized() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let source_id = SourceId::random();
        let secret = format!("fixture-secret-{}", Uuid::new_v4());
        let mut adapter = probe.server_playlist_adapter("failing");
        adapter.server_playlist_list_failure = Some(secret.clone());
        connect_playlist_fixture(&registry, source_id, adapter).await;

        let Err(error) = registry.list_server_playlists(source_id).await else {
            panic!("fixture listing must fail");
        };
        assert_eq!(
            error,
            ServerPlaylistError::BackendFailure(FailureCategory::Connection)
        );
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains("native-playlist"));

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn detail_failure_is_sanitized_and_cannot_become_absence_evidence() {
        let registry = registry();
        let probe = FakeProbe::new(true);
        let source_id = SourceId::random();
        let secret = format!("detail-secret-{}", Uuid::new_v4());
        let mut adapter = probe.server_playlist_adapter("detail-failing");
        adapter.server_playlist_snapshot_failure = Some(secret.clone());
        connect_playlist_fixture(&registry, source_id, adapter).await;

        let listing = registry
            .list_server_playlists(source_id)
            .await
            .expect("complete listing succeeds before detail failure");
        let native_id = listing.playlists()[0].native_id().clone();
        assert!(
            listing.prove_absent(&native_id).is_none(),
            "detail failure cannot change complete-list presence evidence"
        );
        let selection = listing.select(&native_id).expect("present selection");
        let Err(error) = registry.get_server_playlist(selection).await else {
            panic!("fixture detail must fail");
        };
        assert_eq!(
            error,
            ServerPlaylistError::BackendFailure(FailureCategory::Connection)
        );
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains(native_id.as_str()));

        registry.shutdown().wait().await;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_browser_binds_exact_ids_and_revokes_tokens() {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect browser database");
        Migrator::up(&database, None)
            .await
            .expect("migrate browser database");
        let manager = PlaylistManager::new(database.clone());
        let registry = registry();
        let source_id = SourceId::random();
        let probe = FakeProbe::new(true);
        probe.server_playlist_release.send_replace(false);
        let snapshots = vec![
            browser_playlist_snapshot("native-a", "Same name", "track-a"),
            browser_playlist_snapshot("native-b", "Same name", "track-b"),
        ];
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.server_playlist_adapter_with_snapshots("browser-exact", snapshots),
        )
        .await;

        let (refresh, _refresh_rx) =
            crate::local::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let (browser, browser_rx) = server_playlist_browser_channel();
        let browser_shutdown = CancellationToken::new();
        let browser_owner = tokio::spawn(run_server_playlist_browser(
            browser_rx,
            database,
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
            browser_shutdown,
        ));

        let first_browse = browser.browse(source_id, "Fallback");
        timeout(Duration::from_secs(2), async {
            while probe.server_playlist_list_calls.load(Ordering::Acquire) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first browser listing starts");
        let replaced_pending_browse = browser.browse(source_id, "Fallback");
        let latest_browse = browser.browse(source_id, "Fallback");
        assert!(matches!(
            timeout(Duration::from_secs(2), replaced_pending_browse.completion())
                .await
                .expect("replaced pending browse settles"),
            ServerPlaylistBrowseOutcome::Superseded
        ));
        assert!(matches!(
            timeout(Duration::from_secs(2), first_browse.completion())
                .await
                .expect("superseded browse settles"),
            ServerPlaylistBrowseOutcome::Superseded
        ));
        probe.server_playlist_release.send_replace(true);
        let ServerPlaylistBrowseOutcome::Ready(snapshot) = latest_browse.completion().await else {
            panic!("latest browser listing should publish");
        };
        assert_eq!(
            probe.server_playlist_list_calls.load(Ordering::Acquire),
            2,
            "one active listing and one replaceable pending request must spawn only A and C"
        );
        assert_eq!(snapshot.entries().len(), 2);
        let redacted = format!("{:?}", ServerPlaylistBrowseOutcome::Ready(snapshot.clone()));
        assert!(!redacted.contains("Same name"));
        assert!(!redacted.contains("browser fixture owner"));
        assert!(!redacted.contains("native-a"));
        assert!(!redacted.contains("native-b"));

        let stale_unused = snapshot.entries()[0].action_token();
        let imported = snapshot.entries()[1].action_token();
        assert_eq!(
            browser.import_copy(imported.clone()).completion().await,
            ServerPlaylistBrowserActionOutcome::Imported
        );
        let playlists = manager
            .list_playlists()
            .await
            .expect("list exact browser import");
        assert_eq!(playlists.len(), 1);
        let entries = manager
            .get_playlist_entries(&playlists[0].id)
            .await
            .expect("load exact browser import");
        assert_eq!(
            entries[0].track_id.as_ref().map(TrackId::as_str),
            Some("track-b"),
            "the second same-name token must preserve its exact native identity"
        );

        let ServerPlaylistBrowseOutcome::Ready(relisted) =
            browser.browse(source_id, "Fallback").completion().await
        else {
            panic!("relisting should publish fresh tokens");
        };
        assert_eq!(
            browser.import_copy(stale_unused).completion().await,
            ServerPlaylistBrowserActionOutcome::Rejected,
            "relisting revokes unused predecessor tokens"
        );
        assert_eq!(
            browser.import_copy(imported).completion().await,
            ServerPlaylistBrowserActionOutcome::Rejected,
            "accepted action tokens are one-shot"
        );
        assert_eq!(
            browser
                .import_copy(relisted.entries()[0].action_token())
                .completion()
                .await,
            ServerPlaylistBrowserActionOutcome::Imported,
            "relisting mints fresh usable tokens"
        );
        assert_eq!(
            browser.close_session(relisted.session_token()),
            ServerPlaylistBrowserRequestStatus::Queued
        );
        assert_eq!(
            browser
                .import_copy(relisted.entries()[1].action_token())
                .completion()
                .await,
            ServerPlaylistBrowserActionOutcome::Rejected,
            "closing the exact session revokes every remaining token"
        );

        browser.close();
        timeout(Duration::from_secs(2), browser_owner)
            .await
            .expect("browser owner drains")
            .expect("browser owner joins");
        coordinator.close();
        coordinator_shutdown
            .shutdown()
            .await
            .expect("browser coordinator drains");
        refresh.close();
        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_browser_gates_capability_and_listing_size() {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect browser gate database");
        let registry = registry();
        let unsupported_source = SourceId::random();
        let unsupported_probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            unsupported_source,
            unsupported_probe.adapter("browser-unsupported"),
        )
        .await;

        let oversized_source = SourceId::random();
        let oversized_probe = FakeProbe::new(true);
        let mut oversized = oversized_probe.server_playlist_adapter("browser-oversized");
        let summary = oversized.server_playlists[0].clone();
        oversized.server_playlists =
            vec![summary; crate::architecture::MAX_SERVER_PLAYLISTS_PER_LIST + 1];
        connect_playlist_fixture(&registry, oversized_source, oversized).await;

        let (refresh, _refresh_rx) =
            crate::local::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let (browser, browser_rx) = server_playlist_browser_channel();
        let browser_owner = tokio::spawn(run_server_playlist_browser(
            browser_rx,
            database,
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
            CancellationToken::new(),
        ));

        assert!(matches!(
            browser
                .browse(unsupported_source, "Fallback")
                .completion()
                .await,
            ServerPlaylistBrowseOutcome::Unsupported
        ));
        assert_eq!(
            unsupported_probe
                .server_playlist_list_calls
                .load(Ordering::Acquire),
            0,
            "the typed capability gate must reject before adapter work"
        );
        assert!(matches!(
            browser
                .browse(oversized_source, "Fallback")
                .completion()
                .await,
            ServerPlaylistBrowseOutcome::Failed
        ));
        assert_eq!(
            oversized_probe
                .server_playlist_list_calls
                .load(Ordering::Acquire),
            1
        );

        browser.close();
        timeout(Duration::from_secs(2), browser_owner)
            .await
            .expect("gate browser owner drains")
            .expect("gate browser owner joins");
        coordinator.close();
        coordinator_shutdown
            .shutdown()
            .await
            .expect("gate coordinator drains");
        refresh.close();
        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_browser_capacity_preserves_the_ninth_token() {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect browser capacity database");
        Migrator::up(&database, None)
            .await
            .expect("migrate browser capacity database");
        let registry = registry();
        let source_id = SourceId::random();
        let probe = FakeProbe::new(true);
        probe.server_playlist_snapshot_release.send_replace(false);
        let snapshots = (0..=MAX_SERVER_PLAYLIST_BROWSER_ACTIONS)
            .map(|index| {
                browser_playlist_snapshot(
                    &format!("capacity-native-{index}"),
                    "Capacity",
                    &format!("capacity-track-{index}"),
                )
            })
            .collect();
        let secret = "browser-detail-secret-must-not-escape".to_string();
        let mut adapter =
            probe.server_playlist_adapter_with_snapshots("browser-capacity", snapshots);
        adapter.server_playlist_snapshot_failure = Some(secret.clone());
        connect_playlist_fixture(&registry, source_id, adapter).await;

        let (refresh, _refresh_rx) =
            crate::local::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let (browser, browser_rx) = server_playlist_browser_channel();
        let browser_owner = tokio::spawn(run_server_playlist_browser(
            browser_rx,
            database,
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
            CancellationToken::new(),
        ));
        let ServerPlaylistBrowseOutcome::Ready(snapshot) =
            browser.browse(source_id, "Fallback").completion().await
        else {
            panic!("capacity listing should publish");
        };
        let tokens = snapshot
            .entries()
            .iter()
            .map(|entry| entry.action_token())
            .collect::<Vec<_>>();
        assert_eq!(tokens.len(), MAX_SERVER_PLAYLIST_BROWSER_ACTIONS + 1);

        let mut admitted = Vec::with_capacity(MAX_SERVER_PLAYLIST_BROWSER_ACTIONS);
        for token in tokens.iter().take(MAX_SERVER_PLAYLIST_BROWSER_ACTIONS) {
            admitted.push(browser.import_copy(token.clone()));
        }
        wait_for_server_playlist_snapshot_calls(&probe, MAX_SERVER_PLAYLIST_BROWSER_ACTIONS).await;
        let ninth = browser.import_copy(tokens[MAX_SERVER_PLAYLIST_BROWSER_ACTIONS].clone());
        assert_eq!(ninth.status(), ServerPlaylistBrowserRequestStatus::Queued);
        assert_eq!(
            ninth.completion().await,
            ServerPlaylistBrowserActionOutcome::Busy
        );

        probe.server_playlist_snapshot_release.send_replace(true);
        for action in admitted {
            let outcome = action.completion().await;
            assert_eq!(outcome, ServerPlaylistBrowserActionOutcome::Failed);
            assert!(!format!("{outcome:?}").contains(&secret));
        }
        assert_eq!(
            browser
                .import_copy(tokens[MAX_SERVER_PLAYLIST_BROWSER_ACTIONS].clone())
                .completion()
                .await,
            ServerPlaylistBrowserActionOutcome::Failed,
            "Busy must leave the ninth one-shot token available for retry"
        );
        assert_eq!(
            probe.server_playlist_snapshot_calls.load(Ordering::Acquire),
            MAX_SERVER_PLAYLIST_BROWSER_ACTIONS + 1
        );

        browser.close();
        timeout(Duration::from_secs(2), browser_owner)
            .await
            .expect("capacity browser owner drains")
            .expect("capacity browser owner joins");
        coordinator.close();
        coordinator_shutdown
            .shutdown()
            .await
            .expect("capacity coordinator drains");
        refresh.close();
        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_browser_shutdown_cancels_and_drains_an_action() {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect browser shutdown database");
        Migrator::up(&database, None)
            .await
            .expect("migrate browser shutdown database");
        let registry = registry();
        let source_id = SourceId::random();
        let probe = FakeProbe::new(true);
        probe.server_playlist_snapshot_release.send_replace(false);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.server_playlist_adapter("browser-shutdown"),
        )
        .await;

        let (refresh, _refresh_rx) =
            crate::local::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let (browser, browser_rx) = server_playlist_browser_channel();
        let browser_shutdown = CancellationToken::new();
        let browser_owner = tokio::spawn(run_server_playlist_browser(
            browser_rx,
            database,
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
            browser_shutdown.clone(),
        ));
        let ServerPlaylistBrowseOutcome::Ready(snapshot) =
            browser.browse(source_id, "Fallback").completion().await
        else {
            panic!("shutdown listing should publish");
        };
        let action = browser.import_copy(snapshot.entries()[0].action_token());
        wait_for_server_playlist_snapshot_calls(&probe, 1).await;
        browser_shutdown.cancel();
        assert_eq!(
            timeout(Duration::from_secs(2), action.completion())
                .await
                .expect("cancelled action settles"),
            ServerPlaylistBrowserActionOutcome::Closed
        );
        timeout(Duration::from_secs(2), browser_owner)
            .await
            .expect("shutdown browser owner drains")
            .expect("shutdown browser owner joins");
        assert!(browser.is_closed());

        coordinator.close();
        coordinator_shutdown
            .shutdown()
            .await
            .expect("shutdown coordinator drains");
        refresh.close();
        registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_playlist_link_inspection_is_redacted_and_in_memory() {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect inspection database");
        Migrator::up(&database, None)
            .await
            .expect("migrate inspection database");
        let manager = PlaylistManager::new(database.clone());
        let registry = registry();
        let source_id = SourceId::random();
        let probe = FakeProbe::new(true);
        connect_playlist_fixture(
            &registry,
            source_id,
            probe.server_playlist_adapter("browser-inspection"),
        )
        .await;

        let (refresh, _refresh_rx) =
            crate::local::playlist_sidebar::playlist_sidebar_refresh_channel();
        let (coordinator, coordinator_shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let operations = ServerPlaylistOperations::new(
            database.clone(),
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
        );
        assert_eq!(
            operations.inspect_link("not-linked").await,
            crate::local::server_playlist_runtime::ServerPlaylistLinkInspection::NotLinked
        );

        let (browser, browser_rx) = server_playlist_browser_channel();
        let browser_owner = tokio::spawn(run_server_playlist_browser(
            browser_rx,
            database,
            coordinator.clone(),
            registry.clone(),
            refresh.clone(),
            CancellationToken::new(),
        ));
        let ServerPlaylistBrowseOutcome::Ready(snapshot) =
            browser.browse(source_id, "Fallback").completion().await
        else {
            panic!("inspection listing should publish");
        };
        assert_eq!(
            browser
                .keep_synced(snapshot.entries()[0].action_token())
                .completion()
                .await,
            ServerPlaylistBrowserActionOutcome::Linked
        );
        let links = manager
            .list_server_playlist_links(source_id)
            .await
            .expect("load inspection link");
        assert_eq!(links.len(), 1);
        let playlist_id = links[0].playlist_id.clone();
        assert_eq!(
            operations.inspect_link(&playlist_id).await,
            crate::local::server_playlist_runtime::ServerPlaylistLinkInspection::Linked {
                available: true
            }
        );
        let list_calls = probe.server_playlist_list_calls.load(Ordering::Acquire);
        probe
            .server_playlist_capability_enabled
            .store(false, Ordering::Release);
        assert_eq!(
            operations.inspect_link(&playlist_id).await,
            crate::local::server_playlist_runtime::ServerPlaylistLinkInspection::Linked {
                available: false
            }
        );
        assert_eq!(
            probe.server_playlist_list_calls.load(Ordering::Acquire),
            list_calls,
            "link inspection must not issue a network listing"
        );

        browser.close();
        timeout(Duration::from_secs(2), browser_owner)
            .await
            .expect("inspection browser owner drains")
            .expect("inspection browser owner joins");
        coordinator.close();
        assert_eq!(
            operations.inspect_link(&playlist_id).await,
            crate::local::server_playlist_runtime::ServerPlaylistLinkInspection::Closed
        );
        coordinator_shutdown
            .shutdown()
            .await
            .expect("inspection coordinator drains");
        refresh.close();
        registry.shutdown().wait().await;
    }
}
