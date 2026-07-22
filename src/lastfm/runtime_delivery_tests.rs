use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use tokio::sync::watch;
use uuid::Uuid;

use super::*;
use crate::db::entities::lastfm_scrobble::StoredLastFmScrobble;
use crate::db::migration::Migrator;
use crate::lastfm::client::{
    IgnoredReason, LastFmClientError, LastFmTrack, Scrobble, ScrobbleBatchResult, SubmissionResult,
};
use crate::lastfm::credentials::{CredentialError, ProtectedString};
use crate::lastfm::delivery::LastFmDeliveryPrimitiveError;

const SESSION_KEY_A: &str = "0123456789abcdef0123456789abcdef";
const SESSION_KEY_B: &str = "fedcba9876543210fedcba9876543210";
const TEST_WATCHDOG: Duration = Duration::from_secs(2);

struct ManualClock {
    now_unix_ms: AtomicI64,
    changed: watch::Sender<i64>,
    waits: async_channel::Sender<i64>,
}

impl ManualClock {
    fn new(now_unix_ms: i64) -> (Arc<Self>, async_channel::Receiver<i64>) {
        let (changed, _) = watch::channel(now_unix_ms);
        let (waits, wait_events) = async_channel::unbounded();
        (
            Arc::new(Self {
                now_unix_ms: AtomicI64::new(now_unix_ms),
                changed,
                waits,
            }),
            wait_events,
        )
    }

    fn advance_to(&self, now_unix_ms: i64) {
        self.now_unix_ms.store(now_unix_ms, Ordering::SeqCst);
        self.changed.send_replace(now_unix_ms);
    }
}

#[async_trait::async_trait]
impl LastFmClock for ManualClock {
    fn now_unix_ms(&self) -> Result<i64, LastFmDeliveryPrimitiveError> {
        Ok(self.now_unix_ms.load(Ordering::SeqCst))
    }

    async fn wait_until_unix_ms(
        &self,
        deadline_unix_ms: i64,
    ) -> Result<(), LastFmDeliveryPrimitiveError> {
        let mut changed = self.changed.subscribe();
        let _ = self.waits.send(deadline_unix_ms).await;
        loop {
            if *changed.borrow_and_update() >= deadline_unix_ms {
                return Ok(());
            }
            changed
                .changed()
                .await
                .map_err(|_| LastFmDeliveryPrimitiveError::ClockOutOfRange)?;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CallObservation {
    batch_size: usize,
    expected_vault_session: bool,
}

struct InFlightGuard {
    active: Arc<AtomicUsize>,
    retired: async_channel::Sender<()>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        let _ = self.retired.try_send(());
    }
}

struct GatedTransport {
    expected_session: Mutex<StoredSession>,
    calls: async_channel::Sender<CallObservation>,
    payloads: Mutex<Vec<Vec<Scrobble>>>,
    responses: async_channel::Receiver<Result<ScrobbleBatchResult, LastFmClientError>>,
    retired: async_channel::Sender<()>,
    active: Arc<AtomicUsize>,
    maximum_active: AtomicUsize,
}

type GatedTransportFixture = (
    Arc<GatedTransport>,
    async_channel::Receiver<CallObservation>,
    async_channel::Sender<Result<ScrobbleBatchResult, LastFmClientError>>,
    async_channel::Receiver<()>,
    Arc<AtomicUsize>,
);

impl GatedTransport {
    fn new(expected_session: StoredSession) -> GatedTransportFixture {
        let (calls, call_events) = async_channel::unbounded();
        let (response_events, responses) = async_channel::unbounded();
        let (retired, retirement_events) = async_channel::unbounded();
        let active = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                expected_session: Mutex::new(expected_session),
                calls,
                payloads: Mutex::new(Vec::new()),
                responses,
                retired,
                active: Arc::clone(&active),
                maximum_active: AtomicUsize::new(0),
            }),
            call_events,
            response_events,
            retirement_events,
            active,
        )
    }

    fn maximum_active(&self) -> usize {
        self.maximum_active.load(Ordering::SeqCst)
    }

    fn expect_session(&self, session: StoredSession) {
        *self
            .expected_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = session;
    }

    fn payloads(&self) -> Vec<Vec<Scrobble>> {
        self.payloads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl LastFmTransport for GatedTransport {
    async fn update_now_playing(
        &self,
        _session: &StoredSession,
        _track: &LastFmTrack,
    ) -> Result<SubmissionResult, LastFmClientError> {
        Err(LastFmClientError::InvalidInput)
    }

    async fn submit_scrobbles(
        &self,
        session: &StoredSession,
        scrobbles: &[Scrobble],
    ) -> Result<ScrobbleBatchResult, LastFmClientError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum_active.fetch_max(active, Ordering::SeqCst);
        let _in_flight = InFlightGuard {
            active: Arc::clone(&self.active),
            retired: self.retired.clone(),
        };
        let expected_vault_session = {
            let expected = self
                .expected_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            session == &*expected
        };
        self.payloads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(scrobbles.to_vec());
        let _ = self
            .calls
            .send(CallObservation {
                batch_size: scrobbles.len(),
                expected_vault_session,
            })
            .await;
        self.responses
            .recv()
            .await
            .unwrap_or(Err(LastFmClientError::Transport))
    }
}

