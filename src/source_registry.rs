//! Production lifecycle service for authenticated remote sources.
//!
//! This is the single owner for Subsonic, Jellyfin, Plex, and DAAP adapters.
//! Catalogue rows and playback queues retain `(SourceId, TrackId)` plus the
//! non-secret session epoch that published them; URLs, credentials, resolver
//! maps, random lease keys, and DAAP session keys stay out of UI state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use crate::architecture::backend::{BackendResult, MediaBackend};
use crate::architecture::error::BackendError;
use crate::architecture::media::{RemoteMediaResolver, ResolvedHttpRequest};
use crate::architecture::models::Track;
use crate::architecture::{SourceId, TrackId};
use crate::source_lifecycle::{
    AdapterCloseFuture, AdapterTaskResult, CloseAuthority, ConstructionCancellationPolicy,
    FailureCategory, LifecycleAdapter, LifecycleBaseline, LifecycleSnapshot, ProvenanceClaimId,
    RefreshTaskResult, RetirementWaiter, ShutdownBarrier, SourceLifecycleRegistry,
    SourceProvenance,
};

type CatalogueFuture = Pin<Box<dyn Future<Output = BackendResult<Vec<Track>>> + Send + 'static>>;

/// Heterogeneous operational contract stored by one lifecycle registry.
pub trait ManagedRemoteAdapter:
    MediaBackend + RemoteMediaResolver + LifecycleAdapter + Send + Sync
{
    /// Load the first complete catalogue after construction is staged.
    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture;
}

macro_rules! standard_remote_adapter {
    ($adapter:ty) => {
        impl LifecycleAdapter for $adapter {
            fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
                Box::pin(async { Ok(()) })
            }
        }

        impl ManagedRemoteAdapter for $adapter {
            fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
                Box::pin(
                    async move { crate::architecture::load_track_catalog(self.as_ref()).await },
                )
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

impl ManagedRemoteAdapter for crate::jellyfin::JellyfinBackend {
    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move {
            self.ensure_initialized().await?;
            crate::architecture::load_track_catalog(self.as_ref()).await
        })
    }
}

mod sealed {
    pub trait AbortableRemoteAdapter {}
}

/// Marker for constructors whose cancellation cannot strand lifecycle-owned,
/// individually closeable server state. DAAP and interactive Jellyfin login
/// deliberately cannot satisfy it. Plex's legacy durable credential has no
/// safe per-token close operation and is documented separately above.
pub trait AbortableRemoteAdapter: ManagedRemoteAdapter + sealed::AbortableRemoteAdapter {}

macro_rules! abortable_remote_adapter {
    ($adapter:ty) => {
        impl sealed::AbortableRemoteAdapter for $adapter {}
        impl AbortableRemoteAdapter for $adapter {}
    };
}

abortable_remote_adapter!(crate::subsonic::SubsonicBackend);
abortable_remote_adapter!(crate::plex::PlexBackend);

impl ManagedRemoteAdapter for crate::daap::DaapBackend {
    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move { self.load_catalogue().await })
    }
}

/// Cloneable application service around the centralized lifecycle authority.
#[derive(Clone)]
pub struct RemoteSourceRegistry {
    lifecycle: SourceLifecycleRegistry<dyn ManagedRemoteAdapter, Vec<Track>>,
}

