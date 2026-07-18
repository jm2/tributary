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

use crate::architecture::backend::BackendResult;
#[cfg(test)]
use crate::architecture::backend::MediaBackend;
use crate::architecture::error::BackendError;
use crate::architecture::media::{
    MediaLease, MediaRequest, PublicHttpAuthority, PublicHttpEndpoint, RemoteMediaResolver,
    ResolvedHttpRequest, ResolvedPublicHttpRequest,
};
use crate::architecture::models::Track;
use crate::architecture::{SourceId, TrackId, ViewOrigin};
use crate::external_file::{ExternalFileCandidate, ExternalFileHint};
use crate::local::resolver::ResolvedFileMedia;
use crate::source_lifecycle::{
    AdapterCloseFuture, AdapterStream, AdapterTaskResult, CloseAuthority,
    ConstructionCancellationPolicy, FailureCategory, LifecycleAdapter, LifecycleBaseline,
    LifecycleSnapshot, ProvenanceClaimId, RefreshLane, RefreshTaskResult, RetirementWaiter,
    ShutdownBarrier, SourceLifecycleRegistry, SourceProvenance,
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

    fn catalogue(tracks: Vec<Track>) -> Self {
        Self {
            tracks: Arc::new(tracks),
            public_streams: HashMap::new(),
        }
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
    view_lease: MediaLease,
}

impl AcceptedSourcePayload {
    fn from_view(view: AcceptedView) -> Self {
        Self {
            tracks: view.tracks,
            public_streams: view.public_streams,
            view_lease: MediaLease::new(),
        }
    }

    fn catalogue(tracks: Vec<Track>) -> Self {
        Self::from_view(AcceptedView::catalogue(tracks))
    }

    fn published(&self) -> AcceptedView {
        AcceptedView::published(Arc::clone(&self.tracks))
    }
}

impl Drop for AcceptedSourcePayload {
    fn drop(&mut self) {
        self.view_lease.revoke();
    }
}

/// Heterogeneous operational contract stored by one lifecycle registry.
pub trait ManagedSourceAdapter: LifecycleAdapter + Send + Sync {
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

macro_rules! standard_remote_adapter {
    ($adapter:ty) => {
        impl LifecycleAdapter for $adapter {
            fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
                Box::pin(async { Ok(()) })
            }
        }

        impl ManagedSourceAdapter for $adapter {
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

standard_remote_adapter!(crate::subsonic::SubsonicBackend);
// Plex's legacy auth token is a durable credential, not a revocable server
// session: its documented revocation mechanisms are account/device-wide, so
// Tributary has no safe per-adapter close authority. Constructors may therefore
// be aborted, while disconnect only revokes local media/session authority.
standard_remote_adapter!(crate::plex::PlexBackend);

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

impl ManagedSourceAdapter for crate::daap::DaapBackend {
    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move { self.load_catalogue().await })
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
            AcceptedSourcePayload::catalogue(vec![track.clone()]),
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
                match adapter.load_initial_catalogue().await {
                    Ok(tracks) => {
                        RefreshTaskResult::Refreshed(AcceptedSourcePayload::catalogue(tracks))
                    }
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
                match adapter.load_initial_catalogue().await {
                    Ok(tracks) => {
                        RefreshTaskResult::Refreshed(AcceptedSourcePayload::catalogue(tracks))
                    }
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
        let (_, session_epoch) = self.inner.lifecycle.adopt_stateless_session(
            source_id,
            adapter,
            AcceptedSourcePayload::catalogue(Vec::new()),
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

    pub async fn resolve_stream(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        track_id: TrackId,
    ) -> BackendResult<ResolvedSourceStream> {
        if let Some(resolved) = self.inner.lifecycle.resolve_latest_accepted_view(
            source_id,
            expected_session_epoch,
            |payload| {
                payload
                    .public_streams
                    .get(&track_id)
                    .cloned()
                    .map(|endpoint| (endpoint, payload.view_lease.clone()))
            },
        ) {
            let authority: Arc<dyn PublicHttpAuthority> = self.inner.clone();
            let authority = Arc::downgrade(&authority);
            let (endpoint, lease) = resolved.value;
            return Ok(ResolvedSourceStream::Http(MediaRequest::PublicHttp(
                ResolvedPublicHttpRequest::new(
                    endpoint,
                    lease,
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
    crate::source_lifecycle::AcceptedSnapshot {
        generation: snapshot.generation,
        session_epoch: snapshot.session_epoch,
        value: Arc::new(snapshot.value.published()),
    }
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
    use tokio::runtime::Handle;
    use tokio::sync::{oneshot, watch};
    use tokio::time::{timeout, Duration};
    use url::Url;
    use uuid::Uuid;

    use crate::architecture::models::{
        Album, Artist, LibraryStats, SearchResults, SortField, SortOrder,
    };
    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};

    use super::*;

    struct FakeProbe {
        close_calls: AtomicUsize,
        stream_calls: AtomicUsize,
        close_release: watch::Sender<bool>,
        view_specs: Mutex<HashMap<ViewOrigin, VecDeque<ViewSpec>>>,
    }

    struct ViewSpec {
        delay: Duration,
        endpoint: Url,
    }

    impl FakeProbe {
        fn new(close_released: bool) -> Arc<Self> {
            let (close_release, _receiver) = watch::channel(close_released);
            Arc::new(Self {
                close_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
                close_release,
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
            _track_id: &TrackId,
        ) -> BackendResult<Option<ResolvedHttpRequest>> {
            Ok(None)
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
        fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
            Box::pin(async { Ok(Vec::new()) })
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
        }
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