struct PanickingTransport;

#[async_trait::async_trait]
impl LastFmTransport for PanickingTransport {
    async fn update_now_playing(
        &self,
        _session: &StoredSession,
        _track: &LastFmTrack,
    ) -> Result<SubmissionResult, LastFmClientError> {
        Err(LastFmClientError::InvalidInput)
    }

    async fn submit_scrobbles(
        &self,
        _session: &StoredSession,
        _scrobbles: &[Scrobble],
    ) -> Result<ScrobbleBatchResult, LastFmClientError> {
        panic!("synthetic delivery-task failure")
    }
}

struct TestCredentialStore {
    session: Mutex<Option<StoredSession>>,
    active_requests: Arc<AtomicUsize>,
    active_at_delete: Mutex<Vec<usize>>,
}

impl TestCredentialStore {
    fn new(session: StoredSession, active_requests: Arc<AtomicUsize>) -> Self {
        Self {
            session: Mutex::new(Some(session)),
            active_requests,
            active_at_delete: Mutex::new(Vec::new()),
        }
    }

    fn has_session(&self) -> bool {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn stored_session(&self) -> Option<StoredSession> {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn delete_observations(&self) -> Vec<usize> {
        self.active_at_delete
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl SessionCredentialStore for TestCredentialStore {
    fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
        Ok(self
            .session
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .clone())
    }

    fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
        *self
            .session
            .lock()
            .map_err(|_| CredentialError::Unavailable)? = Some(session.clone());
        Ok(())
    }

    fn delete(&self) -> Result<(), CredentialError> {
        self.active_at_delete
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .push(self.active_requests.load(Ordering::SeqCst));
        *self
            .session
            .lock()
            .map_err(|_| CredentialError::Unavailable)? = None;
        Ok(())
    }
}

async fn database() -> DatabaseConnection {
    let database = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&database, None).await.unwrap();
    database
}

async fn spawn_activated_runtime(
    database: DatabaseConnection,
    credentials: Arc<dyn SessionCredentialStore>,
    transport: Arc<dyn LastFmTransport>,
    clock: Arc<dyn LastFmClock>,
) -> Result<(LastFmRuntimeHandle, LastFmRuntimeShutdown), LastFmRuntimeStartError> {
    spawn_lastfm_runtime(
        LastFmRuntimeActivation::issue_after_consent_and_enablement(),
        database,
        credentials,
        transport,
        clock,
    )
    .await
}

fn session() -> StoredSession {
    StoredSession::new("listener", ProtectedString::new(SESSION_KEY_A)).unwrap()
}

fn pending(binding: LastFmAccountBinding, index: usize) -> PendingLastFmScrobble {
    PendingLastFmScrobble::try_new(
        Uuid::new_v4(),
        binding,
        "Artist".to_owned(),
        format!("Track {index}"),
        Some("Album".to_owned()),
        None,
        Some(1),
        180,
        1_700_000_000,
    )
    .unwrap()
}

fn unbound_pending(index: usize) -> UnboundLastFmScrobble {
    UnboundLastFmScrobble::try_new(
        Uuid::new_v4(),
        "Artist".to_owned(),
        format!("Track {index}"),
        Some("Album".to_owned()),
        None,
        Some(1),
        180,
        1_700_000_000,
    )
    .unwrap()
}

async fn enqueue_rows(database: &DatabaseConnection, binding: LastFmAccountBinding, count: usize) {
    for index in 0..count {
        assert!(matches!(
            storage::enqueue(database, &pending(binding, index))
                .await
                .unwrap(),
            LastFmEnqueueOutcome::Inserted { .. }
        ));
    }
}

fn accepted(count: usize) -> ScrobbleBatchResult {
    ScrobbleBatchResult {
        items: vec![SubmissionResult::Accepted { corrected: false }; count],
    }
}

async fn receive<T>(receiver: &async_channel::Receiver<T>) -> T {
    tokio::time::timeout(TEST_WATCHDOG, receiver.recv())
        .await
        .expect("fixture event arrived before the watchdog")
        .expect("fixture sender remained active")
}

async fn wait_for_status(
    status: &mut watch::Receiver<LastFmRuntimeStatus>,
    predicate: impl Fn(LastFmRuntimeStatus) -> bool,
) -> LastFmRuntimeStatus {
    loop {
        let snapshot = *status.borrow_and_update();
        if predicate(snapshot) {
            return snapshot;
        }
        tokio::time::timeout(TEST_WATCHDOG, status.changed())
            .await
            .expect("runtime status changed before the watchdog")
            .expect("runtime owner remained active");
    }
}

async fn ready_row(
    database: &DatabaseConnection,
    binding: LastFmAccountBinding,
    now_unix_ms: i64,
) -> StoredLastFmScrobble {
    let storage::LastFmBatchAvailability::Ready(receipt) =
        storage::batch_availability(database, binding, now_unix_ms, 50)
            .await
            .unwrap()
    else {
        panic!("expected one ready private queue row");
    };
    assert_eq!(receipt.len(), 1);
    receipt.rows()[0].clone()
}

#[tokio::test]
async fn fifty_one_rows_settle_as_exact_fifty_then_one_with_one_request_in_flight() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 51).await;
    let (transport, calls, responses, _retired, active) = GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session, active));
    let (clock, _waits) = ManualClock::new(0);
    let (handle, shutdown) =
        spawn_activated_runtime(database.clone(), store, transport.clone(), clock)
            .await
            .unwrap();
    let mut status = handle.subscribe_status();

    assert_eq!(
        receive(&calls).await,
        CallObservation {
            batch_size: 50,
            expected_vault_session: true,
        }
    );
    assert!(calls.try_recv().is_err());
    assert_eq!(transport.maximum_active(), 1);
    responses.send(Ok(accepted(50))).await.unwrap();

    assert_eq!(
        receive(&calls).await,
        CallObservation {
            batch_size: 1,
            expected_vault_session: true,
        }
    );
    assert_eq!(storage::queue_len(&database).await.unwrap(), 1);
    assert_eq!(transport.maximum_active(), 1);
    responses.send(Ok(accepted(1))).await.unwrap();
    let settled = wait_for_status(&mut status, |snapshot| snapshot.pending_scrobbles == 0).await;
    assert_eq!(settled.phase, LastFmRuntimePhase::Active);
    assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
    assert_eq!(
        shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
}

