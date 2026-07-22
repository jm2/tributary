//! Serialized lifecycle owner for the private Last.fm scrobble queue.
//!
//! Metadata commands use a bounded FIFO while three slots remain reserved for
//! one delivery result and two lifecycle markers. One shared ingress mutex is
//! the linearization point: an
//! enqueue which wins it is ordered before disconnect or shutdown, and an
//! enqueue which loses is rejected. Only the receiver mutates SQLite or the
//! protected credential store.

use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};

use sea_orm::DatabaseConnection;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::client::{LastFmClientError, SubmissionResult};
use super::credentials::{
    CredentialError, LastFmAccountBinding, ProtectedString, SessionCredentialStore, StoredSession,
};
use super::delivery::{
    delivery_disposition, next_retry_at_ms, LastFmClock, LastFmDeliveryDisposition, LastFmTransport,
};
use super::lifecycle::{acquire_vault_lifecycle, LastFmVaultLifecycleLease};
use super::storage::{
    self, LastFmEnqueueOutcome, LastFmQueueError, PendingLastFmScrobble, UnboundLastFmScrobble,
};
use super::worker::{
    spawn_lastfm_delivery_worker, LastFmDeliveryAcknowledgement, LastFmDeliveryDirective,
    LastFmDeliveryEvent, LastFmDeliveryGeneration, LastFmDeliveryWorker,
    LastFmDeliveryWorkerFailure,
};

const METADATA_INGRESS_CAPACITY: usize = 64;
const CONTROL_RESERVED_CAPACITY: usize = 3;
const COMMAND_CAPACITY: usize = METADATA_INGRESS_CAPACITY + CONTROL_RESERVED_CAPACITY;

/// Monotonic identity of the account admitted by one runtime instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LastFmAccountEpoch(u64);

impl LastFmAccountEpoch {
    const INITIAL: Self = Self(1);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransitionState {
    ReauthorizationInFlight,
    DisconnectInFlight,
    PurgeRetry,
    CredentialCleanupInFlight,
    CredentialCleanupRetry,
    Disconnected,
}

#[derive(Clone, Copy)]
enum IngressPhase {
    Active {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
    },
    Transitioning {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        state: TransitionState,
    },
    Closed,
}

struct IngressGate {
    phase: IngressPhase,
    queue_admission_open: bool,
    queued_metadata: usize,
    delivery_event_queued: bool,
    reauthorization_queued: bool,
    delivery_cancellation: Option<CancellationToken>,
}

struct HandleInner {
    commands: async_channel::Sender<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    status: watch::Receiver<LastFmRuntimeStatus>,
}

/// Cloneable, nonblocking submission side of the Last.fm queue owner.
#[derive(Clone)]
pub struct LastFmRuntimeHandle {
    inner: Arc<HandleInner>,
}

impl LastFmRuntimeHandle {
    /// Bind and queue one exact validated occurrence for durable insertion.
    ///
    /// The playback side never receives or retains the vault-derived account
    /// binding. This gate attaches the binding owned by the active runtime.
    pub fn try_enqueue(
        &self,
        scrobble: UnboundLastFmScrobble,
    ) -> Result<LastFmRuntimeOperation<LastFmEnqueueOutcome>, LastFmRuntimeAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        let (account_binding, account_epoch) = match ingress.phase {
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } => (account_binding, account_epoch),
            IngressPhase::Transitioning { .. } => {
                return Err(LastFmRuntimeAdmissionError::Transitioning);
            }
            IngressPhase::Closed => return Err(LastFmRuntimeAdmissionError::Closed),
        };
        if !ingress.queue_admission_open {
            return Err(LastFmRuntimeAdmissionError::Paused);
        }
        if ingress.queued_metadata >= METADATA_INGRESS_CAPACITY {
            return Err(LastFmRuntimeAdmissionError::Busy);
        }

        let scrobble = scrobble.bind(account_binding);
        let (completion, receiver) = oneshot::channel();
        let command = Command::Enqueue {
            account_binding,
            account_epoch,
            scrobble,
            completion,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => ingress.queued_metadata += 1,
            Err(async_channel::TrySendError::Full(_)) => {
                return Err(LastFmRuntimeAdmissionError::Busy);
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                ingress.phase = IngressPhase::Closed;
                return Err(LastFmRuntimeAdmissionError::Closed);
            }
        }
        Ok(LastFmRuntimeOperation { receiver })
    }

    /// Install a renewed session for the exact retained account and restart
    /// delivery. Queue admission remains available while code 9 is waiting so
    /// already-consented offline listening can continue to be retained.
    pub fn reauthorize_same_account(
        &self,
        username: String,
        key: ProtectedString,
    ) -> Result<LastFmRuntimeOperation<()>, LastFmRuntimeAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        let (account_binding, account_epoch, previous_phase) = match ingress.phase {
            phase @ IngressPhase::Active {
                account_binding,
                account_epoch,
            } => (account_binding, account_epoch, phase),
            IngressPhase::Transitioning {
                state: TransitionState::ReauthorizationInFlight,
                ..
            } => return Err(LastFmRuntimeAdmissionError::ReauthorizationPending),
            IngressPhase::Transitioning { .. } => {
                return Err(LastFmRuntimeAdmissionError::Transitioning);
            }
            IngressPhase::Closed => return Err(LastFmRuntimeAdmissionError::Closed),
        };
        if self.inner.status.borrow().phase != LastFmRuntimePhase::ReauthenticationRequired {
            return Err(LastFmRuntimeAdmissionError::NotReadyForReauthorization);
        }
        if ingress.reauthorization_queued {
            return Err(LastFmRuntimeAdmissionError::ReauthorizationPending);
        }
        // The secret-bearing command owns the lifecycle gate before it enters
        // the FIFO. Disconnect therefore cannot overtake it while delivery
        // retirement or the blocking vault save is in progress. Shutdown may
        // still replace this transition with `Closed` and is checked again
        // before a delivery generation is installed or Active is published.
        ingress.phase = IngressPhase::Transitioning {
            account_binding,
            account_epoch,
            state: TransitionState::ReauthorizationInFlight,
        };

        let (completion, receiver) = oneshot::channel();
        let command = Command::Reauthorize {
            account_binding,
            account_epoch,
            username,
            key,
            completion,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => ingress.reauthorization_queued = true,
            Err(async_channel::TrySendError::Full(_)) => {
                ingress.phase = previous_phase;
                return Err(LastFmRuntimeAdmissionError::Busy);
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                ingress.phase = IngressPhase::Closed;
                ingress.queue_admission_open = false;
                cancel_gate_delivery(&mut ingress);
                return Err(LastFmRuntimeAdmissionError::Closed);
            }
        }
        Ok(LastFmRuntimeOperation { receiver })
    }

    /// Close enqueue admission and append (or retry) the destructive marker.
    pub fn disconnect_and_purge(
        &self,
    ) -> Result<LastFmRuntimeOperation<u64>, LastFmRuntimeAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        let (account_binding, account_epoch, previous_phase) = match ingress.phase {
            phase @ IngressPhase::Active {
                account_binding,
                account_epoch,
            } => (account_binding, account_epoch, phase),
            phase @ IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::PurgeRetry,
            } => (account_binding, account_epoch, phase),
            IngressPhase::Transitioning {
                state:
                    TransitionState::CredentialCleanupInFlight | TransitionState::CredentialCleanupRetry,
                ..
            } => return Err(LastFmRuntimeAdmissionError::CredentialCleanupRequired),
            IngressPhase::Transitioning { .. } => {
                return Err(LastFmRuntimeAdmissionError::Transitioning);
            }
            IngressPhase::Closed => return Err(LastFmRuntimeAdmissionError::Closed),
        };
        ingress.phase = IngressPhase::Transitioning {
            account_binding,
            account_epoch,
            state: TransitionState::DisconnectInFlight,
        };

        let (completion, receiver) = oneshot::channel();
        let command = Command::DisconnectAndPurge {
            account_binding,
            account_epoch,
            completion,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => {
                ingress.queue_admission_open = false;
                cancel_gate_delivery(&mut ingress);
                Ok(LastFmRuntimeOperation { receiver })
            }
            Err(async_channel::TrySendError::Full(_)) => {
                ingress.phase = previous_phase;
                Err(LastFmRuntimeAdmissionError::Busy)
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                ingress.phase = IngressPhase::Closed;
                Err(LastFmRuntimeAdmissionError::Closed)
            }
        }
    }

    /// Retry only the vault deletion after queue purge already committed.
    pub fn retry_credential_cleanup(
        &self,
    ) -> Result<LastFmRuntimeOperation<()>, LastFmRuntimeAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        let (account_binding, account_epoch, previous_phase) = match ingress.phase {
            phase @ IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::CredentialCleanupRetry,
            } => (account_binding, account_epoch, phase),
            IngressPhase::Transitioning {
                state: TransitionState::Disconnected,
                ..
            } => return Err(LastFmRuntimeAdmissionError::NotActive),
            IngressPhase::Transitioning { .. } => {
                return Err(LastFmRuntimeAdmissionError::Transitioning);
            }
            IngressPhase::Active { .. } => {
                return Err(LastFmRuntimeAdmissionError::NotReadyForCleanup);
            }
            IngressPhase::Closed => return Err(LastFmRuntimeAdmissionError::Closed),
        };
        ingress.phase = IngressPhase::Transitioning {
            account_binding,
            account_epoch,
            state: TransitionState::CredentialCleanupInFlight,
        };

        let (completion, receiver) = oneshot::channel();
        let command = Command::RetryCredentialCleanup {
            account_binding,
            account_epoch,
            completion,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => Ok(LastFmRuntimeOperation { receiver }),
            Err(async_channel::TrySendError::Full(_)) => {
                ingress.phase = previous_phase;
                Err(LastFmRuntimeAdmissionError::Busy)
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                ingress.phase = IngressPhase::Closed;
                Err(LastFmRuntimeAdmissionError::Closed)
            }
        }
    }

    /// Close all public admission and append the terminal FIFO marker.
    pub fn close_and_flush(&self) -> bool {
        request_shutdown(&self.inner)
    }

    pub fn subscribe_status(&self) -> watch::Receiver<LastFmRuntimeStatus> {
        self.inner.status.clone()
    }

    fn lock_ingress(&self) -> Result<MutexGuard<'_, IngressGate>, LastFmRuntimeAdmissionError> {
        self.inner.ingress.lock().map_err(|_| {
            self.inner.commands.close();
            LastFmRuntimeAdmissionError::Closed
        })
    }
}

impl fmt::Debug for LastFmRuntimeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let phase = self
            .inner
            .ingress
            .lock()
            .ok()
            .map(|ingress| match ingress.phase {
                IngressPhase::Active { .. } => "active",
                IngressPhase::Transitioning { .. } => "transitioning",
                IngressPhase::Closed => "closed",
            });
        formatter
            .debug_struct("LastFmRuntimeHandle")
            .field("phase", &phase.unwrap_or("unavailable"))
            .finish_non_exhaustive()
    }
}

