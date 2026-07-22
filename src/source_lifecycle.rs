//! Central source lifecycle ownership authority.
//!
//! The production source service uses this generic registry for Subsonic,
//! Jellyfin, Plex, DAAP, and the stateless built-in Radio-Browser adapter. One
//! entry atomically owns its adapter, revocable media lease, session epoch,
//! provenance, accepted snapshots, and generation-scoped operations. Every
//! constructed adapter enters exactly-once retirement even when cancelled or
//! stale, and shutdown joins all tracked construction/refresh/close work.
//! Ephemeral external-file and mounted removable adapters also use this
//! authority. The built-in local engine keeps its specialized scan lifecycle
//! while sharing the retained typed-file consumption boundary.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use futures::FutureExt;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::media::ResolvedHttpRequest;
use crate::architecture::media::{MediaLease, MediaLeasePermit};
use crate::architecture::{SourceId, ViewOrigin};
use crate::local::resolver::ResolvedFileMedia;

/// Internal adapter result whose lifecycle lease is attached before it can
/// leave the exact-session resolver.
pub enum AdapterStream {
    ProtectedHttp(Box<ResolvedHttpRequest>),
    File(ResolvedFileMedia),
}

/// Internal distinction used to close raw adapter errors at the public
/// regular-playlist registry boundary.
pub enum CatalogueMediaResolveError {
    Unavailable,
    Backend(BackendError),
}

/// Closed lifecycle result for an adapter operation bound to one exact
/// adopted session.
///
/// The caller can distinguish lifecycle staleness from an adapter failure
/// without receiving the active lease or adapter. Backend details remain
/// private to the higher-level registry that owns the public error policy.
// Plain `pub` is still crate-internal because `source_lifecycle` is private at
// the crate root; it also lets the sibling registry name the closed result.
pub enum SessionOperationError {
    Unavailable,
    Backend(BackendError),
}

/// Opaque proof that one adapter operation completed through an exact live
/// session.
///
/// The receipt is deliberately weaker than authority: it retains neither the
/// registry, adapter, nor session lease and therefore cannot keep a source
/// alive. A commit-scoped caller must present it back to this exact lifecycle
/// registry for a final locked revalidation and permit acquisition.
pub struct SessionOperationReceipt<A: ?Sized> {
    incarnation: Uuid,
    source_id: SourceId,
    session_epoch: u64,
    adapter: Weak<A>,
}

impl<A: ?Sized> Clone for SessionOperationReceipt<A> {
    fn clone(&self) -> Self {
        Self {
            incarnation: self.incarnation,
            source_id: self.source_id,
            session_epoch: self.session_epoch,
            adapter: self.adapter.clone(),
        }
    }
}

/// Opaque admission that orders one database commit before exact session
/// invalidation can complete.
///
/// Dropping this value releases the sole in-flight permit. It contains no
/// adapter, native identity, locator, credential, or public operation.
#[must_use = "session commit authority must be retained through the database commit"]
pub struct SessionCommitAuthority {
    #[allow(dead_code)] // Retention through Drop is the authority operation.
    permit: MediaLeasePermit,
}

impl SessionCommitAuthority {
    #[cfg(test)]
    pub(crate) fn revocation_started(&self) -> bool {
        self.permit.revocation_started()
    }
}

impl AdapterStream {
    fn with_lease(self, lease: MediaLease) -> Self {
        match self {
            Self::ProtectedHttp(request) => {
                Self::ProtectedHttp(Box::new((*request).with_lease(lease)))
            }
            Self::File(media) => Self::File(media.with_lease(lease)),
        }
    }
}

/// Result future for one bounded, best-effort adapter close.
pub type AdapterCloseFuture =
    Pin<Box<dyn Future<Output = Result<(), FailureCategory>> + Send + 'static>>;

/// Session-specific behavior owned by the lifecycle registry.
///
/// `close` consumes the registry's `Arc` owner and is invoked exactly once.
/// Implementations must be bounded internally; shutdown keeps the async
/// runtime alive until every returned close future has completed.
pub trait LifecycleAdapter: Send + Sync + 'static {
    fn close(self: Arc<Self>, authority: CloseAuthority) -> AdapterCloseFuture;
}

/// Unforgeable capability required to start adapter teardown.
///
/// Adapter implementations can name this type in their trait method, but
/// only this module can construct a value. An operational session handle can
/// therefore resolve media without gaining close authority.
pub struct CloseAuthority {
    _private: (),
}

/// Unique submission owner for one newly constructed adapter.
///
/// Construction consumes the adapter before creating its private `Arc`.
/// Callers cannot clone or recover that `Arc` until the registry has accepted
/// it or a staged guard owns its mandatory retirement path.
struct ConstructedAdapter<A: ?Sized> {
    adapter: Arc<A>,
}

impl<A> ConstructedAdapter<A> {
    fn new(adapter: A) -> Self {
        Self {
            adapter: Arc::new(adapter),
        }
    }
}

impl<A: ?Sized> ConstructedAdapter<A> {
    fn from_box(adapter: Box<A>) -> Self {
        Self {
            adapter: Arc::from(adapter),
        }
    }

    fn operational(&self) -> Arc<A> {
        Arc::clone(&self.adapter)
    }

    fn into_operational(self) -> Arc<A> {
        self.adapter
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RegistryGate {
    #[default]
    Running,
    ShuttingDown,
}

/// Independent reasons a logical source currently exists.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SourceProvenance {
    Saved,
    Environment,
    Discovery,
    BuiltIn,
    Removable,
    External,
}

/// Opaque ownership key for one independent provenance publisher.
///
/// Two discovery interfaces, removable monitors, or configuration producers
/// can claim the same provenance kind without one publisher's removal erasing
/// the other. The publisher retains this token until its exact claim ends.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProvenanceClaimId(Uuid);

impl ProvenanceClaimId {
    fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Whether an inactive source remains registered for a later retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retention {
    Retained,
    Ephemeral,
}

/// Whether a source contributes a row to ordinary source navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceVisibility {
    Visible,
    Hidden,
}

/// Set of independent source provenance contributions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProvenanceSet(HashMap<SourceProvenance, usize>);

impl ProvenanceSet {
    pub fn contains(&self, contribution: SourceProvenance) -> bool {
        self.claim_count(contribution) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn claim_count(&self, contribution: SourceProvenance) -> usize {
        self.0.get(&contribution).copied().unwrap_or(0)
    }

    pub fn retention(&self) -> Retention {
        if self.0.keys().any(|contribution| {
            matches!(
                *contribution,
                SourceProvenance::Saved | SourceProvenance::Environment | SourceProvenance::BuiltIn
            )
        }) {
            Retention::Retained
        } else {
            Retention::Ephemeral
        }
    }

    pub fn visibility(&self) -> SourceVisibility {
        if self
            .0
            .keys()
            .any(|contribution| !matches!(*contribution, SourceProvenance::External))
        {
            SourceVisibility::Visible
        } else {
            SourceVisibility::Hidden
        }
    }
}

/// Closed operation classes retained with a sanitized failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureOperation {
    Connect,
    Refresh,
    Disconnect,
}

/// Closed failure categories safe to retain and publish.
///
/// This type cannot hold a URL, credential, path, native ID, response body,
/// or backend error chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureCategory {
    AuthenticationRejected,
    Connection,
    Timeout,
    InvalidResponse,
    UnsupportedAuthentication,
    UnavailableOrPermission,
    Backend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceFailure {
    operation: FailureOperation,
    category: FailureCategory,
}

/// Exact operation identity retained alongside a closed failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationCorrelation {
    pub generation: u64,
    pub session_epoch: Option<u64>,
}

/// Snapshot-safe failure annotation. A lagged/dropped observer event can
/// resynchronize this exact generation instead of clearing a newer retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CorrelatedFailure {
    pub correlation: OperationCorrelation,
    pub failure: SourceFailure,
}

impl SourceFailure {
    pub const fn connect(category: FailureCategory) -> Self {
        Self {
            operation: FailureOperation::Connect,
            category,
        }
    }

    pub const fn refresh(category: FailureCategory) -> Self {
        Self {
            operation: FailureOperation::Refresh,
            category,
        }
    }

    pub const fn disconnect(category: FailureCategory) -> Self {
        Self {
            operation: FailureOperation::Disconnect,
            category,
        }
    }

    pub const fn operation(self) -> FailureOperation {
        self.operation
    }

    pub const fn category(self) -> FailureCategory {
        self.category
    }
}

/// Externally observable lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceState {
    Dormant,
    Connecting,
    Ready,
    Refreshing,
    Failed,
    Disconnecting,
    Retired,
}

/// One source-wide catalogue lane or one independently refreshed view.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RefreshLane {
    Catalogue,
    View(ViewOrigin),
}

/// Failure annotation location in a typed lifecycle change.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FailureLane {
    Session,
    Refresh(RefreshLane),
}

/// Typed observer event. Every accepted event has a strictly newer revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleChange {
    ProvenanceChanged {
        contributions: ProvenanceSet,
        visibility: SourceVisibility,
        retention: Retention,
    },
    StateChanged {
        from: SourceState,
        to: SourceState,
        session_epoch: Option<u64>,
    },
    ConnectStarted {
        generation: u64,
    },
    RefreshStarted {
        lane: RefreshLane,
        generation: u64,
        session_epoch: u64,
    },
    OperationCancelled {
        lane: RefreshLane,
        generation: u64,
    },
    ConnectCancelled {
        generation: u64,
    },
    SessionAdopted {
        session_epoch: u64,
        replaced_epoch: Option<u64>,
    },
    CatalogueAccepted {
        generation: u64,
        session_epoch: u64,
    },
    ViewAccepted {
        view: ViewOrigin,
        generation: u64,
        session_epoch: u64,
    },
    ViewRemoved {
        view: ViewOrigin,
    },
    SnapshotsCleared,
    FailureChanged {
        lane: FailureLane,
        correlation: OperationCorrelation,
        failure: Option<SourceFailure>,
    },
    SessionRetired {
        session_epoch: u64,
        failure: Option<SourceFailure>,
    },
    Pruned,
}

/// One typed event with total ordering inside this registry incarnation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionedLifecycleChange {
    pub revision: u64,
    pub source_id: SourceId,
    pub change: LifecycleChange,
}

struct CancellationSwitch {
    sender: watch::Sender<bool>,
}

impl CancellationSwitch {
    fn pair() -> (Self, CancellationObserver) {
        let (sender, receiver) = watch::channel(false);
        (Self { sender }, CancellationObserver { receiver })
    }

    fn cancel(&self) {
        self.sender.send_replace(true);
    }
}

/// Wakeable cancellation observation safe when cancellation precedes waiting.
#[derive(Clone)]
pub struct CancellationObserver {
    receiver: watch::Receiver<bool>,
}

impl fmt::Debug for CancellationObserver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationObserver")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancellationObserver {
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    pub async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        loop {
            if self.receiver.changed().await.is_err() || self.is_cancelled() {
                return;
            }
        }
    }
}

struct TrackerState {
    active: usize,
}

struct OperationTracker {
    state: Mutex<TrackerState>,
    active_sender: watch::Sender<usize>,
}

/// Exact lifetime and sanitized close outcome for one spawned connect task.
/// The task holds one participant, and any adapter it constructs transfers a
/// second participant into mandatory retirement before the task can finish.
struct ConnectSettlement {
    tracker: Arc<OperationTracker>,
    close_failure: Mutex<Option<SourceFailure>>,
}

impl ConnectSettlement {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            tracker: OperationTracker::new(),
            close_failure: Mutex::new(None),
        })
    }

    fn participate(&self) -> OperationParticipant {
        self.tracker.participate()
    }

    fn active(&self) -> usize {
        self.tracker.active()
    }

    async fn wait(&self) {
        self.tracker.wait_idle().await;
    }

    fn record_close_failure(&self, category: FailureCategory) {
        let mut failure = lock(&self.close_failure);
        if failure.is_none() {
            *failure = Some(SourceFailure::disconnect(category));
        }
    }

    fn close_failure(&self) -> Option<SourceFailure> {
        *lock(&self.close_failure)
    }
}

impl OperationTracker {
    fn new() -> Arc<Self> {
        let (active_sender, _receiver) = watch::channel(0);
        Arc::new(Self {
            state: Mutex::new(TrackerState { active: 0 }),
            active_sender,
        })
    }

    fn participate(self: &Arc<Self>) -> OperationParticipant {
        let mut state = lock(&self.state);
        state.active = state
            .active
            .checked_add(1)
            .expect("source lifecycle operation count exhausted");
        self.active_sender.send_replace(state.active);
        OperationParticipant {
            tracker: Arc::clone(self),
            active: true,
        }
    }

    fn finish(&self) {
        let mut state = lock(&self.state);
        state.active = state
            .active
            .checked_sub(1)
            .expect("source lifecycle operation count underflow");
        self.active_sender.send_replace(state.active);
    }

    fn active(&self) -> usize {
        lock(&self.state).active
    }

    async fn wait_idle(&self) {
        let mut receiver = self.active_sender.subscribe();
        loop {
            if *receiver.borrow_and_update() == 0 {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

struct OperationParticipant {
    tracker: Arc<OperationTracker>,
    active: bool,
}

impl Drop for OperationParticipant {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.tracker.finish();
        }
    }
}

/// Persistent join barrier returned by every concurrent/repeated shutdown.
#[derive(Clone)]
pub struct ShutdownBarrier {
    tracker: Arc<OperationTracker>,
}

impl ShutdownBarrier {
    pub fn is_complete(&self) -> bool {
        self.tracker.active() == 0
    }

    pub fn pending_operations(&self) -> usize {
        self.tracker.active()
    }

    pub async fn wait(&self) {
        self.tracker.wait_idle().await;
    }
}

/// Immutable accepted source-wide or per-view snapshot metadata.
pub struct AcceptedSnapshot<S> {
    pub generation: u64,
    pub session_epoch: u64,
    pub value: Arc<S>,
    authority: MediaLease,
}

/// Value selected from the newest accepted view that contributes it.
///
/// View refresh generations are source-registry-global, so choosing the
/// greatest accepted generation is deterministic even when overlapping views
/// complete out of order.
pub struct LatestAcceptedView<T> {
    pub generation: u64,
    pub session_epoch: u64,
    pub(crate) authority: MediaLease,
    pub value: T,
}

/// Value selected from the one currently accepted source-wide catalogue.
///
/// The immutable catalogue snapshot, generation, active session epoch, and
/// authority are captured atomically. The selector then runs outside the
/// lifecycle lock. The selected value must not retain the catalogue payload
/// itself.
pub struct CurrentAcceptedCatalogue<T> {
    pub generation: u64,
    pub session_epoch: u64,
    pub authority: MediaLease,
    pub value: T,
}

/// One exact catalogue member required by a commit-scoped caller.
///
/// The selected key remains generic so the lifecycle layer can validate it
/// through a bounded registry-owned payload selector without learning any
/// backend catalogue representation.
pub struct CatalogueCommitRequest<K> {
    pub source_id: SourceId,
    pub catalogue_generation: u64,
    pub session_epoch: u64,
    pub selected: K,
}

/// Opaque admission that orders one commit before exact session/catalogue
/// invalidation can complete or publish.
///
/// Dropping this value is the only operation: it releases every in-flight
/// permit. Callers must not retain it while asking the lifecycle registry to
/// replace or disconnect one of the admitted sources.
pub struct CatalogueCommitAuthority {
    permits: Vec<MediaLeasePermit>,
}

impl CatalogueCommitAuthority {
    #[cfg(test)]
    pub(crate) fn permit_count(&self) -> usize {
        self.permits.len()
    }

    #[cfg(test)]
    pub(crate) fn revocation_started(&self) -> bool {
        self.permits
            .iter()
            .any(MediaLeasePermit::revocation_started)
    }
}

impl<S> AcceptedSnapshot<S> {
    fn new(generation: u64, session_epoch: u64, value: S) -> Self {
        Self {
            generation,
            session_epoch,
            value: Arc::new(value),
            authority: MediaLease::new(),
        }
    }

    fn revoke(&self) {
        self.authority.revoke();
    }

    pub(crate) fn map<T>(self, project: impl FnOnce(&S) -> T) -> AcceptedSnapshot<T> {
        let value = project(self.value.as_ref());
        AcceptedSnapshot {
            generation: self.generation,
            session_epoch: self.session_epoch,
            value: Arc::new(value),
            authority: self.authority,
        }
    }
}

impl<S> Clone for AcceptedSnapshot<S> {
    fn clone(&self) -> Self {
        Self {
            generation: self.generation,
            session_epoch: self.session_epoch,
            value: Arc::clone(&self.value),
            authority: self.authority.clone(),
        }
    }
}

/// Immutable source state read by observers that subscribe after an event.
#[derive(Clone)]
pub struct LifecycleSnapshot<S> {
    pub revision: u64,
    pub state: SourceState,
    pub session_epoch: Option<u64>,
    pub provenance: ProvenanceSet,
    pub visibility: SourceVisibility,
    pub retention: Retention,
    pub catalogue: Option<AcceptedSnapshot<S>>,
    pub views: HashMap<ViewOrigin, AcceptedSnapshot<S>>,
    pub failure: Option<CorrelatedFailure>,
    pub refresh_failures: HashMap<RefreshLane, CorrelatedFailure>,
    pub pending_connect: Option<u64>,
    pub pending_refreshes: HashMap<RefreshLane, u64>,
    pub pending_retirements: usize,
}

/// One atomic observer baseline. The global revision, shutdown gate, and all
/// per-source snapshots are captured under the same registry lock so a
/// reducer never combines live rows with a stale admission state.
#[derive(Clone)]
pub struct LifecycleBaseline<S> {
    pub revision: u64,
    pub shutting_down: bool,
    pub sources: Vec<(SourceId, LifecycleSnapshot<S>)>,
}

struct PendingOperation {
    generation: u64,
    session_epoch: Option<u64>,
    cancellation: CancellationSwitch,
    abort: Option<AbortHandle>,
    abortable: bool,
    /// Exact task/late-adapter settlement for a connect operation. A protected
    /// constructor can outlive cancellation and mint an adapter which must be
    /// closed before an explicit-disconnect waiter may complete.
    settlement: Option<Arc<ConnectSettlement>>,
}

/// Whether cancellation may abort a connect constructor before it returns.
///
/// Sessionful protocols whose constructor can acquire remote state before the
/// returned adapter exists must use `FinishConstruction`. Such constructors
/// must be internally bounded: cancellation is signalled, the constructor is
/// allowed to finish its own error cleanup, and any returned adapter is
/// synchronously staged and retired. `Abortable` is only for constructors
/// whose cancellation cannot strand externally owned state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstructionCancellationPolicy {
    Abortable,
    FinishConstruction,
}

impl ConstructionCancellationPolicy {
    const fn abortable(self) -> bool {
        matches!(self, Self::Abortable)
    }
}

struct ActiveSession<A: ?Sized> {
    epoch: u64,
    adapter: ConstructedAdapter<A>,
    lease: MediaLease,
}

struct Entry<A: ?Sized, S> {
    state: SourceState,
    provenance: ProvenanceSet,
    provenance_claims: HashMap<ProvenanceClaimId, SourceProvenance>,
    active: Option<ActiveSession<A>>,
    connect: Option<PendingOperation>,
    refreshes: HashMap<RefreshLane, PendingOperation>,
    disconnect_retirement: Option<u64>,
    disconnect_waiter: Option<RetirementWaiter>,
    connect_settlements: HashMap<u64, Arc<ConnectSettlement>>,
    retirement_ids: HashSet<u64>,
    catalogue: Option<AcceptedSnapshot<S>>,
    views: HashMap<ViewOrigin, AcceptedSnapshot<S>>,
    failure: Option<CorrelatedFailure>,
    refresh_failures: HashMap<RefreshLane, CorrelatedFailure>,
    revision: u64,
}

impl<A: ?Sized, S> Entry<A, S> {
    fn new(claim_id: ProvenanceClaimId, contribution: SourceProvenance) -> Self {
        let mut provenance = ProvenanceSet::default();
        provenance.0.insert(contribution, 1);
        let mut provenance_claims = HashMap::new();
        provenance_claims.insert(claim_id, contribution);
        Self {
            state: SourceState::Dormant,
            provenance,
            provenance_claims,
            active: None,
            connect: None,
            refreshes: HashMap::new(),
            disconnect_retirement: None,
            disconnect_waiter: None,
            connect_settlements: HashMap::new(),
            retirement_ids: HashSet::new(),
            catalogue: None,
            views: HashMap::new(),
            failure: None,
            refresh_failures: HashMap::new(),
            revision: 0,
        }
    }

    fn session_epoch(&self) -> Option<u64> {
        self.active.as_ref().map(|session| session.epoch)
    }

    fn replace_catalogue(&mut self, accepted: AcceptedSnapshot<S>) {
        if let Some(previous) = self.catalogue.as_ref() {
            previous.revoke();
        }
        self.catalogue = Some(accepted);
    }

    fn replace_view(&mut self, view: ViewOrigin, accepted: AcceptedSnapshot<S>) {
        if let Some(previous) = self.views.get(&view) {
            previous.revoke();
        }
        self.views.insert(view, accepted);
    }

    fn remove_view(&mut self, view: &ViewOrigin) -> bool {
        let Some(removed) = self.views.get(view) else {
            return false;
        };
        removed.revoke();
        self.views.remove(view);
        true
    }

    fn revoke_snapshots(&mut self) -> bool {
        let had_snapshots = self.catalogue.is_some() || !self.views.is_empty();
        if let Some(catalogue) = self.catalogue.as_ref() {
            catalogue.revoke();
        }
        for view in self.views.values() {
            view.revoke();
        }
        self.catalogue = None;
        self.views.clear();
        had_snapshots
    }

    fn active_state(&self) -> SourceState {
        if self.disconnect_retirement.is_some() {
            SourceState::Disconnecting
        } else if self.connect.is_some() {
            SourceState::Connecting
        } else if self.active.is_some() {
            if self.refreshes.is_empty() {
                SourceState::Ready
            } else {
                SourceState::Refreshing
            }
        } else if self.failure.is_some() {
            SourceState::Failed
        } else if self.state == SourceState::Retired {
            SourceState::Retired
        } else {
            SourceState::Dormant
        }
    }