#[tokio::test]
async fn terminal_outcomes_update_only_bounded_aggregate_status() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 2).await;
    let (transport, calls, responses, _retired, active) = GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session, active));
    let (clock, _waits) = ManualClock::new(0);
    let (handle, shutdown) = spawn_activated_runtime(database.clone(), store, transport, clock)
        .await
        .unwrap();
    let mut status = handle.subscribe_status();

    assert_eq!(receive(&calls).await.batch_size, 2);
    responses
        .send(Ok(ScrobbleBatchResult {
            items: vec![
                SubmissionResult::Accepted { corrected: true },
                SubmissionResult::Ignored {
                    reason: IgnoredReason::Other(65_535),
                },
            ],
        }))
        .await
        .unwrap();
    let first = wait_for_status(&mut status, |snapshot| snapshot.pending_scrobbles == 0).await;
    assert_eq!(first.accepted_scrobbles, 1);
    assert_eq!(first.ignored_scrobbles, 1);
    assert_eq!(first.rejected_scrobbles, 0);

    handle
        .try_enqueue(unbound_pending(2))
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(receive(&calls).await.batch_size, 1);
    responses
        .send(Err(LastFmClientError::ServiceRejected { code: 29 }))
        .await
        .unwrap();
    let second = wait_for_status(&mut status, |snapshot| {
        snapshot.pending_scrobbles == 0 && snapshot.rejected_scrobbles == 1
    })
    .await;
    assert_eq!(second.accepted_scrobbles, 1);
    assert_eq!(second.ignored_scrobbles, 1);
    assert_eq!(second.rejected_scrobbles, 1);
    assert_eq!(second.phase, LastFmRuntimePhase::Active);
    assert_eq!(second.failure, None);
    shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn transient_retry_persists_exact_thirty_seconds_across_restart_and_deadline() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 1).await;

    let (first_transport, first_calls, first_responses, _first_retired, first_active) =
        GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session.clone(), first_active));
    let (first_clock, _first_waits) = ManualClock::new(1_000);
    let (first_handle, first_shutdown) = spawn_activated_runtime(
        database.clone(),
        store.clone(),
        first_transport,
        first_clock,
    )
    .await
    .unwrap();
    let mut first_status = first_handle.subscribe_status();
    assert_eq!(receive(&first_calls).await.batch_size, 1);
    first_responses
        .send(Err(LastFmClientError::Timeout))
        .await
        .unwrap();
    let retrying = wait_for_status(&mut first_status, |snapshot| {
        snapshot.failure == Some(LastFmRuntimeCommandError::Delivery)
    })
    .await;
    assert_eq!(retrying.pending_scrobbles, 1);
    assert!(matches!(
        storage::batch_availability(&database, binding, 1_000, 50)
            .await
            .unwrap(),
        storage::LastFmBatchAvailability::DelayedUntil {
            next_attempt_at_ms: 31_000
        }
    ));
    let rescheduled = ready_row(&database, binding, 31_000).await;
    assert_eq!(rescheduled.attempt_count, 1);
    assert_eq!(rescheduled.next_attempt_at_ms, 31_000);
    first_shutdown.shutdown().await.unwrap();
    assert!(store.has_session());
    assert!(store.delete_observations().is_empty());

    let (second_transport, second_calls, second_responses, _second_retired, second_active) =
        GatedTransport::new(session);
    assert_eq!(second_active.load(Ordering::SeqCst), 0);
    let (second_clock, second_waits) = ManualClock::new(30_999);
    let (second_handle, second_shutdown) = spawn_activated_runtime(
        database.clone(),
        store.clone(),
        second_transport,
        second_clock.clone(),
    )
    .await
    .unwrap();
    let mut second_status = second_handle.subscribe_status();
    assert_eq!(receive(&second_waits).await, 31_000);
    assert!(second_calls.try_recv().is_err());
    assert_eq!(ready_row(&database, binding, 31_000).await, rescheduled);

    second_clock.advance_to(31_000);
    assert_eq!(receive(&second_calls).await.batch_size, 1);
    second_responses.send(Ok(accepted(1))).await.unwrap();
    wait_for_status(&mut second_status, |snapshot| {
        snapshot.pending_scrobbles == 0
    })
    .await;
    assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
    second_shutdown.shutdown().await.unwrap();
    assert!(store.delete_observations().is_empty());
}

