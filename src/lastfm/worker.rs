//! Non-mutating Last.fm queue-delivery worker.
//!
//! This task may inspect the durable FIFO and perform one bounded network
//! request at a time. It never settles, reschedules, or purges queue rows.
//! Instead it transfers the exact opaque receipt and sanitized client result
//! to the serialized runtime actor, then waits until that actor confirms its
//! durable mutation before inspecting another batch.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use sea_orm::DatabaseConnection;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::client::{LastFmClientError, ScrobbleBatchResult, MAX_SCROBBLES_PER_BATCH};
use super::credentials::{LastFmAccountBinding, StoredSession};
use super::delivery::{
    scrobbles_from_receipt, LastFmClock, LastFmDeliveryPrimitiveError, LastFmTransport,
};
use super::storage::{self, LastFmBatchAvailability, LastFmBatchReceipt, LastFmQueueError};

/// Opaque identity of one delivery-worker lifetime.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct LastFmDeliveryGeneration(u64);

impl LastFmDeliveryGeneration {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub(super) const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl fmt::Debug for LastFmDeliveryGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmDeliveryGeneration(..)")
    }
}

/// Actor decision after it has handled one exact delivery result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmDeliveryDirective {
    Continue,
    Stop,
}

/// Content-free reason automatic delivery stopped before a network result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmDeliveryWorkerFailure {
    Storage(LastFmQueueError),
    Clock(LastFmDeliveryPrimitiveError),
    Preparation(LastFmDeliveryPrimitiveError),
    /// The delivery future unwound before producing a typed outcome.
    ///
    /// Panic payloads are deliberately discarded at this boundary: they can
    /// contain arbitrary transport state and must never cross into status,
    /// diagnostics, or UI error handling.
    UnexpectedTaskExit,
}

/// Acknowledgement capability for one exact result event.
///
/// Dropping this capability without responding stops the worker. That keeps an
/// actor failure from allowing a later batch to bypass an unsettled receipt.
pub struct LastFmDeliveryAcknowledgement {
    sender: Option<oneshot::Sender<LastFmDeliveryDirective>>,
}

impl LastFmDeliveryAcknowledgement {
    #[must_use]
    pub fn acknowledge(mut self, directive: LastFmDeliveryDirective) -> bool {
        self.sender
            .take()
            .is_some_and(|sender| sender.send(directive).is_ok())
    }
}

impl fmt::Debug for LastFmDeliveryAcknowledgement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmDeliveryAcknowledgement(..)")
    }
}

/// One sanitized client result joined to the exact private FIFO receipt.
pub struct LastFmDeliveryResultEvent {
    generation: LastFmDeliveryGeneration,
    receipt: LastFmBatchReceipt,
    result: Result<ScrobbleBatchResult, LastFmClientError>,
    acknowledgement: Option<oneshot::Sender<LastFmDeliveryDirective>>,
}

impl LastFmDeliveryResultEvent {
    #[must_use]
    pub const fn generation(&self) -> LastFmDeliveryGeneration {
        self.generation
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.receipt.len()
    }

    /// Transfer every value needed for the actor's generation check and
    /// durable settlement/reschedule decision.
    pub fn into_parts(
        mut self,
    ) -> (
        LastFmDeliveryGeneration,
        LastFmBatchReceipt,
        Result<ScrobbleBatchResult, LastFmClientError>,
        LastFmDeliveryAcknowledgement,
    ) {
        let acknowledgement = LastFmDeliveryAcknowledgement {
            sender: self.acknowledgement.take(),
        };
        (self.generation, self.receipt, self.result, acknowledgement)
    }
}

impl fmt::Debug for LastFmDeliveryResultEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmDeliveryResultEvent")
            .field("generation", &self.generation)
            .field("row_count", &self.receipt.len())
            .field(
                "result_category",
                &if self.result.is_ok() {
                    "complete"
                } else {
                    "client-failure"
                },
            )
            .finish_non_exhaustive()
    }
}