/// Immediate reason a command did not cross the runtime admission boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmRuntimeAdmissionError {
    #[error("Last.fm runtime command ingress is busy")]
    Busy,
    #[error("Last.fm runtime is changing account state")]
    Transitioning,
    #[error("Last.fm queue admission is paused")]
    Paused,
    #[error("Last.fm reauthorization is not currently required")]
    NotReadyForReauthorization,
    #[error("Last.fm reauthorization is already pending")]
    ReauthorizationPending,
    #[error("Last.fm credential cleanup must be retried")]
    CredentialCleanupRequired,
    #[error("Last.fm credential cleanup is not ready")]
    NotReadyForCleanup,
    #[error("Last.fm account is not active")]
    NotActive,
    #[error("Last.fm runtime is closed")]
    Closed,
}

/// Content-free result of one command which crossed ingress admission.
pub struct LastFmRuntimeOperation<T> {
    receiver: oneshot::Receiver<Result<T, LastFmRuntimeCommandError>>,
}

impl<T> LastFmRuntimeOperation<T> {
    pub async fn wait(self) -> Result<T, LastFmRuntimeCommandError> {
        self.receiver
            .await
            .unwrap_or(Err(LastFmRuntimeCommandError::OwnerStopped))
    }
}

impl<T> fmt::Debug for LastFmRuntimeOperation<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmRuntimeOperation(..)")
    }
}

/// Sanitized failure from an admitted runtime command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmRuntimeCommandError {
    #[error("Last.fm queue is full")]
    QueueFull,
    #[error("Last.fm queue is unavailable")]
    Queue,
    #[error("Last.fm protected credential store is unavailable")]
    CredentialStore,
    #[error("Last.fm command no longer belongs to the active account")]
    StaleAccount,
    #[error("Last.fm authorization belongs to a different account")]
    AccountReplacementRequired,
    #[error("Last.fm runtime owner stopped")]
    OwnerStopped,
    #[error("Last.fm delivery must be reauthorized")]
    ReauthenticationRequired,
    #[error("Last.fm delivery response is incompatible")]
    Compatibility,
    #[error("Last.fm delivery capability is unavailable")]
    DeliveryCapability,
    #[error("Last.fm delivery is temporarily unavailable")]
    Delivery,
}

impl From<LastFmQueueError> for LastFmRuntimeCommandError {
    fn from(error: LastFmQueueError) -> Self {
        match error {
            LastFmQueueError::Full => Self::QueueFull,
            LastFmQueueError::AccountMismatch => Self::StaleAccount,
            LastFmQueueError::InvalidInput
            | LastFmQueueError::InvalidBatch
            | LastFmQueueError::OccurrenceConflict
            | LastFmQueueError::StaleBatch
            | LastFmQueueError::CorruptStorage
            | LastFmQueueError::Storage => Self::Queue,
        }
    }
}

/// Privacy-safe lifecycle state published by the owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmRuntimePhase {
    Active,
    BackingOff,
    Paused,
    ReauthenticationRequired,
    CompatibilityPaused,
    CapabilityPaused,
    Disconnecting,
    DisconnectRetry,
    CredentialCleanup,
    Disconnected,
    ShuttingDown,
    Stopped,
    Failed,
}

/// Latest content-free runtime snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LastFmRuntimeStatus {
    pub revision: u64,
    pub phase: LastFmRuntimePhase,
    pub pending_scrobbles: u64,
    pub accepted_scrobbles: u64,
    pub ignored_scrobbles: u64,
    pub rejected_scrobbles: u64,
    pub failure: Option<LastFmRuntimeCommandError>,
}