impl RemoteSourceRegistry {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            lifecycle: SourceLifecycleRegistry::new(runtime),
        }
    }

    pub fn subscribe_invalidations(&self) -> tokio::sync::watch::Receiver<u64> {
        self.lifecycle.subscribe_invalidations()
    }

    pub fn claim_provenance(
        &self,
        source_id: SourceId,
        provenance: SourceProvenance,
    ) -> Option<ProvenanceClaimId> {
        self.lifecycle.claim_provenance(source_id, provenance)
    }

    pub fn release_provenance(&self, source_id: SourceId, claim_id: ProvenanceClaimId) -> bool {
        if !self.lifecycle.release_provenance(source_id, claim_id) {
            return false;
        }
        self.lifecycle
            .schedule_prune_after_current_retirement(source_id);
        true
    }

    pub fn snapshot(&self, source_id: SourceId) -> Option<LifecycleSnapshot<Vec<Track>>> {
        self.lifecycle.snapshot(source_id)
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
        A: AbortableRemoteAdapter + 'static,
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

    fn spawn_connect<A, OnGeneration, Authenticate, AuthenticateFuture>(
        &self,
        source_id: SourceId,
        policy: ConstructionCancellationPolicy,
        on_generation: OnGeneration,
        authenticate: Authenticate,
    ) -> Option<u64>
    where
        A: ManagedRemoteAdapter + 'static,
        OnGeneration: FnOnce(u64),
        Authenticate: FnOnce() -> AuthenticateFuture + Send + 'static,
        AuthenticateFuture: Future<Output = BackendResult<A>> + Send + 'static,
    {
        let owner = self.lifecycle.begin_connect(source_id)?;
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
                    Ok(tracks) => RefreshTaskResult::Refreshed(tracks),
                    Err(error) => RefreshTaskResult::Failed(failure_category(&error)),
                }
            },
        );
        Some(generation)
    }

    pub fn disconnect(&self, source_id: SourceId) -> Option<RetirementWaiter> {
        self.lifecycle.disconnect(source_id)
    }

    pub fn shutdown(&self) -> ShutdownBarrier {
        self.lifecycle.shutdown()
    }

    #[cfg(test)]
    pub fn is_shutting_down(&self) -> bool {
        self.lifecycle.is_shutting_down()
    }

    /// Validate the exact accepted catalogue at the GTK publication boundary.
    #[cfg(test)]
    pub fn is_current_catalogue(
        &self,
        source_id: SourceId,
        generation: u64,
        session_epoch: u64,
    ) -> bool {
        self.lifecycle
            .is_current_catalogue(source_id, generation, session_epoch)
    }

    #[cfg(test)]
    pub fn has_session_epoch(&self, source_id: SourceId, session_epoch: u64) -> bool {
        self.lifecycle.active_session_epoch(source_id) == Some(session_epoch)
    }

    pub fn snapshot_all(&self) -> LifecycleBaseline<Vec<Track>> {
        self.lifecycle.snapshot_all()
    }

    pub async fn resolve_stream(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        track_id: TrackId,
    ) -> BackendResult<ResolvedHttpRequest> {
        self.lifecycle
            .resolve_http(
                source_id,
                expected_session_epoch,
                move |adapter| async move { adapter.resolve_stream(&track_id).await },
            )
            .await
    }

    pub async fn resolve_artwork(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        track_id: TrackId,
    ) -> BackendResult<Option<ResolvedHttpRequest>> {
        self.lifecycle
            .resolve_optional_http(
                source_id,
                expected_session_epoch,
                move |adapter| async move { adapter.resolve_artwork(&track_id).await },
            )
            .await
    }
}

/// Convert one concrete backend into the task result accepted by the
/// heterogeneous lifecycle registry.
fn constructed_adapter<A>(adapter: A) -> AdapterTaskResult<dyn ManagedRemoteAdapter>
where
    A: ManagedRemoteAdapter + 'static,
{
    AdapterTaskResult::Constructed(Box::new(adapter))
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
/// retirement authority remain exclusively in [`RemoteSourceRegistry`].
#[derive(Clone, Default)]
pub struct ProvenanceClaims {
    claims: Rc<RefCell<ProvenanceClaimMap>>,
}

type ProvenanceClaimKey = (SourceId, SourceProvenance, String);
type ProvenanceClaimMap = HashMap<ProvenanceClaimKey, ProvenanceClaimId>;

impl ProvenanceClaims {
    pub fn ensure(
        &self,
        registry: &RemoteSourceRegistry,
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
        registry: &RemoteSourceRegistry,
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
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::http::{Method, StatusCode};
    use tokio::runtime::Handle;
    use tokio::sync::watch;
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
    }

    impl FakeProbe {
        fn new(close_released: bool) -> Arc<Self> {
            let (close_release, _receiver) = watch::channel(close_released);
            Arc::new(Self {
                close_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
                close_release,
            })
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

    impl ManagedRemoteAdapter for FakeAdapter {
        fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    impl sealed::AbortableRemoteAdapter for FakeAdapter {}
    impl AbortableRemoteAdapter for FakeAdapter {}

    fn registry() -> RemoteSourceRegistry {
        RemoteSourceRegistry::new(Handle::current())
    }

    async fn wait_for_catalogue(
        registry: &RemoteSourceRegistry,
        source_id: SourceId,
    ) -> (u64, u64) {
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

    async fn wait_until_pruned(registry: &RemoteSourceRegistry, source_id: SourceId) {
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
        assert!(predecessor_request.is_active());

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

        assert!(!predecessor_request.is_active());
        assert!(registry
            .resolve_stream(source_id, predecessor_epoch, track_id.clone())
            .await
            .is_err());
        assert_eq!(successor.stream_calls.load(Ordering::Acquire), 0);
        let successor_request = registry
            .resolve_stream(source_id, successor_epoch, track_id)
            .await
            .expect("successor media");
        assert!(successor_request.is_active());
        assert!(registry.has_session_epoch(source_id, successor_epoch));
        assert!(registry.is_current_catalogue(source_id, successor_generation, successor_epoch));

        assert!(claims.release(&registry, source_id, SourceProvenance::Saved, "saved"));
        wait_until_pruned(&registry, source_id).await;
        predecessor.wait_for_close_calls(1).await;
        successor.wait_for_close_calls(1).await;
        assert!(!successor_request.is_active());
        assert_eq!(predecessor.close_calls.load(Ordering::Acquire), 1);
        assert_eq!(successor.close_calls.load(Ordering::Acquire), 1);

        registry.shutdown().wait().await;
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
}