/// Message sent from the non-mutating worker to the serialized runtime actor.
pub enum LastFmDeliveryEvent {
    Result(LastFmDeliveryResultEvent),
    Failed {
        generation: LastFmDeliveryGeneration,
        failure: LastFmDeliveryWorkerFailure,
    },
}

impl fmt::Debug for LastFmDeliveryEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Result(result) => result.fmt(formatter),
            Self::Failed {
                generation,
                failure,
            } => formatter
                .debug_struct("LastFmDeliveryFailureEvent")
                .field("generation", generation)
                .field("failure", failure)
                .finish(),
        }
    }
}

/// Privacy-safe reason the worker task ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmDeliveryWorkerExit {
    Cancelled,
    DirectedStop,
    WakeChannelClosed,
    ActorChannelClosed,
    ActorAcknowledgementDropped,
    Failed(LastFmDeliveryWorkerFailure),
}

/// Sanitized worker-task join failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Last.fm delivery worker stopped unexpectedly")]
pub struct LastFmDeliveryWorkerJoinError;

/// Wake, cancellation, and sole join authority for one worker generation.
pub struct LastFmDeliveryWorker {
    wake: watch::Sender<u64>,
    cancellation: CancellationToken,
    task: Option<JoinHandle<LastFmDeliveryWorkerExit>>,
}

impl LastFmDeliveryWorker {
    /// Notify an empty or delayed worker that durable queue state may have
    /// changed. Revisions coalesce without losing the fact of a change.
    #[must_use]
    pub fn wake(&self) -> bool {
        if self.wake.receiver_count() == 0 {
            return false;
        }
        self.wake
            .send_modify(|revision| *revision = revision.saturating_add(1));
        true
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Clone the cancellation capability into the lifecycle admission gate.
    ///
    /// Disconnect and shutdown signal this while they still hold the shared
    /// ingress mutex, so an in-flight request cannot remain live while already
    /// queued metadata delays the actor's lifecycle marker.
    #[must_use]
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub async fn join(mut self) -> Result<LastFmDeliveryWorkerExit, LastFmDeliveryWorkerJoinError> {
        self.join_inner().await
    }

    pub async fn cancel_and_join(
        mut self,
    ) -> Result<LastFmDeliveryWorkerExit, LastFmDeliveryWorkerJoinError> {
        self.cancellation.cancel();
        self.join_inner().await
    }

    async fn join_inner(
        &mut self,
    ) -> Result<LastFmDeliveryWorkerExit, LastFmDeliveryWorkerJoinError> {
        let task = self.task.take().ok_or(LastFmDeliveryWorkerJoinError)?;
        task.await.map_err(|_| LastFmDeliveryWorkerJoinError)
    }
}

impl Drop for LastFmDeliveryWorker {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl fmt::Debug for LastFmDeliveryWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmDeliveryWorker")
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("joined", &self.task.is_none())
            .finish_non_exhaustive()
    }
}

struct DeliveryTask {
    database: DatabaseConnection,
    account_binding: LastFmAccountBinding,
    session: StoredSession,
    generation: LastFmDeliveryGeneration,
    transport: Arc<dyn LastFmTransport>,
    clock: Arc<dyn LastFmClock>,
    cancellation: CancellationToken,
    wake: watch::Receiver<u64>,
    events: async_channel::Sender<LastFmDeliveryEvent>,
}

/// Spawn the sole reader/network task for one authorized account generation.
///
/// The queue binding is derived from the retained vault session, preventing a
/// caller from pairing one account's credentials with another account's FIFO.
#[must_use]
pub fn spawn_lastfm_delivery_worker(
    database: DatabaseConnection,
    session: StoredSession,
    generation: LastFmDeliveryGeneration,
    transport: Arc<dyn LastFmTransport>,
    clock: Arc<dyn LastFmClock>,
    events: async_channel::Sender<LastFmDeliveryEvent>,
) -> LastFmDeliveryWorker {
    let account_binding = session.account_binding();
    let cancellation = CancellationToken::new();
    let (wake_sender, wake) = watch::channel(0_u64);
    let task_cancellation = cancellation.clone();
    let supervisor_cancellation = cancellation.clone();
    let supervisor_events = events.clone();
    let delivery = DeliveryTask {
        database,
        account_binding,
        session,
        generation,
        transport,
        clock,
        cancellation: task_cancellation,
        wake,
        events,
    }
    .run();
    let task = tokio::spawn(async move {
        match AssertUnwindSafe(delivery).catch_unwind().await {
            Ok(exit) => exit,
            Err(_) if supervisor_cancellation.is_cancelled() => LastFmDeliveryWorkerExit::Cancelled,
            Err(_) => {
                send_failure_event(
                    generation,
                    LastFmDeliveryWorkerFailure::UnexpectedTaskExit,
                    &supervisor_events,
                    &supervisor_cancellation,
                )
                .await
            }
        }
    });
    LastFmDeliveryWorker {
        wake: wake_sender,
        cancellation,
        task: Some(task),
    }
}