impl LastFmRuntimeStatus {
    fn active(pending_scrobbles: u64) -> Self {
        Self {
            revision: 0,
            phase: LastFmRuntimePhase::Active,
            pending_scrobbles,
            accepted_scrobbles: 0,
            ignored_scrobbles: 0,
            rejected_scrobbles: 0,
            failure: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TerminalOutcomeCounts {
    accepted: u64,
    ignored: u64,
    rejected: u64,
}

impl TerminalOutcomeCounts {
    fn from_result(
        result: &Result<super::client::ScrobbleBatchResult, LastFmClientError>,
        row_count: u64,
    ) -> Result<Self, LastFmRuntimeCommandError> {
        match result {
            Ok(batch) => {
                let mut counts = Self::default();
                for item in &batch.items {
                    match item {
                        SubmissionResult::Accepted { .. } => {
                            counts.accepted = counts.accepted.saturating_add(1);
                        }
                        SubmissionResult::Ignored { .. } => {
                            counts.ignored = counts.ignored.saturating_add(1);
                        }
                    }
                }
                if counts.accepted.saturating_add(counts.ignored) == row_count {
                    Ok(counts)
                } else {
                    Err(LastFmRuntimeCommandError::DeliveryCapability)
                }
            }
            Err(LastFmClientError::ServiceRejected { .. }) => Ok(Self {
                rejected: row_count,
                ..Self::default()
            }),
            Err(_) => Err(LastFmRuntimeCommandError::DeliveryCapability),
        }
    }
}

enum Command {
    Enqueue {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        scrobble: PendingLastFmScrobble,
        completion: oneshot::Sender<Result<LastFmEnqueueOutcome, LastFmRuntimeCommandError>>,
    },
    Reauthorize {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        username: String,
        key: ProtectedString,
        completion: oneshot::Sender<Result<(), LastFmRuntimeCommandError>>,
    },
    DisconnectAndPurge {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        completion: oneshot::Sender<Result<u64, LastFmRuntimeCommandError>>,
    },
    RetryCredentialCleanup {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        completion: oneshot::Sender<Result<(), LastFmRuntimeCommandError>>,
    },
    Delivery {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        event: LastFmDeliveryEvent,
    },
    Shutdown,
}

struct DeliveryRuntime {
    generation: LastFmDeliveryGeneration,
    worker: LastFmDeliveryWorker,
    relay: JoinHandle<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReauthorizationStart {
    Started,
    Closed,
    Unavailable,
}

impl DeliveryRuntime {
    async fn cancel_and_join(self) -> bool {
        let worker = self.worker.cancel_and_join().await;
        let relay = self.relay.await;
        worker.is_ok() && relay.is_ok()
    }
}

struct ActiveAccount {
    binding: LastFmAccountBinding,
    epoch: LastFmAccountEpoch,
    session: Option<StoredSession>,
    queue_purged: bool,
    last_delivery_generation: LastFmDeliveryGeneration,
    delivery: Option<DeliveryRuntime>,
    #[allow(dead_code)] // Exclusively owns this process's vault generation.
    vault_lease: Option<LastFmVaultLifecycleLease>,
}

struct RuntimeOwner {
    database: DatabaseConnection,
    credentials: Arc<dyn SessionCredentialStore>,
    command_sender: async_channel::Sender<Command>,
    commands: async_channel::Receiver<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    status_sender: watch::Sender<LastFmRuntimeStatus>,
    status: LastFmRuntimeStatus,
    account: Option<ActiveAccount>,
    transport: Arc<dyn LastFmTransport>,
    clock: Arc<dyn LastFmClock>,
}

impl RuntimeOwner {
    async fn run(mut self) -> Result<LastFmRuntimeShutdownReason, LastFmRuntimeShutdownError> {
        loop {
            let Ok(command) = self.commands.recv().await else {
                return self.fail_and_retire().await;
            };
            match command {
                Command::Enqueue {
                    account_binding,
                    account_epoch,
                    scrobble,
                    completion,
                } => {
                    if !self.metadata_received() {
                        let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
                        return self.fail_and_retire().await;
                    }
                    self.enqueue(account_binding, account_epoch, scrobble, completion)
                        .await;
                }
                Command::Reauthorize {
                    account_binding,
                    account_epoch,
                    username,
                    key,
                    completion,
                } => {
                    if !self.reauthorization_received(account_binding, account_epoch) {
                        let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
                        return self.fail_and_retire().await;
                    }
                    self.reauthorize(account_binding, account_epoch, username, key, completion)
                        .await;
                }
                Command::DisconnectAndPurge {
                    account_binding,
                    account_epoch,
                    completion,
                } => {
                    self.disconnect(account_binding, account_epoch, completion)
                        .await;
                }
                Command::RetryCredentialCleanup {
                    account_binding,
                    account_epoch,
                    completion,
                } => {
                    self.retry_cleanup(account_binding, account_epoch, completion)
                        .await;
                }
                Command::Delivery {
                    account_binding,
                    account_epoch,
                    event,
                } => {
                    if !self.delivery_received() {
                        stop_delivery_event(event);
                        return self.fail_and_retire().await;
                    }
                    self.handle_delivery(account_binding, account_epoch, event)
                        .await;
                }
                Command::Shutdown => {
                    self.publish(LastFmRuntimePhase::ShuttingDown, None);
                    let retired = self.retire_delivery().await;
                    self.account = None;
                    if retired {
                        self.publish(LastFmRuntimePhase::Stopped, None);
                        return Ok(LastFmRuntimeShutdownReason::Drained);
                    }
                    self.publish(
                        LastFmRuntimePhase::Failed,
                        Some(LastFmRuntimeCommandError::OwnerStopped),
                    );
                    return Err(LastFmRuntimeShutdownError);
                }
            }
        }
    }

    async fn fail_and_retire(
        &mut self,
    ) -> Result<LastFmRuntimeShutdownReason, LastFmRuntimeShutdownError> {
        self.publish(
            LastFmRuntimePhase::Failed,
            Some(LastFmRuntimeCommandError::OwnerStopped),
        );
        let _ = self.retire_delivery().await;
        self.account = None;
        Err(LastFmRuntimeShutdownError)
    }

    async fn enqueue(
        &mut self,
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        scrobble: PendingLastFmScrobble,
        completion: oneshot::Sender<Result<LastFmEnqueueOutcome, LastFmRuntimeCommandError>>,
    ) {
        if !self.account_matches(account_binding, account_epoch) {
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        }
        match storage::enqueue(&self.database, &scrobble).await {
            Ok(outcome) => {
                if matches!(outcome, LastFmEnqueueOutcome::Inserted { .. }) {
                    let Some(pending_scrobbles) = self.status.pending_scrobbles.checked_add(1)
                    else {
                        self.commands.close();
                        let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
                        return;
                    };
                    self.status.pending_scrobbles = pending_scrobbles;

                    let delivery_generation = self
                        .account
                        .as_ref()
                        .and_then(|account| account.delivery.as_ref())
                        .map(|delivery| delivery.generation);
                    if delivery_generation.is_some_and(|generation| {
                        self.delivery_can_continue(account_binding, account_epoch, generation)
                    }) {
                        if let Some(delivery) = self
                            .account
                            .as_ref()
                            .and_then(|account| account.delivery.as_ref())
                        {
                            let _ = delivery.worker.wake();
                        }
                    }
                    self.publish_current();
                }
                let _ = completion.send(Ok(outcome));
            }
            Err(LastFmQueueError::Full) => {
                self.publish_failure_preserving_phase(LastFmRuntimeCommandError::QueueFull);
                let _ = completion.send(Err(LastFmRuntimeCommandError::QueueFull));
            }
            Err(error) => {
                let mapped = LastFmRuntimeCommandError::from(error);
                if self.pause_delivery_ingress(account_binding, account_epoch, true) {
                    self.publish(LastFmRuntimePhase::Paused, Some(mapped));
                }
                let _ = completion.send(Err(mapped));
            }
        }
    }

    async fn reauthorize(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        username: String,
        key: ProtectedString,
        completion: oneshot::Sender<Result<(), LastFmRuntimeCommandError>>,
    ) {
        if !self.account_matches(binding, epoch)
            || self.status.phase != LastFmRuntimePhase::ReauthenticationRequired
        {
            self.finish_reauthorization_without_delivery(
                binding,
                epoch,
                true,
                LastFmRuntimePhase::ReauthenticationRequired,
                Some(LastFmRuntimeCommandError::StaleAccount),
            );
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        }
        let renewed = self
            .account
            .as_ref()
            .and_then(|account| account.session.as_ref())
            .ok_or(LastFmRuntimeCommandError::StaleAccount)
            .and_then(|session| {
                session
                    .reauthorized(username, key)
                    .map_err(|error| match error {
                        CredentialError::AccountMismatch => {
                            LastFmRuntimeCommandError::AccountReplacementRequired
                        }
                        CredentialError::Unavailable | CredentialError::InvalidData => {
                            LastFmRuntimeCommandError::CredentialStore
                        }
                    })
            });
        let renewed = match renewed {
            Ok(renewed) => renewed,
            Err(error) => {
                self.finish_reauthorization_without_delivery(
                    binding,
                    epoch,
                    true,
                    LastFmRuntimePhase::ReauthenticationRequired,
                    Some(error),
                );
                let _ = completion.send(Err(error));
                return;
            }
        };

        if !self.retire_delivery().await {
            self.finish_reauthorization_without_delivery(
                binding,
                epoch,
                false,
                LastFmRuntimePhase::CapabilityPaused,
                Some(LastFmRuntimeCommandError::DeliveryCapability),
            );
            let _ = completion.send(Err(LastFmRuntimeCommandError::DeliveryCapability));
            return;
        }
        if let Err(error) = self.save_exact_credential(binding, &renewed).await {
            self.finish_reauthorization_without_delivery(
                binding,
                epoch,
                false,
                LastFmRuntimePhase::ReauthenticationRequired,
                Some(error),
            );
            let _ = completion.send(Err(error));
            return;
        }
        match self.start_reauthorized_delivery(binding, epoch, renewed) {
            ReauthorizationStart::Started => {
                let _ = completion.send(Ok(()));
            }
            ReauthorizationStart::Closed => {
                let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
            }
            ReauthorizationStart::Unavailable => {
                self.finish_reauthorization_without_delivery(
                    binding,
                    epoch,
                    false,
                    LastFmRuntimePhase::CapabilityPaused,
                    Some(LastFmRuntimeCommandError::DeliveryCapability),
                );
                let _ = completion.send(Err(LastFmRuntimeCommandError::DeliveryCapability));
            }
        }
    }

    async fn disconnect(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        completion: oneshot::Sender<Result<u64, LastFmRuntimeCommandError>>,
    ) {
        self.publish(LastFmRuntimePhase::Disconnecting, None);
        if !self.account_matches(binding, epoch) {
            self.set_transition_from(
                binding,
                epoch,
                TransitionState::DisconnectInFlight,
                TransitionState::PurgeRetry,
            );
            self.publish(
                LastFmRuntimePhase::DisconnectRetry,
                Some(LastFmRuntimeCommandError::StaleAccount),
            );
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        }

        let delivery_retired = self.retire_delivery().await;

        let deleted = match storage::purge_account(&self.database, binding).await {
            Ok(deleted) => deleted,
            Err(error) => {
                let mapped = LastFmRuntimeCommandError::from(error);
                self.set_transition_from(
                    binding,
                    epoch,
                    TransitionState::DisconnectInFlight,
                    TransitionState::PurgeRetry,
                );
                self.publish(LastFmRuntimePhase::DisconnectRetry, Some(mapped));
                let _ = completion.send(Err(mapped));
                return;
            }
        };
        if let Some(account) = self.account.as_mut() {
            account.queue_purged = true;
            // No further request can be admitted after the destructive gate.
            // Drop and wipe the retired session before vault cleanup, including
            // the retry state reached when that cleanup fails.
            account.session = None;
        }
        self.status.pending_scrobbles = 0;
        self.set_transition_from(
            binding,
            epoch,
            TransitionState::DisconnectInFlight,
            TransitionState::CredentialCleanupInFlight,
        );
        self.publish(LastFmRuntimePhase::CredentialCleanup, None);

        match self.delete_exact_credential(binding).await {
            Ok(()) => {
                self.finish_disconnect(binding, epoch);
                let result = if delivery_retired {
                    Ok(deleted)
                } else {
                    Err(LastFmRuntimeCommandError::OwnerStopped)
                };
                let _ = completion.send(result);
            }
            Err(error) => {
                self.set_transition_from(
                    binding,
                    epoch,
                    TransitionState::CredentialCleanupInFlight,
                    TransitionState::CredentialCleanupRetry,
                );
                self.publish(LastFmRuntimePhase::CredentialCleanup, Some(error));
                let _ = completion.send(Err(error));
            }
        }
    }

    async fn retry_cleanup(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        completion: oneshot::Sender<Result<(), LastFmRuntimeCommandError>>,
    ) {
        let ready = self.account.as_ref().is_some_and(|account| {
            account.binding == binding && account.epoch == epoch && account.queue_purged
        });
        if !ready {
            self.set_transition_from(
                binding,
                epoch,
                TransitionState::CredentialCleanupInFlight,
                TransitionState::CredentialCleanupRetry,
            );
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        }
        match storage::validate_account_queue(&self.database, binding).await {
            Ok(0) => {}
            Ok(_) | Err(_) => {
                self.set_transition_from(
                    binding,
                    epoch,
                    TransitionState::CredentialCleanupInFlight,
                    TransitionState::CredentialCleanupRetry,
                );
                self.publish(
                    LastFmRuntimePhase::CredentialCleanup,
                    Some(LastFmRuntimeCommandError::Queue),
                );
                let _ = completion.send(Err(LastFmRuntimeCommandError::Queue));
                return;
            }
        }

        self.publish(LastFmRuntimePhase::CredentialCleanup, None);
        match self.delete_exact_credential(binding).await {
            Ok(()) => {
                self.finish_disconnect(binding, epoch);
                let _ = completion.send(Ok(()));
            }
            Err(error) => {
                self.set_transition_from(
                    binding,
                    epoch,
                    TransitionState::CredentialCleanupInFlight,
                    TransitionState::CredentialCleanupRetry,
                );
                self.publish(LastFmRuntimePhase::CredentialCleanup, Some(error));
                let _ = completion.send(Err(error));
            }
        }
    }

    async fn handle_delivery(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        event: LastFmDeliveryEvent,
    ) {
        match event {
            LastFmDeliveryEvent::Result(event) => {
                let (generation, receipt, result, acknowledgement) = event.into_parts();
                if !self.delivery_matches(binding, epoch, generation) {
                    let _ = acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
                    return;
                }
                match delivery_disposition(&receipt, &result) {
                    LastFmDeliveryDisposition::SettleTerminal => {
                        let Ok(row_count) = u64::try_from(receipt.len()) else {
                            self.pause_delivery(
                                binding,
                                epoch,
                                LastFmRuntimePhase::CapabilityPaused,
                                LastFmRuntimeCommandError::DeliveryCapability,
                                true,
                                acknowledgement,
                            );
                            return;
                        };
                        let Ok(outcome_counts) =
                            TerminalOutcomeCounts::from_result(&result, row_count)
                        else {
                            self.pause_delivery(
                                binding,
                                epoch,
                                LastFmRuntimePhase::CapabilityPaused,
                                LastFmRuntimeCommandError::DeliveryCapability,
                                true,
                                acknowledgement,
                            );
                            return;
                        };
                        match storage::settle_terminal(&self.database, &receipt).await {
                            Ok(()) => {
                                let Some(pending_scrobbles) =
                                    self.status.pending_scrobbles.checked_sub(row_count)
                                else {
                                    self.pause_delivery(
                                        binding,
                                        epoch,
                                        LastFmRuntimePhase::CapabilityPaused,
                                        LastFmRuntimeCommandError::DeliveryCapability,
                                        true,
                                        acknowledgement,
                                    );
                                    return;
                                };
                                self.status.pending_scrobbles = pending_scrobbles;
                                self.status.accepted_scrobbles = self
                                    .status
                                    .accepted_scrobbles
                                    .saturating_add(outcome_counts.accepted);
                                self.status.ignored_scrobbles = self
                                    .status
                                    .ignored_scrobbles
                                    .saturating_add(outcome_counts.ignored);
                                self.status.rejected_scrobbles = self
                                    .status
                                    .rejected_scrobbles
                                    .saturating_add(outcome_counts.rejected);
                                let directive =
                                    if self.delivery_can_continue(binding, epoch, generation) {
                                        self.publish(LastFmRuntimePhase::Active, None);
                                        LastFmDeliveryDirective::Continue
                                    } else {
                                        LastFmDeliveryDirective::Stop
                                    };
                                let _ = acknowledgement.acknowledge(directive);
                            }
                            Err(error) => {
                                self.pause_delivery(
                                    binding,
                                    epoch,
                                    LastFmRuntimePhase::Paused,
                                    LastFmRuntimeCommandError::from(error),
                                    true,
                                    acknowledgement,
                                );
                            }
                        }
                    }
                    LastFmDeliveryDisposition::RetryTransient => {
                        let retry_at = self
                            .clock
                            .now_unix_ms()
                            .map_err(|_| LastFmRuntimeCommandError::DeliveryCapability)
                            .and_then(|now| {
                                next_retry_at_ms(now, &receipt)
                                    .map_err(|_| LastFmRuntimeCommandError::DeliveryCapability)
                            });
                        let result = match retry_at {
                            Ok(retry_at) => {
                                storage::reschedule_batch(&self.database, &receipt, retry_at)
                                    .await
                                    .map_err(LastFmRuntimeCommandError::from)
                            }
                            Err(error) => Err(error),
                        };
                        match result {
                            Ok(()) => {
                                let directive =
                                    if self.delivery_can_continue(binding, epoch, generation) {
                                        self.publish(
                                            LastFmRuntimePhase::BackingOff,
                                            Some(LastFmRuntimeCommandError::Delivery),
                                        );
                                        LastFmDeliveryDirective::Continue
                                    } else {
                                        LastFmDeliveryDirective::Stop
                                    };
                                let _ = acknowledgement.acknowledge(directive);
                            }
                            Err(error) => self.pause_delivery(
                                binding,
                                epoch,
                                LastFmRuntimePhase::CapabilityPaused,
                                error,
                                true,
                                acknowledgement,
                            ),
                        }
                    }
                    LastFmDeliveryDisposition::PauseForReauthentication => self.pause_delivery(
                        binding,
                        epoch,
                        LastFmRuntimePhase::ReauthenticationRequired,
                        LastFmRuntimeCommandError::ReauthenticationRequired,
                        false,
                        acknowledgement,
                    ),
                    LastFmDeliveryDisposition::QuarantineCompatibility => self.pause_delivery(
                        binding,
                        epoch,
                        LastFmRuntimePhase::CompatibilityPaused,
                        LastFmRuntimeCommandError::Compatibility,
                        false,
                        acknowledgement,
                    ),
                    LastFmDeliveryDisposition::PauseCapabilityOrInternal => self.pause_delivery(
                        binding,
                        epoch,
                        LastFmRuntimePhase::CapabilityPaused,
                        LastFmRuntimeCommandError::DeliveryCapability,
                        true,
                        acknowledgement,
                    ),
                }
            }
            LastFmDeliveryEvent::Failed {
                generation,
                failure,
            } => {
                if !self.delivery_matches(binding, epoch, generation) {
                    return;
                }
                let (phase, error) = match failure {
                    LastFmDeliveryWorkerFailure::Storage(error) => (
                        LastFmRuntimePhase::Paused,
                        LastFmRuntimeCommandError::from(error),
                    ),
                    LastFmDeliveryWorkerFailure::Clock(_)
                    | LastFmDeliveryWorkerFailure::Preparation(_)
                    | LastFmDeliveryWorkerFailure::UnexpectedTaskExit => (
                        LastFmRuntimePhase::CapabilityPaused,
                        LastFmRuntimeCommandError::DeliveryCapability,
                    ),
                };
                if self.pause_delivery_ingress(binding, epoch, true) {
                    self.publish(phase, Some(error));
                }
            }
        }
    }

    fn pause_delivery(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        phase: LastFmRuntimePhase,
        error: LastFmRuntimeCommandError,
        close_queue_admission: bool,
        acknowledgement: LastFmDeliveryAcknowledgement,
    ) {
        if self.pause_delivery_ingress(binding, epoch, close_queue_admission) {
            self.publish(phase, Some(error));
        }
        let _ = acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
    }

    fn pause_delivery_ingress(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        close_queue_admission: bool,
    ) -> bool {
        let Ok(mut ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        if !matches!(
            ingress.phase,
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } if account_binding == binding && account_epoch == epoch
        ) {
            return false;
        }
        cancel_gate_delivery(&mut ingress);
        if close_queue_admission {
            ingress.queue_admission_open = false;
        }
        true
    }

    fn delivery_matches(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmDeliveryGeneration,
    ) -> bool {
        self.account_matches(binding, epoch)
            && self.account.as_ref().is_some_and(|account| {
                !account.queue_purged
                    && account
                        .delivery
                        .as_ref()
                        .is_some_and(|delivery| delivery.generation == generation)
            })
    }

    fn delivery_can_continue(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmDeliveryGeneration,
    ) -> bool {
        self.delivery_matches(binding, epoch, generation)
            && self.ingress_delivery_is_active(binding, epoch)
    }

    fn ingress_delivery_is_active(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
    ) -> bool {
        let Ok(ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        matches!(
            ingress.phase,
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } if account_binding == binding && account_epoch == epoch
        ) && ingress.delivery_cancellation.is_some()
    }

    async fn retire_delivery(&mut self) -> bool {
        let delivery = self
            .account
            .as_mut()
            .and_then(|account| account.delivery.take());
        match delivery {
            Some(delivery) => delivery.cancel_and_join().await,
            None => true,
        }
    }

    fn start_reauthorized_delivery(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        session: StoredSession,
    ) -> ReauthorizationStart {
        let Some(generation) = self
            .account
            .as_ref()
            .filter(|account| account.binding == binding && account.epoch == epoch)
            .and_then(|account| account.last_delivery_generation.checked_next())
        else {
            return ReauthorizationStart::Unavailable;
        };

        // Hold the same mutex used by shutdown from the final ownership check
        // through worker installation and status publication. If shutdown
        // already replaced the transition with Closed, no renewed worker is
        // spawned and Active cannot be published. If this block wins, the
        // complete restart linearizes before shutdown, which will immediately
        // cancel the installed generation when it claims Closed afterward.
        let ingress_owner = Arc::clone(&self.ingress);
        let Ok(mut ingress) = ingress_owner.lock() else {
            self.commands.close();
            return ReauthorizationStart::Unavailable;
        };
        match ingress.phase {
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::ReauthorizationInFlight,
            } if account_binding == binding
                && account_epoch == epoch
                && ingress.reauthorization_queued => {}
            IngressPhase::Closed => {
                ingress.reauthorization_queued = false;
                return ReauthorizationStart::Closed;
            }
            IngressPhase::Active { .. } | IngressPhase::Transitioning { .. } => {
                ingress.phase = IngressPhase::Closed;
                ingress.queue_admission_open = false;
                ingress.reauthorization_queued = false;
                cancel_gate_delivery(&mut ingress);
                self.commands.close();
                return ReauthorizationStart::Unavailable;
            }
        }
        if !self.account_matches(binding, epoch) {
            ingress.phase = IngressPhase::Closed;
            ingress.queue_admission_open = false;
            ingress.reauthorization_queued = false;
            cancel_gate_delivery(&mut ingress);
            self.commands.close();
            return ReauthorizationStart::Unavailable;
        }

        let (delivery_sender, delivery_events) = async_channel::bounded(1);
        let worker = spawn_lastfm_delivery_worker(
            self.database.clone(),
            session.clone(),
            generation,
            Arc::clone(&self.transport),
            Arc::clone(&self.clock),
            delivery_sender,
        );
        let cancellation = worker.cancellation_token();
        let relay = spawn_delivery_relay(
            delivery_events,
            self.command_sender.clone(),
            Arc::clone(&self.ingress),
            binding,
            epoch,
        );
        let delivery = DeliveryRuntime {
            generation,
            worker,
            relay,
        };
        // `account_matches` above and serialized actor ownership prove this
        // account cannot change before the following mutation.
        let account = self
            .account
            .as_mut()
            .expect("validated Last.fm account remains actor-owned");
        account.session = Some(session);
        account.last_delivery_generation = generation;
        account.delivery = Some(delivery);

        cancel_gate_delivery(&mut ingress);
        ingress.delivery_cancellation = Some(cancellation);
        ingress.queue_admission_open = true;
        ingress.reauthorization_queued = false;
        ingress.phase = IngressPhase::Active {
            account_binding: binding,
            account_epoch: epoch,
        };
        self.publish(LastFmRuntimePhase::Active, None);
        ReauthorizationStart::Started
    }

    /// Release a failed reauthorization only if it still owns the exact
    /// account transition. Publication occurs while holding the ingress lock,
    /// so shutdown cannot claim Closed immediately before an obsolete phase is
    /// exposed. Closed still consumes the retained single-flight marker but
    /// deliberately publishes nothing.
    fn finish_reauthorization_without_delivery(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        queue_admission_open: bool,
        phase: LastFmRuntimePhase,
        failure: Option<LastFmRuntimeCommandError>,
    ) -> bool {
        let ingress_owner = Arc::clone(&self.ingress);
        let Ok(mut ingress) = ingress_owner.lock() else {
            self.commands.close();
            return false;
        };
        match ingress.phase {
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::ReauthorizationInFlight,
            } if account_binding == binding
                && account_epoch == epoch
                && ingress.reauthorization_queued =>
            {
                ingress.reauthorization_queued = false;
                ingress.queue_admission_open = queue_admission_open;
                if !queue_admission_open {
                    cancel_gate_delivery(&mut ingress);
                }
                ingress.phase = IngressPhase::Active {
                    account_binding: binding,
                    account_epoch: epoch,
                };
                self.publish(phase, failure);
                true
            }
            IngressPhase::Closed => {
                ingress.reauthorization_queued = false;
                false
            }
            IngressPhase::Active { .. } | IngressPhase::Transitioning { .. } => {
                ingress.phase = IngressPhase::Closed;
                ingress.queue_admission_open = false;
                ingress.reauthorization_queued = false;
                cancel_gate_delivery(&mut ingress);
                self.commands.close();
                false
            }
        }
    }

    fn finish_disconnect(&mut self, binding: LastFmAccountBinding, epoch: LastFmAccountEpoch) {
        self.account = None;
        self.status.pending_scrobbles = 0;
        self.set_transition_from(
            binding,
            epoch,
            TransitionState::CredentialCleanupInFlight,
            TransitionState::Disconnected,
        );
        self.publish(LastFmRuntimePhase::Disconnected, None);
    }

    async fn save_exact_credential(
        &mut self,
        binding: LastFmAccountBinding,
        renewed: &StoredSession,
    ) -> Result<(), LastFmRuntimeCommandError> {
        let lease = self.take_vault_lease(binding)?;
        let credentials = Arc::clone(&self.credentials);
        let renewed = renewed.clone();
        let operation = tokio::task::spawn_blocking(move || {
            let result = catch_unwind(AssertUnwindSafe(|| match credentials.load() {
                Ok(Some(stored)) if stored.account_binding() == binding => credentials
                    .save(&renewed)
                    .map_err(|_| LastFmRuntimeCommandError::CredentialStore),
                Ok(Some(_)) => Err(LastFmRuntimeCommandError::StaleAccount),
                Ok(None) | Err(_) => Err(LastFmRuntimeCommandError::CredentialStore),
            }))
            .unwrap_or(Err(LastFmRuntimeCommandError::CredentialStore));
            (lease, result)
        })
        .await;
        let Ok((lease, result)) = operation else {
            self.close_after_vault_owner_failure();
            return Err(LastFmRuntimeCommandError::CredentialStore);
        };
        self.restore_vault_lease(binding, lease)?;
        result
    }

    async fn delete_exact_credential(
        &mut self,
        binding: LastFmAccountBinding,
    ) -> Result<(), LastFmRuntimeCommandError> {
        let lease = self.take_vault_lease(binding)?;
        let credentials = Arc::clone(&self.credentials);
        let operation = tokio::task::spawn_blocking(move || {
            let result = catch_unwind(AssertUnwindSafe(|| match credentials.load() {
                Ok(Some(stored)) if stored.account_binding() == binding => credentials
                    .delete()
                    .map_err(|_| LastFmRuntimeCommandError::CredentialStore),
                Ok(Some(_)) => Err(LastFmRuntimeCommandError::StaleAccount),
                Ok(None) => Ok(()),
                Err(_) => Err(LastFmRuntimeCommandError::CredentialStore),
            }))
            .unwrap_or(Err(LastFmRuntimeCommandError::CredentialStore));
            (lease, result)
        })
        .await;
        let Ok((lease, result)) = operation else {
            self.close_after_vault_owner_failure();
            return Err(LastFmRuntimeCommandError::CredentialStore);
        };
        self.restore_vault_lease(binding, lease)?;
        result
    }

    fn take_vault_lease(
        &mut self,
        binding: LastFmAccountBinding,
    ) -> Result<LastFmVaultLifecycleLease, LastFmRuntimeCommandError> {
        self.account
            .as_mut()
            .filter(|account| account.binding == binding)
            .and_then(|account| account.vault_lease.take())
            .ok_or(LastFmRuntimeCommandError::StaleAccount)
    }

    fn restore_vault_lease(
        &mut self,
        binding: LastFmAccountBinding,
        lease: LastFmVaultLifecycleLease,
    ) -> Result<(), LastFmRuntimeCommandError> {
        let Some(account) = self
            .account
            .as_mut()
            .filter(|account| account.binding == binding && account.vault_lease.is_none())
        else {
            return Err(LastFmRuntimeCommandError::StaleAccount);
        };
        account.vault_lease = Some(lease);
        Ok(())
    }

    fn close_after_vault_owner_failure(&self) {
        if let Ok(mut ingress) = self.ingress.lock() {
            ingress.phase = IngressPhase::Closed;
            ingress.queue_admission_open = false;
            cancel_gate_delivery(&mut ingress);
        }
        self.commands.close();
    }

    fn account_matches(&self, binding: LastFmAccountBinding, epoch: LastFmAccountEpoch) -> bool {
        self.account.as_ref().is_some_and(|account| {
            account.binding == binding
                && account.epoch == epoch
                && account
                    .session
                    .as_ref()
                    .is_some_and(|session| session.account_binding() == binding)
        })
    }

    fn metadata_received(&self) -> bool {
        let Ok(mut ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        let Some(remaining) = ingress.queued_metadata.checked_sub(1) else {
            self.commands.close();
            return false;
        };
        ingress.queued_metadata = remaining;
        true
    }

    fn reauthorization_received(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
    ) -> bool {
        let Ok(ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        if !ingress.reauthorization_queued {
            self.commands.close();
            return false;
        }
        match ingress.phase {
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::ReauthorizationInFlight,
            } => account_binding == binding && account_epoch == epoch,
            // Shutdown may close after admission but before actor receipt. The
            // earlier admitted vault operation still drains; its completion
            // path is forbidden from restarting delivery or publishing Active.
            IngressPhase::Closed => true,
            IngressPhase::Active { .. } | IngressPhase::Transitioning { .. } => {
                self.commands.close();
                false
            }
        }
    }

    fn delivery_received(&self) -> bool {
        let Ok(mut ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        if !ingress.delivery_event_queued {
            self.commands.close();
            return false;
        }
        ingress.delivery_event_queued = false;
        true
    }

    fn set_transition_from(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        expected: TransitionState,
        next: TransitionState,
    ) -> bool {
        let Ok(mut ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        match ingress.phase {
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state,
            } if account_binding == binding && account_epoch == epoch && state == expected => {}
            IngressPhase::Closed => return false,
            IngressPhase::Active { .. } | IngressPhase::Transitioning { .. } => {
                self.commands.close();
                return false;
            }
        }
        cancel_gate_delivery(&mut ingress);
        ingress.phase = IngressPhase::Transitioning {
            account_binding: binding,
            account_epoch: epoch,
            state: next,
        };
        true
    }

    fn publish(&mut self, phase: LastFmRuntimePhase, failure: Option<LastFmRuntimeCommandError>) {
        self.status.revision = self.status.revision.saturating_add(1);
        self.status.phase = phase;
        self.status.failure = failure;
        self.status_sender.send_replace(self.status);
    }

    fn publish_current(&mut self) {
        self.status.revision = self.status.revision.saturating_add(1);
        self.status_sender.send_replace(self.status);
    }

    fn publish_failure_preserving_phase(&mut self, failure: LastFmRuntimeCommandError) {
        self.status.failure = Some(failure);
        self.publish_current();
    }
}

fn spawn_delivery_relay(
    events: async_channel::Receiver<LastFmDeliveryEvent>,
    commands: async_channel::Sender<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    account_binding: LastFmAccountBinding,
    account_epoch: LastFmAccountEpoch,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            let Ok(mut gate) = ingress.lock() else {
                commands.close();
                stop_delivery_event(event);
                return;
            };
            let active = matches!(
                gate.phase,
                IngressPhase::Active {
                    account_binding: binding,
                    account_epoch: epoch,
                } if binding == account_binding && epoch == account_epoch
            );
            if !active {
                drop(gate);
                stop_delivery_event(event);
                continue;
            }
            if gate.delivery_event_queued {
                commands.close();
                drop(gate);
                stop_delivery_event(event);
                return;
            }
            let command = Command::Delivery {
                account_binding,
                account_epoch,
                event,
            };
            match commands.try_send(command) {
                Ok(()) => gate.delivery_event_queued = true,
                Err(async_channel::TrySendError::Full(command)) => {
                    commands.close();
                    drop(gate);
                    stop_delivery_command(command);
                    return;
                }
                Err(async_channel::TrySendError::Closed(command)) => {
                    drop(gate);
                    stop_delivery_command(command);
                    return;
                }
            }
        }
    })
}

fn stop_delivery_command(command: Command) {
    if let Command::Delivery { event, .. } = command {
        stop_delivery_event(event);
    }
}

fn stop_delivery_event(event: LastFmDeliveryEvent) {
    if let LastFmDeliveryEvent::Result(event) = event {
        let (_, _, _, acknowledgement) = event.into_parts();
        let _ = acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
    }
}

/// Start failure before an active handle can escape.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmRuntimeStartError {
    #[error("Last.fm protected credential store is unavailable")]
    CredentialStore,
    #[error("Last.fm protected credential store has no matching account")]
    CredentialMismatch,
    #[error("Last.fm queue belongs to another account")]
    AccountMismatch,
    #[error("Last.fm queue storage is not canonical")]
    CorruptQueue,
    #[error("Last.fm queue storage is unavailable")]
    Storage,
}