    fn snapshot(&self) -> LifecycleSnapshot<S> {
        LifecycleSnapshot {
            revision: self.revision,
            state: self.state,
            session_epoch: self.session_epoch(),
            provenance: self.provenance.clone(),
            visibility: self.provenance.visibility(),
            retention: self.provenance.retention(),
            catalogue: self.catalogue.clone(),
            views: self.views.clone(),
            failure: self.failure,
            refresh_failures: self.refresh_failures.clone(),
            pending_connect: self.connect.as_ref().map(|operation| operation.generation),
            pending_refreshes: self
                .refreshes
                .iter()
                .map(|(lane, operation)| (lane.clone(), operation.generation))
                .collect(),
            pending_retirements: self.retirement_ids.len(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RetirementPurpose {
    Replacement,
    Disconnect,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetirementOutcome {
    Pending,
    Finished(Option<SourceFailure>),
}

/// Join-only capability for one exact disconnect settlement.
///
/// A settlement can combine an adopted-adapter retirement with every spawned
/// connect generation that was still capable of returning a late adapter when
/// disconnect began. It cannot start or complete close work, and clones or
/// repeated disconnect callers await the same registry-owned work. Sanitized
/// close failure from either the adopted adapter or a late rejected adapter is
/// returned after all joined work settles.
#[derive(Clone)]
pub struct RetirementWaiter {
    retirement_id: Option<u64>,
    outcome: watch::Receiver<RetirementOutcome>,
    settlements: Vec<Arc<ConnectSettlement>>,
}

impl RetirementWaiter {
    fn completed() -> Self {
        let (_sender, outcome) = watch::channel(RetirementOutcome::Finished(None));
        Self {
            retirement_id: None,
            outcome,
            settlements: Vec::new(),
        }
    }

    fn join_settlement(mut self, generation: u64, settlement: Arc<ConnectSettlement>) -> Self {
        if self.retirement_id.is_none() {
            self.retirement_id = Some(generation);
        }
        self.settlements.push(settlement);
        self
    }

    pub const fn retirement_id(&self) -> Option<u64> {
        self.retirement_id
    }

    pub fn is_complete(&self) -> bool {
        matches!(*self.outcome.borrow(), RetirementOutcome::Finished(_))
            && self
                .settlements
                .iter()
                .all(|settlement| settlement.active() == 0)
    }

    pub async fn wait(mut self) -> Option<SourceFailure> {
        let base_failure = loop {
            let outcome = *self.outcome.borrow_and_update();
            if let RetirementOutcome::Finished(failure) = outcome {
                break failure;
            }
            if self.outcome.changed().await.is_err() {
                break Some(SourceFailure::disconnect(FailureCategory::Backend));
            }
        };
        for settlement in &self.settlements {
            settlement.wait().await;
        }
        base_failure.or_else(|| {
            self.settlements
                .iter()
                .find_map(|settlement| settlement.close_failure())
        })
    }
}

struct RetirementRecord {
    source_id: SourceId,
    session_epoch: Option<u64>,
    purpose: RetirementPurpose,
    outcome: watch::Sender<RetirementOutcome>,
}

struct RegistryState<A: ?Sized, S> {
    gate: RegistryGate,
    revision: u64,
    next_generation: u64,
    next_session_epoch: u64,
    entries: HashMap<SourceId, Entry<A, S>>,
    retirements: HashMap<u64, RetirementRecord>,
}

impl<A: ?Sized, S> Default for RegistryState<A, S> {
    fn default() -> Self {
        Self {
            gate: RegistryGate::Running,
            revision: 0,
            next_generation: 0,
            next_session_epoch: 0,
            entries: HashMap::new(),
            retirements: HashMap::new(),
        }
    }
}

struct RegistryInner<A: LifecycleAdapter + ?Sized, S> {
    incarnation: Uuid,
    runtime: Handle,
    tracker: Arc<OperationTracker>,
    changes: broadcast::Sender<RevisionedLifecycleChange>,
    invalidations: watch::Sender<u64>,
    state: Mutex<RegistryState<A, S>>,
    external_handles: AtomicUsize,
}

/// Atomic adapter, lease, and epoch access for one adopted session.
pub struct SessionHandle<A: ?Sized> {
    session_epoch: u64,
    adapter: Arc<A>,
    lease: MediaLease,
}

impl<A: ?Sized> Clone for SessionHandle<A> {
    fn clone(&self) -> Self {
        Self {
            session_epoch: self.session_epoch,
            adapter: Arc::clone(&self.adapter),
            lease: self.lease.clone(),
        }
    }
}

impl<A: ?Sized> SessionHandle<A> {
    pub const fn session_epoch(&self) -> u64 {
        self.session_epoch
    }

    pub fn adapter(&self) -> Arc<A> {
        Arc::clone(&self.adapter)
    }

    pub fn lease(&self) -> MediaLease {
        self.lease.clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct RetirementJob<A: LifecycleAdapter + ?Sized, S> {
    inner: Arc<RegistryInner<A, S>>,
    retirement_id: u64,
    adapter: Option<ConstructedAdapter<A>>,
    participant: Option<OperationParticipant>,
    settlement_participant: Option<OperationParticipant>,
    settlement: Option<Arc<ConnectSettlement>>,
    completed: bool,
    waiter: RetirementWaiter,
}

impl<A: LifecycleAdapter + ?Sized, S> RetirementJob<A, S> {
    fn waiter(&self) -> RetirementWaiter {
        self.waiter.clone()
    }
}

impl<A: LifecycleAdapter + ?Sized, S> RetirementJob<A, S>
where
    S: Send + Sync + 'static,
{
    fn complete(&mut self, result: Result<(), FailureCategory>) {
        if self.completed {
            return;
        }
        self.completed = true;
        if let (Some(settlement), Err(category)) = (&self.settlement, result) {
            settlement.record_close_failure(category);
        }
        self.inner
            .finish_retirement(self.retirement_id, result, &mut self.participant);
        self.settlement_participant.take();
    }
}

impl<A: LifecycleAdapter + ?Sized, S> Drop for RetirementJob<A, S> {
    fn drop(&mut self) {
        if !self.completed {
            self.completed = true;
            if let Some(settlement) = &self.settlement {
                settlement.record_close_failure(FailureCategory::Backend);
            }
            self.inner.finish_retirement(
                self.retirement_id,
                Err(FailureCategory::Backend),
                &mut self.participant,
            );
            self.settlement_participant.take();
        }
    }
}

impl<A: LifecycleAdapter + ?Sized, S> RegistryInner<A, S> {
    /// Last external registry-handle safety net. This path intentionally does
    /// not publish or retain close results: no registry observer remains. It
    /// does synchronously close admission, cancel work, revoke every media
    /// lease, and transfer each active adapter to a tracked close task.
    fn fail_closed_last_handle(&self) {
        let mut retirements = Vec::new();
        {
            let mut state = lock(&self.state);
            if state.gate == RegistryGate::ShuttingDown {
                return;
            }
            state.gate = RegistryGate::ShuttingDown;
            for entry in state.entries.values_mut() {
                if let Some(connect) = entry.connect.take() {
                    Self::cancel_pending(connect);
                }
                for refresh in entry.refreshes.drain().map(|(_, operation)| operation) {
                    Self::cancel_pending(refresh);
                }
                if let Some(active) = entry.active.take() {
                    active.lease.revoke();
                    retirements.push((active.adapter, self.tracker.participate()));
                }
                entry.revoke_snapshots();
                entry.failure = None;
                entry.refresh_failures.clear();
                entry.disconnect_retirement = None;
                entry.disconnect_waiter = None;
                entry.connect_settlements.clear();
                entry.state = SourceState::Retired;
            }
        }
        for (constructed, participant) in retirements {
            let adapter = constructed.into_operational();
            let close = std::panic::catch_unwind(AssertUnwindSafe(|| {
                adapter.close(CloseAuthority { _private: () })
            }));
            match close {
                Ok(close) => {
                    self.runtime.spawn(async move {
                        let _participant = participant;
                        let _ = AssertUnwindSafe(close).catch_unwind().await;
                    });
                }
                Err(_) => drop(participant),
            }
        }
    }

    fn next_generation(state: &mut RegistryState<A, S>) -> u64 {
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .expect("source lifecycle operation generation exhausted");
        state.next_generation
    }

    fn next_session_epoch(state: &mut RegistryState<A, S>) -> u64 {
        state.next_session_epoch = state
            .next_session_epoch
            .checked_add(1)
            .expect("source lifecycle session epoch exhausted");
        state.next_session_epoch
    }

    fn publish_locked(
        &self,
        state: &mut RegistryState<A, S>,
        source_id: SourceId,
        change: LifecycleChange,
    ) {
        state.revision = state
            .revision
            .checked_add(1)
            .expect("source lifecycle revision exhausted");
        self.invalidations.send_replace(state.revision);
        if let Some(entry) = state.entries.get_mut(&source_id) {
            entry.revision = state.revision;
        }
        let _ = self.changes.send(RevisionedLifecycleChange {
            revision: state.revision,
            source_id,
            change,
        });
    }

    fn source_is_prunable(state: &RegistryState<A, S>, source_id: SourceId) -> bool {
        state.entries.get(&source_id).is_some_and(|entry| {
            entry.state == SourceState::Retired
                && entry.provenance.is_empty()
                && entry.active.is_none()
                && entry.connect.is_none()
                && entry.refreshes.is_empty()
                && entry.disconnect_retirement.is_none()
                && entry
                    .disconnect_waiter
                    .as_ref()
                    .is_none_or(RetirementWaiter::is_complete)
                && entry
                    .connect_settlements
                    .values()
                    .all(|settlement| settlement.active() == 0)
                && entry.retirement_ids.is_empty()
        })
    }

    fn prune_source_locked(&self, state: &mut RegistryState<A, S>, source_id: SourceId) -> bool {
        if !Self::source_is_prunable(state, source_id) {
            return false;
        }
        self.publish_locked(state, source_id, LifecycleChange::Pruned);
        if let Some(entry) = state.entries.get_mut(&source_id) {
            entry.revoke_snapshots();
        }
        state.entries.remove(&source_id);
        true
    }

    fn transition_locked(
        &self,
        state: &mut RegistryState<A, S>,
        source_id: SourceId,
        next: SourceState,
    ) {
        let Some((previous, session_epoch)) = state
            .entries
            .get(&source_id)
            .map(|entry| (entry.state, entry.session_epoch()))
        else {
            return;
        };
        if previous == next {
            return;
        }
        state
            .entries
            .get_mut(&source_id)
            .expect("entry checked above")
            .state = next;
        self.publish_locked(
            state,
            source_id,
            LifecycleChange::StateChanged {
                from: previous,
                to: next,
                session_epoch,
            },
        );
    }

    fn set_session_failure_locked(
        &self,
        state: &mut RegistryState<A, S>,
        source_id: SourceId,
        failure: Option<SourceFailure>,
        correlation: OperationCorrelation,
    ) {
        let Some(previous) = state.entries.get(&source_id).map(|entry| entry.failure) else {
            return;
        };
        let retained = failure.map(|failure| CorrelatedFailure {
            correlation,
            failure,
        });
        if previous == retained {
            return;
        }
        state
            .entries
            .get_mut(&source_id)
            .expect("entry checked above")
            .failure = retained;
        self.publish_locked(
            state,
            source_id,
            LifecycleChange::FailureChanged {
                lane: FailureLane::Session,
                correlation,
                failure,
            },
        );
    }

    fn set_refresh_failure_locked(
        &self,
        state: &mut RegistryState<A, S>,
        source_id: SourceId,
        lane: RefreshLane,
        failure: Option<SourceFailure>,
        correlation: OperationCorrelation,
    ) {
        let Some(entry) = state.entries.get_mut(&source_id) else {
            return;
        };
        let previous = entry.refresh_failures.get(&lane).copied();
        let retained = failure.map(|failure| CorrelatedFailure {
            correlation,
            failure,
        });
        if previous == retained {
            return;
        }
        match retained {
            Some(value) => {
                entry.refresh_failures.insert(lane.clone(), value);
            }
            None => {
                entry.refresh_failures.remove(&lane);
            }
        }
        self.publish_locked(
            state,
            source_id,
            LifecycleChange::FailureChanged {
                lane: FailureLane::Refresh(lane),
                correlation,
                failure,
            },
        );
    }

    fn cancel_pending(operation: PendingOperation) {
        operation.cancellation.cancel();
        if operation.abortable {
            if let Some(abort) = operation.abort {
                abort.abort();
            }
        }
    }

    fn prepare_retirement_locked(
        self: &Arc<Self>,
        state: &mut RegistryState<A, S>,
        source_id: SourceId,
        session: Option<ActiveSession<A>>,
        rejected_adapter: Option<ConstructedAdapter<A>>,
        purpose: RetirementPurpose,
        associate_with_entry: bool,
    ) -> RetirementJob<A, S> {
        let (adapter, session_epoch) = match (session, rejected_adapter) {
            (Some(session), None) => {
                session.lease.revoke();
                (session.adapter, Some(session.epoch))
            }
            (None, Some(adapter)) => (adapter, None),
            _ => unreachable!("one retirement adapter owner is required"),
        };
        let retirement_id = Self::next_generation(state);
        let (outcome, outcome_receiver) = watch::channel(RetirementOutcome::Pending);
        state.retirements.insert(
            retirement_id,
            RetirementRecord {
                source_id,
                session_epoch,
                purpose,
                outcome,
            },
        );
        if associate_with_entry {
            if let Some(entry) = state.entries.get_mut(&source_id) {
                entry.retirement_ids.insert(retirement_id);
                if purpose == RetirementPurpose::Disconnect {
                    entry.disconnect_retirement = Some(retirement_id);
                }
            }
        }
        RetirementJob {
            inner: Arc::clone(self),
            retirement_id,
            adapter: Some(adapter),
            participant: Some(self.tracker.participate()),
            completed: false,
            waiter: RetirementWaiter {
                retirement_id: Some(retirement_id),
                outcome: outcome_receiver,
                settlements: Vec::new(),
            },
            settlement_participant: None,
            settlement: None,
        }
    }

    fn retirement_waiter_locked(
        state: &RegistryState<A, S>,
        retirement_id: u64,
    ) -> Option<RetirementWaiter> {
        state
            .retirements
            .get(&retirement_id)
            .map(|record| RetirementWaiter {
                retirement_id: Some(retirement_id),
                outcome: record.outcome.subscribe(),
                settlements: Vec::new(),
            })
    }

    fn spawn_retirement(self: &Arc<Self>, mut job: RetirementJob<A, S>)
    where
        S: Send + Sync + 'static,
    {
        let adapter = job
            .adapter
            .take()
            .expect("retirement adapter owner is consumed once")
            .into_operational();
        let close = std::panic::catch_unwind(AssertUnwindSafe(|| {
            adapter.close(CloseAuthority { _private: () })
        }));
        match close {
            Ok(close) => {
                self.runtime.spawn(async move {
                    let result = AssertUnwindSafe(close)
                        .catch_unwind()
                        .await
                        .unwrap_or(Err(FailureCategory::Backend));
                    job.complete(result);
                });
            }
            Err(_) => job.complete(Err(FailureCategory::Backend)),
        }
    }

    fn finish_retirement(
        &self,
        retirement_id: u64,
        result: Result<(), FailureCategory>,
        participant: &mut Option<OperationParticipant>,
    ) {
        let mut state = lock(&self.state);
        let Some(record) = state.retirements.remove(&retirement_id) else {
            return;
        };
        let failure = result.err().map(SourceFailure::disconnect);
        let (removed_association, foreground_disconnect) = state
            .entries
            .get_mut(&record.source_id)
            .map_or((false, false), |entry| {
                let removed = entry.retirement_ids.remove(&retirement_id);
                let foreground = removed
                    && record.purpose == RetirementPurpose::Disconnect
                    && entry.disconnect_retirement == Some(retirement_id);
                if foreground {
                    entry.disconnect_retirement = None;
                }
                (removed, foreground)
            });
        if foreground_disconnect {
            self.set_session_failure_locked(
                &mut state,
                record.source_id,
                failure,
                OperationCorrelation {
                    generation: retirement_id,
                    session_epoch: record.session_epoch,
                },
            );
        }
        if removed_association {
            if let Some(session_epoch) = record.session_epoch {
                self.publish_locked(
                    &mut state,
                    record.source_id,
                    LifecycleChange::SessionRetired {
                        session_epoch,
                        failure,
                    },
                );
            }
        }
        if foreground_disconnect {
            let next = if state.gate == RegistryGate::ShuttingDown {
                SourceState::Retired
            } else {
                let entry = state
                    .entries
                    .get(&record.source_id)
                    .expect("foreground retirement entry retained");
                if entry.provenance.is_empty() {
                    SourceState::Retired
                } else {
                    SourceState::Dormant
                }
            };
            self.transition_locked(&mut state, record.source_id, next);
        }
        // End shutdown-barrier participation only after all entry bookkeeping,
        // typed events, and the terminal state transition are visible, then
        // publish waiter completion. A waiter waking on another runtime thread
        // may therefore immediately snapshot the finalized row and observe a
        // complete shutdown barrier.
        participant.take();
        record
            .outcome
            .send_replace(RetirementOutcome::Finished(failure));
    }

    fn register_connect_task(
        &self,
        source_id: SourceId,
        generation: u64,
        policy: ConstructionCancellationPolicy,
    ) -> Option<(OperationParticipant, OperationParticipant)> {
        let mut state = lock(&self.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let operation = state
            .entries
            .get_mut(&source_id)
            .and_then(|entry| entry.connect.as_mut())
            .filter(|operation| operation.generation == generation)?;
        operation.abortable = policy.abortable();
        let settlement = Arc::clone(
            operation
                .settlement
                .as_ref()
                .expect("connect operation owns settlement tracker"),
        );
        Some((self.tracker.participate(), settlement.participate()))
    }

    fn register_refresh_task(
        &self,
        source_id: SourceId,
        lane: &RefreshLane,
        generation: u64,
    ) -> Option<OperationParticipant> {
        let state = lock(&self.state);
        if state.gate != RegistryGate::Running
            || !state.entries.get(&source_id).is_some_and(|entry| {
                entry
                    .refreshes
                    .get(lane)
                    .is_some_and(|operation| operation.generation == generation)
            })
        {
            return None;
        }
        Some(self.tracker.participate())
    }

    fn make_connect_abortable(&self, source_id: SourceId, generation: u64) -> bool {
        let mut state = lock(&self.state);
        let Some(operation) = state
            .entries
            .get_mut(&source_id)
            .and_then(|entry| entry.connect.as_mut())
            .filter(|operation| operation.generation == generation)
        else {
            return false;
        };
        operation.abortable = true;
        true
    }

    fn attach_connect_abort(
        &self,
        source_id: SourceId,
        generation: u64,
        abort: AbortHandle,
        detached_abortable: bool,
    ) {
        let mut state = lock(&self.state);
        let attached = state
            .entries
            .get_mut(&source_id)
            .and_then(|entry| entry.connect.as_mut())
            .filter(|operation| operation.generation == generation)
            .is_some_and(|operation| {
                operation.abort = Some(abort.clone());
                true
            });
        if !attached && detached_abortable {
            abort.abort();
        }
    }

    fn attach_refresh_abort(
        &self,
        source_id: SourceId,
        lane: &RefreshLane,
        generation: u64,
        abort: AbortHandle,
    ) {
        let mut state = lock(&self.state);
        let attached = state
            .entries
            .get_mut(&source_id)
            .and_then(|entry| entry.refreshes.get_mut(lane))
            .filter(|operation| operation.generation == generation)
            .is_some_and(|operation| {
                operation.abort = Some(abort.clone());
                true
            });
        if !attached {
            abort.abort();
        }
    }

    fn abandon_connect(&self, incarnation: Uuid, source_id: SourceId, generation: u64) -> bool {
        if incarnation != self.incarnation {
            return false;
        }
        let mut state = lock(&self.state);
        let Some(operation) = state.entries.get_mut(&source_id).and_then(|entry| {
            (entry.connect.as_ref().map(|value| value.generation) == Some(generation))
                .then(|| entry.connect.take())
                .flatten()
        }) else {
            return false;
        };
        Self::cancel_pending(operation);
        self.publish_locked(
            &mut state,
            source_id,
            LifecycleChange::ConnectCancelled { generation },
        );
        let next = state
            .entries
            .get(&source_id)
            .map_or(SourceState::Retired, Entry::active_state);
        self.transition_locked(&mut state, source_id, next);
        true
    }

    fn finish_connect_failure(
        &self,
        incarnation: Uuid,
        source_id: SourceId,
        generation: u64,
        category: FailureCategory,
    ) -> bool {
        if incarnation != self.incarnation {
            return false;
        }
        let mut state = lock(&self.state);
        let current = state.gate == RegistryGate::Running
            && state.entries.get(&source_id).is_some_and(|entry| {
                entry
                    .connect
                    .as_ref()
                    .is_some_and(|operation| operation.generation == generation)
            });
        if !current {
            return false;
        }
        state
            .entries
            .get_mut(&source_id)
            .expect("current source exists")
            .connect = None;
        let session_epoch = state.entries.get(&source_id).and_then(Entry::session_epoch);
        self.set_session_failure_locked(
            &mut state,
            source_id,
            Some(SourceFailure::connect(category)),
            OperationCorrelation {
                generation,
                session_epoch,
            },
        );
        let next = if state
            .entries
            .get(&source_id)
            .is_some_and(|entry| entry.active.is_some())
        {
            state
                .entries
                .get(&source_id)
                .map_or(SourceState::Ready, Entry::active_state)
        } else {
            SourceState::Failed
        };
        self.transition_locked(&mut state, source_id, next);
        true
    }

    fn refresh_is_current(
        &self,
        state: &RegistryState<A, S>,
        incarnation: Uuid,
        source_id: SourceId,
        lane: &RefreshLane,
        generation: u64,
        session_epoch: u64,
    ) -> bool {
        incarnation == self.incarnation
            && state.gate == RegistryGate::Running
            && state.entries.get(&source_id).is_some_and(|entry| {
                entry.active.as_ref().map(|active| active.epoch) == Some(session_epoch)
                    && entry.refreshes.get(lane).is_some_and(|operation| {
                        operation.generation == generation
                            && operation.session_epoch == Some(session_epoch)
                    })
            })
    }

    fn finish_refresh_success(
        &self,
        incarnation: Uuid,
        source_id: SourceId,
        lane: &RefreshLane,
        generation: u64,
        session_epoch: u64,
        snapshot: S,
    ) -> bool {
        let mut state = lock(&self.state);
        if !self.refresh_is_current(
            &state,
            incarnation,
            source_id,
            lane,
            generation,
            session_epoch,
        ) {
            return false;
        }
        let accepted = AcceptedSnapshot::new(generation, session_epoch, snapshot);
        let entry = state
            .entries
            .get_mut(&source_id)
            .expect("current source exists");
        entry.refreshes.remove(lane);
        match lane {
            RefreshLane::Catalogue => entry.replace_catalogue(accepted),
            RefreshLane::View(view) => {
                entry.replace_view(view.clone(), accepted);
            }
        }
        self.set_refresh_failure_locked(
            &mut state,
            source_id,
            lane.clone(),
            None,
            OperationCorrelation {
                generation,
                session_epoch: Some(session_epoch),
            },
        );
        let change = match lane {
            RefreshLane::Catalogue => LifecycleChange::CatalogueAccepted {
                generation,
                session_epoch,
            },
            RefreshLane::View(view) => LifecycleChange::ViewAccepted {
                view: view.clone(),
                generation,
                session_epoch,
            },
        };
        self.publish_locked(&mut state, source_id, change);
        let next = state
            .entries
            .get(&source_id)
            .map_or(SourceState::Retired, Entry::active_state);
        self.transition_locked(&mut state, source_id, next);
        true
    }

    fn finish_refresh_failure(
        &self,
        incarnation: Uuid,
        source_id: SourceId,
        lane: &RefreshLane,
        generation: u64,
        session_epoch: u64,
        category: FailureCategory,
    ) -> bool {
        let mut state = lock(&self.state);
        if !self.refresh_is_current(
            &state,
            incarnation,
            source_id,
            lane,
            generation,
            session_epoch,
        ) {
            return false;
        }
        state
            .entries
            .get_mut(&source_id)
            .expect("current source exists")
            .refreshes
            .remove(lane);
        self.set_refresh_failure_locked(
            &mut state,
            source_id,
            lane.clone(),
            Some(SourceFailure::refresh(category)),
            OperationCorrelation {
                generation,
                session_epoch: Some(session_epoch),
            },
        );
        let next = state
            .entries
            .get(&source_id)
            .map_or(SourceState::Retired, Entry::active_state);
        self.transition_locked(&mut state, source_id, next);
        true
    }

    fn abandon_refresh(
        &self,
        incarnation: Uuid,
        source_id: SourceId,
        lane: &RefreshLane,
        generation: u64,
        session_epoch: u64,
    ) -> bool {
        if incarnation != self.incarnation {
            return false;
        }
        let mut state = lock(&self.state);
        let current = state.entries.get(&source_id).is_some_and(|entry| {
            entry.active.as_ref().map(|active| active.epoch) == Some(session_epoch)
                && entry.refreshes.get(lane).is_some_and(|operation| {
                    operation.generation == generation
                        && operation.session_epoch == Some(session_epoch)
                })
        });
        if !current {
            return false;
        }
        let operation = state
            .entries
            .get_mut(&source_id)
            .expect("current source exists")
            .refreshes
            .remove(lane)
            .expect("current refresh exists");
        Self::cancel_pending(operation);
        self.publish_locked(
            &mut state,
            source_id,
            LifecycleChange::OperationCancelled {
                lane: lane.clone(),
                generation,
            },
        );
        let next = state
            .entries
            .get(&source_id)
            .map_or(SourceState::Retired, Entry::active_state);
        self.transition_locked(&mut state, source_id, next);
        true
    }
}

/// Centralized lifecycle authority for every typed `SourceId`.
pub struct SourceLifecycleRegistry<A: LifecycleAdapter + ?Sized, S> {
    inner: Arc<RegistryInner<A, S>>,
}

impl<A: LifecycleAdapter + ?Sized, S> Clone for SourceLifecycleRegistry<A, S> {
    fn clone(&self) -> Self {
        self.inner.external_handles.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<A: LifecycleAdapter + ?Sized, S> Drop for SourceLifecycleRegistry<A, S> {
    fn drop(&mut self) {
        if self.inner.external_handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.fail_closed_last_handle();
        }
    }
}

impl<A: LifecycleAdapter + ?Sized, S> SourceLifecycleRegistry<A, S> {
    /// Construct a registry whose operation and retirement tasks run on the
    /// supplied process-owned runtime.
    pub fn new(runtime: Handle) -> Self {
        let (changes, _receiver) = broadcast::channel(256);
        let (invalidations, _receiver) = watch::channel(0);
        Self {
            inner: Arc::new(RegistryInner {
                incarnation: Uuid::new_v4(),
                runtime,
                tracker: OperationTracker::new(),
                changes,
                invalidations,
                state: Mutex::new(RegistryState::default()),
                external_handles: AtomicUsize::new(1),
            }),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RevisionedLifecycleChange> {
        self.inner.changes.subscribe()
    }

    /// Subscribe to the registry-wide invalidation revision. Unlike the
    /// source-scoped diagnostic event stream, this also advances when the
    /// global admission gate closes with no live source rows.
    pub fn subscribe_invalidations(&self) -> watch::Receiver<u64> {
        self.inner.invalidations.subscribe()
    }

    pub fn revision(&self) -> u64 {
        lock(&self.inner.state).revision
    }

    pub fn is_shutting_down(&self) -> bool {
        lock(&self.inner.state).gate == RegistryGate::ShuttingDown
    }

    /// Add one independent reason for a source to exist. Re-announcing an
    /// ephemeral source reactivates a retired entry without adopting an old
    /// session or locator.
    pub fn claim_provenance(
        &self,
        source_id: SourceId,
        contribution: SourceProvenance,
    ) -> Option<ProvenanceClaimId> {
        let mut state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let claim_id = loop {
            let candidate = ProvenanceClaimId::random();
            let collision = state
                .entries
                .get(&source_id)
                .is_some_and(|entry| entry.provenance_claims.contains_key(&candidate));
            if !collision {
                break candidate;
            }
        };
        let mut reactivate = false;
        match state.entries.entry(source_id) {
            std::collections::hash_map::Entry::Occupied(mut occupied) => {
                let entry = occupied.get_mut();
                entry.provenance_claims.insert(claim_id, contribution);
                let claim_count = entry.provenance.0.entry(contribution).or_default();
                *claim_count = claim_count
                    .checked_add(1)
                    .expect("source provenance claim count exhausted");
                reactivate = matches!(
                    entry.state,
                    SourceState::Retired | SourceState::Disconnecting
                );
                if reactivate {
                    // A new publisher owns the logical source immediately.
                    // The old close remains tracked by retirement_ids and its
                    // waiter, but may no longer transition or annotate this
                    // reappeared incarnation.
                    entry.disconnect_retirement = None;
                    entry.disconnect_waiter = None;
                }
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(Entry::new(claim_id, contribution));
            }
        }
        let provenance = state
            .entries
            .get(&source_id)
            .expect("entry inserted")
            .provenance
            .clone();
        self.inner.publish_locked(
            &mut state,
            source_id,
            LifecycleChange::ProvenanceChanged {
                contributions: provenance.clone(),
                visibility: provenance.visibility(),
                retention: provenance.retention(),
            },
        );
        if reactivate {
            self.inner
                .transition_locked(&mut state, source_id, SourceState::Dormant);
        }
        Some(claim_id)
    }

    /// Remove one independent provenance contribution. Other contributions
    /// continue to own visibility and retention; removing Saved from a still
    /// discovered source therefore demotes rather than disconnects it.
    pub fn release_provenance(&self, source_id: SourceId, claim_id: ProvenanceClaimId) -> bool
    where
        S: Send + Sync + 'static,
    {
        let mut jobs = Vec::new();
        let mut state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return false;
        }
        let Some(entry) = state.entries.get_mut(&source_id) else {
            return false;
        };
        let Some(contribution) = entry.provenance_claims.remove(&claim_id) else {
            return false;
        };
        let claim_count = entry
            .provenance
            .0
            .get_mut(&contribution)
            .expect("claim kind exists");
        *claim_count = claim_count
            .checked_sub(1)
            .expect("source provenance claim count underflow");
        if *claim_count == 0 {
            entry.provenance.0.remove(&contribution);
        }
        let provenance = entry.provenance.clone();
        self.inner.publish_locked(
            &mut state,
            source_id,
            LifecycleChange::ProvenanceChanged {
                visibility: provenance.visibility(),
                retention: provenance.retention(),
                contributions: provenance.clone(),
            },
        );
        if provenance.is_empty() {
            jobs.extend(self.disconnect_locked(&mut state, source_id, true));
        }
        drop(state);
        for job in jobs {
            self.inner.spawn_retirement(job);
        }
        true
    }

    pub fn snapshot(&self, source_id: SourceId) -> Option<LifecycleSnapshot<S>> {
        let state = lock(&self.inner.state);
        state.entries.get(&source_id).map(Entry::snapshot)
    }

    /// Test-only proof that another thread currently owns the lifecycle state
    /// lock. This avoids timing-based assertions when a test callback is
    /// deliberately holding an exact admission transaction open.
    #[cfg(test)]
    pub(crate) fn state_lock_is_contended_for_test(&self) -> bool {
        matches!(
            self.inner.state.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }

    /// Read only the current adopted session epoch without cloning catalogue
    /// or view snapshot handles.
    pub fn active_session_epoch(&self, source_id: SourceId) -> Option<u64> {
        let state = lock(&self.inner.state);
        (state.gate == RegistryGate::Running)
            .then(|| state.entries.get(&source_id).and_then(Entry::session_epoch))
            .flatten()
    }

    /// Project a bounded property from the exact active adapter while the
    /// lifecycle gate and session lease are observed under one lock.
    ///
    /// The adapter and lease never leave the registry. Callers should use
    /// this only for cheap, synchronous capability inspection; operations
    /// which can block belong on the exact-session async boundary instead.
    pub(crate) fn inspect_active_session<T>(
        &self,
        source_id: SourceId,
        inspect: impl FnOnce(&A) -> T,
    ) -> Option<(u64, T)> {
        let state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let active = state.entries.get(&source_id)?.active.as_ref()?;
        if !active.lease.is_active() {
            return None;
        }
        Some((active.epoch, inspect(active.adapter.adapter.as_ref())))
    }

    /// Perform one bounded synchronous admission through an exact live
    /// session.
    ///
    /// The eligibility predicate and admitted action both execute while the
    /// lifecycle state lock is retained. Disconnect, replacement, shutdown,
    /// and provenance changes therefore linearize entirely before or after
    /// the action instead of slipping between a snapshot check and its use.
    /// The action receives no adapter, lease, or permit, so no lifecycle
    /// authority can escape into a future or network operation.
    ///
    /// Both callbacks must be non-blocking and must not re-enter this
    /// lifecycle registry. The admitted action should do no more than mutate
    /// bounded in-memory ownership or use a non-blocking `try_*` ingress.
    pub(crate) fn admit_exact_session_if<T, Eligible, Admit>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        eligible: Eligible,
        admit: Admit,
    ) -> Option<T>
    where
        Eligible: FnOnce(&A, &ProvenanceSet) -> bool,
        Admit: FnOnce() -> T,
    {
        let state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let entry = state.entries.get(&source_id)?;
        let active = entry.active.as_ref()?;
        if active.epoch != expected_session_epoch || !active.lease.is_active() {
            return None;
        }
        if !eligible(active.adapter.adapter.as_ref(), &entry.provenance) {
            return None;
        }

        // Keep `state` live through admission. Returning a value is allowed,
        // but it cannot contain source authority because the callback never
        // receives any.
        Some(admit())
    }

    /// Project one bounded, non-authoritative value from an exact live
    /// session while its adapter, provenance, epoch, lease, and registry gate
    /// are observed under the lifecycle state lock.
    ///
    /// This is the minting counterpart of [`Self::admit_exact_session_if`].
    /// The callback must be non-blocking and non-reentrant, and its result
    /// must not retain the adapter, lease, locator, credential, or permit.
    pub(crate) fn project_exact_session<T, Project>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        project: Project,
    ) -> Option<T>
    where
        Project: FnOnce(&A, &ProvenanceSet) -> Option<T>,
    {
        let state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let entry = state.entries.get(&source_id)?;
        let active = entry.active.as_ref()?;
        if active.epoch != expected_session_epoch || !active.lease.is_active() {
            return None;
        }
        project(active.adapter.adapter.as_ref(), &entry.provenance)
    }

    /// Perform one bounded synchronous admission through an exact live
    /// session and accepted catalogue.
    ///
    /// This is the catalogue-scoped counterpart of
    /// [`Self::admit_exact_session_if`]. Exact epoch/generation validation,
    /// eligibility and catalogue membership inspection, and the admitted
    /// action form one state-lock transaction. No session or catalogue permit
    /// is returned or exposed.
    pub(crate) fn admit_exact_catalogue_if<T, Eligible, Admit>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        expected_catalogue_generation: u64,
        eligible: Eligible,
        admit: Admit,
    ) -> Option<T>
    where
        Eligible: FnOnce(&A, &ProvenanceSet, &S) -> bool,
        Admit: FnOnce() -> T,
    {
        let state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let entry = state.entries.get(&source_id)?;
        let active = entry.active.as_ref()?;
        let catalogue = entry.catalogue.as_ref()?;
        if active.epoch != expected_session_epoch
            || catalogue.session_epoch != expected_session_epoch
            || catalogue.generation != expected_catalogue_generation
            || !active.lease.is_active()
            || !catalogue.authority.is_active()
        {
            return None;
        }
        if !eligible(
            active.adapter.adapter.as_ref(),
            &entry.provenance,
            catalogue.value.as_ref(),
        ) {
            return None;
        }

        Some(admit())
    }

    /// Project one bounded, non-authoritative value from an exact live
    /// session and accepted catalogue under the lifecycle state lock.
    ///
    /// Exact epoch/generation checks, adapter/profile inspection, provenance,
    /// and catalogue membership selection therefore form one atomic minting
    /// observation. The callback follows the same non-blocking,
    /// non-reentrant contract as [`Self::project_exact_session`].
    pub(crate) fn project_exact_catalogue<T, Project>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        expected_catalogue_generation: u64,
        project: Project,
    ) -> Option<T>
    where
        Project: FnOnce(&A, &ProvenanceSet, &S) -> Option<T>,
    {
        let state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let entry = state.entries.get(&source_id)?;
        let active = entry.active.as_ref()?;
        let catalogue = entry.catalogue.as_ref()?;
        if active.epoch != expected_session_epoch
            || catalogue.session_epoch != expected_session_epoch
            || catalogue.generation != expected_catalogue_generation
            || !active.lease.is_active()
            || !catalogue.authority.is_active()
        {
            return None;
        }
        project(
            active.adapter.adapter.as_ref(),
            &entry.provenance,
            catalogue.value.as_ref(),
        )
    }

    /// Select a contribution from the greatest-generation accepted view.
    ///
    /// The selector executes while the lifecycle state is locked and must be
    /// a bounded, non-blocking projection over registry-owned immutable data.
    /// This keeps the chosen value, generation, session epoch, and active
    /// lease in one atomic observation.
    pub(crate) fn resolve_latest_accepted_view<T, Select>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        mut select: Select,
    ) -> Option<LatestAcceptedView<T>>
    where
        Select: FnMut(&S) -> Option<T>,
    {
        let state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let entry = state.entries.get(&source_id)?;
        let active = entry.active.as_ref()?;
        if active.epoch != expected_session_epoch || !active.lease.is_active() {
            return None;
        }

        entry
            .views
            .values()
            .filter(|accepted| {
                accepted.session_epoch == expected_session_epoch && accepted.authority.is_active()
            })
            .filter_map(|accepted| {
                select(accepted.value.as_ref()).map(|value| LatestAcceptedView {
                    generation: accepted.generation,
                    session_epoch: accepted.session_epoch,
                    authority: accepted.authority.clone(),
                    value,
                })
            })
            .max_by_key(|accepted| accepted.generation)
    }

    /// Recheck the exact newest accepted view generation at final use.
    pub(crate) fn is_current_latest_accepted_view<Select>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        winner_generation: u64,
        select: Select,
    ) -> bool
    where
        Select: FnMut(&S) -> Option<()>,
    {
        self.resolve_latest_accepted_view(source_id, expected_session_epoch, select)
            .is_some_and(|accepted| accepted.generation == winner_generation)
    }

    /// Select a bounded value from the currently accepted source-wide
    /// catalogue and its exact active session in one locked observation.
    ///
    /// This deliberately does not inspect [`SourceState`]. A predecessor
    /// catalogue remains authoritative while a replacement is connecting or
    /// after that replacement fails. Conversely, a catalogue without its
    /// exact active, unrevoked session is never selectable.
    pub(crate) fn resolve_current_accepted_catalogue<T, Select>(
        &self,
        source_id: SourceId,
        select: Select,
    ) -> Option<CurrentAcceptedCatalogue<T>>
    where
        Select: FnOnce(&S) -> Option<T>,
    {
        let (generation, session_epoch, session_lease, authority, catalogue_value) = {
            let state = lock(&self.inner.state);
            if state.gate != RegistryGate::Running {
                return None;
            }
            let entry = state.entries.get(&source_id)?;
            let active = entry.active.as_ref()?;
            if !active.lease.is_active() {
                return None;
            }
            let catalogue = entry.catalogue.as_ref()?;
            if catalogue.session_epoch != active.epoch || !catalogue.authority.is_active() {
                return None;
            }
            (
                catalogue.generation,
                catalogue.session_epoch,
                active.lease.clone(),
                catalogue.authority.clone(),
                Arc::clone(&catalogue.value),
            )
        };
        let value = select(catalogue_value.as_ref())?;
        if !session_lease.is_active()
            || !authority.is_active()
            || !self.is_exact_catalogue_snapshot_current(
                source_id,
                generation,
                session_epoch,
                &catalogue_value,
            )
        {
            return None;
        }
        Some(CurrentAcceptedCatalogue {
            generation,
            session_epoch,
            authority,
            value,
        })
    }

    /// Select from one exact accepted catalogue identity.
    pub(crate) fn resolve_exact_accepted_catalogue<T, Select>(
        &self,
        source_id: SourceId,
        expected_generation: u64,
        expected_session_epoch: u64,
        select: Select,
    ) -> Option<T>
    where
        Select: FnOnce(&S) -> Option<T>,
    {
        let (session_lease, authority, catalogue_value) = {
            let state = lock(&self.inner.state);
            if state.gate != RegistryGate::Running {
                return None;
            }
            let entry = state.entries.get(&source_id)?;
            let active = entry.active.as_ref()?;
            let catalogue = entry.catalogue.as_ref()?;
            if !active.lease.is_active()
                || active.epoch != expected_session_epoch
                || catalogue.session_epoch != expected_session_epoch
                || catalogue.generation != expected_generation
                || !catalogue.authority.is_active()
            {
                return None;
            }
            (
                active.lease.clone(),
                catalogue.authority.clone(),
                Arc::clone(&catalogue.value),
            )
        };
        let selected = select(catalogue_value.as_ref())?;
        (session_lease.is_active()
            && authority.is_active()
            && self.is_exact_catalogue_snapshot_current(
                source_id,
                expected_generation,
                expected_session_epoch,
                &catalogue_value,
            ))
        .then_some(selected)
    }

    /// Atomically admit one commit section over exact catalogue members.
    ///
    /// All unique catalogue guards are validated and acquire both their
    /// session and catalogue permits while the lifecycle state lock remains
    /// held. Immutable payload snapshots for all unique selected keys are
    /// cloned in that same observation, then the bounded membership selector
    /// runs after the lock is released. The permits keep those snapshots
    /// authoritative throughout that unlocked validation.
    ///
    /// Replacement and disconnect also revoke under the state lock, so they
    /// can neither slip between validation and acquisition nor publish an
    /// invalidation until the returned authority is dropped. There is
    /// intentionally no registry-lock recheck after permits are acquired: an
    /// invalidator may already be waiting for those permits while holding the
    /// state lock.
    pub(crate) fn acquire_catalogue_commit_authority<K, Validate>(
        &self,
        requests: &[CatalogueCommitRequest<K>],
        validate: Validate,
    ) -> Option<CatalogueCommitAuthority>
    where
        K: Eq + Hash,
        Validate: Fn(&S, &K) -> bool,
    {
        let (authority, selections) = {
            let state = lock(&self.inner.state);
            if state.gate != RegistryGate::Running {
                return None;
            }

            let mut unique_tracks = HashSet::with_capacity(requests.len());
            let mut unique_guards = HashSet::with_capacity(requests.len());
            let mut leases = Vec::with_capacity(requests.len());
            let mut selections = Vec::with_capacity(requests.len());
            for request in requests {
                let track_key = (
                    request.source_id,
                    request.session_epoch,
                    request.catalogue_generation,
                    &request.selected,
                );
                if !unique_tracks.insert(track_key) {
                    continue;
                }

                let entry = state.entries.get(&request.source_id)?;
                let active = entry.active.as_ref()?;
                let catalogue = entry.catalogue.as_ref()?;
                if active.epoch != request.session_epoch {
                    return None;
                }
                if catalogue.session_epoch != request.session_epoch {
                    return None;
                }
                if catalogue.generation != request.catalogue_generation {
                    return None;
                }
                if !active.lease.is_active() || !catalogue.authority.is_active() {
                    return None;
                }
                selections.push((Arc::clone(&catalogue.value), &request.selected));

                let guard = (
                    request.source_id,
                    request.session_epoch,
                    request.catalogue_generation,
                );
                if unique_guards.insert(guard) {
                    leases.push((active.lease.clone(), catalogue.authority.clone()));
                }
            }

            let mut permits = Vec::with_capacity(leases.len().saturating_mul(2));
            for (session, catalogue) in leases {
                permits.push(session.try_acquire()?);
                permits.push(catalogue.try_acquire()?);
            }
            (CatalogueCommitAuthority { permits }, selections)
        };

        selections
            .into_iter()
            .all(|(catalogue, selected)| validate(catalogue.as_ref(), selected))
            .then_some(authority)
    }

    /// Recheck the exact immutable catalogue captured before an unlocked
    /// selector ran. Generation and epoch are necessary but the pointer check
    /// also proves that no replacement reused the expected metadata.
    fn is_exact_catalogue_snapshot_current(
        &self,
        source_id: SourceId,
        expected_generation: u64,
        expected_session_epoch: u64,
        expected_value: &Arc<S>,
    ) -> bool {
        let state = lock(&self.inner.state);
        state.gate == RegistryGate::Running
            && state.entries.get(&source_id).is_some_and(|entry| {
                entry.active.as_ref().is_some_and(|active| {
                    active.epoch == expected_session_epoch && active.lease.is_active()
                }) && entry.catalogue.as_ref().is_some_and(|catalogue| {
                    catalogue.generation == expected_generation
                        && catalogue.session_epoch == expected_session_epoch
                        && catalogue.authority.is_active()
                        && Arc::ptr_eq(&catalogue.value, expected_value)
                })
            })
    }

    /// Validate one accepted catalogue identity without cloning its value.
    pub fn is_current_catalogue(
        &self,
        source_id: SourceId,
        generation: u64,
        session_epoch: u64,
    ) -> bool {
        self.resolve_exact_accepted_catalogue(source_id, generation, session_epoch, |_| Some(()))
            .is_some()
    }

    /// Atomically capture the global revision and every logical source.
    /// A reducer subscribes to monotonic invalidations first, takes this
    /// baseline, and resnapshots whenever the watched revision advances.
    pub fn snapshot_all(&self) -> LifecycleBaseline<S> {
        let state = lock(&self.inner.state);
        let sources = state
            .entries
            .iter()
            .map(|(source_id, entry)| (*source_id, entry.snapshot()))
            .collect();
        LifecycleBaseline {
            revision: state.revision,
            shutting_down: state.gate != RegistryGate::Running,
            sources,
        }
    }

    /// Snapshot the adapter, lease, and epoch in one registry-lock operation.
    fn session(&self, source_id: SourceId) -> Option<SessionHandle<A>> {
        let state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let session = state.entries.get(&source_id)?.active.as_ref()?;
        if !session.lease.is_active() {
            return None;
        }
        Some(SessionHandle {
            session_epoch: session.epoch,
            adapter: session.adapter.operational(),
            lease: session.lease.clone(),
        })
    }

    /// Resolve through one exact accepted catalogue and active adapter.
    ///
    /// The catalogue snapshot is captured atomically with the adapter, epoch,
    /// and leases. Its selector runs outside the lifecycle lock, and exact
    /// identity and authority are checked again after the async adapter call.
    /// This is intentionally separate from exact-session media
    /// resolution: a same-session catalogue replacement must revoke a
    /// persisted playlist reference even though the adapter epoch is stable.
    async fn resolve_exact_catalogue_session<R, T, Select, F, Fut>(
        &self,
        source_id: SourceId,
        expected_generation: u64,
        expected_session_epoch: u64,
        select: Select,
        resolve: F,
    ) -> Result<(R, MediaLease, MediaLease, T), CatalogueMediaResolveError>
    where
        S: Send + Sync,
        T: Send,
        Select: Fn(&S) -> Option<T> + Send + Sync,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<R>> + Send,
    {
        let (session, catalogue_authority, catalogue_value) = {
            let state = lock(&self.inner.state);
            if state.gate != RegistryGate::Running {
                return Err(CatalogueMediaResolveError::Unavailable);
            }
            let Some(entry) = state.entries.get(&source_id) else {
                return Err(CatalogueMediaResolveError::Unavailable);
            };
            let Some(active) = entry.active.as_ref() else {
                return Err(CatalogueMediaResolveError::Unavailable);
            };
            let Some(catalogue) = entry.catalogue.as_ref() else {
                return Err(CatalogueMediaResolveError::Unavailable);
            };
            if !active.lease.is_active()
                || active.epoch != expected_session_epoch
                || catalogue.session_epoch != expected_session_epoch
                || catalogue.generation != expected_generation
                || !catalogue.authority.is_active()
            {
                return Err(CatalogueMediaResolveError::Unavailable);
            }
            (
                SessionHandle {
                    session_epoch: active.epoch,
                    adapter: active.adapter.operational(),
                    lease: active.lease.clone(),
                },
                catalogue.authority.clone(),
                Arc::clone(&catalogue.value),
            )
        };

        let selected =
            select(catalogue_value.as_ref()).ok_or(CatalogueMediaResolveError::Unavailable)?;
        if !session.lease.is_active()
            || !catalogue_authority.is_active()
            || !self.is_exact_catalogue_snapshot_current(
                source_id,
                expected_generation,
                expected_session_epoch,
                &catalogue_value,
            )
        {
            return Err(CatalogueMediaResolveError::Unavailable);
        }

        let lease = session.lease.clone();
        let resolved = resolve(session.adapter).await;
        let remains_current = lease.is_active()
            && catalogue_authority.is_active()
            && self.is_exact_catalogue_snapshot_current(
                source_id,
                expected_generation,
                expected_session_epoch,
                &catalogue_value,
            );
        if !remains_current {
            return Err(CatalogueMediaResolveError::Unavailable);
        }
        let resolved = resolved.map_err(CatalogueMediaResolveError::Backend)?;
        Ok((resolved, lease, catalogue_authority, selected))
    }

    /// Resolve one stream through exact catalogue membership and attach the
    /// captured session lease before returning it with the selector value.
    pub(crate) async fn resolve_catalogue_stream<T, Select, F, Fut>(
        &self,
        source_id: SourceId,
        expected_generation: u64,
        expected_session_epoch: u64,
        select: Select,
        resolve: F,
    ) -> Result<(AdapterStream, MediaLease, T), CatalogueMediaResolveError>
    where
        S: Send + Sync,
        T: Send,
        Select: Fn(&S) -> Option<T> + Send + Sync,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<AdapterStream>> + Send,
    {
        let (stream, lease, catalogue_authority, selected) = self
            .resolve_exact_catalogue_session(
                source_id,
                expected_generation,
                expected_session_epoch,
                select,
                resolve,
            )
            .await?;
        Ok((stream.with_lease(lease), catalogue_authority, selected))
    }

    /// Optional-artwork counterpart of [`Self::resolve_catalogue_stream`].
    pub(crate) async fn resolve_catalogue_optional_http<T, Select, F, Fut>(
        &self,
        source_id: SourceId,
        expected_generation: u64,
        expected_session_epoch: u64,
        select: Select,
        resolve: F,
    ) -> Result<(Option<ResolvedHttpRequest>, MediaLease, T), CatalogueMediaResolveError>
    where
        S: Send + Sync,
        T: Send,
        Select: Fn(&S) -> Option<T> + Send + Sync,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<Option<ResolvedHttpRequest>>> + Send,
    {
        let (request, lease, catalogue_authority, selected) = self
            .resolve_exact_catalogue_session(
                source_id,
                expected_generation,
                expected_session_epoch,
                select,
                resolve,
            )
            .await?;
        Ok((
            request.map(|request| request.with_lease(lease)),
            catalogue_authority,
            selected,
        ))
    }

    /// Resolve an adapter-owned capability through one exact session and
    /// return the lease captured by that same atomic adapter/epoch snapshot.
    /// Typed boundary helpers attach the lease only after the post-resolution
    /// current-session recheck succeeds.
    async fn resolve_exact_session<R, F, Fut>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        resolve: F,
    ) -> BackendResult<(R, MediaLease)>
    where
        S: Send + Sync,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<R>> + Send,
    {
        let session = self.session(source_id).ok_or_else(|| {
            crate::architecture::error::BackendError::Internal(anyhow::anyhow!(
                "source session unavailable"
            ))
        })?;
        if session.session_epoch != expected_session_epoch {
            return Err(crate::architecture::error::BackendError::Internal(
                anyhow::anyhow!("source session changed before media resolution"),
            ));
        }
        let epoch = session.session_epoch;
        let lease = session.lease.clone();
        let request = resolve(session.adapter).await?;
        let state = lock(&self.inner.state);
        let remains_current = state.gate == RegistryGate::Running
            && lease.is_active()
            && state.entries.get(&source_id).is_some_and(|entry| {
                entry.active.as_ref().map(|active| active.epoch) == Some(epoch)
            });
        drop(state);
        if !remains_current {
            return Err(crate::architecture::error::BackendError::Internal(
                anyhow::anyhow!("source session changed during media resolution"),
            ));
        }
        Ok((request, lease))
    }

    /// Run one read-only adapter operation through an exact current session.
    ///
    /// Unlike media resolution, this returns no lease to the caller. The
    /// adapter, epoch, and active lease are captured atomically, adapter work
    /// runs outside the lifecycle mutex, and the exact session is rechecked
    /// after the future completes. A successful value carries only a weak,
    /// opaque receipt for later commit admission. Lifecycle replacement
    /// therefore wins over either a successful result or a stale adapter
    /// error.
    pub async fn run_exact_session_operation<R, F, Fut>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        operation: F,
    ) -> Result<(R, SessionOperationReceipt<A>), SessionOperationError>
    where
        S: Send + Sync,
        R: Send,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<R>> + Send,
    {
        let session = self
            .session(source_id)
            .ok_or(SessionOperationError::Unavailable)?;
        if session.session_epoch != expected_session_epoch {
            return Err(SessionOperationError::Unavailable);
        }

        let epoch = session.session_epoch;
        let lease = session.lease.clone();
        let expected_adapter = Arc::clone(&session.adapter);
        let result = operation(session.adapter).await;
        let state = lock(&self.inner.state);
        let remains_current = state.gate == RegistryGate::Running
            && lease.is_active()
            && state.entries.get(&source_id).is_some_and(|entry| {
                entry.active.as_ref().is_some_and(|active| {
                    active.epoch == epoch
                        && Arc::ptr_eq(&active.adapter.operational(), &expected_adapter)
                })
            });
        drop(state);

        if !remains_current {
            return Err(SessionOperationError::Unavailable);
        }
        let value = result.map_err(SessionOperationError::Backend)?;
        Ok((
            value,
            SessionOperationReceipt {
                incarnation: self.inner.incarnation,
                source_id,
                session_epoch: epoch,
                adapter: Arc::downgrade(&expected_adapter),
            },
        ))
    }

    /// Run a follow-up adapter operation through the exact session captured
    /// by a prior successful operation receipt.
    ///
    /// The receipt is validated against this registry incarnation and exact
    /// adapter pointer before any adapter work. The same post-operation check
    /// as [`Self::run_exact_session_operation`] then mints a fresh receipt.
    pub async fn run_receipted_session_operation<R, F, Fut>(
        &self,
        receipt: &SessionOperationReceipt<A>,
        operation: F,
    ) -> Result<(R, SessionOperationReceipt<A>), SessionOperationError>
    where
        S: Send + Sync,
        R: Send,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<R>> + Send,
    {
        let source_id = receipt.source_id;
        let epoch = receipt.session_epoch;
        let (adapter, lease) = {
            let state = lock(&self.inner.state);
            if receipt.incarnation != self.inner.incarnation || state.gate != RegistryGate::Running
            {
                return Err(SessionOperationError::Unavailable);
            }
            let expected_adapter = receipt
                .adapter
                .upgrade()
                .ok_or(SessionOperationError::Unavailable)?;
            let active = state
                .entries
                .get(&source_id)
                .and_then(|entry| entry.active.as_ref())
                .ok_or(SessionOperationError::Unavailable)?;
            if active.epoch != epoch
                || !active.lease.is_active()
                || !Arc::ptr_eq(&active.adapter.adapter, &expected_adapter)
            {
                return Err(SessionOperationError::Unavailable);
            }
            (active.adapter.operational(), active.lease.clone())
        };

        let expected_adapter = Arc::clone(&adapter);
        let result = operation(adapter).await;
        let state = lock(&self.inner.state);
        let remains_current = state.gate == RegistryGate::Running
            && lease.is_active()
            && state.entries.get(&source_id).is_some_and(|entry| {
                entry.active.as_ref().is_some_and(|active| {
                    active.epoch == epoch
                        && Arc::ptr_eq(&active.adapter.operational(), &expected_adapter)
                })
            });
        drop(state);

        if !remains_current {
            return Err(SessionOperationError::Unavailable);
        }
        let value = result.map_err(SessionOperationError::Backend)?;
        Ok((
            value,
            SessionOperationReceipt {
                incarnation: self.inner.incarnation,
                source_id,
                session_epoch: epoch,
                adapter: Arc::downgrade(&expected_adapter),
            },
        ))
    }

    /// Atomically admit one commit section through the exact session that
    /// minted `receipt`.
    ///
    /// Validation and permit acquisition share the lifecycle state lock. A
    /// replacement, disconnect, or shutdown therefore either wins first and
    /// rejects admission, or waits for the returned authority to be dropped.
    /// The receipt's registry incarnation and adapter pointer prevent an
    /// epoch collision in another registry from gaining authority. The
    /// caller's non-reentrant predicate is evaluated against that exact
    /// adapter under the same lock before the permit is acquired.
    pub(crate) fn acquire_session_commit_authority_if<P>(
        &self,
        receipt: &SessionOperationReceipt<A>,
        predicate: P,
    ) -> Option<SessionCommitAuthority>
    where
        P: FnOnce(&A) -> bool,
    {
        let state = lock(&self.inner.state);
        if receipt.incarnation != self.inner.incarnation || state.gate != RegistryGate::Running {
            return None;
        }

        let expected_adapter = receipt.adapter.upgrade()?;
        let active = state.entries.get(&receipt.source_id)?.active.as_ref()?;
        if active.epoch != receipt.session_epoch
            || !Arc::ptr_eq(&active.adapter.adapter, &expected_adapter)
            || !active.lease.is_active()
        {
            return None;
        }
        // The predicate is deliberately synchronous and receives only the
        // already-validated exact adapter. Callers must keep it to a local,
        // non-blocking capability read and must not re-enter this lifecycle
        // registry. Evaluating it while the state lock is retained makes the
        // capability decision and permit acquisition one admission event.
        if !predicate(active.adapter.adapter.as_ref()) {
            return None;
        }

        Some(SessionCommitAuthority {
            permit: active.lease.try_acquire()?,
        })
    }

    /// Resolve one protected HTTP locator through the exact expected adapter
    /// epoch, then recheck its epoch and lease before attaching the production
    /// `MediaLease`. Requiring the caller's captured epoch prevents a queued
    /// reference from being resolved against a later same-source session.
    /// This is the migration seam that replaces the standard resolver map and
    /// DAAP lease map without introducing a second lookup authority.
    pub async fn resolve_http<F, Fut>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        resolve: F,
    ) -> BackendResult<ResolvedHttpRequest>
    where
        S: Send + Sync,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<ResolvedHttpRequest>> + Send,
    {
        let (request, lease) = self
            .resolve_exact_session(source_id, expected_session_epoch, resolve)
            .await?;
        Ok(request.with_lease(lease))
    }

    /// Resolve one exact retained file through the same adapter, expected
    /// epoch, and pre/post lease checks used for protected HTTP. The attached
    /// lease is checked again immediately before and after every file-handle
    /// clone, so retirement cannot retarget a queued external identity.
    pub async fn resolve_file<F, Fut>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        resolve: F,
    ) -> BackendResult<ResolvedFileMedia>
    where
        S: Send + Sync,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<ResolvedFileMedia>> + Send,
    {
        let (media, lease) = self
            .resolve_exact_session(source_id, expected_session_epoch, resolve)
            .await?;
        Ok(media.with_lease(lease))
    }

    /// Resolve one heterogeneous adapter stream without ever exposing its
    /// source lease as a separable value.
    pub(crate) async fn resolve_stream<F, Fut>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        resolve: F,
    ) -> BackendResult<AdapterStream>
    where
        S: Send + Sync,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<AdapterStream>> + Send,
    {
        let (stream, lease) = self
            .resolve_exact_session(source_id, expected_session_epoch, resolve)
            .await?;
        Ok(stream.with_lease(lease))
    }