impl DeliveryTask {
    async fn run(mut self) -> LastFmDeliveryWorkerExit {
        loop {
            // Treat the current revision as represented by the following
            // authoritative database read. A racing later revision remains
            // observable by `changed()`.
            let _ = self.wake.borrow_and_update();
            let now_unix_ms = match self.clock.now_unix_ms() {
                Ok(now_unix_ms) => now_unix_ms,
                Err(error) => {
                    return self.fail(LastFmDeliveryWorkerFailure::Clock(error)).await;
                }
            };
            if self.cancellation.is_cancelled() {
                return LastFmDeliveryWorkerExit::Cancelled;
            }
            // Join the bounded local read instead of dropping its SQL future
            // when lifecycle cancellation wins. Runtime shutdown may release
            // the process-wide vault generation only after this worker joins;
            // completing the read makes that join a true database-quiescence
            // boundary for a successor runtime.
            let availability = storage::batch_availability(
                &self.database,
                self.account_binding,
                now_unix_ms,
                MAX_SCROBBLES_PER_BATCH,
            )
            .await;
            if self.cancellation.is_cancelled() {
                return LastFmDeliveryWorkerExit::Cancelled;
            }
            let availability = match availability {
                Ok(availability) => availability,
                Err(error) => {
                    return self.fail(LastFmDeliveryWorkerFailure::Storage(error)).await;
                }
            };

            match availability {
                LastFmBatchAvailability::Empty => match self.wait_for_wake().await {
                    WaitOutcome::Wake => {}
                    WaitOutcome::Cancelled => return LastFmDeliveryWorkerExit::Cancelled,
                    WaitOutcome::WakeChannelClosed => {
                        return LastFmDeliveryWorkerExit::WakeChannelClosed;
                    }
                    WaitOutcome::ClockFailed(_) => unreachable!("empty wait has no clock"),
                },
                LastFmBatchAvailability::DelayedUntil { next_attempt_at_ms } => {
                    match self.wait_for_deadline_or_wake(next_attempt_at_ms).await {
                        WaitOutcome::Wake => {}
                        WaitOutcome::Cancelled => return LastFmDeliveryWorkerExit::Cancelled,
                        WaitOutcome::WakeChannelClosed => {
                            return LastFmDeliveryWorkerExit::WakeChannelClosed;
                        }
                        WaitOutcome::ClockFailed(error) => {
                            return self.fail(LastFmDeliveryWorkerFailure::Clock(error)).await;
                        }
                    }
                }
                LastFmBatchAvailability::Ready(receipt) => {
                    let scrobbles = match scrobbles_from_receipt(&receipt) {
                        Ok(scrobbles) => scrobbles,
                        Err(error) => {
                            return self
                                .fail(LastFmDeliveryWorkerFailure::Preparation(error))
                                .await;
                        }
                    };
                    let result = tokio::select! {
                        () = self.cancellation.cancelled() => {
                            return LastFmDeliveryWorkerExit::Cancelled;
                        }
                        result = self.transport.submit_scrobbles(&self.session, &scrobbles) => {
                            result
                        }
                    };
                    let (acknowledgement, directive) = oneshot::channel();
                    let event = LastFmDeliveryEvent::Result(LastFmDeliveryResultEvent {
                        generation: self.generation,
                        receipt,
                        result,
                        acknowledgement: Some(acknowledgement),
                    });
                    let sent = tokio::select! {
                        biased;
                        sent = self.events.send(event) => sent,
                        () = self.cancellation.cancelled() => {
                            return LastFmDeliveryWorkerExit::Cancelled;
                        }
                    };
                    if sent.is_err() {
                        return LastFmDeliveryWorkerExit::ActorChannelClosed;
                    }
                    let directive = tokio::select! {
                        () = self.cancellation.cancelled() => {
                            return LastFmDeliveryWorkerExit::Cancelled;
                        }
                        directive = directive => directive,
                    };
                    match directive {
                        Ok(LastFmDeliveryDirective::Continue) => {}
                        Ok(LastFmDeliveryDirective::Stop) => {
                            return LastFmDeliveryWorkerExit::DirectedStop;
                        }
                        Err(_) => {
                            return LastFmDeliveryWorkerExit::ActorAcknowledgementDropped;
                        }
                    }
                }
            }
        }
    }