impl From<LastFmQueueError> for LastFmRuntimeStartError {
    fn from(error: LastFmQueueError) -> Self {
        match error {
            LastFmQueueError::AccountMismatch => Self::AccountMismatch,
            LastFmQueueError::Storage => Self::Storage,
            LastFmQueueError::InvalidInput
            | LastFmQueueError::InvalidBatch
            | LastFmQueueError::Full
            | LastFmQueueError::OccurrenceConflict
            | LastFmQueueError::StaleBatch
            | LastFmQueueError::CorruptStorage => Self::CorruptQueue,
        }
    }
}

/// Why the actor completed its explicit FIFO drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmRuntimeShutdownReason {
    Drained,
}

/// Sanitized owner/barrier failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Last.fm runtime did not complete its FIFO drain")]
pub struct LastFmRuntimeShutdownError;

/// Typed persistent state of the shutdown proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmRuntimeDrainState {
    Pending,
    Drained,
    Failed,
}

/// Sole join side for the Last.fm runtime owner.
pub struct LastFmRuntimeShutdown {
    inner: Arc<HandleInner>,
    owner: Option<JoinHandle<Result<LastFmRuntimeShutdownReason, LastFmRuntimeShutdownError>>>,
    completion: watch::Receiver<LastFmRuntimeDrainState>,
}