    /// Optional-artwork form of [`Self::resolve_http`] with the same exact
    /// pre-resolution expected-epoch check and post-resolution epoch/lease
    /// recheck.
    pub async fn resolve_optional_http<F, Fut>(
        &self,
        source_id: SourceId,
        expected_session_epoch: u64,
        resolve: F,
    ) -> BackendResult<Option<ResolvedHttpRequest>>
    where
        S: Send + Sync,
        F: FnOnce(Arc<A>) -> Fut + Send,
        Fut: Future<Output = BackendResult<Option<ResolvedHttpRequest>>> + Send,
    {
        let session = self.session(source_id).ok_or_else(|| {
            crate::architecture::error::BackendError::Internal(anyhow::anyhow!(
                "source session unavailable"
            ))
        })?;
        if session.session_epoch != expected_session_epoch {
            return Err(crate::architecture::error::BackendError::Internal(
                anyhow::anyhow!("source session changed before media resolution"),
            ));
        }
        let epoch = session.session_epoch;
        let lease = session.lease.clone();
        let request = resolve(session.adapter).await?;
        let state = lock(&self.inner.state);
        let remains_current = state.gate == RegistryGate::Running
            && lease.is_active()
            && state.entries.get(&source_id).is_some_and(|entry| {
                entry.active.as_ref().map(|active| active.epoch) == Some(epoch)
            });
        drop(state);
        if !remains_current {
            return Err(crate::architecture::error::BackendError::Internal(
                anyhow::anyhow!("source session changed during media resolution"),
            ));
        }
        Ok(request.map(|request| request.with_lease(lease)))
    }