    async fn wait_for_wake(&mut self) -> WaitOutcome {
        tokio::select! {
            () = self.cancellation.cancelled() => WaitOutcome::Cancelled,
            changed = self.wake.changed() => if changed.is_ok() {
                WaitOutcome::Wake
            } else {
                WaitOutcome::WakeChannelClosed
            },
        }
    }

    async fn wait_for_deadline_or_wake(&mut self, deadline_unix_ms: i64) -> WaitOutcome {
        tokio::select! {
            () = self.cancellation.cancelled() => WaitOutcome::Cancelled,
            changed = self.wake.changed() => if changed.is_ok() {
                WaitOutcome::Wake
            } else {
                WaitOutcome::WakeChannelClosed
            },
            waited = self.clock.wait_until_unix_ms(deadline_unix_ms) => match waited {
                Ok(()) => WaitOutcome::Wake,
                Err(error) => WaitOutcome::ClockFailed(error),
            },
        }
    }

    async fn fail(&self, failure: LastFmDeliveryWorkerFailure) -> LastFmDeliveryWorkerExit {
        send_failure_event(self.generation, failure, &self.events, &self.cancellation).await
    }
}

async fn send_failure_event(
    generation: LastFmDeliveryGeneration,
    failure: LastFmDeliveryWorkerFailure,
    events: &async_channel::Sender<LastFmDeliveryEvent>,
    cancellation: &CancellationToken,
) -> LastFmDeliveryWorkerExit {
    let event = LastFmDeliveryEvent::Failed {
        generation,
        failure,
    };
    let sent = tokio::select! {
        biased;
        sent = events.send(event) => sent,
        () = cancellation.cancelled() => {
            return LastFmDeliveryWorkerExit::Cancelled;
        }
    };
    if sent.is_ok() {
        LastFmDeliveryWorkerExit::Failed(failure)
    } else {
        LastFmDeliveryWorkerExit::ActorChannelClosed
    }
}

enum WaitOutcome {
    Wake,
    Cancelled,
    WakeChannelClosed,
    ClockFailed(LastFmDeliveryPrimitiveError),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;
    use uuid::Uuid;

    use super::*;
    use crate::db::migration::Migrator;
    use crate::lastfm::client::{LastFmTrack, Scrobble, SubmissionResult};
    use crate::lastfm::credentials::ProtectedString;
    use crate::lastfm::storage::{LastFmEnqueueOutcome, PendingLastFmScrobble};

    const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";
    const TEST_DEADLINE: Duration = Duration::from_secs(2);

    struct ManualClock {
        now: AtomicI64,
        changed: watch::Sender<i64>,
        waits: async_channel::Sender<i64>,
    }

    impl ManualClock {
        fn new(now: i64) -> (Arc<Self>, async_channel::Receiver<i64>) {
            let (changed, _) = watch::channel(now);
            let (waits, wait_events) = async_channel::unbounded();
            (
                Arc::new(Self {
                    now: AtomicI64::new(now),
                    changed,
                    waits,
                }),
                wait_events,
            )
        }

        fn advance_to(&self, now: i64) {
            self.now.store(now, Ordering::SeqCst);
            self.changed.send_replace(now);
        }
    }