impl LastFmRuntimeShutdown {
    pub fn barrier(&self) -> LastFmRuntimeBarrier {
        LastFmRuntimeBarrier {
            completion: self.completion.clone(),
        }
    }

    pub async fn shutdown(
        mut self,
    ) -> Result<LastFmRuntimeShutdownReason, LastFmRuntimeShutdownError> {
        request_shutdown(&self.inner);
        let owner = self.owner.take().ok_or(LastFmRuntimeShutdownError)?;
        match owner.await {
            Ok(Ok(reason)) => Ok(reason),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(LastFmRuntimeShutdownError),
        }
    }
}

impl Drop for LastFmRuntimeShutdown {
    fn drop(&mut self) {
        request_shutdown(&self.inner);
    }
}

impl fmt::Debug for LastFmRuntimeShutdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmRuntimeShutdown")
            .field("drain_state", &*self.completion.borrow())
            .finish_non_exhaustive()
    }
}

/// Persistent actor-exit proof. Sender loss while pending is failure.
#[derive(Clone)]
pub struct LastFmRuntimeBarrier {
    completion: watch::Receiver<LastFmRuntimeDrainState>,
}

impl LastFmRuntimeBarrier {
    pub fn state(&self) -> LastFmRuntimeDrainState {
        *self.completion.borrow()
    }

    pub fn is_complete(&self) -> bool {
        self.state() != LastFmRuntimeDrainState::Pending || self.completion.has_changed().is_err()
    }

    pub async fn wait(&self) -> Result<(), LastFmRuntimeShutdownError> {
        let mut completion = self.completion.clone();
        loop {
            let state = *completion.borrow_and_update();
            match state {
                LastFmRuntimeDrainState::Drained => return Ok(()),
                LastFmRuntimeDrainState::Failed => return Err(LastFmRuntimeShutdownError),
                LastFmRuntimeDrainState::Pending => {}
            }
            if completion.changed().await.is_err() {
                return Err(LastFmRuntimeShutdownError);
            }
        }
    }
}

impl fmt::Debug for LastFmRuntimeBarrier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmRuntimeBarrier")
            .field("state", &self.state())
            .finish()
    }
}

struct CompletionGuard {
    sender: watch::Sender<LastFmRuntimeDrainState>,
    drained: bool,
}

impl CompletionGuard {
    fn mark_drained(&mut self) {
        self.sender.send_replace(LastFmRuntimeDrainState::Drained);
        self.drained = true;
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.drained {
            self.sender.send_replace(LastFmRuntimeDrainState::Failed);
        }
    }
}

/// Opaque proof that build capability, saved enablement, and explicit user
/// consent were validated before retained listening history can be delivered.
///
/// Construction stays inside the Last.fm integration. Future settings/auth
/// wiring must issue it only after enforcing the accepted privacy contract;
/// unrelated application code cannot start the delivery runtime from the mere
/// presence of a vault record.
pub struct LastFmRuntimeActivation {
    _private: (),
}