    /// Begin or supersede one connection/replacement operation.
    pub fn begin_connect(&self, source_id: SourceId) -> Option<ConnectOwner<A, S>> {
        let mut state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let generation = RegistryInner::next_generation(&mut state);
        let entry = state.entries.get_mut(&source_id)?;
        if entry.state == SourceState::Retired {
            return None;
        }
        if entry.state == SourceState::Disconnecting {
            if entry.provenance.is_empty() {
                return None;
            }
            // The old close remains globally tracked and its waiter remains
            // valid, but it is no longer allowed to transition or annotate
            // the reconnecting entry.
            entry.disconnect_retirement = None;
        }
        // A reconnect is a new foreground incarnation. Any predecessor
        // disconnect waiter remains valid through its own cloned settlement
        // trackers, but must not be reused by a later disconnect.
        entry.disconnect_waiter = None;
        entry
            .connect_settlements
            .retain(|_, settlement| settlement.active() != 0);
        let displaced_generation = entry.connect.as_ref().map(|operation| operation.generation);
        if let Some(previous) = entry.connect.take() {
            RegistryInner::<A, S>::cancel_pending(previous);
        }
        let (cancellation, observer) = CancellationSwitch::pair();
        let settlement = ConnectSettlement::new();
        entry
            .connect_settlements
            .insert(generation, Arc::clone(&settlement));
        entry.connect = Some(PendingOperation {
            generation,
            session_epoch: None,
            cancellation,
            abort: None,
            abortable: true,
            settlement: Some(Arc::clone(&settlement)),
        });
        let session_epoch = entry.session_epoch();
        if let Some(generation) = displaced_generation {
            self.inner.publish_locked(
                &mut state,
                source_id,
                LifecycleChange::ConnectCancelled { generation },
            );
        }
        self.inner.set_session_failure_locked(
            &mut state,
            source_id,
            None,
            OperationCorrelation {
                generation,
                session_epoch,
            },
        );
        self.inner.publish_locked(
            &mut state,
            source_id,
            LifecycleChange::ConnectStarted { generation },
        );
        self.inner
            .transition_locked(&mut state, source_id, SourceState::Connecting);
        Some(ConnectOwner {
            inner: Arc::clone(&self.inner),
            incarnation: self.inner.incarnation,
            source_id,
            generation,
            cancellation: observer,
            settlement,
            completed: false,
        })
    }

    /// Synchronously adopt one already-constructed stateless built-in source.
    ///
    /// The normal connect/adopt path still mints its generation, session
    /// epoch, lease, events, and replacement retirement. No caller callback
    /// runs while lifecycle or outer installation state is locked.
    pub(crate) fn adopt_stateless_session(
        &self,
        source_id: SourceId,
        adapter: Box<A>,
        snapshot: S,
    ) -> Option<(u64, u64)>
    where
        S: Send + Sync + 'static,
    {
        let owner = self.begin_connect(source_id)?;
        let generation = owner.generation();
        let settlement = Arc::clone(&owner.settlement);
        let submission = owner.submit_constructed(ConstructedAdapter::from_box(adapter), snapshot);
        self.remove_inactive_connect_settlement(source_id, generation, &settlement);
        match submission {
            ConnectSubmission::Adopted { session_epoch, .. } => Some((generation, session_epoch)),
            ConnectSubmission::Rejected => None,
        }
    }

    /// Record failure to construct one stateless built-in adapter through the
    /// ordinary connect-generation state transition.
    pub(crate) fn fail_stateless_session(
        &self,
        source_id: SourceId,
        category: FailureCategory,
    ) -> Option<u64> {
        let owner = self.begin_connect(source_id)?;
        let generation = owner.generation();
        let settlement = Arc::clone(&owner.settlement);
        owner.fail(category);
        self.remove_inactive_connect_settlement(source_id, generation, &settlement);
        Some(generation)
    }

    fn remove_inactive_connect_settlement(
        &self,
        source_id: SourceId,
        generation: u64,
        settlement: &Arc<ConnectSettlement>,
    ) {
        if settlement.active() != 0 {
            return;
        }
        let mut state = lock(&self.inner.state);
        let Some(entry) = state.entries.get_mut(&source_id) else {
            return;
        };
        let is_exact = entry
            .connect_settlements
            .get(&generation)
            .is_some_and(|current| Arc::ptr_eq(current, settlement));
        if is_exact {
            entry.connect_settlements.remove(&generation);
        }
    }

    /// Begin or supersede one refresh lane against the exact current epoch.
    pub fn begin_refresh(
        &self,
        source_id: SourceId,
        lane: RefreshLane,
    ) -> Option<RefreshOwner<A, S>> {
        let mut state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running {
            return None;
        }
        let generation = RegistryInner::next_generation(&mut state);
        let entry = state.entries.get_mut(&source_id)?;
        let active = entry.active.as_ref()?;
        let session_epoch = active.epoch;
        let session = SessionHandle {
            session_epoch,
            adapter: active.adapter.operational(),
            lease: active.lease.clone(),
        };
        if matches!(
            entry.state,
            SourceState::Disconnecting | SourceState::Retired
        ) {
            return None;
        }
        let displaced_generation = entry
            .refreshes
            .get(&lane)
            .map(|operation| operation.generation);
        if let Some(previous) = entry.refreshes.remove(&lane) {
            RegistryInner::<A, S>::cancel_pending(previous);
        }
        let (cancellation, observer) = CancellationSwitch::pair();
        entry.refreshes.insert(
            lane.clone(),
            PendingOperation {
                generation,
                session_epoch: Some(session_epoch),
                cancellation,
                abort: None,
                abortable: true,
                settlement: None,
            },
        );
        if let Some(generation) = displaced_generation {
            self.inner.publish_locked(
                &mut state,
                source_id,
                LifecycleChange::OperationCancelled {
                    lane: lane.clone(),
                    generation,
                },
            );
        }
        self.inner.publish_locked(
            &mut state,
            source_id,
            LifecycleChange::RefreshStarted {
                lane: lane.clone(),
                generation,
                session_epoch,
            },
        );
        let next = state
            .entries
            .get(&source_id)
            .map_or(SourceState::Retired, Entry::active_state);
        self.inner.transition_locked(&mut state, source_id, next);
        Some(RefreshOwner {
            inner: Arc::clone(&self.inner),
            incarnation: self.inner.incarnation,
            source_id,
            lane,
            generation,
            session_epoch,
            session,
            cancellation: observer,
            completed: false,
        })
    }

    /// Cancel one current refresh without recording a user-visible failure.
    pub fn cancel_refresh(&self, source_id: SourceId, lane: &RefreshLane) -> bool {
        let mut state = lock(&self.inner.state);
        let Some(operation) = state
            .entries
            .get_mut(&source_id)
            .and_then(|entry| entry.refreshes.remove(lane))
        else {
            return false;
        };
        let generation = operation.generation;
        RegistryInner::<A, S>::cancel_pending(operation);
        self.inner.publish_locked(
            &mut state,
            source_id,
            LifecycleChange::OperationCancelled {
                lane: lane.clone(),
                generation,
            },
        );
        let next = state
            .entries
            .get(&source_id)
            .map_or(SourceState::Retired, Entry::active_state);
        self.inner.transition_locked(&mut state, source_id, next);
        true
    }

    /// Cancel a view refresh and remove only that view's accepted generation,
    /// snapshot, and failure annotation.
    pub fn remove_view(&self, source_id: SourceId, view: &ViewOrigin) -> bool {
        let lane = RefreshLane::View(view.clone());
        let mut state = lock(&self.inner.state);
        let Some(entry) = state.entries.get_mut(&source_id) else {
            return false;
        };
        let pending = entry.refreshes.remove(&lane);
        let removed_snapshot = entry.remove_view(view);
        let removed_failure = entry.refresh_failures.get(&lane).copied();
        let had_pending = pending.is_some();
        if let Some(operation) = pending {
            RegistryInner::<A, S>::cancel_pending(operation);
        }
        if !removed_snapshot && removed_failure.is_none() && !had_pending {
            return false;
        }
        if let Some(removed_failure) = removed_failure {
            self.inner.set_refresh_failure_locked(
                &mut state,
                source_id,
                lane,
                None,
                removed_failure.correlation,
            );
        }
        self.inner.publish_locked(
            &mut state,
            source_id,
            LifecycleChange::ViewRemoved { view: view.clone() },
        );
        let next = state
            .entries
            .get(&source_id)
            .map_or(SourceState::Retired, Entry::active_state);
        self.inner.transition_locked(&mut state, source_id, next);
        true
    }

    /// Synchronously revoke operation and media authority, start one
    /// registry-owned close for the exact adopted adapter if present, and join
    /// every spawned connect generation which can still return a late adapter.
    pub fn disconnect(&self, source_id: SourceId) -> Option<RetirementWaiter>
    where
        S: Send + Sync + 'static,
    {
        let mut state = lock(&self.inner.state);
        if state.gate != RegistryGate::Running || !state.entries.contains_key(&source_id) {
            return None;
        }
        if let Some(waiter) = state
            .entries
            .get(&source_id)
            .and_then(|entry| entry.disconnect_waiter.clone())
        {
            return Some(waiter);
        }
        let jobs = self.disconnect_locked(&mut state, source_id, false);
        let waiter = state
            .entries
            .get(&source_id)
            .and_then(|entry| entry.disconnect_waiter.clone())
            .or_else(|| {
                // A reconnect can dissociate an older foreground close before
                // adopting a successor. Disconnecting that still-unadopted
                // reconnect continues to join the one close already in flight.
                let latest_disconnect = state
                    .entries
                    .get(&source_id)
                    .into_iter()
                    .flat_map(|entry| entry.retirement_ids.iter().copied())
                    .filter(|retirement_id| {
                        state
                            .retirements
                            .get(retirement_id)
                            .is_some_and(|record| record.purpose == RetirementPurpose::Disconnect)
                    })
                    .max();
                latest_disconnect.and_then(|retirement_id| {
                    RegistryInner::retirement_waiter_locked(&state, retirement_id)
                })
            })
            .unwrap_or_else(RetirementWaiter::completed);
        if !waiter.is_complete() {
            state
                .entries
                .get_mut(&source_id)
                .expect("disconnect entry retained")
                .disconnect_waiter = Some(waiter.clone());
        }
        drop(state);
        for job in jobs {
            self.inner.spawn_retirement(job);
        }
        Some(waiter)
    }

    /// Return a join-only waiter for an already-started foreground
    /// disconnect. This never cancels a successor operation or initiates a new
    /// disconnect, so production pruning can safely race reappearance.
    pub fn current_disconnect_waiter(&self, source_id: SourceId) -> Option<RetirementWaiter> {
        let state = lock(&self.inner.state);
        state
            .entries
            .get(&source_id)
            .and_then(|entry| entry.disconnect_waiter.clone())
            .filter(|waiter| !waiter.is_complete())
    }

    /// Arrange lifecycle-owned cleanup after the disconnect already started
    /// by a final provenance release. This never initiates or cancels work.
    /// The maintenance task captures only the internal Arc, so it cannot
    /// suppress last-external-handle fail-closed teardown.
    pub fn schedule_prune_after_current_retirement(&self, source_id: SourceId)
    where
        S: Send + Sync + 'static,
    {
        let waiter = {
            let mut state = lock(&self.inner.state);
            if self.inner.prune_source_locked(&mut state, source_id) {
                return;
            }
            state
                .entries
                .get(&source_id)
                .and_then(|entry| entry.disconnect_waiter.clone())
                .filter(|waiter| !waiter.is_complete())
        };
        let Some(waiter) = waiter else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let runtime = inner.runtime.clone();
        runtime.spawn(async move {
            waiter.wait().await;
            let mut state = lock(&inner.state);
            inner.prune_source_locked(&mut state, source_id);
        });
    }

    fn disconnect_locked(
        &self,
        state: &mut RegistryState<A, S>,
        source_id: SourceId,
        forced_retirement: bool,
    ) -> Vec<RetirementJob<A, S>> {
        let Some(entry) = state.entries.get_mut(&source_id) else {
            return Vec::new();
        };
        let disconnect_in_progress = entry
            .disconnect_waiter
            .as_ref()
            .is_some_and(|waiter| !waiter.is_complete())
            || entry.disconnect_retirement.is_some();
        if disconnect_in_progress {
            if forced_retirement {
                // A prior explicit disconnect may be settlement-only: there is
                // no adopted-session retirement callback left to observe the
                // final provenance release. Record retirement intent now while
                // preserving the one existing waiter and its exact work.
                self.inner
                    .transition_locked(state, source_id, SourceState::Retired);
            }
            return Vec::new();
        }
        entry.disconnect_waiter = None;
        entry
            .connect_settlements
            .retain(|_, settlement| settlement.active() != 0);
        let mut connect_settlements: Vec<_> = entry
            .connect_settlements
            .iter()
            .map(|(generation, settlement)| (*generation, Arc::clone(settlement)))
            .collect();
        connect_settlements.sort_unstable_by_key(|(generation, _)| *generation);
        if let Some(connect) = entry.connect.take() {
            RegistryInner::<A, S>::cancel_pending(connect);
        }
        for refresh in entry.refreshes.drain().map(|(_, operation)| operation) {
            RegistryInner::<A, S>::cancel_pending(refresh);
        }
        let active = entry.active.take();
        let had_snapshots = entry.revoke_snapshots();
        let session_failure = entry.failure;
        let refresh_failure_lanes: Vec<_> = entry
            .refresh_failures
            .iter()
            .map(|(lane, failure)| (lane.clone(), failure.correlation))
            .collect();
        if had_snapshots {
            self.inner
                .publish_locked(state, source_id, LifecycleChange::SnapshotsCleared);
        }
        if let Some(session_failure) = session_failure {
            self.inner.set_session_failure_locked(
                state,
                source_id,
                None,
                session_failure.correlation,
            );
        }
        for (lane, correlation) in refresh_failure_lanes {
            self.inner
                .set_refresh_failure_locked(state, source_id, lane, None, correlation);
        }
        let mut jobs = Vec::new();
        if let Some(active) = active {
            let job = self.inner.prepare_retirement_locked(
                state,
                source_id,
                Some(active),
                None,
                RetirementPurpose::Disconnect,
                true,
            );
            self.inner
                .transition_locked(state, source_id, SourceState::Disconnecting);
            jobs.push(job);
        } else {
            let next = if forced_retirement {
                SourceState::Retired
            } else {
                let entry = state.entries.get(&source_id).expect("entry retained");
                if entry.provenance.is_empty() {
                    SourceState::Retired
                } else {
                    SourceState::Dormant
                }
            };
            self.inner.transition_locked(state, source_id, next);
        }

        let mut waiter =
            jobs.first()
                .map(RetirementJob::waiter)
                .or_else(|| {
                    // Reconnecting dissociates an older foreground close so it
                    // cannot mutate the successor row, but a disconnect before
                    // successor adoption must still join that already-owned close.
                    let latest_disconnect = state
                        .entries
                        .get(&source_id)
                        .into_iter()
                        .flat_map(|entry| entry.retirement_ids.iter().copied())
                        .filter(|retirement_id| {
                            state.retirements.get(retirement_id).is_some_and(|record| {
                                record.purpose == RetirementPurpose::Disconnect
                            })
                        })
                        .max();
                    latest_disconnect.and_then(|retirement_id| {
                        RegistryInner::retirement_waiter_locked(state, retirement_id)
                    })
                })
                .unwrap_or_else(RetirementWaiter::completed);
        for (generation, settlement) in connect_settlements {
            waiter = waiter.join_settlement(generation, settlement);
        }
        if !waiter.is_complete() {
            state
                .entries
                .get_mut(&source_id)
                .expect("disconnect entry retained")
                .disconnect_waiter = Some(waiter);
        }
        jobs
    }

    /// Close admission, synchronously cancel/abort every operation and revoke
    /// every lease, then return the one persistent operation/retirement join
    /// barrier. Concurrent and repeated calls observe the same barrier.
    pub fn shutdown(&self) -> ShutdownBarrier
    where
        S: Send + Sync + 'static,
    {
        let barrier = ShutdownBarrier {
            tracker: Arc::clone(&self.inner.tracker),
        };
        let mut jobs = Vec::new();
        let mut state = lock(&self.inner.state);
        if state.gate == RegistryGate::ShuttingDown {
            return barrier;
        }
        state.gate = RegistryGate::ShuttingDown;
        // Closing the global gate is observable even for an empty registry or
        // one containing only inert retired entries. Reducers use this wakeup
        // to release their final registry/window references on shutdown.
        state.revision = state
            .revision
            .checked_add(1)
            .expect("source lifecycle revision exhausted");
        self.inner.invalidations.send_replace(state.revision);
        let sources: Vec<_> = state.entries.keys().copied().collect();
        for source_id in sources {
            let (active, disconnect_pending, had_snapshots, session_failure, refresh_failure_lanes) = {
                let Some(entry) = state.entries.get_mut(&source_id) else {
                    continue;
                };
                if let Some(connect) = entry.connect.take() {
                    RegistryInner::<A, S>::cancel_pending(connect);
                }
                for refresh in entry.refreshes.drain().map(|(_, operation)| operation) {
                    RegistryInner::<A, S>::cancel_pending(refresh);
                }
                let had_snapshots = entry.revoke_snapshots();
                let session_failure = entry.failure;
                let refresh_failure_lanes: Vec<_> = entry
                    .refresh_failures
                    .iter()
                    .map(|(lane, failure)| (lane.clone(), failure.correlation))
                    .collect();
                (
                    entry.active.take(),
                    entry.disconnect_retirement.is_some(),
                    had_snapshots,
                    session_failure,
                    refresh_failure_lanes,
                )
            };
            if had_snapshots {
                self.inner
                    .publish_locked(&mut state, source_id, LifecycleChange::SnapshotsCleared);
            }
            if let Some(session_failure) = session_failure {
                self.inner.set_session_failure_locked(
                    &mut state,
                    source_id,
                    None,
                    session_failure.correlation,
                );
            }
            for (lane, correlation) in refresh_failure_lanes {
                self.inner.set_refresh_failure_locked(
                    &mut state,
                    source_id,
                    lane,
                    None,
                    correlation,
                );
            }
            if disconnect_pending {
                self.inner
                    .transition_locked(&mut state, source_id, SourceState::Disconnecting);
                continue;
            }
            if let Some(active) = active {
                let job = self.inner.prepare_retirement_locked(
                    &mut state,
                    source_id,
                    Some(active),
                    None,
                    RetirementPurpose::Disconnect,
                    true,
                );
                jobs.push(job);
                self.inner
                    .transition_locked(&mut state, source_id, SourceState::Disconnecting);
            } else {
                self.inner
                    .transition_locked(&mut state, source_id, SourceState::Retired);
            }
        }
        drop(state);
        for job in jobs {
            self.inner.spawn_retirement(job);
        }
        barrier
    }

