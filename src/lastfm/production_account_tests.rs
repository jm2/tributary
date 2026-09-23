//! Account controls composed through the application handle: connect,
//! disconnect, reconnect, and reauthorization leave the queue and the next
//! runtime start usable, and none of them waits on a running runtime's vault
//! lease.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use uuid::Uuid;

use super::tests::{
    assert_owner_drains, live_policy_for_test, migrated_database, FixedClock, PendingTransport,
};
use super::*;
use crate::lastfm::credentials::{LastFmAccountBinding, ProtectedString};
use crate::lastfm::playback_coordinator::LastFmPlaybackCoordinatorOwner;
use crate::lastfm::runtime::LastFmRuntimePhase;
use crate::lastfm::storage::{LastFmDurablePause, LastFmEnqueueOutcome, PendingLastFmScrobble};
use crate::source_registry::SourceRegistry;

/// Generous enough for lease contention from concurrently running runtime
/// tests, far below "forever" for a real deadlock.
const DEADLINE: Duration = Duration::from_secs(10);
const KEY: &str = "0123456789abcdef0123456789abcdef";
const RENEWED_KEY: &str = "fedcba9876543210fedcba9876543210";

/// In-memory vault whose next `failing_deletes` deletions fail.
#[derive(Default)]
struct TestVault {
    session: Mutex<Option<StoredSession>>,
    failing_deletes: AtomicUsize,
}

impl TestVault {
    fn holding(username: &str) -> Arc<Self> {
        let vault = Self::default();
        *vault.session.lock().unwrap() = Some(session(username));
        Arc::new(vault)
    }

    fn stored(&self) -> Option<StoredSession> {
        self.session.lock().unwrap().clone()
    }
}

impl SessionCredentialStore for TestVault {
    fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
        Ok(self.stored())
    }

    fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
        *self.session.lock().unwrap() = Some(session.clone());
        Ok(())
    }

    fn delete(&self) -> Result<(), CredentialError> {
        if self
            .failing_deletes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(CredentialError::Unavailable);
        }
        *self.session.lock().unwrap() = None;
        Ok(())
    }
}

fn session(username: &str) -> StoredSession {
    StoredSession::new(username, ProtectedString::new(KEY)).expect("valid test session")
}

fn pending(binding: LastFmAccountBinding) -> PendingLastFmScrobble {
    PendingLastFmScrobble::try_new(
        Uuid::new_v4(),
        binding,
        "artist".to_owned(),
        "title".to_owned(),
        None,
        None,
        None,
        60,
        1_700_000_000,
    )
    .expect("valid test scrobble")
}

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("account control deadline")
}

struct Harness {
    handle: LastFmApplicationHandle,
    shutdown: LastFmApplicationShutdown,
    database: DatabaseConnection,
    vault: Arc<TestVault>,
    live: LastFmLivePolicy,
    registry: SourceRegistry,
    coordinator: LastFmPlaybackCoordinatorOwner,
}

impl Harness {
    async fn new(vault: Arc<TestVault>, live: LastFmLivePolicy) -> Self {
        let database = migrated_database().await;
        let registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let window = coordinator
            .bind_window(registry.clone())
            .expect("window binding");
        let (handle, shutdown) = spawn_with_dependencies(
            window,
            tokio::runtime::Handle::current(),
            vault.clone(),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
            live.clone(),
        );
        handle
            .try_attach_database(database.clone())
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        Self {
            handle,
            shutdown,
            database,
            vault,
            live,
            registry,
            coordinator,
        }
    }

    fn status(&self) -> LastFmApplicationStatus {
        *self.handle.subscribe_status().borrow()
    }

    /// Wait until the relayed runtime phase satisfies `accept`. The runtime
    /// status reaches the application snapshot through an asynchronous relay,
    /// so it can trail a command's completion.
    async fn wait_for_runtime_phase(&self, accept: fn(Option<LastFmRuntimePhase>) -> bool) {
        let mut status = self.handle.subscribe_status();
        within(status.wait_for(|status| accept(status.runtime.map(|runtime| runtime.phase))))
            .await
            .expect("application owner remains active");
    }

    fn binding(&self) -> LastFmAccountBinding {
        self.vault
            .stored()
            .expect("a stored account")
            .account_binding()
    }

    async fn activate(&self) -> Result<(), LastFmApplicationCommandError> {
        let activation =
            LastFmApplicationActivation::issue_from_policy_generation(&self.live.snapshot())
                .expect("consented generation");
        let operation = self.handle.try_activate(activation).expect("admitted");
        within(operation.wait()).await
    }