    #[async_trait::async_trait]
    impl LastFmClock for ManualClock {
        fn now_unix_ms(&self) -> Result<i64, LastFmDeliveryPrimitiveError> {
            Ok(self.now.load(Ordering::SeqCst))
        }

        async fn wait_until_unix_ms(
            &self,
            deadline_unix_ms: i64,
        ) -> Result<(), LastFmDeliveryPrimitiveError> {
            let _ = self.waits.try_send(deadline_unix_ms);
            let mut changed = self.changed.subscribe();
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

    struct InFlightGuard<'a>(&'a AtomicUsize);

    impl Drop for InFlightGuard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct ScriptedTransport {
        calls: Mutex<Vec<Vec<Scrobble>>>,
        call_events: async_channel::Sender<usize>,
        responses: async_channel::Receiver<Result<ScrobbleBatchResult, LastFmClientError>>,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
    }

    impl ScriptedTransport {
        fn new() -> (
            Arc<Self>,
            async_channel::Receiver<usize>,
            async_channel::Sender<Result<ScrobbleBatchResult, LastFmClientError>>,
        ) {
            let (call_events, calls) = async_channel::unbounded();
            let (responses, response_events) = async_channel::unbounded();
            (
                Arc::new(Self {
                    calls: Mutex::new(Vec::new()),
                    call_events,
                    responses: response_events,
                    active: AtomicUsize::new(0),
                    maximum_active: AtomicUsize::new(0),
                }),
                calls,
                responses,
            )
        }

        fn maximum_active(&self) -> usize {
            self.maximum_active.load(Ordering::SeqCst)
        }

        fn calls(&self) -> MutexGuard<'_, Vec<Vec<Scrobble>>> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    #[async_trait::async_trait]
    impl LastFmTransport for ScriptedTransport {
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
            scrobbles: &[Scrobble],
        ) -> Result<ScrobbleBatchResult, LastFmClientError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(scrobbles.to_vec());
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum_active.fetch_max(active, Ordering::SeqCst);
            let _guard = InFlightGuard(&self.active);
            let _ = self.call_events.send(scrobbles.len()).await;
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

    async fn database() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        database
    }

    fn session() -> StoredSession {
        StoredSession::new("listener", ProtectedString::new(SESSION_KEY)).unwrap()
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
            60,
            1_700_000_000,
        )
        .unwrap()
    }

    fn accepted(count: usize) -> ScrobbleBatchResult {
        ScrobbleBatchResult {
            items: vec![SubmissionResult::Accepted { corrected: false }; count],
        }
    }

    async fn receive<T>(receiver: &async_channel::Receiver<T>) -> T {
        tokio::time::timeout(TEST_DEADLINE, receiver.recv())
            .await
            .expect("fixture event before watchdog")
            .expect("fixture sender remains active")
    }

    fn spawn_fixture(
        database: DatabaseConnection,
        session: StoredSession,
        transport: Arc<ScriptedTransport>,
        clock: Arc<ManualClock>,
    ) -> (
        LastFmDeliveryWorker,
        async_channel::Receiver<LastFmDeliveryEvent>,
    ) {
        let (events, event_receiver) = async_channel::unbounded();
        let transport: Arc<dyn LastFmTransport> = transport;
        let clock: Arc<dyn LastFmClock> = clock;
        (
            spawn_lastfm_delivery_worker(
                database,
                session,
                LastFmDeliveryGeneration::new(7),
                transport,
                clock,
                events,
            ),
            event_receiver,
        )
    }

    async fn settle_and_acknowledge(
        database: &DatabaseConnection,
        event: LastFmDeliveryEvent,
        directive: LastFmDeliveryDirective,
    ) -> usize {
        let LastFmDeliveryEvent::Result(event) = event else {
            panic!("expected result event");
        };
        let count = event.row_count();
        let (generation, receipt, result, acknowledgement) = event.into_parts();
        assert_eq!(generation, LastFmDeliveryGeneration::new(7));
        assert_eq!(result.unwrap().items.len(), count);
        storage::settle_terminal(database, &receipt).await.unwrap();
        assert!(acknowledgement.acknowledge(directive));
        count
    }