#[tokio::test]
async fn enqueue_during_durable_backoff_preserves_delivery_state_and_updates_count() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 1).await;
    let (transport, calls, responses, _retired, active) = GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session, active));
    let (clock, _waits) = ManualClock::new(1_000);
    let (handle, shutdown) = spawn_activated_runtime(database.clone(), store, transport, clock)
        .await
        .unwrap();
    let mut status = handle.subscribe_status();

    assert_eq!(receive(&calls).await.batch_size, 1);
    responses
        .send(Err(LastFmClientError::Transport))
        .await
        .unwrap();
    wait_for_status(&mut status, |snapshot| {
        snapshot.phase == LastFmRuntimePhase::BackingOff
    })
    .await;

    handle
        .try_enqueue(unbound_pending(1))
        .unwrap()
        .wait()
        .await
        .unwrap();
    let after_enqueue =
        wait_for_status(&mut status, |snapshot| snapshot.pending_scrobbles == 2).await;
    assert_eq!(after_enqueue.phase, LastFmRuntimePhase::BackingOff);
    assert_eq!(
        after_enqueue.failure,
        Some(LastFmRuntimeCommandError::Delivery)
    );
    assert_eq!(storage::queue_len(&database).await.unwrap(), 2);
    assert!(calls.try_recv().is_err());
    shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_retires_an_in_flight_request_without_mutating_or_deleting() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 1).await;
    let before = ready_row(&database, binding, 0).await;
    let (transport, calls, _responses, retired, active) = GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session, active.clone()));
    let (clock, _waits) = ManualClock::new(0);
    let (_handle, shutdown) =
        spawn_activated_runtime(database.clone(), store.clone(), transport, clock)
            .await
            .unwrap();

    assert_eq!(receive(&calls).await.batch_size, 1);
    assert_eq!(active.load(Ordering::SeqCst), 1);
    assert_eq!(
        shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
    receive(&retired).await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(ready_row(&database, binding, 0).await, before);
    assert!(store.has_session());
    assert!(store.delete_observations().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_before_actor_settlement_is_retained_for_at_least_once_replay() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 1).await;
    let before = ready_row(&database, binding, 0).await;
    let (transport, calls, responses, _retired, active) = GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session, active.clone()));
    let (clock, _waits) = ManualClock::new(0);
    let (handle, shutdown) =
        spawn_activated_runtime(database.clone(), store.clone(), transport.clone(), clock)
            .await
            .unwrap();
    let status = handle.subscribe_status();

    assert_eq!(receive(&calls).await.batch_size, 1);
    let first_payloads = transport.payloads();
    assert_eq!(first_payloads.len(), 1);
    {
        let mut ingress = handle.inner.ingress.lock().unwrap();
        responses.try_send(Ok(accepted(1))).unwrap();
        let deadline = Instant::now() + TEST_WATCHDOG;
        while active.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(status.borrow().pending_scrobbles, 1);
        assert_eq!(status.borrow().accepted_scrobbles, 0);
        assert_eq!(status.borrow().ignored_scrobbles, 0);
        assert_eq!(status.borrow().rejected_scrobbles, 0);

        // Linearize shutdown after the transport returned success but while the
        // relay is still excluded from the serialized mutation lane. The result
        // has no committed terminal proof, so it must be retired without delete.
        ingress.phase = IngressPhase::Closed;
        ingress.queue_admission_open = false;
        handle.inner.commands.try_send(Command::Shutdown).unwrap();
        cancel_gate_delivery(&mut ingress);
    }

    assert_eq!(
        shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
    assert_eq!(ready_row(&database, binding, 0).await, before);
    assert!(store.has_session());
    assert!(store.delete_observations().is_empty());

    let retained = ready_row(&database, binding, 0).await;
    let session = store.stored_session().unwrap();
    let (successor_transport, successor_calls, successor_responses, _retired, successor_active) =
        GatedTransport::new(session);
    assert_eq!(successor_active.load(Ordering::SeqCst), 0);
    let (successor_clock, _waits) = ManualClock::new(0);
    let (successor_handle, successor_shutdown) = spawn_activated_runtime(
        database.clone(),
        store.clone(),
        successor_transport.clone(),
        successor_clock,
    )
    .await
    .unwrap();
    let mut successor_status = successor_handle.subscribe_status();

    assert_eq!(receive(&successor_calls).await.batch_size, 1);
    assert_eq!(ready_row(&database, binding, 0).await, retained);
    assert_eq!(successor_transport.payloads(), first_payloads);
    let before_settlement = *successor_status.borrow_and_update();
    assert_eq!(before_settlement.pending_scrobbles, 1);
    assert_eq!(before_settlement.accepted_scrobbles, 0);
    assert_eq!(before_settlement.ignored_scrobbles, 0);
    assert_eq!(before_settlement.rejected_scrobbles, 0);

    successor_responses.send(Ok(accepted(1))).await.unwrap();
    let settled = wait_for_status(&mut successor_status, |snapshot| {
        snapshot.pending_scrobbles == 0
    })
    .await;
    assert_eq!(settled.accepted_scrobbles, 1);
    assert_eq!(settled.ignored_scrobbles, 0);
    assert_eq!(settled.rejected_scrobbles, 0);
    assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
    assert_eq!(
        successor_shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
    assert!(store.has_session());
    assert!(store.delete_observations().is_empty());
}

#[tokio::test]
async fn committed_delivery_result_precedes_a_later_disconnect_purge() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 1).await;
    let (transport, calls, responses, retired, active) = GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session, active));
    let (clock, _waits) = ManualClock::new(0);
    let (handle, shutdown) =
        spawn_activated_runtime(database.clone(), store.clone(), transport, clock)
            .await
            .unwrap();
    let mut status = handle.subscribe_status();

    assert_eq!(receive(&calls).await.batch_size, 1);
    responses.send(Ok(accepted(1))).await.unwrap();
    receive(&retired).await;
    wait_for_status(&mut status, |snapshot| snapshot.pending_scrobbles == 0).await;
    assert_eq!(storage::queue_len(&database).await.unwrap(), 0);

    assert_eq!(
        handle.disconnect_and_purge().unwrap().wait().await.unwrap(),
        0
    );
    assert_eq!(store.delete_observations(), vec![0]);
    assert!(!store.has_session());
    shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnect_cancels_and_joins_delivery_before_queue_purge_and_vault_delete() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 1).await;
    let (transport, calls, _responses, retired, active) = GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session, active.clone()));
    let (clock, _waits) = ManualClock::new(0);
    let (handle, shutdown) =
        spawn_activated_runtime(database.clone(), store.clone(), transport, clock)
            .await
            .unwrap();

    assert_eq!(receive(&calls).await.batch_size, 1);
    assert_eq!(active.load(Ordering::SeqCst), 1);
    assert_eq!(
        handle.disconnect_and_purge().unwrap().wait().await.unwrap(),
        1
    );
    receive(&retired).await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
    assert_eq!(store.delete_observations(), vec![0]);
    assert!(!store.has_session());
    assert_eq!(
        handle.subscribe_status().borrow().phase,
        LastFmRuntimePhase::Disconnected
    );
    shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn reauthentication_and_cardinality_pauses_retain_the_exact_private_row() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 1).await;
    let before = ready_row(&database, binding, 0).await;

    let (reauth_transport, reauth_calls, reauth_responses, _retired, reauth_active) =
        GatedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session.clone(), reauth_active));
    let (reauth_clock, _waits) = ManualClock::new(0);
    let (reauth_handle, reauth_shutdown) = spawn_activated_runtime(
        database.clone(),
        store.clone(),
        reauth_transport,
        reauth_clock,
    )
    .await
    .unwrap();
    let mut reauth_status = reauth_handle.subscribe_status();
    assert_eq!(receive(&reauth_calls).await.batch_size, 1);
    reauth_responses
        .send(Err(LastFmClientError::ReauthenticationRequired))
        .await
        .unwrap();
    let paused = wait_for_status(&mut reauth_status, |snapshot| {
        snapshot.phase == LastFmRuntimePhase::ReauthenticationRequired
    })
    .await;
    assert_eq!(
        paused.failure,
        Some(LastFmRuntimeCommandError::ReauthenticationRequired)
    );
    assert_eq!(paused.pending_scrobbles, 1);
    assert_eq!(ready_row(&database, binding, 0).await, before);
    reauth_shutdown.shutdown().await.unwrap();

    let (cardinality_transport, cardinality_calls, cardinality_responses, _retired, active) =
        GatedTransport::new(session);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    let (cardinality_clock, _waits) = ManualClock::new(0);
    let (cardinality_handle, cardinality_shutdown) = spawn_activated_runtime(
        database.clone(),
        store.clone(),
        cardinality_transport,
        cardinality_clock,
    )
    .await
    .unwrap();
    let mut cardinality_status = cardinality_handle.subscribe_status();
    assert_eq!(receive(&cardinality_calls).await.batch_size, 1);
    cardinality_responses.send(Ok(accepted(0))).await.unwrap();
    let paused = wait_for_status(&mut cardinality_status, |snapshot| {
        snapshot.phase == LastFmRuntimePhase::CompatibilityPaused
    })
    .await;
    assert_eq!(
        paused.failure,
        Some(LastFmRuntimeCommandError::Compatibility)
    );
    assert_eq!(paused.pending_scrobbles, 1);
    assert_eq!(ready_row(&database, binding, 0).await, before);
    cardinality_shutdown.shutdown().await.unwrap();
    assert!(store.has_session());
    assert!(store.delete_observations().is_empty());
}