impl LastFmRuntimeActivation {
    #[must_use]
    pub(in crate::lastfm) const fn issue_after_consent_and_enablement() -> Self {
        Self { _private: () }
    }
}

impl fmt::Debug for LastFmRuntimeActivation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmRuntimeActivation")
    }
}

/// Load the exact vault authority and validate every retained row before
/// exposing an active handle.
pub async fn spawn_lastfm_runtime(
    _activation: LastFmRuntimeActivation,
    database: DatabaseConnection,
    credentials: Arc<dyn SessionCredentialStore>,
    transport: Arc<dyn LastFmTransport>,
    clock: Arc<dyn LastFmClock>,
) -> Result<(LastFmRuntimeHandle, LastFmRuntimeShutdown), LastFmRuntimeStartError> {
    let vault_lease = acquire_vault_lifecycle().await;
    let credentials_for_load = Arc::clone(&credentials);
    // Move the lease into the blocking operation. If startup itself is
    // cancelled, the detached blocking load still retains this vault
    // generation until it has stopped touching the process-global record.
    let (vault_lease, stored_session) =
        tokio::task::spawn_blocking(move || (vault_lease, credentials_for_load.load()))
            .await
            .map_err(|_| LastFmRuntimeStartError::CredentialStore)?;
    let stored_session = stored_session
        .map_err(|_| LastFmRuntimeStartError::CredentialStore)?
        .ok_or(LastFmRuntimeStartError::CredentialMismatch)?;
    let binding = stored_session.account_binding();
    let pending_scrobbles = storage::validate_account_queue(&database, binding)
        .await
        .map_err(LastFmRuntimeStartError::from)?;
    let epoch = LastFmAccountEpoch::INITIAL;
    let (command_sender, command_receiver) = async_channel::bounded(COMMAND_CAPACITY);
    let initial_status = LastFmRuntimeStatus::active(pending_scrobbles);
    let (status_sender, status) = watch::channel(initial_status);
    let (delivery_sender, delivery_events) = async_channel::bounded(1);
    let generation = LastFmDeliveryGeneration::new(1);
    let worker = spawn_lastfm_delivery_worker(
        database.clone(),
        stored_session.clone(),
        generation,
        Arc::clone(&transport),
        Arc::clone(&clock),
        delivery_sender,
    );
    let delivery_cancellation = worker.cancellation_token();
    let ingress = Arc::new(Mutex::new(IngressGate {
        phase: IngressPhase::Active {
            account_binding: binding,
            account_epoch: epoch,
        },
        queue_admission_open: true,
        queued_metadata: 0,
        delivery_event_queued: false,
        reauthorization_queued: false,
        delivery_cancellation: Some(delivery_cancellation),
    }));
    let inner = Arc::new(HandleInner {
        commands: command_sender.clone(),
        ingress: Arc::clone(&ingress),
        status,
    });
    let relay = spawn_delivery_relay(
        delivery_events,
        command_sender.clone(),
        Arc::clone(&ingress),
        binding,
        epoch,
    );
    let owner = RuntimeOwner {
        database,
        credentials,
        command_sender: command_sender.clone(),
        commands: command_receiver,
        ingress,
        status_sender,
        status: initial_status,
        transport,
        clock,
        account: Some(ActiveAccount {
            binding,
            epoch,
            session: Some(stored_session),
            queue_purged: false,
            last_delivery_generation: generation,
            delivery: Some(DeliveryRuntime {
                generation,
                worker,
                relay,
            }),
            vault_lease: Some(vault_lease),
        }),
    };
    let (completion_sender, completion) = watch::channel(LastFmRuntimeDrainState::Pending);
    let owner_task = tokio::spawn(async move {
        let mut completion = CompletionGuard {
            sender: completion_sender,
            drained: false,
        };
        let result = owner.run().await;
        if result.is_ok() {
            completion.mark_drained();
        }
        result
    });
    Ok((
        LastFmRuntimeHandle {
            inner: Arc::clone(&inner),
        },
        LastFmRuntimeShutdown {
            inner,
            owner: Some(owner_task),
            completion,
        },
    ))
}

fn request_shutdown(inner: &Arc<HandleInner>) -> bool {
    let Ok(mut ingress) = inner.ingress.lock() else {
        inner.commands.close();
        return false;
    };
    if matches!(ingress.phase, IngressPhase::Closed) {
        return false;
    }
    ingress.phase = IngressPhase::Closed;
    ingress.queue_admission_open = false;
    match inner.commands.try_send(Command::Shutdown) {
        Ok(()) => {
            cancel_gate_delivery(&mut ingress);
            true
        }
        Err(_) => {
            cancel_gate_delivery(&mut ingress);
            inner.commands.close();
            false
        }
    }
}

fn cancel_gate_delivery(ingress: &mut IngressGate) {
    if let Some(cancellation) = ingress.delivery_cancellation.take() {
        cancellation.cancel();
    }
}

#[cfg(test)]
#[path = "runtime_delivery_tests.rs"]
mod runtime_delivery_tests;