    /// Remove only inert, provenance-free retired entries. Late owners carry
    /// the registry incarnation and global generation, so a submission after
    /// pruning can only enter rejected-adapter retirement.
    pub fn prune_retired(&self) -> usize {
        let mut state = lock(&self.inner.state);
        let candidates: Vec<_> = state
            .entries
            .keys()
            .filter_map(|source_id| {
                RegistryInner::source_is_prunable(&state, *source_id).then_some(*source_id)
            })
            .collect();
        for source_id in &candidates {
            let pruned = self.inner.prune_source_locked(&mut state, *source_id);
            debug_assert!(pruned);
        }
        candidates.len()
    }
}

/// Result of consuming one constructed adapter under connect authority.
#[derive(Clone)]
enum ConnectSubmission {
    Adopted {
        session_epoch: u64,
        lease: MediaLease,
    },
    Rejected,
}

/// Authentication/construction phase of a two-stage connect task.
pub enum AdapterTaskResult<A: LifecycleAdapter + ?Sized> {
    Constructed(Box<A>),
    Failed(FailureCategory),
    Cancelled,
}

impl<A: LifecycleAdapter> AdapterTaskResult<A> {
    pub fn constructed(adapter: A) -> Self {
        Self::Constructed(Box::new(adapter))
    }
}

/// Output of a registry-tracked refresh task.
pub enum RefreshTaskResult<S> {
    Refreshed(S),
    Failed(FailureCategory),
    Cancelled,
}

/// Optional non-cloneable abort capability for a registry-owned task.
///
/// Dropping this value is inert: the registry retains task ownership and the
/// task retains its join participation, so callers need no keepalive map.
/// Explicit abort and registry supersession/shutdown still clean up the exact
/// owner moved into the task.
pub struct OperationAbort {
    abort: Option<AbortHandle>,
    request_cancellation: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl OperationAbort {
    /// Request cancellation through registry policy. During a protected
    /// sessionful constructor this only signals cancellation; once the
    /// adapter has been staged, the registry may abort the task safely.
    pub fn abort(mut self) {
        if let Some(request) = self.request_cancellation.take() {
            request();
        }
    }

    pub fn is_finished(&self) -> bool {
        self.abort.as_ref().is_none_or(AbortHandle::is_finished)
    }
}

/// Non-cloneable exact owner for one connect generation.
pub struct ConnectOwner<A: LifecycleAdapter + ?Sized, S> {
    inner: Arc<RegistryInner<A, S>>,
    incarnation: Uuid,
    source_id: SourceId,
    generation: u64,
    cancellation: CancellationObserver,
    settlement: Arc<ConnectSettlement>,
    completed: bool,
}

impl<A: LifecycleAdapter + ?Sized, S> ConnectOwner<A, S> {
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn cancellation(&self) -> CancellationObserver {
        self.cancellation.clone()
    }

    /// Transfer a constructed/logged-in adapter into a mandatory retirement
    /// guard before catalogue loading or other post-login work begins.
    fn stage_constructed(self, adapter: ConstructedAdapter<A>) -> StagedConnect<A, S>
    where
        S: Send + Sync + 'static,
    {
        StagedConnect {
            owner: Some(self),
            adapter: Some(adapter),
        }
    }

    /// Consume a constructed adapter. A stale, cancelled, pruned, or
    /// shutdown submission is not dropped: it is transferred into the same
    /// tracked, exactly-once retirement path as an adopted predecessor.
    fn submit_constructed(
        mut self,
        adapter: ConstructedAdapter<A>,
        snapshot: S,
    ) -> ConnectSubmission
    where
        S: Send + Sync + 'static,
    {
        let mut jobs = Vec::new();
        let mut state = lock(&self.inner.state);
        let accepted = self.incarnation == self.inner.incarnation
            && state.gate == RegistryGate::Running
            && !self.cancellation.is_cancelled()
            && state.entries.get(&self.source_id).is_some_and(|entry| {
                entry
                    .connect
                    .as_ref()
                    .is_some_and(|operation| operation.generation == self.generation)
            });
        let submission = if accepted {
            let session_epoch = RegistryInner::next_session_epoch(&mut state);
            let (predecessor, refreshes, refresh_failure_lanes) = {
                let entry = state
                    .entries
                    .get_mut(&self.source_id)
                    .expect("accepted source exists");
                entry.connect = None;
                let predecessor = entry.active.take();
                let refreshes: Vec<_> = entry
                    .refreshes
                    .drain()
                    .map(|(_, operation)| operation)
                    .collect();
                let refresh_failure_lanes: Vec<_> =
                    entry.refresh_failures.keys().cloned().collect();
                (predecessor, refreshes, refresh_failure_lanes)
            };
            for refresh in refreshes {
                RegistryInner::<A, S>::cancel_pending(refresh);
            }
            let replaced_epoch = predecessor.as_ref().map(|session| session.epoch);
            if let Some(predecessor) = predecessor.as_ref() {
                // Lock-free consumers must lose predecessor authority before
                // any successor state is installed. Retirement repeats this
                // idempotent revocation when it takes close ownership.
                predecessor.lease.revoke();
            }
            let lease = MediaLease::new();
            let accepted_snapshot = AcceptedSnapshot::new(self.generation, session_epoch, snapshot);
            {
                let entry = state
                    .entries
                    .get_mut(&self.source_id)
                    .expect("accepted source exists");
                entry.revoke_snapshots();
                entry.active = Some(ActiveSession {
                    epoch: session_epoch,
                    adapter,
                    lease: lease.clone(),
                });
                entry.catalogue = Some(accepted_snapshot);
            }
            if let Some(predecessor) = predecessor {
                jobs.push(self.inner.prepare_retirement_locked(
                    &mut state,
                    self.source_id,
                    Some(predecessor),
                    None,
                    RetirementPurpose::Replacement,
                    true,
                ));
            }
            self.inner.set_session_failure_locked(
                &mut state,
                self.source_id,
                None,
                OperationCorrelation {
                    generation: self.generation,
                    session_epoch: Some(session_epoch),
                },
            );
            for lane in refresh_failure_lanes {
                self.inner.set_refresh_failure_locked(
                    &mut state,
                    self.source_id,
                    lane,
                    None,
                    OperationCorrelation {
                        generation: self.generation,
                        session_epoch: Some(session_epoch),
                    },
                );
            }
            self.inner.publish_locked(
                &mut state,
                self.source_id,
                LifecycleChange::SessionAdopted {
                    session_epoch,
                    replaced_epoch,
                },
            );
            self.inner.publish_locked(
                &mut state,
                self.source_id,
                LifecycleChange::CatalogueAccepted {
                    generation: self.generation,
                    session_epoch,
                },
            );
            self.inner
                .transition_locked(&mut state, self.source_id, SourceState::Ready);
            ConnectSubmission::Adopted {
                session_epoch,
                lease,
            }
        } else {
            let mut job = self.inner.prepare_retirement_locked(
                &mut state,
                self.source_id,
                None,
                Some(adapter),
                RetirementPurpose::Rejected,
                false,
            );
            // Transfer exact connect-settlement ownership before the task's
            // participant can drop. A disconnect waiter therefore observes no
            // zero-count gap between protected construction and late close.
            job.settlement_participant = Some(self.settlement.participate());
            job.settlement = Some(Arc::clone(&self.settlement));
            jobs.push(job);
            ConnectSubmission::Rejected
        };
        self.completed = true;
        drop(state);
        for job in jobs {
            self.inner.spawn_retirement(job);
        }
        submission
    }

    fn fail(mut self, category: FailureCategory) -> bool {
        let accepted = self.inner.finish_connect_failure(
            self.incarnation,
            self.source_id,
            self.generation,
            category,
        );
        self.completed = true;
        accepted
    }

    fn cancel(mut self) -> bool {
        let accepted =
            self.inner
                .abandon_connect(self.incarnation, self.source_id, self.generation);
        self.completed = true;
        accepted
    }

    fn reject_adapter(
        mut self,
        adapter: ConstructedAdapter<A>,
        failure: Option<FailureCategory>,
    ) -> bool
    where
        S: Send + Sync + 'static,
    {
        let mut state = lock(&self.inner.state);
        let current = self.incarnation == self.inner.incarnation
            && state.gate == RegistryGate::Running
            && state.entries.get(&self.source_id).is_some_and(|entry| {
                entry
                    .connect
                    .as_ref()
                    .is_some_and(|operation| operation.generation == self.generation)
            });
        let mut job = self.inner.prepare_retirement_locked(
            &mut state,
            self.source_id,
            None,
            Some(adapter),
            RetirementPurpose::Rejected,
            false,
        );
        // The staged adapter's mandatory close is part of the exact connect
        // settlement. Enroll it before the enclosing connect task can finish.
        job.settlement_participant = Some(self.settlement.participate());
        job.settlement = Some(Arc::clone(&self.settlement));
        if current {
            state
                .entries
                .get_mut(&self.source_id)
                .expect("current source exists")
                .connect = None;
            if let Some(category) = failure {
                let session_epoch = state
                    .entries
                    .get(&self.source_id)
                    .and_then(Entry::session_epoch);
                self.inner.set_session_failure_locked(
                    &mut state,
                    self.source_id,
                    Some(SourceFailure::connect(category)),
                    OperationCorrelation {
                        generation: self.generation,
                        session_epoch,
                    },
                );
            } else {
                self.inner.publish_locked(
                    &mut state,
                    self.source_id,
                    LifecycleChange::ConnectCancelled {
                        generation: self.generation,
                    },
                );
            }
            let next = if failure.is_some()
                && state
                    .entries
                    .get(&self.source_id)
                    .is_some_and(|entry| entry.active.is_none())
            {
                SourceState::Failed
            } else {
                state
                    .entries
                    .get(&self.source_id)
                    .map_or(SourceState::Retired, Entry::active_state)
            };
            self.inner
                .transition_locked(&mut state, self.source_id, next);
        }
        self.completed = true;
        drop(state);
        self.inner.spawn_retirement(job);
        current
    }

    /// Run authentication/construction and catalogue loading as one tracked
    /// operation while transferring the adapter into a staged retirement
    /// guard immediately between the two phases.
    pub fn spawn_staged<Authenticate, AuthenticateFuture, Catalogue, CatalogueFuture>(
        self,
        construction_policy: ConstructionCancellationPolicy,
        authenticate: Authenticate,
        catalogue: Catalogue,
    ) -> OperationAbort
    where
        A: 'static,
        S: Send + Sync + 'static,
        Authenticate: FnOnce(CancellationObserver) -> AuthenticateFuture + Send + 'static,
        AuthenticateFuture: Future<Output = AdapterTaskResult<A>> + Send + 'static,
        Catalogue: FnOnce(Arc<A>, CancellationObserver) -> CatalogueFuture + Send + 'static,
        CatalogueFuture: Future<Output = RefreshTaskResult<S>> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        let source_id = self.source_id;
        let generation = self.generation;
        let incarnation = self.incarnation;
        let authentication_cancellation = self.cancellation();
        let settlement_cleanup = Arc::clone(&self.settlement);
        let Some((participant, settlement_participant)) =
            inner.register_connect_task(source_id, generation, construction_policy)
        else {
            self.cancel();
            return OperationAbort {
                abort: None,
                request_cancellation: None,
            };
        };
        let cleanup_inner = Arc::clone(&inner);
        inner.runtime.spawn(async move {
            settlement_cleanup.wait().await;
            let mut state = lock(&cleanup_inner.state);
            let Some(entry) = state.entries.get_mut(&source_id) else {
                return;
            };
            let is_exact = entry
                .connect_settlements
                .get(&generation)
                .is_some_and(|current| Arc::ptr_eq(current, &settlement_cleanup));
            if is_exact {
                entry.connect_settlements.remove(&generation);
            }
        });
        let task_inner = Arc::clone(&inner);
        let (start_task, task_started) = oneshot::channel();
        let join = inner.runtime.spawn(async move {
            let _participant = participant;
            let _settlement_participant = settlement_participant;
            if task_started.await.is_err() {
                return;
            }
            if authentication_cancellation.is_cancelled() {
                self.cancel();
                return;
            }
            let authentication =
                AssertUnwindSafe(async move { authenticate(authentication_cancellation).await })
                    .catch_unwind()
                    .await;
            match authentication {
                Ok(AdapterTaskResult::Constructed(adapter)) => {
                    let staged = self.stage_constructed(ConstructedAdapter::from_box(adapter));
                    if staged.cancellation().is_cancelled()
                        || !task_inner.make_connect_abortable(source_id, generation)
                    {
                        staged.cancel();
                        return;
                    }
                    if staged.cancellation().is_cancelled() {
                        staged.cancel();
                        return;
                    }
                    let adapter = staged.adapter();
                    let cancellation = staged.cancellation();
                    let result =
                        AssertUnwindSafe(async move { catalogue(adapter, cancellation).await })
                            .catch_unwind()
                            .await;
                    match result {
                        Ok(RefreshTaskResult::Refreshed(snapshot)) => {
                            staged.complete(snapshot);
                        }
                        Ok(RefreshTaskResult::Failed(category)) => {
                            staged.fail(category);
                        }
                        Ok(RefreshTaskResult::Cancelled) => {
                            staged.cancel();
                        }
                        Err(_) => {
                            staged.fail(FailureCategory::Backend);
                        }
                    }
                }
                Ok(AdapterTaskResult::Failed(category)) => {
                    self.fail(category);
                }
                Ok(AdapterTaskResult::Cancelled) => {
                    self.cancel();
                }
                Err(_) => {
                    self.fail(FailureCategory::Backend);
                }
            }
        });
        let abort = join.abort_handle();
        drop(join);
        inner.attach_connect_abort(
            source_id,
            generation,
            abort.clone(),
            construction_policy.abortable(),
        );
        let _ = start_task.send(());
        let cancellation_inner = Arc::clone(&inner);
        OperationAbort {
            abort: Some(abort),
            request_cancellation: Some(Box::new(move || {
                cancellation_inner.abandon_connect(incarnation, source_id, generation);
            })),
        }
    }
}

impl<A: LifecycleAdapter + ?Sized, S> Drop for ConnectOwner<A, S> {
    fn drop(&mut self) {
        if !self.completed {
            self.completed = true;
            self.inner
                .abandon_connect(self.incarnation, self.source_id, self.generation);
        }
    }
}

/// Mandatory retirement guard for an adapter that exists before its first
/// complete catalogue snapshot. It is intentionally non-cloneable.
struct StagedConnect<A: LifecycleAdapter + ?Sized, S: Send + Sync + 'static> {
    owner: Option<ConnectOwner<A, S>>,
    adapter: Option<ConstructedAdapter<A>>,
}

impl<A: LifecycleAdapter + ?Sized, S: Send + Sync + 'static> StagedConnect<A, S> {
    fn adapter(&self) -> Arc<A> {
        self.adapter
            .as_ref()
            .expect("staged adapter present")
            .operational()
    }

    fn cancellation(&self) -> CancellationObserver {
        self.owner
            .as_ref()
            .expect("staged owner present")
            .cancellation()
    }

    fn complete(mut self, snapshot: S) -> ConnectSubmission {
        let owner = self.owner.take().expect("staged owner present");
        let adapter = self.adapter.take().expect("staged adapter present");
        owner.submit_constructed(adapter, snapshot)
    }

    fn fail(mut self, category: FailureCategory) -> bool {
        let owner = self.owner.take().expect("staged owner present");
        let adapter = self.adapter.take().expect("staged adapter present");
        owner.reject_adapter(adapter, Some(category))
    }

    fn cancel(mut self) -> bool {
        let owner = self.owner.take().expect("staged owner present");
        let adapter = self.adapter.take().expect("staged adapter present");
        owner.reject_adapter(adapter, None)
    }
}

impl<A: LifecycleAdapter + ?Sized, S: Send + Sync + 'static> Drop for StagedConnect<A, S> {
    fn drop(&mut self) {
        if let (Some(owner), Some(adapter)) = (self.owner.take(), self.adapter.take()) {
            owner.reject_adapter(adapter, None);
        }
    }
}

/// Non-cloneable exact owner for one refresh lane and session epoch.
pub struct RefreshOwner<A: LifecycleAdapter + ?Sized, S> {
    inner: Arc<RegistryInner<A, S>>,
    incarnation: Uuid,
    source_id: SourceId,
    lane: RefreshLane,
    generation: u64,
    session_epoch: u64,
    session: SessionHandle<A>,
    cancellation: CancellationObserver,
    completed: bool,
}

impl<A: LifecycleAdapter + ?Sized, S> RefreshOwner<A, S> {
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn lane(&self) -> &RefreshLane {
        &self.lane
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn session_epoch(&self) -> u64 {
        self.session_epoch
    }

    pub fn cancellation(&self) -> CancellationObserver {
        self.cancellation.clone()
    }

    /// Exact operational adapter/lease/epoch captured atomically when this
    /// refresh generation began.
    fn session(&self) -> SessionHandle<A> {
        self.session.clone()
    }

    fn submit(mut self, snapshot: S) -> bool {
        let accepted = self.inner.finish_refresh_success(
            self.incarnation,
            self.source_id,
            &self.lane,
            self.generation,
            self.session_epoch,
            snapshot,
        );
        self.completed = true;
        accepted
    }

    fn fail(mut self, category: FailureCategory) -> bool {
        let accepted = self.inner.finish_refresh_failure(
            self.incarnation,
            self.source_id,
            &self.lane,
            self.generation,
            self.session_epoch,
            category,
        );
        self.completed = true;
        accepted
    }

    fn cancel(mut self) -> bool {
        let accepted = self.inner.abandon_refresh(
            self.incarnation,
            self.source_id,
            &self.lane,
            self.generation,
            self.session_epoch,
        );
        self.completed = true;
        accepted
    }

    pub fn spawn<F, Fut>(self, work: F) -> OperationAbort
    where
        A: 'static,
        S: Send + Sync + 'static,
        F: FnOnce(SessionHandle<A>, CancellationObserver) -> Fut + Send + 'static,
        Fut: Future<Output = RefreshTaskResult<S>> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        let source_id = self.source_id;
        let lane = self.lane.clone();
        let generation = self.generation;
        let incarnation = self.incarnation;
        let session = self.session();
        let session_epoch = session.session_epoch();
        let cancellation = self.cancellation();
        let Some(participant) = inner.register_refresh_task(source_id, &lane, generation) else {
            self.cancel();
            return OperationAbort {
                abort: None,
                request_cancellation: None,
            };
        };
        let (start_task, task_started) = oneshot::channel();
        let join = inner.runtime.spawn(async move {
            let _participant = participant;
            if task_started.await.is_err() {
                return;
            }
            let result = AssertUnwindSafe(async move { work(session, cancellation).await })
                .catch_unwind()
                .await;
            match result {
                Ok(RefreshTaskResult::Refreshed(snapshot)) => {
                    self.submit(snapshot);
                }
                Ok(RefreshTaskResult::Failed(category)) => {
                    self.fail(category);
                }
                Ok(RefreshTaskResult::Cancelled) => {
                    self.cancel();
                }
                Err(_) => {
                    self.fail(FailureCategory::Backend);
                }
            }
        });
        let abort = join.abort_handle();
        drop(join);
        inner.attach_refresh_abort(source_id, &lane, generation, abort.clone());
        let _ = start_task.send(());
        let cancellation_inner = Arc::clone(&inner);
        OperationAbort {
            abort: Some(abort),
            request_cancellation: Some(Box::new(move || {
                cancellation_inner.abandon_refresh(
                    incarnation,
                    source_id,
                    &lane,
                    generation,
                    session_epoch,
                );
            })),
        }
    }
}

impl<A: LifecycleAdapter + ?Sized, S> Drop for RefreshOwner<A, S> {
    fn drop(&mut self) {
        if !self.completed {
            self.completed = true;
            self.inner.abandon_refresh(
                self.incarnation,
                self.source_id,
                &self.lane,
                self.generation,
                self.session_epoch,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::time::{timeout, Duration};

    struct CloseProbe {
        calls: AtomicUsize,
        completions: watch::Sender<usize>,
    }

    impl CloseProbe {
        fn new() -> Arc<Self> {
            let (completions, _receiver) = watch::channel(0);
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                completions,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Acquire)
        }

        async fn wait_for_completions(&self, expected: usize) {
            let mut receiver = self.completions.subscribe();
            timeout(Duration::from_secs(2), async {
                loop {
                    if *receiver.borrow_and_update() >= expected {
                        return;
                    }
                    receiver.changed().await.expect("completion sender alive");
                }
            })
            .await
            .expect("adapter close completed");
        }

        async fn wait_for_calls(&self, expected: usize) {
            timeout(Duration::from_secs(2), async {
                while self.calls() < expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("adapter close started");
        }
    }

    struct TestAdapter {
        probe: Arc<CloseProbe>,
        release: watch::Receiver<bool>,
        result: Result<(), FailureCategory>,
    }

    impl LifecycleAdapter for TestAdapter {
        fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
            self.probe.calls.fetch_add(1, Ordering::AcqRel);
            let mut release = self.release.clone();
            Box::pin(async move {
                while !*release.borrow_and_update() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
                let next = *self.probe.completions.borrow() + 1;
                self.probe.completions.send_replace(next);
                self.result
            })
        }
    }

    struct AdapterFixture {
        adapter: Option<TestAdapter>,
        probe: Arc<CloseProbe>,
        release: watch::Sender<bool>,
    }

    impl AdapterFixture {
        fn immediate() -> Self {
            Self::new(true, Ok(()))
        }

        fn held() -> Self {
            Self::new(false, Ok(()))
        }

        fn failing(category: FailureCategory) -> Self {
            Self::new(true, Err(category))
        }

        fn new(released: bool, result: Result<(), FailureCategory>) -> Self {
            let probe = CloseProbe::new();
            let (release, receiver) = watch::channel(released);
            Self {
                adapter: Some(TestAdapter {
                    probe: Arc::clone(&probe),
                    release: receiver,
                    result,
                }),
                probe,
                release,
            }
        }

        fn allow_close(&self) {
            self.release.send_replace(true);
        }

        fn take(&mut self) -> ConstructedAdapter<TestAdapter> {
            ConstructedAdapter::new(self.adapter.take().expect("fixture adapter available"))
        }

        fn take_raw(&mut self) -> TestAdapter {
            self.adapter.take().expect("fixture adapter available")
        }

        fn matches(&self, session: &SessionHandle<TestAdapter>) -> bool {
            Arc::ptr_eq(&session.adapter().probe, &self.probe)
        }
    }

    type Registry = SourceLifecycleRegistry<TestAdapter, Vec<&'static str>>;

    fn registry() -> Registry {
        Registry::new(Handle::current())
    }

    fn claim(
        registry: &Registry,
        source_id: SourceId,
        contribution: SourceProvenance,
    ) -> ProvenanceClaimId {
        registry
            .claim_provenance(source_id, contribution)
            .expect("provenance claim")
    }

    fn adopt(
        registry: &Registry,
        source_id: SourceId,
        adapter: &mut AdapterFixture,
        snapshot: Vec<&'static str>,
    ) -> (u64, MediaLease) {
        let owner = registry.begin_connect(source_id).expect("connect owner");
        match owner.submit_constructed(adapter.take(), snapshot) {
            ConnectSubmission::Adopted {
                session_epoch,
                lease,
            } => (session_epoch, lease),
            ConnectSubmission::Rejected => panic!("initial adapter rejected"),
        }
    }

    async fn shutdown_immediate(registry: &Registry) {
        let barrier = registry.shutdown();
        timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("shutdown completed");
    }

    #[tokio::test]
    async fn cancellation_observer_wakes_when_cancelled_before_wait() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let owner = registry.begin_connect(source_id).expect("connect owner");
        let mut cancellation = owner.cancellation();

        drop(owner);

        assert!(cancellation.is_cancelled());
        timeout(Duration::from_secs(1), cancellation.cancelled())
            .await
            .expect("cancel-before-wait is wakeable");
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Dormant
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn stale_constructed_adapter_is_retired_and_cannot_replace_current() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let stale = registry.begin_connect(source_id).expect("stale owner");
        let stale_cancellation = stale.cancellation();
        let current = registry.begin_connect(source_id).expect("current owner");
        let mut rejected = AdapterFixture::immediate();
        let mut accepted = AdapterFixture::immediate();

        assert!(stale_cancellation.is_cancelled());
        assert!(matches!(
            stale.submit_constructed(rejected.take(), vec!["stale"]),
            ConnectSubmission::Rejected
        ));
        assert!(matches!(
            current.submit_constructed(accepted.take(), vec!["current"]),
            ConnectSubmission::Adopted { .. }
        ));
        rejected.probe.wait_for_completions(1).await;
        assert_eq!(rejected.probe.calls(), 1);
        let session = registry.session(source_id).expect("current session");
        assert!(accepted.matches(&session));
        assert_eq!(
            registry
                .snapshot(source_id)
                .expect("source")
                .catalogue
                .expect("catalogue")
                .value
                .as_ref(),
            &vec!["current"]
        );

        shutdown_immediate(&registry).await;
        accepted.probe.wait_for_completions(1).await;
        assert_eq!(accepted.probe.calls(), 1);
    }

    #[tokio::test]
    async fn replacement_atomically_revokes_and_retires_predecessor() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        let mut successor = AdapterFixture::immediate();
        let (first_epoch, predecessor_lease) =
            adopt(&registry, source_id, &mut predecessor, vec!["first"]);
        let replacement = registry.begin_connect(source_id).expect("replacement");

        let ConnectSubmission::Adopted {
            session_epoch,
            lease: successor_lease,
        } = replacement.submit_constructed(successor.take(), vec!["second"])
        else {
            panic!("replacement rejected");
        };

        assert_ne!(first_epoch, session_epoch);
        assert!(!predecessor_lease.is_active());
        assert!(successor_lease.is_active());
        assert_eq!(predecessor.probe.calls(), 1);
        let before_close = registry.snapshot(source_id).expect("source");
        assert_eq!(before_close.pending_retirements, 1);
        let mut changes = registry.subscribe();
        assert!(successor.matches(&registry.session(source_id).expect("successor")));
        predecessor.allow_close();
        predecessor.probe.wait_for_completions(1).await;
        let after_close = timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = registry.snapshot(source_id).expect("source");
                if snapshot.pending_retirements == 0 {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement retirement finalized");
        assert!(after_close.revision > before_close.revision);
        assert_eq!(after_close.state, SourceState::Ready);
        assert!(after_close.failure.is_none());
        let mut retirement_event = false;
        while let Ok(change) = changes.try_recv() {
            retirement_event |= matches!(
                change.change,
                LifecycleChange::SessionRetired {
                    session_epoch,
                    failure: None,
                } if session_epoch == first_epoch
            );
        }
        assert!(retirement_event);

        shutdown_immediate(&registry).await;
        assert_eq!(predecessor.probe.calls(), 1);
        assert_eq!(successor.probe.calls(), 1);
    }

    #[tokio::test]
    async fn failed_replacement_restores_predecessor_with_closed_failure() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::immediate();
        let (epoch, lease) = adopt(&registry, source_id, &mut adapter, vec!["stable"]);
        let replacement = registry.begin_connect(source_id).expect("replacement");
        let replacement_generation = replacement.generation();

        assert!(replacement.fail(FailureCategory::AuthenticationRejected));

        let snapshot = registry.snapshot(source_id).expect("source");
        assert_eq!(snapshot.state, SourceState::Ready);
        assert_eq!(snapshot.session_epoch, Some(epoch));
        assert_eq!(
            snapshot.failure,
            Some(CorrelatedFailure {
                correlation: OperationCorrelation {
                    generation: replacement_generation,
                    session_epoch: Some(epoch),
                },
                failure: SourceFailure::connect(FailureCategory::AuthenticationRejected),
            })
        );
        assert!(lease.is_active());
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn independent_views_keep_exact_accepted_generations() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::BuiltIn);
        let mut adapter = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut adapter, vec!["catalogue"]);
        let left_lane = RefreshLane::View(ViewOrigin::radio("left").expect("view"));
        let right_lane = RefreshLane::View(ViewOrigin::radio("right").expect("view"));
        let stale_left = registry
            .begin_refresh(source_id, left_lane.clone())
            .expect("stale left");
        let right = registry
            .begin_refresh(source_id, right_lane.clone())
            .expect("right");
        let current_left = registry
            .begin_refresh(source_id, left_lane.clone())
            .expect("current left");
        let left_generation = current_left.generation();
        let right_generation = right.generation();