    async fn connect(&self, username: &str) -> Result<(), LastFmApplicationCommandError> {
        let grant = LastFmAuthorizationGrant::for_test(username, KEY);
        let operation = self.handle.try_connect(grant).expect("admitted");
        within(operation.wait()).await
    }

    async fn disconnect(&self) -> Result<u64, LastFmApplicationDisconnectError> {
        let operation = self.handle.try_disconnect_and_purge().expect("admitted");
        within(operation.wait()).await
    }

    /// Forward a reauthorization and await the runtime's own result.
    async fn reauthorize(&self, username: &str) -> Result<(), LastFmRuntimeCommandError> {
        let grant = LastFmAuthorizationGrant::for_test(username, RENEWED_KEY);
        let forward = self
            .handle
            .try_reauthorize_same_account(grant)
            .expect("admitted");
        let operation = within(forward.wait())
            .await
            .expect("forwarded")
            .expect("runtime admitted the reauthorization");
        within(operation.wait()).await
    }

    async fn enqueue_admits(&self, binding: LastFmAccountBinding) -> bool {
        matches!(
            storage::enqueue(&self.database, &pending(binding)).await,
            Ok(LastFmEnqueueOutcome::Inserted { .. })
        )
    }

    async fn pause(&self, binding: LastFmAccountBinding) -> Option<LastFmDurablePause> {
        storage::validate_account_queue_state(&self.database, binding)
            .await
            .expect("queue state")
            .durable_pause
    }

    async fn finish(self) {
        let Self {
            shutdown,
            registry,
            mut coordinator,
            ..
        } = self;
        assert_owner_drains(shutdown, &mut coordinator).await;
        registry.shutdown().wait().await;
    }
}

/// A disconnect clears its cleanup marker, so reconnecting either the same
/// or a different account starts a runtime, admits queue rows, and can be
/// disconnected again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_then_reconnect_same_or_other_account_never_wedges() {
    let harness = Harness::new(TestVault::holding("listener"), live_policy_for_test()).await;
    harness.activate().await.expect("stored account activates");
    for username in ["listener", "other-listener"] {
        let previous = harness.binding();
        harness.disconnect().await.expect("disconnect completes");
        assert!(harness.vault.stored().is_none());
        assert_eq!(
            harness.status().phase,
            LastFmApplicationPhase::AwaitingConsent
        );

        harness
            .connect(username)
            .await
            .expect("reconnect activates");
        assert_eq!(harness.status().phase, LastFmApplicationPhase::Active);
        let binding = harness.binding();
        assert_ne!(
            binding, previous,
            "a reconnect mints a fresh account identity"
        );
        assert!(harness.enqueue_admits(binding).await);
    }
    assert_eq!(harness.disconnect().await, Ok(1));
    harness.finish().await;
}

/// A failed credential deletion keeps the generation; the next disconnect
/// retries only that deletion, after which a reconnect works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeat_disconnect_finishes_a_failed_credential_deletion() {
    let vault = TestVault::holding("listener");
    vault.failing_deletes.store(1, Ordering::SeqCst);
    let harness = Harness::new(vault, live_policy_for_test()).await;
    harness.activate().await.expect("stored account activates");

    assert_eq!(
        harness.disconnect().await,
        Err(LastFmApplicationDisconnectError::Incomplete)
    );
    assert_eq!(harness.status().phase, LastFmApplicationPhase::Active);
    harness
        .wait_for_runtime_phase(|phase| phase == Some(LastFmRuntimePhase::CredentialCleanup))
        .await;
    assert!(harness.vault.stored().is_some());

    assert_eq!(harness.disconnect().await, Ok(0));
    assert_eq!(
        harness.status().phase,
        LastFmApplicationPhase::AwaitingConsent
    );
    assert!(harness.vault.stored().is_none());
    harness
        .connect("listener")
        .await
        .expect("reconnect activates");
    assert!(harness.enqueue_admits(harness.binding()).await);
    harness.finish().await;
}