    #[tokio::test]
    async fn empty_worker_wakes_for_a_new_durable_row() {
        let database = database().await;
        let session = session();
        let binding = session.account_binding();
        let (transport, calls, responses) = ScriptedTransport::new();
        let (clock, _waits) = ManualClock::new(0);
        let (worker, events) =
            spawn_fixture(database.clone(), session, Arc::clone(&transport), clock);

        assert!(matches!(
            storage::enqueue(&database, &pending(binding, 0))
                .await
                .unwrap(),
            LastFmEnqueueOutcome::Inserted { .. }
        ));
        assert!(worker.wake());
        assert_eq!(receive(&calls).await, 1);
        responses.send(Ok(accepted(1))).await.unwrap();
        assert_eq!(
            settle_and_acknowledge(
                &database,
                receive(&events).await,
                LastFmDeliveryDirective::Stop,
            )
            .await,
            1
        );
        assert_eq!(
            worker.join().await.unwrap(),
            LastFmDeliveryWorkerExit::DirectedStop
        );
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn delayed_head_runs_at_the_exact_injected_deadline() {
        let database = database().await;
        let session = session();
        let binding = session.account_binding();
        storage::enqueue(&database, &pending(binding, 0))
            .await
            .unwrap();
        let LastFmBatchAvailability::Ready(receipt) =
            storage::batch_availability(&database, binding, 0, 50)
                .await
                .unwrap()
        else {
            panic!("fixture row is initially due");
        };
        storage::reschedule_batch(&database, &receipt, 100)
            .await
            .unwrap();

        let (transport, calls, responses) = ScriptedTransport::new();
        let (clock, waits) = ManualClock::new(99);
        let (worker, events) = spawn_fixture(
            database.clone(),
            session,
            Arc::clone(&transport),
            Arc::clone(&clock),
        );
        assert_eq!(receive(&waits).await, 100);
        assert!(calls.try_recv().is_err());
        clock.advance_to(100);
        assert_eq!(receive(&calls).await, 1);
        responses.send(Ok(accepted(1))).await.unwrap();
        settle_and_acknowledge(
            &database,
            receive(&events).await,
            LastFmDeliveryDirective::Stop,
        )
        .await;
        assert_eq!(
            worker.join().await.unwrap(),
            LastFmDeliveryWorkerExit::DirectedStop
        );
    }

    #[tokio::test]
    async fn fifty_one_rows_are_serialized_as_fifty_then_one_after_actor_ack() {
        let database = database().await;
        let session = session();
        let binding = session.account_binding();
        for index in 0..51 {
            storage::enqueue(&database, &pending(binding, index))
                .await
                .unwrap();
        }
        let (transport, calls, responses) = ScriptedTransport::new();
        let (clock, _waits) = ManualClock::new(0);
        let (worker, events) =
            spawn_fixture(database.clone(), session, Arc::clone(&transport), clock);

        assert_eq!(receive(&calls).await, 50);
        assert!(calls.try_recv().is_err());
        responses.send(Ok(accepted(50))).await.unwrap();
        let first = receive(&events).await;
        assert!(calls.try_recv().is_err(), "actor ack is a strict barrier");
        assert_eq!(
            settle_and_acknowledge(&database, first, LastFmDeliveryDirective::Continue).await,
            50
        );

        assert_eq!(receive(&calls).await, 1);
        responses.send(Ok(accepted(1))).await.unwrap();
        assert_eq!(
            settle_and_acknowledge(
                &database,
                receive(&events).await,
                LastFmDeliveryDirective::Stop,
            )
            .await,
            1
        );
        assert_eq!(
            worker.join().await.unwrap(),
            LastFmDeliveryWorkerExit::DirectedStop
        );
        assert_eq!(transport.maximum_active(), 1);
        let calls = transport.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][0].track.title, "Track 0");
        assert_eq!(calls[0][49].track.title, "Track 49");
        assert_eq!(calls[1][0].track.title, "Track 50");
    }

    #[tokio::test]
    async fn cancellation_drops_the_inflight_request_and_retains_its_receipt() {
        let database = database().await;
        let session = session();
        let binding = session.account_binding();
        storage::enqueue(&database, &pending(binding, 0))
            .await
            .unwrap();
        let (transport, calls, _responses) = ScriptedTransport::new();
        let (clock, _waits) = ManualClock::new(0);
        let (worker, events) =
            spawn_fixture(database.clone(), session, Arc::clone(&transport), clock);

        assert_eq!(receive(&calls).await, 1);
        worker.cancel();
        assert_eq!(
            worker.join().await.unwrap(),
            LastFmDeliveryWorkerExit::Cancelled
        );
        assert_eq!(transport.active.load(Ordering::SeqCst), 0);
        assert_eq!(storage::queue_len(&database).await.unwrap(), 1);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn panicking_delivery_future_emits_only_a_sanitized_failure_and_retains_its_receipt() {
        let database = database().await;
        let session = session();
        let binding = session.account_binding();
        storage::enqueue(&database, &pending(binding, 0))
            .await
            .unwrap();
        let (clock, _waits) = ManualClock::new(0);
        let (events, event_receiver) = async_channel::unbounded();
        let transport: Arc<dyn LastFmTransport> = Arc::new(PanickingTransport);
        let worker = spawn_lastfm_delivery_worker(
            database.clone(),
            session,
            LastFmDeliveryGeneration::new(7),
            transport,
            clock,
            events,
        );

        let failure = LastFmDeliveryWorkerFailure::UnexpectedTaskExit;
        assert!(matches!(
            receive(&event_receiver).await,
            LastFmDeliveryEvent::Failed {
                generation,
                failure: observed,
            } if generation == LastFmDeliveryGeneration::new(7) && observed == failure
        ));
        assert_eq!(
            worker.join().await.unwrap(),
            LastFmDeliveryWorkerExit::Failed(failure)
        );
        assert_eq!(storage::queue_len(&database).await.unwrap(), 1);
        assert_eq!(format!("{failure:?}"), "UnexpectedTaskExit");
    }

    #[tokio::test]
    async fn corrupt_storage_emits_a_typed_failure_without_network_or_mutation() {
        let database = database().await;
        let session = session();
        let binding = session.account_binding();
        database
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO lastfm_scrobble_queue (
                     occurrence_id, account_binding, artist, track_title,
                     duration_secs, started_at_unix_secs, attempt_count,
                     next_attempt_at_ms
                 ) VALUES (?, ?, 'Artist', 'Track', 60, 1, 0, 0)",
                [
                    Uuid::nil().as_bytes().to_vec().into(),
                    binding.as_bytes().to_vec().into(),
                ],
            ))
            .await
            .unwrap();
        let (transport, calls, _responses) = ScriptedTransport::new();
        let (clock, _waits) = ManualClock::new(0);
        let (worker, events) = spawn_fixture(database.clone(), session, transport, clock);

        let failure = LastFmDeliveryWorkerFailure::Storage(LastFmQueueError::CorruptStorage);
        assert!(matches!(
            receive(&events).await,
            LastFmDeliveryEvent::Failed {
                generation,
                failure: observed,
            } if generation == LastFmDeliveryGeneration::new(7) && observed == failure
        ));
        assert_eq!(
            worker.join().await.unwrap(),
            LastFmDeliveryWorkerExit::Failed(failure)
        );
        assert!(calls.try_recv().is_err());
        assert_eq!(storage::queue_len(&database).await.unwrap(), 1);
    }

    #[test]
    fn worker_diagnostics_are_content_free() {
        let generation = LastFmDeliveryGeneration::new(7);
        let diagnostics = format!(
            "{generation:?} {:?} {:?}",
            LastFmDeliveryWorkerFailure::Preparation(
                LastFmDeliveryPrimitiveError::InvalidStoredRow
            ),
            LastFmDeliveryAcknowledgement { sender: None },
        );
        assert!(!diagnostics.contains("Track"));
        assert!(!diagnostics.contains(SESSION_KEY));
        assert!(!diagnostics.contains('7'));
    }
}