        assert!(stale_left.cancellation().is_cancelled());
        assert!(!stale_left.submit(vec!["stale"]));
        assert!(current_left.submit(vec!["left"]));
        assert!(right.submit(vec!["right"]));

        let snapshot = registry.snapshot(source_id).expect("source");
        assert_eq!(snapshot.state, SourceState::Ready);
        assert_eq!(
            snapshot
                .views
                .get(match &left_lane {
                    RefreshLane::View(view) => view,
                    RefreshLane::Catalogue => unreachable!(),
                })
                .expect("left")
                .generation,
            left_generation
        );
        assert_eq!(
            snapshot
                .views
                .get(match &right_lane {
                    RefreshLane::View(view) => view,
                    RefreshLane::Catalogue => unreachable!(),
                })
                .expect("right")
                .generation,
            right_generation
        );
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn refresh_failure_retains_complete_snapshot_and_other_lane() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::BuiltIn);
        let mut adapter = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut adapter, vec!["old-catalogue"]);
        let view = ViewOrigin::radio("near-me").expect("view");
        let view_lane = RefreshLane::View(view.clone());
        let catalogue = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("catalogue refresh");
        let catalogue_generation = catalogue.generation();
        let view_refresh = registry
            .begin_refresh(source_id, view_lane.clone())
            .expect("view refresh");

        assert!(catalogue.fail(FailureCategory::Timeout));
        assert!(view_refresh.submit(Vec::new()));

        let snapshot = registry.snapshot(source_id).expect("source");
        assert_eq!(
            snapshot
                .catalogue
                .as_ref()
                .expect("catalogue")
                .value
                .as_ref(),
            &vec!["old-catalogue"]
        );
        assert_eq!(
            snapshot.refresh_failures.get(&RefreshLane::Catalogue),
            Some(&CorrelatedFailure {
                correlation: OperationCorrelation {
                    generation: catalogue_generation,
                    session_epoch: snapshot.session_epoch,
                },
                failure: SourceFailure::refresh(FailureCategory::Timeout),
            })
        );
        assert!(snapshot
            .views
            .get(&view)
            .expect("empty accepted view")
            .value
            .is_empty());
        assert!(!snapshot.refresh_failures.contains_key(&view_lane));
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn remove_view_cancels_pending_and_clears_snapshot_and_failure() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::BuiltIn);
        let mut adapter = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut adapter, vec![]);
        let view = ViewOrigin::radio("top-clicked").expect("view");
        let lane = RefreshLane::View(view.clone());
        assert!(registry
            .begin_refresh(source_id, lane.clone())
            .expect("initial refresh")
            .submit(vec!["station"]));
        assert!(registry
            .begin_refresh(source_id, lane.clone())
            .expect("failed refresh")
            .fail(FailureCategory::Timeout));
        let pending = registry
            .begin_refresh(source_id, lane.clone())
            .expect("pending refresh");
        let cancellation = pending.cancellation();

        assert!(registry.remove_view(source_id, &view));
        assert!(cancellation.is_cancelled());
        assert!(!pending.submit(vec!["late"]));
        let snapshot = registry.snapshot(source_id).expect("source");
        assert!(!snapshot.views.contains_key(&view));
        assert!(!snapshot.refresh_failures.contains_key(&lane));
        assert_eq!(snapshot.state, SourceState::Ready);
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn cancel_refresh_retains_last_snapshot_without_failure() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut adapter, vec!["first"]);
        let refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("refresh");
        let generation = refresh.generation();

        assert!(registry.cancel_refresh(source_id, &RefreshLane::Catalogue));
        assert!(!refresh.submit(vec!["late"]));
        let snapshot = registry.snapshot(source_id).expect("source");
        assert_eq!(
            snapshot.catalogue.expect("catalogue").value.as_ref(),
            &vec!["first"]
        );
        assert!(!snapshot
            .refresh_failures
            .contains_key(&RefreshLane::Catalogue));
        assert!(!snapshot
            .pending_refreshes
            .values()
            .any(|value| *value == generation));
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn saved_and_discovered_provenance_demotes_without_disconnect() {
        let registry = registry();
        let source_id = SourceId::random();
        let saved_claim = claim(&registry, source_id, SourceProvenance::Saved);
        let discovery_claim = claim(&registry, source_id, SourceProvenance::Discovery);
        let mut adapter = AdapterFixture::immediate();
        let (_, lease) = adopt(&registry, source_id, &mut adapter, vec![]);

        assert!(registry.release_provenance(source_id, saved_claim));
        let demoted = registry.snapshot(source_id).expect("demoted source");
        assert_eq!(demoted.retention, Retention::Ephemeral);
        assert_eq!(demoted.visibility, SourceVisibility::Visible);
        assert_eq!(demoted.state, SourceState::Ready);
        assert!(lease.is_active());
        assert_eq!(adapter.probe.calls(), 0);

        assert!(registry.release_provenance(source_id, discovery_claim));
        adapter.probe.wait_for_completions(1).await;
        assert!(!lease.is_active());
        assert_eq!(
            registry.snapshot(source_id).expect("retired source").state,
            SourceState::Retired
        );
        let reactivated_claim = claim(&registry, source_id, SourceProvenance::Discovery);
        assert_eq!(
            registry
                .snapshot(source_id)
                .expect("reactivated source")
                .state,
            SourceState::Dormant
        );
        assert!(registry.release_provenance(source_id, reactivated_claim));
        assert_eq!(registry.prune_retired(), 1);
        assert!(registry.snapshot(source_id).is_none());
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn external_provenance_is_hidden_and_ephemeral() {
        let registry = registry();
        let source_id = SourceId::external();
        let external_claim = claim(&registry, source_id, SourceProvenance::External);
        let snapshot = registry.snapshot(source_id).expect("external source");
        assert_eq!(snapshot.visibility, SourceVisibility::Hidden);
        assert_eq!(snapshot.retention, Retention::Ephemeral);
        assert!(registry.release_provenance(source_id, external_claim));
        assert_eq!(
            registry.snapshot(source_id).expect("external source").state,
            SourceState::Retired
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn every_provenance_contribution_has_derived_visibility_and_retention() {
        let retained = [
            SourceProvenance::Saved,
            SourceProvenance::Environment,
            SourceProvenance::BuiltIn,
        ];
        let ephemeral_visible = [SourceProvenance::Discovery, SourceProvenance::Removable];
        for contribution in retained {
            let registry = registry();
            let source_id = SourceId::random();
            claim(&registry, source_id, contribution);
            let snapshot = registry.snapshot(source_id).expect("source");
            assert_eq!(snapshot.retention, Retention::Retained);
            assert_eq!(snapshot.visibility, SourceVisibility::Visible);
            assert!(registry.shutdown().is_complete());
        }
        for contribution in ephemeral_visible {
            let registry = registry();
            let source_id = SourceId::random();
            claim(&registry, source_id, contribution);
            let snapshot = registry.snapshot(source_id).expect("source");
            assert_eq!(snapshot.retention, Retention::Ephemeral);
            assert_eq!(snapshot.visibility, SourceVisibility::Visible);
            assert!(registry.shutdown().is_complete());
        }
    }

    #[tokio::test]
    async fn unspawned_owner_cannot_hold_or_reopen_shutdown_barrier() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let owner = registry.begin_connect(source_id).expect("connect owner");
        let cancellation = owner.cancellation();
        let barrier = registry.shutdown();

        assert!(cancellation.is_cancelled());
        assert!(barrier.is_complete());
        drop(owner);
        timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("unspawned owner is not a task");
        assert!(barrier.is_complete());
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Retired
        );
    }

    #[tokio::test]
    async fn repeated_shutdown_shares_one_barrier_and_close_owner() {
        let registry = registry();
        let left_id = SourceId::random();
        let right_id = SourceId::random();
        claim(&registry, left_id, SourceProvenance::Saved);
        claim(&registry, right_id, SourceProvenance::Saved);
        let mut left = AdapterFixture::held();
        let mut right = AdapterFixture::held();
        adopt(&registry, left_id, &mut left, vec![]);
        adopt(&registry, right_id, &mut right, vec![]);

        let first = registry.shutdown();
        let repeated = registry.shutdown();

        assert_eq!(left.probe.calls(), 1);
        assert_eq!(right.probe.calls(), 1);
        assert_eq!(first.pending_operations(), 2);
        assert_eq!(repeated.pending_operations(), 2);
        left.allow_close();
        right.allow_close();
        let ((), ()) = tokio::join!(first.wait(), repeated.wait());
        assert_eq!(left.probe.calls(), 1);
        assert_eq!(right.probe.calls(), 1);
        assert_eq!(
            registry.snapshot(left_id).expect("left").state,
            SourceState::Retired
        );
        assert_eq!(
            registry.snapshot(right_id).expect("right").state,
            SourceState::Retired
        );
    }

    #[tokio::test]
    async fn predecessor_close_completion_cannot_finish_successor_disconnect() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        let mut successor = AdapterFixture::held();
        adopt(&registry, source_id, &mut predecessor, vec!["first"]);
        assert!(matches!(
            registry
                .begin_connect(source_id)
                .expect("replacement")
                .submit_constructed(successor.take(), vec!["second"]),
            ConnectSubmission::Adopted { .. }
        ));
        assert!(registry.disconnect(source_id).is_some());
        assert_eq!(predecessor.probe.calls(), 1);
        assert_eq!(successor.probe.calls(), 1);

        predecessor.allow_close();
        predecessor.probe.wait_for_completions(1).await;
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Disconnecting
        );
        successor.allow_close();
        successor.probe.wait_for_completions(1).await;
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Dormant
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn shutdown_aborts_and_joins_tracked_noncooperative_connect_task() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let operation = registry
            .begin_connect(source_id)
            .expect("connect owner")
            .spawn_staged(
                ConstructionCancellationPolicy::Abortable,
                |_cancellation| async {
                    std::future::pending::<AdapterTaskResult<TestAdapter>>().await
                },
                |_adapter, _cancellation| async { RefreshTaskResult::Refreshed(Vec::new()) },
            );

        let barrier = registry.shutdown();
        timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("aborted task joined");
        assert!(operation.is_finished());
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Retired
        );
    }

    #[tokio::test]
    async fn close_failure_is_sanitized_and_never_resurrects_session() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::failing(FailureCategory::Timeout);
        let (epoch, lease) = adopt(&registry, source_id, &mut adapter, vec![]);

        assert!(registry.disconnect(source_id).is_some());
        adapter.probe.wait_for_completions(1).await;

        let snapshot = registry.snapshot(source_id).expect("source");
        assert!(!lease.is_active());
        assert_eq!(snapshot.state, SourceState::Dormant);
        let failure = snapshot.failure.expect("disconnect failure");
        assert_eq!(failure.correlation.session_epoch, Some(epoch));
        assert_eq!(
            failure.failure,
            SourceFailure::disconnect(FailureCategory::Timeout)
        );
        assert!(registry.session(source_id).is_none());
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn pruning_cannot_let_stale_owner_capture_recreated_source() {
        let registry = registry();
        let source_id = SourceId::random();
        let discovery_claim = claim(&registry, source_id, SourceProvenance::Discovery);
        let stale = registry.begin_connect(source_id).expect("stale owner");
        assert!(registry.release_provenance(source_id, discovery_claim));
        assert_eq!(registry.prune_retired(), 1);
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut current_adapter = AdapterFixture::immediate();
        let current = registry.begin_connect(source_id).expect("current owner");
        assert!(matches!(
            current.submit_constructed(current_adapter.take(), vec!["current"]),
            ConnectSubmission::Adopted { .. }
        ));
        let mut stale_adapter = AdapterFixture::immediate();

        assert!(matches!(
            stale.submit_constructed(stale_adapter.take(), vec!["stale"]),
            ConnectSubmission::Rejected
        ));
        stale_adapter.probe.wait_for_completions(1).await;
        assert!(current_adapter.matches(&registry.session(source_id).expect("current session")));
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn dropping_operation_capability_is_inert_and_task_can_adopt() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::immediate();
        let adapter = fixture.take_raw();
        let (started, started_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |_cancellation| async move {
                    let _ = started.send(());
                    let _ = release_rx.await;
                    AdapterTaskResult::constructed(adapter)
                },
                |_adapter, _cancellation| async { RefreshTaskResult::Refreshed(vec!["catalogue"]) },
            );
        started_rx.await.expect("constructor started");

        drop(operation);
        release.send(()).expect("release constructor");
        timeout(Duration::from_secs(2), async {
            while registry.snapshot(source_id).expect("source").state != SourceState::Ready {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task retained by registry");
        assert!(fixture.matches(&registry.session(source_id).expect("session")));
        shutdown_immediate(&registry).await;
        assert_eq!(fixture.probe.calls(), 1);
    }

    #[tokio::test]
    async fn protected_constructor_finishes_after_cancel_then_retires_exactly_once() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        let adapter = fixture.take_raw();
        let catalogue_calls = Arc::new(AtomicUsize::new(0));
        let catalogue_calls_task = Arc::clone(&catalogue_calls);
        let (minted, minted_rx) = oneshot::channel();
        let (finish_constructor, finish_constructor_rx) = oneshot::channel();
        let operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |cancellation| async move {
                    let _ = minted.send(());
                    let _ = finish_constructor_rx.await;
                    assert!(cancellation.is_cancelled());
                    AdapterTaskResult::constructed(adapter)
                },
                move |_adapter, _cancellation| async move {
                    catalogue_calls_task.fetch_add(1, Ordering::AcqRel);
                    RefreshTaskResult::Refreshed(Vec::new())
                },
            );
        minted_rx.await.expect("remote session minted");

        operation.abort();
        let barrier = registry.shutdown();
        assert!(!barrier.is_complete());
        finish_constructor.send(()).expect("finish constructor");
        fixture.probe.wait_for_calls(1).await;
        assert_eq!(catalogue_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.probe.calls(), 1);
        assert!(!barrier.is_complete());

        fixture.allow_close();
        timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("late adapter retirement joined");
        assert_eq!(fixture.probe.calls(), 1);
    }

    #[tokio::test]
    async fn disconnect_waiter_joins_protected_constructor_and_rejected_close() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        let adapter = fixture.take_raw();
        let catalogue_calls = Arc::new(AtomicUsize::new(0));
        let catalogue_calls_task = Arc::clone(&catalogue_calls);
        let (constructor_started, constructor_started_rx) = oneshot::channel();
        let (finish_constructor, finish_constructor_rx) = oneshot::channel();
        let _operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |cancellation| async move {
                    let _ = constructor_started.send(());
                    let _ = finish_constructor_rx.await;
                    assert!(cancellation.is_cancelled());
                    AdapterTaskResult::constructed(adapter)
                },
                move |_adapter, _cancellation| async move {
                    catalogue_calls_task.fetch_add(1, Ordering::AcqRel);
                    RefreshTaskResult::Refreshed(Vec::new())
                },
            );
        constructor_started_rx.await.expect("constructor started");

        let waiter = registry.disconnect(source_id).expect("disconnect");
        let repeated = registry.disconnect(source_id).expect("repeated disconnect");
        assert_eq!(waiter.retirement_id(), repeated.retirement_id());
        assert!(!waiter.is_complete());
        assert!(!repeated.is_complete());

        finish_constructor.send(()).expect("finish constructor");
        fixture.probe.wait_for_calls(1).await;
        assert_eq!(catalogue_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.probe.calls(), 1);
        assert!(!waiter.is_complete());
        assert!(!repeated.is_complete());

        fixture.allow_close();
        let (left, right) = tokio::join!(waiter.wait(), repeated.wait());
        assert_eq!(left, None);
        assert_eq!(right, None);
        assert_eq!(fixture.probe.calls(), 1);
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Dormant
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn disconnect_waiter_joins_superseded_and_current_protected_settlements() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut superseded = AdapterFixture::held();
        let mut current = AdapterFixture::held();
        let superseded_adapter = superseded.take_raw();
        let current_adapter = current.take_raw();
        let catalogue_calls = Arc::new(AtomicUsize::new(0));

        let (superseded_started, superseded_started_rx) = oneshot::channel();
        let (finish_superseded, finish_superseded_rx) = oneshot::channel();
        let superseded_catalogue_calls = Arc::clone(&catalogue_calls);
        let _superseded_operation = registry
            .begin_connect(source_id)
            .expect("superseded connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |cancellation| async move {
                    let _ = superseded_started.send(());
                    let _ = finish_superseded_rx.await;
                    assert!(cancellation.is_cancelled());
                    AdapterTaskResult::constructed(superseded_adapter)
                },
                move |_adapter, _cancellation| async move {
                    superseded_catalogue_calls.fetch_add(1, Ordering::AcqRel);
                    RefreshTaskResult::Refreshed(Vec::new())
                },
            );
        superseded_started_rx
            .await
            .expect("superseded constructor started");

        let (current_started, current_started_rx) = oneshot::channel();
        let (finish_current, finish_current_rx) = oneshot::channel();
        let current_catalogue_calls = Arc::clone(&catalogue_calls);
        let _current_operation = registry
            .begin_connect(source_id)
            .expect("current connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |cancellation| async move {
                    let _ = current_started.send(());
                    let _ = finish_current_rx.await;
                    assert!(cancellation.is_cancelled());
                    AdapterTaskResult::constructed(current_adapter)
                },
                move |_adapter, _cancellation| async move {
                    current_catalogue_calls.fetch_add(1, Ordering::AcqRel);
                    RefreshTaskResult::Refreshed(Vec::new())
                },
            );
        current_started_rx
            .await
            .expect("current constructor started");

        let waiter = registry.disconnect(source_id).expect("disconnect current");
        assert!(!waiter.is_complete());
        finish_current.send(()).expect("finish current constructor");
        current.probe.wait_for_calls(1).await;
        finish_superseded
            .send(())
            .expect("finish superseded constructor");
        superseded.probe.wait_for_calls(1).await;
        assert_eq!(catalogue_calls.load(Ordering::Acquire), 0);

        current.allow_close();
        current.probe.wait_for_completions(1).await;
        assert!(
            !waiter.is_complete(),
            "late superseded close remains part of the exact disconnect"
        );
        superseded.allow_close();
        assert_eq!(waiter.wait().await, None);
        assert_eq!(superseded.probe.calls(), 1);
        assert_eq!(current.probe.calls(), 1);
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn disconnect_waiter_reports_late_rejected_adapter_close_failure() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::failing(FailureCategory::Timeout);
        let adapter = fixture.take_raw();
        let (constructor_started, constructor_started_rx) = oneshot::channel();
        let (finish_constructor, finish_constructor_rx) = oneshot::channel();
        let _operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |cancellation| async move {
                    let _ = constructor_started.send(());
                    let _ = finish_constructor_rx.await;
                    assert!(cancellation.is_cancelled());
                    AdapterTaskResult::constructed(adapter)
                },
                |_adapter, _cancellation| async { RefreshTaskResult::Refreshed(Vec::new()) },
            );
        constructor_started_rx.await.expect("constructor started");

        let waiter = registry.disconnect(source_id).expect("disconnect");
        assert!(!waiter.is_complete());
        finish_constructor.send(()).expect("finish constructor");
        assert_eq!(
            waiter.wait().await,
            Some(SourceFailure::disconnect(FailureCategory::Timeout))
        );
        assert_eq!(fixture.probe.calls(), 1);
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn final_claim_release_prunes_after_settlement_only_disconnect() {
        let registry = registry();
        let source_id = SourceId::random();
        let saved = claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        let adapter = fixture.take_raw();
        let (constructor_started, constructor_started_rx) = oneshot::channel();
        let (finish_constructor, finish_constructor_rx) = oneshot::channel();
        let _operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |cancellation| async move {
                    let _ = constructor_started.send(());
                    let _ = finish_constructor_rx.await;
                    assert!(cancellation.is_cancelled());
                    AdapterTaskResult::constructed(adapter)
                },
                |_adapter, _cancellation| async { RefreshTaskResult::Refreshed(Vec::new()) },
            );
        constructor_started_rx.await.expect("constructor started");

        let waiter = registry.disconnect(source_id).expect("explicit disconnect");
        assert!(!waiter.is_complete());
        assert!(registry.release_provenance(source_id, saved));
        assert_eq!(
            registry.snapshot(source_id).expect("retiring source").state,
            SourceState::Retired
        );
        registry.schedule_prune_after_current_retirement(source_id);

        finish_constructor.send(()).expect("finish constructor");
        fixture.probe.wait_for_calls(1).await;
        assert!(!waiter.is_complete());
        fixture.allow_close();
        assert_eq!(waiter.wait().await, None);
        timeout(Duration::from_secs(2), async {
            while registry.snapshot(source_id).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("settled provenance-free source pruned");
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn protected_constructor_supersession_waits_then_retires_without_catalogue() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        let adapter = fixture.take_raw();
        let catalogue_calls = Arc::new(AtomicUsize::new(0));
        let catalogue_calls_task = Arc::clone(&catalogue_calls);
        let (minted, minted_rx) = oneshot::channel();
        let (finish_constructor, finish_constructor_rx) = oneshot::channel();
        let operation = registry
            .begin_connect(source_id)
            .expect("first connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |cancellation| async move {
                    let _ = minted.send(());
                    let _ = finish_constructor_rx.await;
                    assert!(cancellation.is_cancelled());
                    AdapterTaskResult::constructed(adapter)
                },
                move |_adapter, _cancellation| async move {
                    catalogue_calls_task.fetch_add(1, Ordering::AcqRel);
                    RefreshTaskResult::Refreshed(Vec::new())
                },
            );
        minted_rx.await.expect("remote session minted");
        let successor = registry
            .begin_connect(source_id)
            .expect("superseding connect");
        drop(operation);
        let barrier = registry.shutdown();
        assert!(successor.cancellation().is_cancelled());
        assert!(!barrier.is_complete());

        finish_constructor.send(()).expect("finish constructor");
        fixture.probe.wait_for_calls(1).await;
        assert_eq!(catalogue_calls.load(Ordering::Acquire), 0);
        assert!(!barrier.is_complete());
        fixture.allow_close();
        timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("superseded retirement joined");
        assert_eq!(fixture.probe.calls(), 1);
        drop(successor);
    }

    #[tokio::test]
    async fn abort_after_stage_drops_catalogue_and_retires_once() {
        struct DropNotice(Arc<AtomicBool>);
        impl Drop for DropNotice {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        let adapter = fixture.take_raw();
        let catalogue_dropped = Arc::new(AtomicBool::new(false));
        let drop_notice = DropNotice(Arc::clone(&catalogue_dropped));
        let (catalogue_started, catalogue_started_rx) = oneshot::channel();
        let operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |_cancellation| async move { AdapterTaskResult::constructed(adapter) },
                move |_adapter, _cancellation| async move {
                    let _notice = drop_notice;
                    let _ = catalogue_started.send(());
                    std::future::pending::<RefreshTaskResult<Vec<&'static str>>>().await
                },
            );
        catalogue_started_rx.await.expect("adapter staged");

        operation.abort();
        fixture.probe.wait_for_calls(1).await;
        assert!(catalogue_dropped.load(Ordering::Acquire));
        assert_eq!(fixture.probe.calls(), 1);
        let barrier = registry.shutdown();
        assert!(!barrier.is_complete());
        fixture.allow_close();
        timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("staged retirement joined");
        assert_eq!(fixture.probe.calls(), 1);
    }

    #[tokio::test]
    async fn panic_after_stage_records_failure_and_retires_once() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::immediate();
        let adapter = fixture.take_raw();
        let operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |_cancellation| async move { AdapterTaskResult::constructed(adapter) },
                |_adapter, _cancellation| async move {
                    panic!("catalogue panic");
                    #[allow(unreachable_code)]
                    RefreshTaskResult::Refreshed(Vec::new())
                },
            );
        timeout(Duration::from_secs(2), async {
            while !operation.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panicking task completed");
        fixture.probe.wait_for_completions(1).await;
        assert_eq!(fixture.probe.calls(), 1);
        let snapshot = registry.snapshot(source_id).expect("source");
        assert_eq!(snapshot.state, SourceState::Failed);
        assert_eq!(
            snapshot.failure.expect("failure").failure,
            SourceFailure::connect(FailureCategory::Backend)
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn synchronous_authentication_invocation_panic_is_sanitized() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                |_cancellation| -> std::future::Ready<AdapterTaskResult<TestAdapter>> {
                    panic!("synchronous authentication panic")
                },
                |_adapter, _cancellation| async { RefreshTaskResult::Refreshed(Vec::new()) },
            );
        timeout(Duration::from_secs(2), async {
            while !operation.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("authentication panic caught");
        let snapshot = registry.snapshot(source_id).expect("source");
        assert_eq!(snapshot.state, SourceState::Failed);
        assert_eq!(
            snapshot.failure.expect("failure").failure,
            SourceFailure::connect(FailureCategory::Backend)
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn synchronous_catalogue_invocation_panic_retires_staged_adapter() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::immediate();
        let adapter = fixture.take_raw();
        let operation = registry
            .begin_connect(source_id)
            .expect("connect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |_cancellation| async move { AdapterTaskResult::constructed(adapter) },
                |_adapter,
                 _cancellation|
                 -> std::future::Ready<RefreshTaskResult<Vec<&'static str>>> {
                    panic!("synchronous catalogue panic")
                },
            );
        timeout(Duration::from_secs(2), async {
            while !operation.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("catalogue panic caught");
        fixture.probe.wait_for_completions(1).await;
        assert_eq!(fixture.probe.calls(), 1);
        let snapshot = registry.snapshot(source_id).expect("source");
        assert_eq!(snapshot.state, SourceState::Failed);
        assert_eq!(
            snapshot.failure.expect("failure").failure,
            SourceFailure::connect(FailureCategory::Backend)
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn synchronous_refresh_invocation_panic_retains_session_and_snapshot() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut fixture, vec!["stable"]);
        let lane = RefreshLane::Catalogue;
        let refresh = registry
            .begin_refresh(source_id, lane.clone())
            .expect("refresh");
        let refresh_generation = refresh.generation();
        let operation = refresh.spawn(
            |_session, _cancellation| -> std::future::Ready<RefreshTaskResult<Vec<&'static str>>> {
                panic!("synchronous refresh panic")
            },
        );
        timeout(Duration::from_secs(2), async {
            while !operation.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("refresh panic caught");
        let snapshot = registry.snapshot(source_id).expect("source");
        assert_eq!(snapshot.state, SourceState::Ready);
        assert_eq!(
            snapshot
                .catalogue
                .as_ref()
                .expect("catalogue")
                .value
                .as_ref(),
            &vec!["stable"]
        );
        let failure = snapshot.refresh_failures.get(&lane).expect("failure");
        assert_eq!(failure.correlation.generation, refresh_generation);
        assert_eq!(
            failure.failure,
            SourceFailure::refresh(FailureCategory::Backend)
        );
        assert_eq!(fixture.probe.calls(), 0);
        shutdown_immediate(&registry).await;
        assert_eq!(fixture.probe.calls(), 1);
    }

    #[tokio::test]
    async fn duplicate_provenance_publishers_release_only_their_exact_claim() {
        let registry = registry();
        let source_id = SourceId::random();
        let first = claim(&registry, source_id, SourceProvenance::Discovery);
        let second = claim(&registry, source_id, SourceProvenance::Discovery);
        let mut fixture = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut fixture, Vec::new());
        assert_eq!(
            registry
                .snapshot(source_id)
                .expect("source")
                .provenance
                .claim_count(SourceProvenance::Discovery),
            2
        );

        assert!(registry.release_provenance(source_id, first));
        let remaining = registry.snapshot(source_id).expect("source");
        assert_eq!(
            remaining
                .provenance
                .claim_count(SourceProvenance::Discovery),
            1
        );
        assert_eq!(remaining.state, SourceState::Ready);
        assert_eq!(fixture.probe.calls(), 0);

        assert!(registry.release_provenance(source_id, second));
        fixture.probe.wait_for_completions(1).await;
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Retired
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn discovery_reclaim_dissociates_held_close_and_successor_stays_ready() {
        let registry = registry();
        let source_id = SourceId::random();
        let first_claim = claim(&registry, source_id, SourceProvenance::Discovery);
        let mut predecessor = AdapterFixture::held();
        let (predecessor_epoch, _) = adopt(&registry, source_id, &mut predecessor, vec!["old"]);

        assert!(registry.release_provenance(source_id, first_claim));
        predecessor.probe.wait_for_calls(1).await;
        assert_eq!(
            registry.snapshot(source_id).expect("closing").state,
            SourceState::Disconnecting
        );
        let second_claim = claim(&registry, source_id, SourceProvenance::Discovery);
        assert_eq!(
            registry.snapshot(source_id).expect("reappeared").state,
            SourceState::Dormant
        );
        let mut successor = AdapterFixture::immediate();
        assert!(matches!(
            registry
                .begin_connect(source_id)
                .expect("reconnect")
                .submit_constructed(successor.take(), vec!["new"]),
            ConnectSubmission::Adopted { .. }
        ));
        let before_close = registry.snapshot(source_id).expect("successor");
        assert_eq!(before_close.pending_retirements, 1);
        let mut changes = registry.subscribe();

        predecessor.allow_close();
        predecessor.probe.wait_for_completions(1).await;
        let ready = timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = registry.snapshot(source_id).expect("successor");
                if snapshot.pending_retirements == 0 {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dissociated retirement finalized");
        assert!(ready.revision > before_close.revision);
        assert_eq!(ready.state, SourceState::Ready);
        assert!(ready.failure.is_none());
        assert!(successor.matches(&registry.session(source_id).expect("successor")));
        let mut retirement_event = false;
        while let Ok(change) = changes.try_recv() {
            retirement_event |= matches!(
                change.change,
                LifecycleChange::SessionRetired {
                    session_epoch,
                    failure: None,
                } if session_epoch == predecessor_epoch
            );
        }
        assert!(retirement_event);

        assert!(registry.release_provenance(source_id, second_claim));
        successor.probe.wait_for_completions(1).await;
        assert_eq!(predecessor.probe.calls(), 1);
        assert_eq!(successor.probe.calls(), 1);
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn repeated_disconnect_returns_same_exact_waiter_and_one_close() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        adopt(&registry, source_id, &mut fixture, Vec::new());

        let first = registry.disconnect(source_id).expect("first disconnect");
        let second = registry.disconnect(source_id).expect("repeated disconnect");
        assert_eq!(first.retirement_id(), second.retirement_id());
        assert_eq!(fixture.probe.calls(), 1);
        assert!(!first.is_complete());
        assert!(!second.is_complete());
        fixture.allow_close();
        let (left, right) = tokio::join!(first.wait(), second.wait());
        assert_eq!(left, None);
        assert_eq!(right, None);
        assert_eq!(fixture.probe.calls(), 1);
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retirement_waiter_wakes_only_after_snapshot_and_events_are_finalized() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        let (session_epoch, _) = adopt(&registry, source_id, &mut fixture, Vec::new());
        let mut changes = registry.subscribe();
        let waiter = registry.disconnect(source_id).expect("disconnect");

        fixture.allow_close();
        assert_eq!(waiter.wait().await, None);
        let finalized = registry.snapshot(source_id).expect("source");
        assert_eq!(finalized.state, SourceState::Dormant);
        assert_eq!(finalized.pending_retirements, 0);
        assert!(finalized.failure.is_none());

        let mut observed = Vec::new();
        while let Ok(change) = changes.try_recv() {
            observed.push(change);
        }
        let retired = observed
            .iter()
            .position(|change| {
                matches!(
                    change.change,
                    LifecycleChange::SessionRetired {
                        session_epoch: observed_epoch,
                        failure: None,
                    } if observed_epoch == session_epoch
                )
            })
            .expect("session-retired event visible before waiter return");
        let dormant = observed
            .iter()
            .position(|change| {
                matches!(
                    change.change,
                    LifecycleChange::StateChanged {
                        to: SourceState::Dormant,
                        ..
                    }
                )
            })
            .expect("dormant transition visible before waiter return");
        assert!(retired < dormant);
        assert!(observed[dormant].revision <= finalized.revision);
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn reconnect_then_disconnect_before_adoption_reuses_old_pending_waiter() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        adopt(&registry, source_id, &mut fixture, Vec::new());
        let first = registry.disconnect(source_id).expect("first disconnect");
        let reconnect = registry.begin_connect(source_id).expect("reconnect");

        let second = registry
            .disconnect(source_id)
            .expect("disconnect reconnect attempt");
        assert!(reconnect.cancellation().is_cancelled());
        assert_eq!(first.retirement_id(), second.retirement_id());
        assert!(!second.is_complete());
        drop(reconnect);
        fixture.allow_close();
        let (left, right) = tokio::join!(first.wait(), second.wait());
        assert_eq!(left, None);
        assert_eq!(right, None);
        assert_eq!(fixture.probe.calls(), 1);
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Dormant
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn reconnect_disconnect_joins_dissociated_predecessor_and_protected_constructor() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        let mut reconnect = AdapterFixture::held();
        adopt(&registry, source_id, &mut predecessor, Vec::new());
        let predecessor_waiter = registry
            .disconnect(source_id)
            .expect("predecessor disconnect");

        let reconnect_adapter = reconnect.take_raw();
        let (constructor_started, constructor_started_rx) = oneshot::channel();
        let (finish_constructor, finish_constructor_rx) = oneshot::channel();
        let _operation = registry
            .begin_connect(source_id)
            .expect("protected reconnect")
            .spawn_staged(
                ConstructionCancellationPolicy::FinishConstruction,
                move |cancellation| async move {
                    let _ = constructor_started.send(());
                    let _ = finish_constructor_rx.await;
                    assert!(cancellation.is_cancelled());
                    AdapterTaskResult::constructed(reconnect_adapter)
                },
                |_adapter, _cancellation| async { RefreshTaskResult::Refreshed(Vec::new()) },
            );
        constructor_started_rx.await.expect("constructor started");

        let composite = registry
            .disconnect(source_id)
            .expect("disconnect protected reconnect");
        assert_eq!(
            predecessor_waiter.retirement_id(),
            composite.retirement_id()
        );
        finish_constructor.send(()).expect("finish constructor");
        reconnect.probe.wait_for_calls(1).await;
        reconnect.allow_close();
        reconnect.probe.wait_for_completions(1).await;
        assert!(
            !composite.is_complete(),
            "dissociated predecessor close remains part of the reconnect disconnect"
        );

        predecessor.allow_close();
        let (predecessor_result, composite_result) =
            tokio::join!(predecessor_waiter.wait(), composite.wait());
        assert_eq!(predecessor_result, None);
        assert_eq!(composite_result, None);
        assert_eq!(predecessor.probe.calls(), 1);
        assert_eq!(reconnect.probe.calls(), 1);
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn successor_disconnect_waiter_is_not_completed_by_predecessor_close() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        let mut successor = AdapterFixture::held();
        adopt(&registry, source_id, &mut predecessor, vec!["old"]);
        let predecessor_waiter = registry.disconnect(source_id).expect("predecessor close");
        assert!(matches!(
            registry
                .begin_connect(source_id)
                .expect("reconnect")
                .submit_constructed(successor.take(), vec!["new"]),
            ConnectSubmission::Adopted { .. }
        ));
        let successor_waiter = registry.disconnect(source_id).expect("successor close");
        assert_ne!(
            predecessor_waiter.retirement_id(),
            successor_waiter.retirement_id()
        );
        assert_eq!(predecessor.probe.calls(), 1);
        assert_eq!(successor.probe.calls(), 1);

        predecessor.allow_close();
        assert_eq!(predecessor_waiter.wait().await, None);
        assert!(!successor_waiter.is_complete());
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Disconnecting
        );
        successor.allow_close();
        assert_eq!(successor_waiter.wait().await, None);
        assert_eq!(
            registry.snapshot(source_id).expect("source").state,
            SourceState::Dormant
        );
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn supersession_events_cancel_old_generation_before_new_start() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut changes = registry.subscribe();
        let first_connect = registry.begin_connect(source_id).expect("first connect");
        let first_connect_generation = first_connect.generation();
        let second_connect = registry.begin_connect(source_id).expect("second connect");
        let second_connect_generation = second_connect.generation();
        let mut fixture = AdapterFixture::immediate();
        assert!(matches!(
            second_connect.submit_constructed(fixture.take(), Vec::new()),
            ConnectSubmission::Adopted { .. }
        ));
        assert!(!first_connect.fail(FailureCategory::Timeout));

        let first_refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("first refresh");
        let first_refresh_generation = first_refresh.generation();
        let second_refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("second refresh");
        let second_refresh_generation = second_refresh.generation();
        drop(first_refresh);
        drop(second_refresh);

        let mut observed = Vec::new();
        while let Ok(change) = changes.try_recv() {
            observed.push(change.change);
        }
        let connect_cancelled = observed
            .iter()
            .position(|change| {
                matches!(
                    change,
                    LifecycleChange::ConnectCancelled { generation }
                        if *generation == first_connect_generation
                )
            })
            .expect("old connect cancellation");
        let connect_started = observed
            .iter()
            .position(|change| {
                matches!(
                    change,
                    LifecycleChange::ConnectStarted { generation }
                        if *generation == second_connect_generation
                )
            })
            .expect("new connect start");
        assert!(connect_cancelled < connect_started);
        let refresh_cancelled = observed
            .iter()
            .position(|change| {
                matches!(
                    change,
                    LifecycleChange::OperationCancelled { generation, .. }
                        if *generation == first_refresh_generation
                )
            })
            .expect("old refresh cancellation");
        let refresh_started = observed
            .iter()
            .position(|change| {
                matches!(
                    change,
                    LifecycleChange::RefreshStarted { generation, .. }
                        if *generation == second_refresh_generation
                )
            })
            .expect("new refresh start");
        assert!(refresh_cancelled < refresh_started);
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn failure_events_are_correlated_and_stale_attempt_cannot_clear_retry() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut changes = registry.subscribe();
        let stale = registry.begin_connect(source_id).expect("stale");
        let failed = registry.begin_connect(source_id).expect("failed");
        let failed_generation = failed.generation();
        assert!(!stale.fail(FailureCategory::Connection));
        assert!(failed.fail(FailureCategory::Timeout));
        let retained = registry
            .snapshot(source_id)
            .expect("source")
            .failure
            .expect("retained failure");
        assert_eq!(retained.correlation.generation, failed_generation);
        assert_eq!(
            retained.failure,
            SourceFailure::connect(FailureCategory::Timeout)
        );

        let retry = registry.begin_connect(source_id).expect("retry");
        let retry_generation = retry.generation();
        assert!(registry
            .snapshot(source_id)
            .expect("source")
            .failure
            .is_none());
        let mut observed = Vec::new();
        while let Ok(change) = changes.try_recv() {
            observed.push(change.change);
        }
        assert!(observed.iter().any(|change| matches!(
            change,
            LifecycleChange::FailureChanged {
                correlation: OperationCorrelation { generation, .. },
                failure: Some(SourceFailure { .. }),
                ..
            } if *generation == failed_generation
        )));
        assert!(observed.iter().any(|change| matches!(
            change,
            LifecycleChange::FailureChanged {
                correlation: OperationCorrelation { generation, .. },
                failure: None,
                ..
            } if *generation == retry_generation
        )));
        drop(retry);
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn media_resolution_attaches_exact_lease_and_rejects_stale_epoch() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        let (predecessor_epoch, _) = adopt(&registry, source_id, &mut predecessor, Vec::new());

        let request = registry
            .resolve_http(source_id, predecessor_epoch, |_adapter| async {
                ResolvedHttpRequest::new(
                    url::Url::parse("http://example.test/stream").expect("URL"),
                )
            })
            .await
            .expect("request");
        assert!(request.is_active());

        let delayed_registry = registry.clone();
        let (resolution_started, resolution_started_rx) = oneshot::channel();
        let (release_resolution, release_resolution_rx) = oneshot::channel();
        let delayed = tokio::spawn(async move {
            delayed_registry
                .resolve_http(source_id, predecessor_epoch, move |_adapter| async move {
                    let _ = resolution_started.send(());
                    let _ = release_resolution_rx.await;
                    ResolvedHttpRequest::new(
                        url::Url::parse("http://example.test/delayed").expect("URL"),
                    )
                })
                .await
        });
        resolution_started_rx.await.expect("resolution started");
        let mut successor = AdapterFixture::immediate();
        assert!(matches!(
            registry
                .begin_connect(source_id)
                .expect("replace")
                .submit_constructed(successor.take(), Vec::new()),
            ConnectSubmission::Adopted { .. }
        ));
        assert!(!request.is_active());
        release_resolution.send(()).expect("release resolution");
        assert!(delayed.await.expect("resolution task").is_err());
        predecessor.allow_close();
        predecessor.probe.wait_for_completions(1).await;
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn exact_session_operations_close_epoch_races_and_preserve_backend_category() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        let (predecessor_epoch, _) = adopt(&registry, source_id, &mut predecessor, Vec::new());

        let calls = Arc::new(AtomicUsize::new(0));
        let stale_calls = Arc::clone(&calls);
        assert!(matches!(
            registry
                .run_exact_session_operation(
                    source_id,
                    predecessor_epoch.wrapping_add(1),
                    move |_| async move {
                        stale_calls.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    },
                )
                .await,
            Err(SessionOperationError::Unavailable)
        ));
        assert_eq!(calls.load(Ordering::Acquire), 0);

        let delayed_registry = registry.clone();
        let (started, started_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let delayed = tokio::spawn(async move {
            delayed_registry
                .run_exact_session_operation(source_id, predecessor_epoch, move |_| async move {
                    let _ = started.send(());
                    let _ = release_rx.await;
                    Err::<(), _>(BackendError::ConnectionFailed {
                        message: "stale replacement detail".to_string(),
                        source: None,
                    })
                })
                .await
        });
        started_rx.await.expect("session operation started");

        let mut successor = AdapterFixture::immediate();
        let successor_epoch = match registry
            .begin_connect(source_id)
            .expect("replace")
            .submit_constructed(successor.take(), Vec::new())
        {
            ConnectSubmission::Adopted { session_epoch, .. } => session_epoch,
            ConnectSubmission::Rejected => panic!("successor rejected"),
        };
        release.send(()).expect("release session operation");
        assert!(matches!(
            delayed.await.expect("session operation task"),
            Err(SessionOperationError::Unavailable)
        ));

        let backend = registry
            .run_exact_session_operation(source_id, successor_epoch, |_| async {
                Err::<(), _>(BackendError::ConnectionFailed {
                    message: "fixture detail".to_string(),
                    source: None,
                })
            })
            .await;
        assert!(matches!(
            backend,
            Err(SessionOperationError::Backend(
                BackendError::ConnectionFailed { .. }
            ))
        ));

        let shutdown_registry = registry.clone();
        let (shutdown_started, shutdown_started_rx) = oneshot::channel();
        let (release_shutdown, release_shutdown_rx) = oneshot::channel();
        let during_shutdown = tokio::spawn(async move {
            shutdown_registry
                .run_exact_session_operation(source_id, successor_epoch, move |_| async move {
                    let _ = shutdown_started.send(());
                    let _ = release_shutdown_rx.await;
                    Err::<(), _>(BackendError::ConnectionFailed {
                        message: "stale shutdown detail".to_string(),
                        source: None,
                    })
                })
                .await
        });
        shutdown_started_rx
            .await
            .expect("shutdown operation started");
        let shutdown = registry.shutdown();
        release_shutdown
            .send(())
            .expect("release shutdown operation");
        assert!(matches!(
            during_shutdown.await.expect("shutdown operation task"),
            Err(SessionOperationError::Unavailable)
        ));

        predecessor.allow_close();
        predecessor.probe.wait_for_completions(1).await;
        timeout(Duration::from_secs(2), shutdown.wait())
            .await
            .expect("shutdown completed");
    }

    #[tokio::test]
    async fn accepted_catalogue_selector_preserves_predecessor_and_rejects_exact_staleness() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::immediate();
        let (epoch, _) = adopt(&registry, source_id, &mut predecessor, vec!["predecessor"]);
        let initial = registry
            .resolve_current_accepted_catalogue(source_id, |catalogue| catalogue.first().copied())
            .expect("accepted predecessor catalogue");
        assert_eq!(initial.session_epoch, epoch);
        assert_eq!(initial.value, "predecessor");
        assert!(initial.authority.is_active());

        let replacement = registry
            .begin_connect(source_id)
            .expect("replacement pending");
        let during_connect = registry
            .resolve_current_accepted_catalogue(source_id, |catalogue| catalogue.first().copied())
            .expect("predecessor remains accepted while connecting");
        assert_eq!(during_connect.generation, initial.generation);
        assert!(replacement.fail(FailureCategory::Timeout));
        assert_eq!(
            registry
                .resolve_current_accepted_catalogue(source_id, |catalogue| {
                    catalogue.first().copied()
                })
                .expect("failed replacement preserves predecessor")
                .generation,
            initial.generation
        );

        let stale_selector_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&stale_selector_calls);
        assert!(registry
            .resolve_exact_accepted_catalogue(
                source_id,
                initial.generation.wrapping_add(1),
                epoch,
                move |_| {
                    calls.fetch_add(1, Ordering::AcqRel);
                    Some(())
                },
            )
            .is_none());
        assert_eq!(stale_selector_calls.load(Ordering::Acquire), 0);

        let refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("catalogue refresh");
        let refreshed_generation = refresh.generation();
        assert!(refresh.submit(vec!["refreshed"]));
        assert!(!initial.authority.is_active());
        assert!(registry
            .resolve_exact_accepted_catalogue(source_id, initial.generation, epoch, |_| Some(()),)
            .is_none());
        assert_eq!(
            registry
                .resolve_current_accepted_catalogue(source_id, |catalogue| {
                    catalogue.first().copied()
                })
                .expect("refreshed catalogue")
                .generation,
            refreshed_generation
        );
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn accepted_snapshot_authority_revokes_while_parked_clone_remains_alive() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut adapter, vec!["initial"]);

        let parked_refresh = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("parked accepted snapshot");
        assert!(parked_refresh.authority.is_active());
        let refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("refresh");
        assert!(refresh.submit(vec!["replacement"]));
        assert!(
            !parked_refresh.authority.is_active(),
            "replacement revokes authority independently of payload Arc lifetime"
        );
        assert_eq!(parked_refresh.value.as_slice(), &["initial"]);

        let parked_disconnect = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("parked replacement snapshot");
        assert!(parked_disconnect.authority.is_active());
        let waiter = registry.disconnect(source_id).expect("disconnect");
        assert!(
            !parked_disconnect.authority.is_active(),
            "disconnect revokes authority synchronously while clone is parked"
        );
        waiter.wait().await;
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn catalogue_selectors_run_unlocked_and_revalidate_before_return_or_adapter_work() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::immediate();
        let (epoch, _) = adopt(&registry, source_id, &mut adapter, vec!["initial"]);

        let selector_registry = registry.clone();
        let current = registry.resolve_current_accepted_catalogue(source_id, move |catalogue| {
            assert!(selector_registry.inner.state.try_lock().is_ok());
            let refresh = selector_registry
                .begin_refresh(source_id, RefreshLane::Catalogue)
                .expect("refresh from current selector");
            assert!(refresh.submit(vec!["after-current"]));
            catalogue.first().copied()
        });
        assert!(
            current.is_none(),
            "selector-time refresh must stale the result"
        );

        let after_current = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("catalogue after current-selector refresh");
        let selector_registry = registry.clone();
        let exact = registry.resolve_exact_accepted_catalogue(
            source_id,
            after_current.generation,
            epoch,
            move |catalogue| {
                assert!(selector_registry.inner.state.try_lock().is_ok());
                let refresh = selector_registry
                    .begin_refresh(source_id, RefreshLane::Catalogue)
                    .expect("refresh from exact selector");
                assert!(refresh.submit(vec!["after-exact"]));
                catalogue.first().copied()
            },
        );
        assert!(
            exact.is_none(),
            "exact selector must recheck after selection"
        );

        let after_exact = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("catalogue after exact-selector refresh");
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&adapter_calls);
        let selector_registry = registry.clone();
        let result = registry
            .resolve_catalogue_optional_http(
                source_id,
                after_exact.generation,
                epoch,
                move |catalogue| {
                    assert!(selector_registry.inner.state.try_lock().is_ok());
                    let refresh = selector_registry
                        .begin_refresh(source_id, RefreshLane::Catalogue)
                        .expect("refresh from media selector");
                    assert!(refresh.submit(vec!["after-media"]));
                    catalogue.contains(&"after-exact").then_some(())
                },
                move |_| {
                    calls.fetch_add(1, Ordering::AcqRel);
                    async { Ok(None) }
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            adapter_calls.load(Ordering::Acquire),
            0,
            "a selector-time refresh must prevent stale adapter work"
        );

        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn guarded_catalogue_resolution_closes_pre_call_and_in_flight_refresh_races() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::immediate();
        let (epoch, _) = adopt(&registry, source_id, &mut adapter, vec!["track"]);
        let generation = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("catalogue")
            .generation;

        let calls = Arc::new(AtomicUsize::new(0));
        let stale_calls = Arc::clone(&calls);
        assert!(registry
            .resolve_catalogue_stream(
                source_id,
                generation.wrapping_add(1),
                epoch,
                |_| Some(()),
                move |_| async move {
                    stale_calls.fetch_add(1, Ordering::AcqRel);
                    Ok(AdapterStream::ProtectedHttp(Box::new(
                        ResolvedHttpRequest::new(
                            url::Url::parse("http://example.test/stale").expect("URL"),
                        )?,
                    )))
                },
            )
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);

        let missing_calls = Arc::clone(&calls);
        assert!(registry
            .resolve_catalogue_optional_http(
                source_id,
                generation,
                epoch,
                |_| None::<()>,
                move |_| async move {
                    missing_calls.fetch_add(1, Ordering::AcqRel);
                    Ok(None)
                },
            )
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);

        let delayed_registry = registry.clone();
        let (started, started_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let delayed = tokio::spawn(async move {
            delayed_registry
                .resolve_catalogue_stream(
                    source_id,
                    generation,
                    epoch,
                    |catalogue| catalogue.contains(&"track").then_some(()),
                    move |_| async move {
                        let _ = started.send(());
                        let _ = release_rx.await;
                        Ok(AdapterStream::ProtectedHttp(Box::new(
                            ResolvedHttpRequest::new(
                                url::Url::parse("http://example.test/delayed").expect("URL"),
                            )?,
                        )))
                    },
                )
                .await
        });
        started_rx.await.expect("resolver entered");
        let refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("same-session refresh");
        assert!(refresh.submit(vec!["other"]));
        release.send(()).expect("release resolver");
        assert!(delayed.await.expect("resolution task").is_err());

        let after_stream_refresh = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("refreshed catalogue");
        let delayed_registry = registry.clone();
        let (error_started, error_started_rx) = oneshot::channel();
        let (release_error, release_error_rx) = oneshot::channel();
        let delayed_error = tokio::spawn(async move {
            delayed_registry
                .resolve_catalogue_stream(
                    source_id,
                    after_stream_refresh.generation,
                    epoch,
                    |catalogue| catalogue.contains(&"other").then_some(()),
                    move |_| async move {
                        let _ = error_started.send(());
                        let _ = release_error_rx.await;
                        Err(BackendError::Timeout { duration_secs: 1 })
                    },
                )
                .await
        });
        error_started_rx.await.expect("error resolver entered");
        let refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("backend-error-racing refresh");
        assert!(refresh.submit(vec!["after-error"]));
        release_error.send(()).expect("release error resolver");
        assert!(matches!(
            delayed_error.await.expect("error resolution task"),
            Err(CatalogueMediaResolveError::Unavailable)
        ));

        let after_error_refresh = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("catalogue after backend-error race");
        let delayed_registry = registry.clone();
        let (art_started, art_started_rx) = oneshot::channel();
        let (release_art, release_art_rx) = oneshot::channel();
        let delayed_art = tokio::spawn(async move {
            delayed_registry
                .resolve_catalogue_optional_http(
                    source_id,
                    after_error_refresh.generation,
                    epoch,
                    |catalogue| catalogue.contains(&"after-error").then_some(()),
                    move |_| async move {
                        let _ = art_started.send(());
                        let _ = release_art_rx.await;
                        Ok(Some(ResolvedHttpRequest::new(
                            url::Url::parse("http://example.test/delayed-art").expect("URL"),
                        )?))
                    },
                )
                .await
        });
        art_started_rx.await.expect("artwork resolver entered");
        let refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("artwork-racing refresh");
        assert!(refresh.submit(vec!["third"]));
        release_art.send(()).expect("release artwork resolver");
        assert!(delayed_art.await.expect("artwork task").is_err());

        let refreshed = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("twice-refreshed catalogue");
        let selector_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&selector_calls);
        let resolved = registry
            .resolve_catalogue_optional_http(
                source_id,
                refreshed.generation,
                epoch,
                move |catalogue| {
                    calls.fetch_add(1, Ordering::AcqRel);
                    catalogue.contains(&"third").then_some("selected")
                },
                |_| async { Ok(None) },
            )
            .await;
        let Ok((none, authority, selected)) = resolved else {
            panic!("authorized absent artwork must resolve");
        };
        assert!(none.is_none());
        assert!(authority.is_active());
        assert_eq!(selected, "selected");
        assert_eq!(selector_calls.load(Ordering::Acquire), 1);
        shutdown_immediate(&registry).await;
        assert!(!authority.is_active());
    }

    #[tokio::test]
    async fn file_resolution_uses_exact_epoch_and_revokes_handle_clones_on_retirement() {
        let registry = registry();
        let source_id = SourceId::external();
        claim(&registry, source_id, SourceProvenance::External);
        let mut adapter = AdapterFixture::held();
        let (session_epoch, _) = adopt(&registry, source_id, &mut adapter, Vec::new());
        let directory = tempfile::tempdir().expect("retained file directory");
        let path = directory.path().join("external.bin");
        std::fs::write(&path, b"retained").expect("write retained file");
        let file = std::fs::File::open(&path).expect("open retained file");

        let invoked = Arc::new(AtomicBool::new(false));
        let stale_invoked = Arc::clone(&invoked);
        assert!(registry
            .resolve_file(
                source_id,
                session_epoch.wrapping_add(1),
                move |_adapter| async move {
                    stale_invoked.store(true, Ordering::Release);
                    ResolvedFileMedia::from_open_regular_file(file, None)
                        .map_err(crate::architecture::error::BackendError::Io)
                },
            )
            .await
            .is_err());
        assert!(!invoked.load(Ordering::Acquire));

        let file = std::fs::File::open(&path).expect("reopen retained file");
        let media = registry
            .resolve_file(source_id, session_epoch, move |_adapter| async move {
                ResolvedFileMedia::from_open_regular_file(file, None)
                    .map_err(crate::architecture::error::BackendError::Io)
            })
            .await
            .expect("resolve exact retained file");
        assert!(media.is_active());
        media
            .try_clone_file()
            .expect("current lease permits handle clone");

        let waiter = registry
            .disconnect(source_id)
            .expect("disconnect external file");
        assert!(!media.is_active());
        assert_eq!(
            media
                .try_clone_file()
                .expect_err("retired lease rejects handle clone")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        adapter.allow_close();
        waiter.wait().await;
        assert_eq!(adapter.probe.calls(), 1);
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn media_resolution_rejects_stale_expected_epoch_before_invoking_adapter() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        let (predecessor_epoch, predecessor_lease) =
            adopt(&registry, source_id, &mut predecessor, Vec::new());

        let mut successor = AdapterFixture::immediate();
        let (successor_epoch, _) = adopt(&registry, source_id, &mut successor, Vec::new());
        assert_ne!(predecessor_epoch, successor_epoch);
        assert!(!predecessor_lease.is_active());

        let stream_calls = Arc::new(AtomicUsize::new(0));
        let stream_calls_in_resolver = Arc::clone(&stream_calls);
        let stream = registry
            .resolve_http(source_id, predecessor_epoch, move |_adapter| async move {
                stream_calls_in_resolver.fetch_add(1, Ordering::AcqRel);
                ResolvedHttpRequest::new(
                    url::Url::parse("http://example.test/stale-stream").expect("URL"),
                )
            })
            .await;
        assert!(stream.is_err());
        assert_eq!(stream_calls.load(Ordering::Acquire), 0);

        let artwork_calls = Arc::new(AtomicUsize::new(0));
        let artwork_calls_in_resolver = Arc::clone(&artwork_calls);
        let artwork = registry
            .resolve_optional_http(source_id, predecessor_epoch, move |_adapter| async move {
                artwork_calls_in_resolver.fetch_add(1, Ordering::AcqRel);
                Ok(Some(ResolvedHttpRequest::new(
                    url::Url::parse("http://example.test/stale-artwork").expect("URL"),
                )?))
            })
            .await;
        assert!(artwork.is_err());
        assert_eq!(artwork_calls.load(Ordering::Acquire), 0);

        predecessor.allow_close();
        predecessor.probe.wait_for_completions(1).await;
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn refresh_owner_keeps_exact_predecessor_session_not_successor() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        adopt(&registry, source_id, &mut predecessor, Vec::new());
        let refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("refresh");
        let captured = refresh.session();
        assert!(predecessor.matches(&captured));
        assert!(captured.lease().is_active());

        let mut successor = AdapterFixture::immediate();
        assert!(matches!(
            registry
                .begin_connect(source_id)
                .expect("replace")
                .submit_constructed(successor.take(), Vec::new()),
            ConnectSubmission::Adopted { .. }
        ));
        assert!(!captured.lease().is_active());
        assert!(successor.matches(&registry.session(source_id).expect("successor")));
        assert!(!refresh.submit(vec!["stale"]));
        predecessor.allow_close();
        predecessor.probe.wait_for_completions(1).await;
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn late_connect_and_refresh_spawn_do_not_run_or_reopen_shutdown() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut fixture, Vec::new());
        let connect = registry.begin_connect(source_id).expect("connect owner");
        let refresh = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("refresh owner");
        let barrier = registry.shutdown();
        timeout(Duration::from_secs(2), barrier.clone().wait())
            .await
            .expect("initial shutdown");
        assert!(barrier.is_complete());

        let connect_work = Arc::new(AtomicUsize::new(0));
        let connect_work_task = Arc::clone(&connect_work);
        let late_connect = connect.spawn_staged(
            ConstructionCancellationPolicy::FinishConstruction,
            move |_cancellation| async move {
                connect_work_task.fetch_add(1, Ordering::AcqRel);
                AdapterTaskResult::Failed(FailureCategory::Backend)
            },
            |_adapter, _cancellation| async { RefreshTaskResult::Refreshed(Vec::new()) },
        );
        let refresh_work = Arc::new(AtomicUsize::new(0));
        let refresh_work_task = Arc::clone(&refresh_work);
        let late_refresh = refresh.spawn(move |_session, _cancellation| async move {
            refresh_work_task.fetch_add(1, Ordering::AcqRel);
            RefreshTaskResult::Refreshed(Vec::new())
        });
        assert!(late_connect.is_finished());
        assert!(late_refresh.is_finished());
        assert_eq!(connect_work.load(Ordering::Acquire), 0);
        assert_eq!(refresh_work.load(Ordering::Acquire), 0);
        assert!(barrier.is_complete());
        timeout(Duration::from_secs(1), barrier.wait())
            .await
            .expect("barrier remains closed");
    }

    #[tokio::test]
    async fn last_registry_handle_revokes_lease_and_starts_fail_closed_close() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut fixture = AdapterFixture::held();
        let (_, lease) = adopt(&registry, source_id, &mut fixture, Vec::new());
        let barrier = ShutdownBarrier {
            tracker: Arc::clone(&registry.inner.tracker),
        };
        let final_handle = registry.clone();
        assert!(lease.is_active());

        drop(registry);
        assert!(lease.is_active());
        assert_eq!(fixture.probe.calls(), 0);
        drop(final_handle);
        assert!(!lease.is_active());
        fixture.probe.wait_for_calls(1).await;
        assert_eq!(fixture.probe.calls(), 1);
        assert!(!barrier.is_complete());
        fixture.allow_close();
        timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("last-handle close joined");
        assert_eq!(fixture.probe.calls(), 1);
    }

    #[tokio::test]
    async fn observer_revisions_are_strict_and_changes_are_typed() {
        let registry = registry();
        let source_id = SourceId::random();
        let mut changes = registry.subscribe();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::immediate();
        adopt(&registry, source_id, &mut adapter, vec!["catalogue"]);

        let mut observed = Vec::new();
        while let Ok(change) = changes.try_recv() {
            observed.push(change);
        }
        assert!(observed.len() >= 4);
        assert!(observed
            .windows(2)
            .all(|pair| pair[0].revision < pair[1].revision));
        assert!(observed.iter().all(|change| change.source_id == source_id));
        assert!(observed
            .iter()
            .any(|change| matches!(change.change, LifecycleChange::ProvenanceChanged { .. })));
        assert!(observed
            .iter()
            .any(|change| matches!(change.change, LifecycleChange::SessionAdopted { .. })));
        assert!(observed
            .iter()
            .any(|change| matches!(change.change, LifecycleChange::CatalogueAccepted { .. })));
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn subscribe_then_atomic_baseline_covers_queued_changes_and_next_revision() {
        let registry = registry();
        let source_id = SourceId::random();
        let mut changes = registry.subscribe();
        claim(&registry, source_id, SourceProvenance::Saved);

        let baseline = registry.snapshot_all();
        assert!(!baseline.shutting_down);
        assert_eq!(baseline.sources.len(), 1);
        assert_eq!(baseline.sources[0].0, source_id);
        assert!(baseline.sources[0]
            .1
            .provenance
            .contains(SourceProvenance::Saved));

        let queued: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok()).collect();
        assert!(!queued.is_empty());
        assert!(queued
            .iter()
            .all(|change| change.revision <= baseline.revision));

        let owner = registry.begin_connect(source_id).expect("next connect");
        let next = changes.recv().await.expect("post-baseline change");
        assert!(next.revision > baseline.revision);
        drop(owner);

        let barrier = registry.shutdown();
        let shutdown = registry.snapshot_all();
        assert!(shutdown.shutting_down);
        assert!(shutdown.revision >= next.revision);
        barrier.wait().await;
    }

    #[tokio::test]
    async fn shutdown_invalidates_empty_registry_observers() {
        let registry = registry();
        let mut invalidations = registry.subscribe_invalidations();
        let baseline = registry.snapshot_all();

        let barrier = registry.shutdown();
        invalidations
            .changed()
            .await
            .expect("shutdown invalidation remains observable");

        let shutdown = registry.snapshot_all();
        assert!(shutdown.shutting_down);
        assert!(shutdown.revision > baseline.revision);
        assert_eq!(*invalidations.borrow_and_update(), shutdown.revision);
        barrier.wait().await;
    }

    #[tokio::test]
    async fn shutdown_invalidates_observer_with_only_inert_retired_entry() {
        let registry = registry();
        let source_id = SourceId::random();
        let claim = claim(&registry, source_id, SourceProvenance::Discovery);
        assert!(registry.release_provenance(source_id, claim));
        assert_eq!(
            registry.snapshot(source_id).expect("retired row").state,
            SourceState::Retired
        );

        let mut invalidations = registry.subscribe_invalidations();
        let baseline = registry.snapshot_all();
        invalidations.borrow_and_update();
        let barrier = registry.shutdown();
        invalidations
            .changed()
            .await
            .expect("gate closure wakes inert-retired observer");

        let shutdown = registry.snapshot_all();
        assert!(shutdown.shutting_down);
        assert!(shutdown.revision > baseline.revision);
        barrier.wait().await;
    }

    #[tokio::test]
    async fn prune_publishes_new_revision_and_authoritative_absence() {
        let registry = registry();
        let source_id = SourceId::random();
        let claim = claim(&registry, source_id, SourceProvenance::Discovery);
        let mut changes = registry.subscribe();
        assert!(registry.release_provenance(source_id, claim));
        let before_prune = registry.snapshot_all().revision;

        assert_eq!(registry.prune_retired(), 1);
        let after_prune = registry.snapshot_all();
        assert!(after_prune.revision > before_prune);
        assert!(after_prune.sources.is_empty());

        let observed: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok()).collect();
        assert!(observed.iter().any(|change| {
            change.source_id == source_id
                && change.revision == after_prune.revision
                && matches!(change.change, LifecycleChange::Pruned)
        }));
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn read_only_disconnect_waiter_cannot_cancel_reappearing_successor() {
        let registry = registry();
        let source_id = SourceId::random();
        let saved = claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        adopt(&registry, source_id, &mut predecessor, vec!["predecessor"]);

        assert!(registry.release_provenance(source_id, saved));
        predecessor.probe.wait_for_calls(1).await;
        let waiter = registry
            .current_disconnect_waiter(source_id)
            .expect("existing close waiter");

        let discovery = claim(&registry, source_id, SourceProvenance::Discovery);
        let successor = registry
            .begin_connect(source_id)
            .expect("successor connect");
        assert!(registry.current_disconnect_waiter(source_id).is_none());
        assert_eq!(registry.prune_retired(), 0);

        let mut successor_adapter = AdapterFixture::immediate();
        assert!(matches!(
            successor.submit_constructed(successor_adapter.take(), vec!["successor"]),
            ConnectSubmission::Adopted { .. }
        ));
        predecessor.allow_close();
        waiter.wait().await;

        let snapshot = registry.snapshot(source_id).expect("successor retained");
        assert_eq!(snapshot.state, SourceState::Ready);
        assert_eq!(
            snapshot
                .catalogue
                .expect("successor catalogue")
                .value
                .as_ref(),
            &vec!["successor"]
        );
        assert_eq!(registry.prune_retired(), 0);

        assert!(registry.release_provenance(source_id, discovery));
        successor_adapter.probe.wait_for_completions(1).await;
        assert_eq!(registry.prune_retired(), 1);
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn lifecycle_owned_prune_removes_inert_final_claim_immediately() {
        let registry = registry();
        let source_id = SourceId::random();
        let claim = claim(&registry, source_id, SourceProvenance::Discovery);

        assert!(registry.release_provenance(source_id, claim));
        registry.schedule_prune_after_current_retirement(source_id);

        assert!(registry.snapshot(source_id).is_none());
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn lifecycle_owned_prune_waits_for_active_retirement() {
        let registry = registry();
        let source_id = SourceId::random();
        let claim = claim(&registry, source_id, SourceProvenance::Discovery);
        let mut adapter = AdapterFixture::held();
        adopt(&registry, source_id, &mut adapter, vec!["catalogue"]);

        assert!(registry.release_provenance(source_id, claim));
        registry.schedule_prune_after_current_retirement(source_id);
        adapter.probe.wait_for_calls(1).await;
        assert!(registry.snapshot(source_id).is_some());

        adapter.allow_close();
        adapter.probe.wait_for_completions(1).await;
        timeout(Duration::from_secs(2), async {
            while registry.snapshot(source_id).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retired source pruned after close");
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn lifecycle_owned_prune_rechecks_reappearing_source() {
        let registry = registry();
        let source_id = SourceId::random();
        let saved = claim(&registry, source_id, SourceProvenance::Saved);
        let mut predecessor = AdapterFixture::held();
        adopt(&registry, source_id, &mut predecessor, vec!["predecessor"]);

        assert!(registry.release_provenance(source_id, saved));
        registry.schedule_prune_after_current_retirement(source_id);
        predecessor.probe.wait_for_calls(1).await;

        let discovery = claim(&registry, source_id, SourceProvenance::Discovery);
        let successor = registry
            .begin_connect(source_id)
            .expect("successor connect");
        let mut successor_adapter = AdapterFixture::immediate();
        assert!(matches!(
            successor.submit_constructed(successor_adapter.take(), vec!["successor"]),
            ConnectSubmission::Adopted { .. }
        ));

        predecessor.allow_close();
        predecessor.probe.wait_for_completions(1).await;
        tokio::task::yield_now().await;
        let snapshot = registry.snapshot(source_id).expect("successor retained");
        assert_eq!(snapshot.state, SourceState::Ready);
        assert!(snapshot.provenance.contains(SourceProvenance::Discovery));

        assert!(registry.release_provenance(source_id, discovery));
        registry.schedule_prune_after_current_retirement(source_id);
        successor_adapter.probe.wait_for_completions(1).await;
        timeout(Duration::from_secs(2), async {
            while registry.snapshot(source_id).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("successor eventually pruned");
        assert!(registry.shutdown().is_complete());
    }

    #[tokio::test]
    async fn prune_maintenance_arc_does_not_suppress_last_handle_teardown() {
        let registry = registry();
        let retiring_id = SourceId::random();
        let active_id = SourceId::random();
        let retiring_claim = claim(&registry, retiring_id, SourceProvenance::Discovery);
        claim(&registry, active_id, SourceProvenance::Saved);
        let mut retiring = AdapterFixture::held();
        let mut active = AdapterFixture::held();
        adopt(&registry, retiring_id, &mut retiring, vec!["retiring"]);
        adopt(&registry, active_id, &mut active, vec!["active"]);

        assert!(registry.release_provenance(retiring_id, retiring_claim));
        registry.schedule_prune_after_current_retirement(retiring_id);
        retiring.probe.wait_for_calls(1).await;

        drop(registry);
        active.probe.wait_for_calls(1).await;
        assert_eq!(retiring.probe.calls(), 1);
        assert_eq!(active.probe.calls(), 1);

        retiring.allow_close();
        active.allow_close();
        retiring.probe.wait_for_completions(1).await;
        active.probe.wait_for_completions(1).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exact_session_admission_orders_disconnect_without_returning_a_permit() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::immediate();
        let (session_epoch, _) = adopt(&registry, source_id, &mut adapter, vec!["catalogue"]);

        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let admitting = registry.clone();
        let admission = std::thread::spawn(move || {
            admitting.admit_exact_session_if(
                source_id,
                session_epoch,
                |_adapter, provenance| provenance.contains(SourceProvenance::Saved),
                || {
                    entered_tx.send(()).expect("admission observer alive");
                    release_rx.recv().expect("release synchronous admission");
                    Arc::new(())
                },
            )
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("synchronous admission entered");

        assert!(
            registry.state_lock_is_contended_for_test(),
            "the admitted callback must retain the lifecycle lock"
        );

        let (waiter_tx, waiter_rx) = std::sync::mpsc::sync_channel(1);
        let disconnecting = registry.clone();
        let disconnect = std::thread::spawn(move || {
            let waiter = disconnecting
                .disconnect(source_id)
                .expect("disconnect admitted");
            waiter_tx.send(waiter).expect("disconnect observer alive");
        });

        release_tx.send(()).expect("admission remains alive");
        let returned_without_authority = admission
            .join()
            .expect("admission thread")
            .expect("exact session admitted");
        let waiter = waiter_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("disconnect resumes after synchronous callback");
        disconnect.join().expect("disconnect thread");
        waiter.wait().await;

        // Retaining the callback result cannot retain lifecycle authority.
        assert_eq!(Arc::strong_count(&returned_without_authority), 1);
        let called = AtomicBool::new(false);
        assert!(registry
            .admit_exact_session_if(
                source_id,
                session_epoch,
                |_adapter, _provenance| true,
                || called.store(true, Ordering::Release),
            )
            .is_none());
        assert!(!called.load(Ordering::Acquire));
        shutdown_immediate(&registry).await;
    }

    #[tokio::test]
    async fn exact_catalogue_admission_requires_current_epoch_generation_and_predicate() {
        let registry = registry();
        let source_id = SourceId::random();
        claim(&registry, source_id, SourceProvenance::Saved);
        let mut adapter = AdapterFixture::immediate();
        let (session_epoch, _) = adopt(&registry, source_id, &mut adapter, vec!["catalogue"]);
        let generation = registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.catalogue)
            .expect("accepted catalogue")
            .generation;

        assert_eq!(
            registry.admit_exact_catalogue_if(
                source_id,
                session_epoch,
                generation,
                |_adapter, provenance, catalogue| {
                    provenance.contains(SourceProvenance::Saved)
                        && catalogue.as_slice() == ["catalogue"]
                },
                || 17,
            ),
            Some(17)
        );

        for (epoch, catalogue_generation) in [
            (session_epoch.wrapping_add(1), generation),
            (session_epoch, generation.wrapping_add(1)),
        ] {
            let called = AtomicBool::new(false);
            assert!(registry
                .admit_exact_catalogue_if(
                    source_id,
                    epoch,
                    catalogue_generation,
                    |_adapter, _provenance, _catalogue| true,
                    || called.store(true, Ordering::Release),
                )
                .is_none());
            assert!(!called.load(Ordering::Acquire));
        }

        let called = AtomicBool::new(false);
        assert!(registry
            .admit_exact_catalogue_if(
                source_id,
                session_epoch,
                generation,
                |_adapter, _provenance, _catalogue| false,
                || called.store(true, Ordering::Release),
            )
            .is_none());
        assert!(!called.load(Ordering::Acquire));
        shutdown_immediate(&registry).await;
    }
}