#[tokio::test]
async fn code_nine_keeps_queue_admission_open_and_exact_reauthorization_restarts_delivery() {
    let database = database().await;
    let original = session();
    let binding = original.account_binding();
    enqueue_rows(&database, binding, 1).await;
    let renewed = original
        .reauthorized("listener", ProtectedString::new(SESSION_KEY_B))
        .unwrap();
    let (transport, calls, responses, _retired, active) = GatedTransport::new(original.clone());
    let store = Arc::new(TestCredentialStore::new(original, active));
    let (clock, _waits) = ManualClock::new(0);
    let (handle, shutdown) =
        spawn_activated_runtime(database.clone(), store.clone(), transport.clone(), clock)
            .await
            .unwrap();
    let mut status = handle.subscribe_status();

    assert_eq!(receive(&calls).await.batch_size, 1);
    responses
        .send(Err(LastFmClientError::ReauthenticationRequired))
        .await
        .unwrap();
    wait_for_status(&mut status, |snapshot| {
        snapshot.phase == LastFmRuntimePhase::ReauthenticationRequired
    })
    .await;

    handle
        .try_enqueue(unbound_pending(1))
        .unwrap()
        .wait()
        .await
        .unwrap();
    let queued_while_paused = wait_for_status(&mut status, |snapshot| {
        snapshot.phase == LastFmRuntimePhase::ReauthenticationRequired
            && snapshot.pending_scrobbles == 2
    })
    .await;
    assert_eq!(
        queued_while_paused.failure,
        Some(LastFmRuntimeCommandError::ReauthenticationRequired)
    );
    assert!(calls.try_recv().is_err());

    assert_eq!(
        handle
            .reauthorize_same_account(
                "different-listener".to_owned(),
                ProtectedString::new(SESSION_KEY_B),
            )
            .unwrap()
            .wait()
            .await,
        Err(LastFmRuntimeCommandError::AccountReplacementRequired)
    );
    assert_eq!(
        status.borrow().phase,
        LastFmRuntimePhase::ReauthenticationRequired
    );
    assert_eq!(storage::queue_len(&database).await.unwrap(), 2);
    assert!(calls.try_recv().is_err());

    transport.expect_session(renewed.clone());
    handle
        .reauthorize_same_account("listener".to_owned(), ProtectedString::new(SESSION_KEY_B))
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(store.stored_session(), Some(renewed));
    assert_eq!(
        receive(&calls).await,
        CallObservation {
            batch_size: 2,
            expected_vault_session: true,
        }
    );
    responses.send(Ok(accepted(2))).await.unwrap();
    let delivered = wait_for_status(&mut status, |snapshot| snapshot.pending_scrobbles == 0).await;
    assert_eq!(delivered.phase, LastFmRuntimePhase::Active);
    assert_eq!(delivered.accepted_scrobbles, 2);
    assert_eq!(delivered.ignored_scrobbles, 0);
    assert_eq!(delivered.rejected_scrobbles, 0);
    assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
    shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_worker_uses_the_same_binding_reauthorized_session_loaded_from_the_vault() {
    let database = database().await;
    let original = session();
    let binding = original.account_binding();
    let reauthorized = original
        .reauthorized("listener", ProtectedString::new(SESSION_KEY_B))
        .unwrap();
    assert_eq!(reauthorized.account_binding(), binding);
    enqueue_rows(&database, binding, 1).await;
    let (transport, calls, responses, _retired, active) = GatedTransport::new(reauthorized.clone());
    let store = Arc::new(TestCredentialStore::new(reauthorized, active));
    let (clock, _waits) = ManualClock::new(0);
    let (handle, shutdown) = spawn_activated_runtime(database.clone(), store, transport, clock)
        .await
        .unwrap();
    let mut status = handle.subscribe_status();

    assert_eq!(
        receive(&calls).await,
        CallObservation {
            batch_size: 1,
            expected_vault_session: true,
        }
    );
    responses.send(Ok(accepted(1))).await.unwrap();
    wait_for_status(&mut status, |snapshot| snapshot.pending_scrobbles == 0).await;
    assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
    shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn panicking_delivery_task_is_sanitized_paused_and_retains_the_exact_queue() {
    let database = database().await;
    let session = session();
    let binding = session.account_binding();
    enqueue_rows(&database, binding, 1).await;
    let before = ready_row(&database, binding, 0).await;
    let active = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(TestCredentialStore::new(session, active));
    let (clock, _waits) = ManualClock::new(0);
    let transport: Arc<dyn LastFmTransport> = Arc::new(PanickingTransport);

    let (handle, shutdown) = spawn_activated_runtime(database.clone(), store, transport, clock)
        .await
        .unwrap();
    let mut status = handle.subscribe_status();
    let paused = wait_for_status(&mut status, |snapshot| {
        snapshot.phase == LastFmRuntimePhase::CapabilityPaused
    })
    .await;

    assert_eq!(
        paused.failure,
        Some(LastFmRuntimeCommandError::DeliveryCapability)
    );
    assert_eq!(paused.pending_scrobbles, 1);
    assert_eq!(ready_row(&database, binding, 0).await, before);
    assert_eq!(
        handle.try_enqueue(unbound_pending(1)).unwrap_err(),
        LastFmRuntimeAdmissionError::Paused
    );
    assert_eq!(
        format!("{:?}", LastFmDeliveryWorkerFailure::UnexpectedTaskExit),
        "UnexpectedTaskExit"
    );
    shutdown.shutdown().await.unwrap();
}