/// Reauthorization runs inside the runtime: a different account is refused
/// with the pause kept, and the same account renews the key under the
/// runtime's lease, keeps the queue binding, and clears the durable pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_account_reauthorization_clears_the_durable_pause() {
    let harness = Harness::new(TestVault::holding("listener"), live_policy_for_test()).await;
    let binding = harness.binding();
    storage::persist_pause_for_account(
        &harness.database,
        binding,
        LastFmDurablePause::ReauthenticationRequired,
    )
    .await
    .expect("seed the code-9 pause");
    harness.activate().await.expect("paused account activates");
    harness
        .wait_for_runtime_phase(|phase| phase == Some(LastFmRuntimePhase::ReauthenticationRequired))
        .await;

    assert_eq!(
        harness.reauthorize("other-listener").await,
        Err(LastFmRuntimeCommandError::AccountReplacementRequired)
    );
    assert_eq!(
        harness.pause(binding).await,
        Some(LastFmDurablePause::ReauthenticationRequired)
    );

    harness
        .reauthorize("listener")
        .await
        .expect("same account renews the session");
    assert_eq!(harness.pause(binding).await, None);
    assert_eq!(harness.binding(), binding);
    assert_eq!(
        harness.vault.stored().expect("stored").key().expose(),
        RENEWED_KEY
    );
    harness
        .wait_for_runtime_phase(|phase| {
            phase.is_some() && phase != Some(LastFmRuntimePhase::ReauthenticationRequired)
        })
        .await;
    harness.finish().await;
}

/// An active runtime holds the process vault lease for its whole life.
/// Connect is refused at admission, and reauthorization and disconnect are
/// answered by the runtime itself, so no control can wait on that lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_controls_never_wait_on_an_active_runtime_lease() {
    let harness = Harness::new(TestVault::holding("listener"), live_policy_for_test()).await;
    harness.activate().await.expect("stored account activates");

    let grant = LastFmAuthorizationGrant::for_test("listener", KEY);
    assert_eq!(
        harness.handle.try_connect(grant).unwrap_err(),
        LastFmApplicationAdmissionError::GenerationActive
    );
    let grant = LastFmAuthorizationGrant::for_test("listener", RENEWED_KEY);
    let forward = harness
        .handle
        .try_reauthorize_same_account(grant)
        .expect("admitted");
    assert_eq!(
        within(forward.wait())
            .await
            .expect("forwarded")
            .unwrap_err(),
        LastFmRuntimeAdmissionError::NotReadyForReauthorization
    );
    assert_eq!(harness.disconnect().await, Ok(0));
    harness.finish().await;
}

/// Activation without a stored account leaves the owner dormant rather than
/// failed. A cleanup marker still bound to a previous account, the state
/// that used to refuse every later enqueue and runtime start, is cleared by
/// the next connect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dormant_owner_connects_over_a_previous_account_cleanup_marker() {
    let harness = Harness::new(Arc::default(), live_policy_for_test()).await;
    assert_eq!(
        harness.activate().await,
        Err(LastFmApplicationCommandError::RuntimeStart)
    );
    assert_eq!(
        harness.status().phase,
        LastFmApplicationPhase::AwaitingConsent
    );

    let previous = session("listener").account_binding();
    storage::purge_account(&harness.database, previous)
        .await
        .expect("leave the previous account's cleanup marker");
    harness
        .connect("listener")
        .await
        .expect("connect activates");
    assert_eq!(harness.status().phase, LastFmApplicationPhase::Active);
    assert!(harness.enqueue_admits(harness.binding()).await);
    harness.finish().await;
}

/// Rows whose account can no longer be loaded are never silently dropped:
/// connect refuses without writing the vault, and the explicit disconnect
/// discards them before a new account can connect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantined_rows_block_connect_until_explicitly_discarded() {
    let harness = Harness::new(Arc::default(), live_policy_for_test()).await;
    assert!(
        harness
            .enqueue_admits(session("lost-listener").account_binding())
            .await
    );

    assert_eq!(
        harness.connect("listener").await,
        Err(LastFmApplicationCommandError::QuarantinedQueue)
    );
    assert!(harness.vault.stored().is_none());
    assert_eq!(
        harness.status().failure,
        Some(LastFmApplicationCommandError::QuarantinedQueue)
    );

    assert_eq!(harness.disconnect().await, Ok(1));
    assert_eq!(harness.status().failure, None);
    harness
        .connect("listener")
        .await
        .expect("connect activates");
    harness.finish().await;
}

/// A grant is never stored without current consent and enablement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_without_consent_never_writes_the_vault() {
    let harness = Harness::new(Arc::default(), LastFmLivePolicy::default()).await;
    assert_eq!(
        harness.connect("listener").await,
        Err(LastFmApplicationCommandError::ConsentRequired)
    );
    assert!(harness.vault.stored().is_none());
    assert_eq!(
        harness.status().phase,
        LastFmApplicationPhase::AwaitingConsent
    );
    harness.finish().await;
}