#[cfg(test)]
#[path = "runtime_reauthorization_tests.rs"]
mod runtime_reauthorization_tests;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier as ThreadBarrier, Mutex};

    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;
    use uuid::Uuid;

    use super::*;
    use crate::db::migration::Migrator;
    use crate::lastfm::client::{
        LastFmClientError, LastFmTrack, Scrobble, ScrobbleBatchResult, SubmissionResult,
    };
    use crate::lastfm::credentials::{CredentialError, ProtectedString};
    use crate::lastfm::delivery::LastFmDeliveryPrimitiveError;
    use crate::lastfm::worker::LastFmDeliveryWorkerExit;

    const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";

    struct PendingTransport;

    struct AcceptedTransport;

    #[async_trait::async_trait]
    impl LastFmTransport for PendingTransport {
        async fn update_now_playing(
            &self,
            _session: &StoredSession,
            _track: &LastFmTrack,
        ) -> Result<SubmissionResult, LastFmClientError> {
            std::future::pending().await
        }

        async fn submit_scrobbles(
            &self,
            _session: &StoredSession,
            _scrobbles: &[Scrobble],
        ) -> Result<ScrobbleBatchResult, LastFmClientError> {
            std::future::pending().await
        }
    }

    #[async_trait::async_trait]
    impl LastFmTransport for AcceptedTransport {
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
            Ok(ScrobbleBatchResult {
                items: vec![SubmissionResult::Accepted { corrected: false }; scrobbles.len()],
            })
        }
    }

    struct FixedClock;

    #[async_trait::async_trait]
    impl LastFmClock for FixedClock {
        fn now_unix_ms(&self) -> Result<i64, LastFmDeliveryPrimitiveError> {
            Ok(0)
        }

        async fn wait_until_unix_ms(
            &self,
            _deadline_unix_ms: i64,
        ) -> Result<(), LastFmDeliveryPrimitiveError> {
            std::future::pending().await
        }
    }

    fn pending_transport() -> Arc<dyn LastFmTransport> {
        Arc::new(PendingTransport)
    }

    fn fixed_clock() -> Arc<dyn LastFmClock> {
        Arc::new(FixedClock)
    }

    struct TestCredentialStore {
        session: Mutex<Option<StoredSession>>,
        delete_failures: AtomicUsize,
        delete_attempts: AtomicUsize,
    }

    impl TestCredentialStore {
        fn new(session: StoredSession) -> Self {
            Self {
                session: Mutex::new(Some(session)),
                delete_failures: AtomicUsize::new(0),
                delete_attempts: AtomicUsize::new(0),
            }
        }

        fn fail_next_deletes(&self, count: usize) {
            self.delete_failures.store(count, Ordering::SeqCst);
        }

        fn delete_attempts(&self) -> usize {
            self.delete_attempts.load(Ordering::SeqCst)
        }
    }

    impl SessionCredentialStore for TestCredentialStore {
        fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
            self.session
                .lock()
                .map(|session| session.clone())
                .map_err(|_| CredentialError::Unavailable)
        }

        fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
            let mut stored = self
                .session
                .lock()
                .map_err(|_| CredentialError::Unavailable)?;
            *stored = Some(session.clone());
            Ok(())
        }

        fn delete(&self) -> Result<(), CredentialError> {
            self.delete_attempts.fetch_add(1, Ordering::SeqCst);
            if self
                .delete_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(CredentialError::Unavailable);
            }
            let mut session = self
                .session
                .lock()
                .map_err(|_| CredentialError::Unavailable)?;
            *session = None;
            Ok(())
        }
    }

    async fn database() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        database
    }

    fn session(username: &str) -> StoredSession {
        StoredSession::new(username, ProtectedString::new(SESSION_KEY)).unwrap()
    }

    fn scrobble(binding: LastFmAccountBinding, title: impl ToString) -> PendingLastFmScrobble {
        PendingLastFmScrobble::try_new(
            Uuid::new_v4(),
            binding,
            "Artist".to_owned(),
            title.to_string(),
            Some("Album".to_owned()),
            None,
            Some(1),
            60,
            1_700_000_000,
        )
        .unwrap()
    }

    fn unbound_scrobble(title: impl ToString) -> UnboundLastFmScrobble {
        UnboundLastFmScrobble::try_new(
            Uuid::new_v4(),
            "Artist".to_owned(),
            title.to_string(),
            Some("Album".to_owned()),
            None,
            Some(1),
            60,
            1_700_000_000,
        )
        .unwrap()
    }

    async fn runtime(
        database: DatabaseConnection,
        _initial_session: StoredSession,
        store: Arc<TestCredentialStore>,
    ) -> (LastFmRuntimeHandle, LastFmRuntimeShutdown) {
        spawn_lastfm_runtime(
            LastFmRuntimeActivation::issue_after_consent_and_enablement(),
            database,
            store,
            pending_transport(),
            fixed_clock(),
        )
        .await
        .unwrap()
    }

    #[test]
    fn lifecycle_gate_cancels_delivery_before_reserved_markers_cross_a_full_metadata_backlog() {
        let stored_session = session("listener");
        let binding = stored_session.account_binding();
        let epoch = LastFmAccountEpoch::INITIAL;
        let cancellation = CancellationToken::new();
        let ingress = Arc::new(Mutex::new(IngressGate {
            phase: IngressPhase::Active {
                account_binding: binding,
                account_epoch: epoch,
            },
            queue_admission_open: true,
            queued_metadata: 0,
            delivery_event_queued: false,
            reauthorization_queued: false,
            delivery_cancellation: Some(cancellation.clone()),
        }));
        let (command_sender, commands) = async_channel::bounded(COMMAND_CAPACITY);
        let (_status_sender, status) = watch::channel(LastFmRuntimeStatus::active(0));
        let handle = LastFmRuntimeHandle {
            inner: Arc::new(HandleInner {
                commands: command_sender,
                ingress,
                status,
            }),
        };

        let mut metadata_operations = Vec::new();
        for index in 0..METADATA_INGRESS_CAPACITY {
            metadata_operations.push(
                handle
                    .try_enqueue(unbound_scrobble(format!("Backlog {index}")))
                    .unwrap(),
            );
        }
        {
            let mut gate = handle.inner.ingress.lock().unwrap();
            assert!(handle
                .inner
                .commands
                .try_send(Command::Delivery {
                    account_binding: binding,
                    account_epoch: epoch,
                    event: LastFmDeliveryEvent::Failed {
                        generation: LastFmDeliveryGeneration::new(1),
                        failure: LastFmDeliveryWorkerFailure::UnexpectedTaskExit,
                    },
                })
                .is_ok());
            gate.delivery_event_queued = true;
        }
        let disconnect = handle.disconnect_and_purge().unwrap();
        assert!(cancellation.is_cancelled());
        assert!(handle.close_and_flush());
        assert!(!handle.close_and_flush());

        for _ in 0..METADATA_INGRESS_CAPACITY {
            assert!(matches!(
                commands.try_recv().unwrap(),
                Command::Enqueue { .. }
            ));
        }
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::Delivery { .. }
        ));
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::DisconnectAndPurge { .. }
        ));
        assert!(matches!(commands.try_recv().unwrap(), Command::Shutdown));
        assert!(commands.try_recv().is_err());

        drop(metadata_operations);
        drop(disconnect);
    }

    #[tokio::test]
    async fn fatal_enqueue_cannot_overwrite_a_disconnect_that_already_owns_the_gate() {
        let database = database().await;
        database
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                "DROP TABLE lastfm_scrobble_queue".to_owned(),
            ))
            .await
            .unwrap();
        let stored_session = session("listener");
        let binding = stored_session.account_binding();
        let epoch = LastFmAccountEpoch::INITIAL;
        let store = Arc::new(TestCredentialStore::new(stored_session.clone()));
        let vault_lease = acquire_vault_lifecycle().await;
        let ingress = Arc::new(Mutex::new(IngressGate {
            phase: IngressPhase::Transitioning {
                account_binding: binding,
                account_epoch: epoch,
                state: TransitionState::DisconnectInFlight,
            },
            queue_admission_open: false,
            queued_metadata: 0,
            delivery_event_queued: false,
            reauthorization_queued: false,
            delivery_cancellation: None,
        }));
        let (command_sender, commands) = async_channel::bounded(COMMAND_CAPACITY);
        let initial_status = LastFmRuntimeStatus::active(0);
        let (status_sender, status) = watch::channel(initial_status);
        let mut owner = RuntimeOwner {
            database,
            credentials: store,
            command_sender,
            commands,
            ingress: Arc::clone(&ingress),
            status_sender,
            status: initial_status,
            transport: pending_transport(),
            account: Some(ActiveAccount {
                binding,
                epoch,
                session: Some(stored_session),
                queue_purged: false,
                last_delivery_generation: LastFmDeliveryGeneration::new(1),
                delivery: None,
                vault_lease: Some(vault_lease),
            }),
            clock: fixed_clock(),
        };
        let (completion, completed) = oneshot::channel();

        owner
            .enqueue(
                binding,
                epoch,
                scrobble(binding, "Storage failure"),
                completion,
            )
            .await;

        assert_eq!(
            completed.await.unwrap(),
            Err(LastFmRuntimeCommandError::Queue)
        );
        assert!(matches!(
            ingress.lock().unwrap().phase,
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::DisconnectInFlight,
            } if account_binding == binding && account_epoch == epoch
        ));
        assert_eq!(status.borrow().phase, LastFmRuntimePhase::Active);
        assert_eq!(status.borrow().failure, None);
    }

    #[tokio::test]
    async fn stale_delivery_generation_cannot_pause_or_mutate_the_active_account() {
        let database = database().await;
        let stored_session = session("listener");
        let binding = stored_session.account_binding();
        storage::enqueue(&database, &scrobble(binding, "Retained"))
            .await
            .unwrap();
        let epoch = LastFmAccountEpoch::INITIAL;
        let generation = LastFmDeliveryGeneration::new(7);
        let (delivery_events, _event_receiver) = async_channel::bounded(1);
        let worker = spawn_lastfm_delivery_worker(
            database.clone(),
            stored_session.clone(),
            generation,
            pending_transport(),
            fixed_clock(),
            delivery_events,
        );
        let cancellation = worker.cancellation_token();
        let relay = tokio::spawn(async {});
        let ingress = Arc::new(Mutex::new(IngressGate {
            phase: IngressPhase::Active {
                account_binding: binding,
                account_epoch: epoch,
            },
            queue_admission_open: true,
            queued_metadata: 0,
            delivery_event_queued: false,
            reauthorization_queued: false,
            delivery_cancellation: Some(cancellation),
        }));
        let (command_sender, commands) = async_channel::bounded(COMMAND_CAPACITY);
        let initial_status = LastFmRuntimeStatus::active(1);
        let (status_sender, status) = watch::channel(initial_status);
        let store = Arc::new(TestCredentialStore::new(stored_session.clone()));
        let vault_lease = acquire_vault_lifecycle().await;
        let mut owner = RuntimeOwner {
            database: database.clone(),
            credentials: store,
            command_sender,
            commands,
            ingress: Arc::clone(&ingress),
            status_sender,
            status: initial_status,
            transport: pending_transport(),
            account: Some(ActiveAccount {
                binding,
                epoch,
                session: Some(stored_session),
                queue_purged: false,
                last_delivery_generation: generation,
                delivery: Some(DeliveryRuntime {
                    generation,
                    worker,
                    relay,
                }),
                vault_lease: Some(vault_lease),
            }),
            clock: fixed_clock(),
        };

        owner
            .handle_delivery(
                binding,
                epoch,
                LastFmDeliveryEvent::Failed {
                    generation: LastFmDeliveryGeneration::new(8),
                    failure: LastFmDeliveryWorkerFailure::Clock(
                        LastFmDeliveryPrimitiveError::ClockOutOfRange,
                    ),
                },
            )
            .await;

        assert_eq!(storage::queue_len(&database).await.unwrap(), 1);
        assert_eq!(*status.borrow(), initial_status);
        assert!(matches!(
            ingress.lock().unwrap().phase,
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } if account_binding == binding && account_epoch == epoch
        ));
        assert!(owner.retire_delivery().await);
    }

    #[tokio::test]
    async fn stale_accepted_result_cannot_delete_or_change_the_active_account_status() {
        let database = database().await;
        let stored_session = session("listener");
        let binding = stored_session.account_binding();
        storage::enqueue(&database, &scrobble(binding, "Retained"))
            .await
            .unwrap();
        let storage::LastFmBatchAvailability::Ready(receipt) =
            storage::batch_availability(&database, binding, 0, 50)
                .await
                .unwrap()
        else {
            panic!("expected one ready private queue row");
        };
        let before = receipt.rows()[0].clone();
        drop(receipt);
        let epoch = LastFmAccountEpoch::INITIAL;
        let generation = LastFmDeliveryGeneration::new(7);
        let stale_generation = LastFmDeliveryGeneration::new(8);
        let (stale_events, stale_event_receiver) = async_channel::bounded(1);
        let stale_worker = spawn_lastfm_delivery_worker(
            database.clone(),
            stored_session.clone(),
            stale_generation,
            Arc::new(AcceptedTransport),
            fixed_clock(),
            stale_events,
        );
        let stale_event = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stale_event_receiver.recv(),
        )
        .await
        .expect("stale worker delivered its accepted result before the watchdog")
        .expect("stale worker event channel remained active");
        let (delivery_events, _event_receiver) = async_channel::bounded(1);
        let worker = spawn_lastfm_delivery_worker(
            database.clone(),
            stored_session.clone(),
            generation,
            pending_transport(),
            fixed_clock(),
            delivery_events,
        );
        let cancellation = worker.cancellation_token();
        let relay = tokio::spawn(async {});
        let ingress = Arc::new(Mutex::new(IngressGate {
            phase: IngressPhase::Active {
                account_binding: binding,
                account_epoch: epoch,
            },
            queue_admission_open: true,
            queued_metadata: 0,
            delivery_event_queued: false,
            reauthorization_queued: false,
            delivery_cancellation: Some(cancellation),
        }));
        let (command_sender, commands) = async_channel::bounded(COMMAND_CAPACITY);
        let initial_status = LastFmRuntimeStatus::active(1);
        let (status_sender, status) = watch::channel(initial_status);
        let store = Arc::new(TestCredentialStore::new(stored_session.clone()));
        let vault_lease = acquire_vault_lifecycle().await;
        let mut owner = RuntimeOwner {
            database: database.clone(),
            credentials: store,
            command_sender,
            commands,
            ingress: Arc::clone(&ingress),
            status_sender,
            status: initial_status,
            transport: pending_transport(),
            account: Some(ActiveAccount {
                binding,
                epoch,
                session: Some(stored_session),
                queue_purged: false,
                last_delivery_generation: generation,
                delivery: Some(DeliveryRuntime {
                    generation,
                    worker,
                    relay,
                }),
                vault_lease: Some(vault_lease),
            }),
            clock: fixed_clock(),
        };

        owner.handle_delivery(binding, epoch, stale_event).await;

        assert_eq!(
            stale_worker.join().await.unwrap(),
            LastFmDeliveryWorkerExit::DirectedStop
        );
        assert_eq!(*status.borrow(), initial_status);
        let storage::LastFmBatchAvailability::Ready(retained) =
            storage::batch_availability(&database, binding, 0, 50)
                .await
                .unwrap()
        else {
            panic!("stale result must retain the ready private queue row");
        };
        assert_eq!(retained.rows(), std::slice::from_ref(&before));
        assert!(matches!(
            ingress.lock().unwrap().phase,
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } if account_binding == binding && account_epoch == epoch
        ));
        assert!(owner.retire_delivery().await);
    }

    #[tokio::test]
    async fn enqueue_before_disconnect_commits_then_purges_before_vault_delete() {
        let database = database().await;
        let initial_session = session("listener");
        let _binding = initial_session.account_binding();
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (handle, shutdown) = runtime(database.clone(), initial_session, store.clone()).await;

        let inserted = handle.try_enqueue(unbound_scrobble("First")).unwrap();
        let disconnected = handle.disconnect_and_purge().unwrap();

        assert!(matches!(
            inserted.wait().await.unwrap(),
            LastFmEnqueueOutcome::Inserted { .. }
        ));
        assert_eq!(disconnected.wait().await.unwrap(), 1);
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        assert_eq!(store.delete_attempts(), 1);
        assert_eq!(
            shutdown.shutdown().await.unwrap(),
            LastFmRuntimeShutdownReason::Drained
        );
    }

    #[tokio::test]
    async fn disconnect_before_enqueue_rejects_without_crossing_fifo() {
        let database = database().await;
        let initial_session = session("listener");
        let _binding = initial_session.account_binding();
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (handle, shutdown) = runtime(database.clone(), initial_session, store.clone()).await;

        let disconnected = handle.disconnect_and_purge().unwrap();
        assert_eq!(
            handle
                .try_enqueue(unbound_scrobble("Too late"))
                .unwrap_err(),
            LastFmRuntimeAdmissionError::Transitioning
        );
        assert_eq!(disconnected.wait().await.unwrap(), 0);
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        assert_eq!(store.delete_attempts(), 1);
        shutdown.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_enqueue_disconnect_has_one_linearized_winner() {
        let database = database().await;
        let initial_session = session("listener");
        let _binding = initial_session.account_binding();
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (handle, shutdown) = runtime(database.clone(), initial_session, store).await;
        let start = Arc::new(ThreadBarrier::new(3));
        let enqueue_handle = handle.clone();
        let enqueue_start = Arc::clone(&start);
        let enqueue = std::thread::spawn(move || {
            enqueue_start.wait();
            enqueue_handle.try_enqueue(unbound_scrobble("Race"))
        });
        let disconnect_handle = handle.clone();
        let disconnect_start = Arc::clone(&start);
        let disconnect = std::thread::spawn(move || {
            disconnect_start.wait();
            disconnect_handle.disconnect_and_purge()
        });
        start.wait();

        let enqueue = enqueue.join().unwrap();
        let disconnect = disconnect.join().unwrap().unwrap();
        match enqueue {
            Ok(operation) => {
                assert!(matches!(
                    operation.wait().await.unwrap(),
                    LastFmEnqueueOutcome::Inserted { .. }
                ));
                assert_eq!(disconnect.wait().await.unwrap(), 1);
            }
            Err(error) => {
                assert_eq!(error, LastFmRuntimeAdmissionError::Transitioning);
                assert_eq!(disconnect.wait().await.unwrap(), 0);
            }
        }
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        shutdown.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bounded_metadata_busy_preserves_reserved_shutdown_capacity() {
        let database = database().await;
        let initial_session = session("listener");
        let _binding = initial_session.account_binding();
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (handle, shutdown) = runtime(database.clone(), initial_session, store).await;
        let mut admitted = Vec::new();

        for index in 0..METADATA_INGRESS_CAPACITY {
            admitted.push(
                handle
                    .try_enqueue(unbound_scrobble(format!("Track {index}")))
                    .unwrap(),
            );
        }
        assert_eq!(
            handle.try_enqueue(unbound_scrobble("Busy")).unwrap_err(),
            LastFmRuntimeAdmissionError::Busy
        );
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmRuntimePhase::Active
        );
        assert!(handle.close_and_flush());
        assert!(!handle.close_and_flush());

        for operation in admitted {
            operation.wait().await.unwrap();
        }
        assert_eq!(
            shutdown.shutdown().await.unwrap(),
            LastFmRuntimeShutdownReason::Drained
        );
        assert_eq!(
            storage::queue_len(&database).await.unwrap(),
            METADATA_INGRESS_CAPACITY as u64
        );
    }

    #[tokio::test]
    async fn many_unwaited_enqueues_drain_before_persistent_barrier() {
        let database = database().await;
        let initial_session = session("listener");
        let _binding = initial_session.account_binding();
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (handle, shutdown) = runtime(database.clone(), initial_session, store).await;
        let barrier = shutdown.barrier();

        for index in 0..32 {
            drop(
                handle
                    .try_enqueue(unbound_scrobble(format!("Unwaited {index}")))
                    .unwrap(),
            );
        }
        assert!(handle.close_and_flush());
        barrier.wait().await.unwrap();
        assert_eq!(barrier.state(), LastFmRuntimeDrainState::Drained);
        assert_eq!(storage::queue_len(&database).await.unwrap(), 32);
        assert_eq!(
            shutdown.shutdown().await.unwrap(),
            LastFmRuntimeShutdownReason::Drained
        );
        barrier.wait().await.unwrap();
    }

    #[tokio::test]
    async fn aborted_owner_marks_barrier_failed_and_never_drained() {
        let database = database().await;
        let initial_session = session("listener");
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (_handle, shutdown) = runtime(database, initial_session, store).await;
        let barrier = shutdown.barrier();

        shutdown.owner.as_ref().unwrap().abort();
        assert_eq!(barrier.wait().await, Err(LastFmRuntimeShutdownError));
        assert_ne!(barrier.state(), LastFmRuntimeDrainState::Drained);
        assert_eq!(shutdown.shutdown().await, Err(LastFmRuntimeShutdownError));
        assert_eq!(barrier.wait().await, Err(LastFmRuntimeShutdownError));
    }

    #[tokio::test]
    async fn purge_failure_never_deletes_vault_and_disconnect_can_retry() {
        let database = database().await;
        let initial_session = session("listener");
        let _binding = initial_session.account_binding();
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (handle, shutdown) = runtime(database.clone(), initial_session, store.clone()).await;
        let rogue_session = session("rogue");
        let rogue_binding = rogue_session.account_binding();
        storage::enqueue(&database, &scrobble(rogue_binding, "Rogue"))
            .await
            .unwrap();

        assert_eq!(
            handle.disconnect_and_purge().unwrap().wait().await,
            Err(LastFmRuntimeCommandError::StaleAccount)
        );
        assert_eq!(store.delete_attempts(), 0);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmRuntimePhase::DisconnectRetry
        );

        assert_eq!(
            storage::purge_account(&database, rogue_binding)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            handle.disconnect_and_purge().unwrap().wait().await.unwrap(),
            0
        );
        assert_eq!(store.delete_attempts(), 1);
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        shutdown.shutdown().await.unwrap();
        drop(rogue_session);
    }

    #[tokio::test]
    async fn delete_failure_reports_zero_pending_and_cleanup_retry_succeeds() {
        let database = database().await;
        let initial_session = session("listener");
        let _binding = initial_session.account_binding();
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        store.fail_next_deletes(1);
        let (handle, shutdown) = runtime(database.clone(), initial_session, store.clone()).await;
        handle
            .try_enqueue(unbound_scrobble("Private"))
            .unwrap()
            .wait()
            .await
            .unwrap();

        assert_eq!(
            handle.disconnect_and_purge().unwrap().wait().await,
            Err(LastFmRuntimeCommandError::CredentialStore)
        );
        let snapshot = *handle.subscribe_status().borrow();
        assert_eq!(snapshot.phase, LastFmRuntimePhase::CredentialCleanup);
        assert_eq!(snapshot.pending_scrobbles, 0);
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        assert_eq!(store.delete_attempts(), 1);

        handle
            .retry_credential_cleanup()
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmRuntimePhase::Disconnected
        );
        assert_eq!(store.delete_attempts(), 2);
        assert_eq!(
            handle.retry_credential_cleanup().unwrap_err(),
            LastFmRuntimeAdmissionError::NotActive
        );
        shutdown.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn global_vault_delete_refuses_a_different_stored_binding() {
        let database = database().await;
        let initial_session = session("listener");
        let different_session = session("different");
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (handle, shutdown) = runtime(database.clone(), initial_session, store.clone()).await;
        store.save(&different_session).unwrap();

        assert_eq!(
            handle.disconnect_and_purge().unwrap().wait().await,
            Err(LastFmRuntimeCommandError::StaleAccount)
        );
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        assert_eq!(store.delete_attempts(), 0);
        assert_eq!(handle.subscribe_status().borrow().pending_scrobbles, 0);
        shutdown.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn successor_start_waits_for_predecessor_vault_generation_to_drain() {
        let database = database().await;
        let initial_session = session("listener");
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (_first_handle, first_shutdown) =
            runtime(database.clone(), initial_session.clone(), store.clone()).await;
        let successor_database = database.clone();
        let successor_store = store.clone();
        let successor = tokio::spawn(async move {
            spawn_lastfm_runtime(
                LastFmRuntimeActivation::issue_after_consent_and_enablement(),
                successor_database,
                successor_store,
                pending_transport(),
                fixed_clock(),
            )
            .await
        });

        tokio::task::yield_now().await;
        assert!(!successor.is_finished());
        assert_eq!(
            first_shutdown.shutdown().await.unwrap(),
            LastFmRuntimeShutdownReason::Drained
        );

        let (_successor_handle, successor_shutdown) = successor.await.unwrap().unwrap();
        assert_eq!(
            successor_shutdown.shutdown().await.unwrap(),
            LastFmRuntimeShutdownReason::Drained
        );
    }

    #[tokio::test]
    async fn duplicate_disconnect_then_shutdown_has_one_purge_and_drains() {
        let database = database().await;
        let initial_session = session("listener");
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        let (handle, shutdown) = runtime(database, initial_session, store.clone()).await;
        let barrier = shutdown.barrier();

        let disconnect = handle.disconnect_and_purge().unwrap();
        assert_eq!(
            handle.disconnect_and_purge().unwrap_err(),
            LastFmRuntimeAdmissionError::Transitioning
        );
        assert!(handle.close_and_flush());
        assert!(!handle.close_and_flush());
        assert_eq!(disconnect.wait().await.unwrap(), 0);
        barrier.wait().await.unwrap();
        assert_eq!(store.delete_attempts(), 1);
        assert_eq!(
            shutdown.shutdown().await.unwrap(),
            LastFmRuntimeShutdownReason::Drained
        );
    }

    #[tokio::test]
    async fn startup_rejects_mismatched_or_corrupt_queue_before_handle_escapes() {
        let mismatched_database = database().await;
        let expected = session("expected");
        let queued = session("queued");
        storage::enqueue(
            &mismatched_database,
            &scrobble(queued.account_binding(), "Wrong account"),
        )
        .await
        .unwrap();
        let store = Arc::new(TestCredentialStore::new(expected.clone()));
        assert!(matches!(
            spawn_lastfm_runtime(
                LastFmRuntimeActivation::issue_after_consent_and_enablement(),
                mismatched_database,
                store,
                pending_transport(),
                fixed_clock(),
            )
            .await,
            Err(LastFmRuntimeStartError::AccountMismatch)
        ));

        let corrupt_database = database().await;
        let expected = session("expected");
        storage::enqueue(
            &corrupt_database,
            &scrobble(expected.account_binding(), "Corrupt"),
        )
        .await
        .unwrap();
        corrupt_database
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE lastfm_scrobble_queue SET occurrence_id = ?",
                [Uuid::nil().as_bytes().to_vec().into()],
            ))
            .await
            .unwrap();
        let store = Arc::new(TestCredentialStore::new(expected.clone()));
        assert!(matches!(
            spawn_lastfm_runtime(
                LastFmRuntimeActivation::issue_after_consent_and_enablement(),
                corrupt_database,
                store,
                pending_transport(),
                fixed_clock(),
            )
            .await,
            Err(LastFmRuntimeStartError::CorruptQueue)
        ));
    }
}
