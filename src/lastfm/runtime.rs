//! Serialized lifecycle owner for the private Last.fm scrobble queue.
//!
//! Metadata commands use a bounded FIFO while four slots remain reserved for
//! one delivery result, two lifecycle markers, and one now-playing clear. One
//! shared ingress mutex is the linearization point: an
//! enqueue which wins it is ordered before disconnect or shutdown, and an
//! enqueue which loses is rejected. Only the receiver mutates SQLite or the
//! protected credential store.

use std::fmt;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll};

use futures::FutureExt;
use sea_orm::DatabaseConnection;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::client::{LastFmClientError, LastFmTrack, SubmissionResult};
use super::credentials::{
    CredentialError, LastFmAccountBinding, ProtectedString, SessionCredentialStore, StoredSession,
};
use super::delivery::{
    delivery_disposition, disposition_for_client_error, next_retry_at_ms, LastFmClock,
    LastFmDeliveryDisposition, LastFmTransport,
};
use super::lifecycle::{acquire_vault_lifecycle, LastFmVaultLifecycleLease};
use super::storage::{
    self, LastFmEnqueueOutcome, LastFmQueueError, PendingLastFmScrobble, UnboundLastFmScrobble,
};
use super::worker::{
    spawn_lastfm_delivery_worker, spawn_lastfm_delivery_worker_suspended,
    LastFmDeliveryAcknowledgement, LastFmDeliveryDirective, LastFmDeliveryEvent,
    LastFmDeliveryGeneration, LastFmDeliveryWorker, LastFmDeliveryWorkerFailure,
};

const METADATA_INGRESS_CAPACITY: usize = 64;
const CONTROL_RESERVED_CAPACITY: usize = 4;
const COMMAND_CAPACITY: usize = METADATA_INGRESS_CAPACITY + CONTROL_RESERVED_CAPACITY;
const MAX_NOW_PLAYING_METADATA_BYTES: usize = 1_024;

/// Monotonic identity of the account admitted by one runtime instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LastFmAccountEpoch(u64);

impl LastFmAccountEpoch {
    const INITIAL: Self = Self(1);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransitionState {
    ReauthorizationInFlight,
    ManualRecoveryInFlight,
    DeliveryRestartCommitted,
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

#[allow(clippy::struct_excessive_bools)] // Each flag is an independent serialized admission proof.
struct IngressGate {
    phase: IngressPhase,
    queue_admission_open: bool,
    queued_metadata: usize,
    delivery_event_queued: bool,
    reauthorization_queued: bool,
    now_playing_clear_queued: bool,
    now_playing_clear_generation: Option<LastFmNowPlayingGeneration>,
    now_playing_generation: LastFmNowPlayingGeneration,
    now_playing_reauthorization_commit: bool,
    delivery_cancellation: Option<CancellationToken>,
    now_playing_cancellation: Option<CancellationToken>,
    shutdown_queued: bool,
    #[cfg(test)]
    recovery_clear_gate: Option<RecoveryClearGate>,
    #[cfg(test)]
    now_playing_result_gate: Option<RecoveryClearGate>,
    #[cfg(test)]
    now_playing_reauthorization_commit_gate: Option<RecoveryClearGate>,
}

#[cfg(test)]
#[derive(Clone)]
struct RecoveryClearGate {
    reached: async_channel::Sender<()>,
    release: async_channel::Receiver<()>,
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

/// Validated account-independent metadata for one now-playing occurrence.
///
/// The wrapper has no account or credential field. Validation happens before
/// admission so the actor never retains malformed private metadata while it
/// waits behind other bounded commands.
pub struct LastFmNowPlaying(LastFmTrack);

impl LastFmNowPlaying {
    /// Validate one protocol track without attaching runtime account state.
    pub fn try_new(track: LastFmTrack) -> Result<Self, LastFmNowPlayingInputError> {
        if !valid_now_playing_required_text(&track.artist)
            || !valid_now_playing_required_text(&track.title)
            || !valid_now_playing_optional_text(track.album.as_deref())
            || !valid_now_playing_optional_text(track.album_artist.as_deref())
            || matches!(track.track_number, Some(0))
            || track.duration_seconds <= 30
        {
            return Err(LastFmNowPlayingInputError);
        }
        Ok(Self(track))
    }
}

impl TryFrom<LastFmTrack> for LastFmNowPlaying {
    type Error = LastFmNowPlayingInputError;

    fn try_from(track: LastFmTrack) -> Result<Self, Self::Error> {
        Self::try_new(track)
    }
}

impl fmt::Debug for LastFmNowPlaying {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmNowPlaying([REDACTED])")
    }
}

/// Fixed-category validation failure for account-independent metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Last.fm now-playing metadata is invalid")]
pub struct LastFmNowPlayingInputError;

/// Content-free terminal disposition of one now-playing occurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmNowPlayingOutcome {
    Accepted,
    Ignored,
    Rejected,
    Unavailable,
    Incompatible,
    CapabilityUnavailable,
}

impl LastFmRuntimeHandle {
    /// Bind and queue one exact validated occurrence for durable insertion.
    ///
    /// The playback side never receives or retains the vault-derived account
    /// binding. This gate attaches the binding owned by the current runtime
    /// account, including while that exact account's credential is renewed.
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
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state:
                    TransitionState::ReauthorizationInFlight | TransitionState::ManualRecoveryInFlight,
            } if ingress.queue_admission_open => (account_binding, account_epoch),
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

    /// Submit one latest-only now-playing occurrence without exposing account
    /// identity or the vault-owned session to playback code.
    ///
    /// Admission is bounded with durable metadata commands. A later admitted
    /// occurrence supersedes the previous network task; this path never
    /// retries a failed now-playing request.
    pub fn try_update_now_playing(
        &self,
        now_playing: LastFmNowPlaying,
    ) -> Result<LastFmRuntimeOperation<LastFmNowPlayingOutcome>, LastFmRuntimeAdmissionError> {
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
        if !matches!(
            self.inner.status.borrow().phase,
            LastFmRuntimePhase::Active | LastFmRuntimePhase::BackingOff
        ) {
            return Err(LastFmRuntimeAdmissionError::Paused);
        }
        if ingress.now_playing_reauthorization_commit {
            return Err(LastFmRuntimeAdmissionError::Transitioning);
        }
        if ingress.queued_metadata >= METADATA_INGRESS_CAPACITY {
            return Err(LastFmRuntimeAdmissionError::Busy);
        }
        let Some(generation) = ingress.now_playing_generation.checked_next() else {
            ingress.phase = IngressPhase::Closed;
            cancel_gate_now_playing(&mut ingress);
            self.inner.commands.close();
            return Err(LastFmRuntimeAdmissionError::Closed);
        };
        ingress.now_playing_generation = generation;
        cancel_gate_now_playing(&mut ingress);

        let (completion, receiver) = oneshot::channel();
        let command = Command::NowPlaying {
            account_binding,
            account_epoch,
            generation,
            now_playing,
            completion,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => ingress.queued_metadata += 1,
            Err(async_channel::TrySendError::Full(_)) => {
                self.inner.commands.close();
                return Err(LastFmRuntimeAdmissionError::Busy);
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                ingress.phase = IngressPhase::Closed;
                return Err(LastFmRuntimeAdmissionError::Closed);
            }
        }
        Ok(LastFmRuntimeOperation { receiver })
    }

    /// Retire the latest now-playing occurrence when playback stops or its
    /// successor is not eligible for Last.fm metadata publication.
    pub fn try_clear_now_playing(
        &self,
    ) -> Result<LastFmRuntimeOperation<()>, LastFmRuntimeAdmissionError> {
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
        if ingress.now_playing_reauthorization_commit {
            return Err(LastFmRuntimeAdmissionError::Transitioning);
        }
        let Some(generation) = ingress.now_playing_generation.checked_next() else {
            ingress.phase = IngressPhase::Closed;
            cancel_gate_now_playing(&mut ingress);
            self.inner.commands.close();
            return Err(LastFmRuntimeAdmissionError::Closed);
        };
        ingress.now_playing_generation = generation;
        ingress.now_playing_clear_generation = Some(generation);
        cancel_gate_now_playing(&mut ingress);
        if ingress.now_playing_clear_queued {
            return Err(LastFmRuntimeAdmissionError::Busy);
        }

        let (completion, receiver) = oneshot::channel();
        let command = Command::ClearNowPlaying {
            account_binding,
            account_epoch,
            completion,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => {
                ingress.now_playing_clear_queued = true;
            }
            Err(async_channel::TrySendError::Full(_)) => {
                ingress.now_playing_clear_generation = None;
                self.inner.commands.close();
                return Err(LastFmRuntimeAdmissionError::Busy);
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                ingress.phase = IngressPhase::Closed;
                ingress.now_playing_clear_generation = None;
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
            Ok(()) => {
                ingress.reauthorization_queued = true;
                cancel_gate_now_playing(&mut ingress);
            }
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

    /// Restart one exact compatibility/capability quarantine only after an
    /// explicit caller has issued the matching manual-recovery capability.
    pub fn resume_after_manual_recovery(
        &self,
        recovery: LastFmManualPauseRecovery,
    ) -> Result<LastFmRuntimeOperation<()>, LastFmRuntimeAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        let (account_binding, account_epoch, previous_phase) = match ingress.phase {
            phase @ IngressPhase::Active {
                account_binding,
                account_epoch,
            } => (account_binding, account_epoch, phase),
            IngressPhase::Transitioning { .. } => {
                return Err(LastFmRuntimeAdmissionError::Transitioning);
            }
            IngressPhase::Closed => return Err(LastFmRuntimeAdmissionError::Closed),
        };
        let status = *self.inner.status.borrow();
        let same_runtime = recovery
            .runtime
            .upgrade()
            .is_some_and(|runtime| Arc::ptr_eq(&runtime, &self.inner));
        if !same_runtime
            || recovery.account_binding != account_binding
            || recovery.account_epoch != account_epoch
            || recovery.status_revision != status.revision
            || recovery.pause.runtime_phase() != status.phase
        {
            return Err(LastFmRuntimeAdmissionError::NotReadyForManualRecovery);
        }
        ingress.phase = IngressPhase::Transitioning {
            account_binding,
            account_epoch,
            state: TransitionState::ManualRecoveryInFlight,
        };
        let (completion, receiver) = oneshot::channel();
        let command = Command::ResumeAfterManualRecovery {
            account_binding,
            account_epoch,
            pause: recovery.pause,
            status_revision: recovery.status_revision,
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
                ingress.queue_admission_open = false;
                Err(LastFmRuntimeAdmissionError::Closed)
            }
        }
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
                cancel_gate_now_playing(&mut ingress);
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

    /// Capture one single-use recovery authority from this exact paused
    /// runtime after an explicit user action.
    pub fn issue_manual_pause_recovery_after_explicit_user_action(
        &self,
    ) -> Result<LastFmManualPauseRecovery, LastFmRuntimeAdmissionError> {
        let ingress = self.lock_ingress()?;
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
        let status = *self.inner.status.borrow();
        let pause = match status.phase {
            LastFmRuntimePhase::CompatibilityPaused => storage::LastFmDurablePause::Compatibility,
            LastFmRuntimePhase::CapabilityPaused => storage::LastFmDurablePause::Capability,
            _ => return Err(LastFmRuntimeAdmissionError::NotReadyForManualRecovery),
        };
        Ok(LastFmManualPauseRecovery {
            runtime: Arc::downgrade(&self.inner),
            account_binding,
            account_epoch,
            status_revision: status.revision,
            pause,
        })
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
    #[error("Last.fm durable pause is not ready for this manual recovery")]
    NotReadyForManualRecovery,
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
    #[error("Last.fm now-playing occurrence was superseded")]
    Superseded,
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

    fn startup(queue: storage::LastFmValidatedQueueState, now_unix_ms: Option<i64>) -> Self {
        let mut status = Self::active(queue.pending_scrobbles);
        if let Some(pause) = queue.durable_pause {
            status.phase = pause.runtime_phase();
            status.failure = Some(pause.runtime_failure());
        } else if queue
            .oldest_next_attempt_at_ms
            .zip(now_unix_ms)
            .is_some_and(|(deadline, now)| deadline > now)
        {
            status.phase = LastFmRuntimePhase::BackingOff;
            status.failure = Some(LastFmRuntimeCommandError::Delivery);
        }
        status
    }
}

impl storage::LastFmDurablePause {
    const fn runtime_phase(self) -> LastFmRuntimePhase {
        match self {
            Self::ReauthenticationRequired => LastFmRuntimePhase::ReauthenticationRequired,
            Self::Compatibility => LastFmRuntimePhase::CompatibilityPaused,
            Self::Capability => LastFmRuntimePhase::CapabilityPaused,
            Self::CredentialCleanupRequired => LastFmRuntimePhase::CredentialCleanup,
        }
    }

    const fn runtime_failure(self) -> LastFmRuntimeCommandError {
        match self {
            Self::ReauthenticationRequired => LastFmRuntimeCommandError::ReauthenticationRequired,
            Self::Compatibility => LastFmRuntimeCommandError::Compatibility,
            Self::Capability => LastFmRuntimeCommandError::DeliveryCapability,
            Self::CredentialCleanupRequired => LastFmRuntimeCommandError::CredentialStore,
        }
    }

    const fn queue_admission_open(self) -> bool {
        matches!(self, Self::ReauthenticationRequired | Self::Compatibility)
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

#[derive(Clone, Copy, Eq, PartialEq)]
struct LastFmNowPlayingGeneration(u64);

impl LastFmNowPlayingGeneration {
    const INITIAL_PREDECESSOR: Self = Self(0);

    fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

enum NowPlayingTaskExit {
    Cancelled,
    Completed(Result<SubmissionResult, LastFmClientError>),
}

struct NowPlayingRuntime {
    account_binding: LastFmAccountBinding,
    account_epoch: LastFmAccountEpoch,
    generation: LastFmNowPlayingGeneration,
    cancellation: CancellationToken,
    task: JoinHandle<NowPlayingTaskExit>,
    completion: Option<oneshot::Sender<Result<LastFmNowPlayingOutcome, LastFmRuntimeCommandError>>>,
}

type SharedVaultLifecycleLease = Arc<LastFmVaultLifecycleLease>;

/// Keeps the process-global account generation until the request future has
/// actually been dropped. Field declaration order is intentional: a hard
/// owner abort may only release this lease share after transport state has
/// been retired, rather than merely after its task was asked to abort.
struct VaultOwnedNowPlayingRequest {
    request:
        Pin<Box<dyn Future<Output = Result<SubmissionResult, LastFmClientError>> + Send + 'static>>,
    _vault_lease: SharedVaultLifecycleLease,
}

impl Future for VaultOwnedNowPlayingRequest {
    type Output = Result<SubmissionResult, LastFmClientError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.request.as_mut().poll(context)
    }
}

impl NowPlayingRuntime {
    async fn cancel_and_join(mut self, reason: LastFmRuntimeCommandError) {
        self.cancellation.cancel();
        self.task.abort();
        let _ = (&mut self.task).await;
        self.complete(Err(reason));
    }

    fn complete(&mut self, result: Result<LastFmNowPlayingOutcome, LastFmRuntimeCommandError>) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(result);
        }
    }
}

impl Drop for NowPlayingRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

enum RuntimeEvent {
    Command(Option<Command>),
    NowPlaying {
        generation: LastFmNowPlayingGeneration,
        result: Result<NowPlayingTaskExit, tokio::task::JoinError>,
    },
}

enum Command {
    Enqueue {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        scrobble: PendingLastFmScrobble,
        completion: oneshot::Sender<Result<LastFmEnqueueOutcome, LastFmRuntimeCommandError>>,
    },
    NowPlaying {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        generation: LastFmNowPlayingGeneration,
        now_playing: LastFmNowPlaying,
        completion: oneshot::Sender<Result<LastFmNowPlayingOutcome, LastFmRuntimeCommandError>>,
    },
    ClearNowPlaying {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        completion: oneshot::Sender<Result<(), LastFmRuntimeCommandError>>,
    },
    Reauthorize {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        username: String,
        key: ProtectedString,
        completion: oneshot::Sender<Result<(), LastFmRuntimeCommandError>>,
    },
    ResumeAfterManualRecovery {
        account_binding: LastFmAccountBinding,
        account_epoch: LastFmAccountEpoch,
        pause: storage::LastFmDurablePause,
        status_revision: u64,
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
    #[cfg(test)]
    PanicForQuiescenceTest,
}

struct DeliveryRuntime {
    generation: LastFmDeliveryGeneration,
    worker: LastFmDeliveryWorker,
    relay: JoinHandle<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DeliveryRetirement {
    joined: bool,
    failure: Option<LastFmDeliveryWorkerFailure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransitionDeliveryStart {
    Started,
    Closed,
    Unavailable,
}

struct TransitionDeliveryPlan {
    transition: TransitionState,
    require_reauthorization_marker: bool,
    restart_status: (LastFmRuntimePhase, Option<LastFmRuntimeCommandError>),
    expected_pause: storage::LastFmDurablePause,
}

impl DeliveryRuntime {
    async fn cancel_and_join(self) -> DeliveryRetirement {
        let worker = self.worker.cancel_and_join().await;
        let relay = self.relay.await;
        let failure = match &worker {
            Ok(super::worker::LastFmDeliveryWorkerExit::Failed(failure)) => Some(*failure),
            Ok(_) | Err(_) => None,
        };
        DeliveryRetirement {
            joined: worker.is_ok() && relay.is_ok(),
            failure,
        }
    }
}

struct ActiveAccount {
    binding: LastFmAccountBinding,
    epoch: LastFmAccountEpoch,
    session: Option<StoredSession>,
    queue_purged: bool,
    last_delivery_generation: LastFmDeliveryGeneration,
    last_now_playing_generation: LastFmNowPlayingGeneration,
    delivery: Option<DeliveryRuntime>,
    #[allow(dead_code)] // Retains the runtime's primary vault-generation share.
    vault_lease: Option<SharedVaultLifecycleLease>,
}

struct RuntimeOwner {
    database: DatabaseConnection,
    credentials: Arc<dyn SessionCredentialStore>,
    command_sender: async_channel::Sender<Command>,
    commands: async_channel::Receiver<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    status_sender: watch::Sender<LastFmRuntimeStatus>,
    status: LastFmRuntimeStatus,
    // Field order is intentional. A hard owner abort has a failed barrier (not
    // joined quiescence), but Drop still asks the child to abort before the
    // account releases its primary vault-lease share. The child independently
    // retains its share until its request future has actually been dropped.
    now_playing: Option<NowPlayingRuntime>,
    account: Option<ActiveAccount>,
    transport: Arc<dyn LastFmTransport>,
    clock: Arc<dyn LastFmClock>,
}

impl RuntimeOwner {
    async fn next_event(&mut self) -> RuntimeEvent {
        let commands = &self.commands;
        let Some(now_playing) = self.now_playing.as_mut() else {
            return RuntimeEvent::Command(commands.recv().await.ok());
        };
        let generation = now_playing.generation;
        tokio::select! {
            biased;
            result = &mut now_playing.task => RuntimeEvent::NowPlaying { generation, result },
            command = commands.recv() => RuntimeEvent::Command(command.ok()),
        }
    }

    async fn run(&mut self) -> Result<LastFmRuntimeShutdownReason, LastFmRuntimeShutdownError> {
        loop {
            let command = match self.next_event().await {
                RuntimeEvent::Command(Some(command)) => command,
                RuntimeEvent::Command(None) => return self.fail_and_retire().await,
                RuntimeEvent::NowPlaying { generation, result } => {
                    self.handle_now_playing_result(generation, result).await;
                    continue;
                }
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
                Command::NowPlaying {
                    account_binding,
                    account_epoch,
                    generation,
                    now_playing,
                    completion,
                } => {
                    if !self.metadata_received() {
                        let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
                        return self.fail_and_retire().await;
                    }
                    self.start_now_playing(
                        account_binding,
                        account_epoch,
                        generation,
                        now_playing,
                        completion,
                    )
                    .await;
                }
                Command::ClearNowPlaying {
                    account_binding,
                    account_epoch,
                    completion,
                } => {
                    let Some(generation) =
                        self.now_playing_clear_received(account_binding, account_epoch)
                    else {
                        let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
                        return self.fail_and_retire().await;
                    };
                    self.clear_now_playing(account_binding, account_epoch, generation, completion)
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
                Command::ResumeAfterManualRecovery {
                    account_binding,
                    account_epoch,
                    pause,
                    status_revision,
                    completion,
                } => {
                    if !self.manual_recovery_received(account_binding, account_epoch) {
                        let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
                        return self.fail_and_retire().await;
                    }
                    self.resume_after_manual_recovery(
                        account_binding,
                        account_epoch,
                        pause,
                        status_revision,
                        completion,
                    )
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
                    let binding = self
                        .account
                        .as_ref()
                        .filter(|account| !account.queue_purged)
                        .map(|account| account.binding);
                    self.retire_now_playing(LastFmRuntimeCommandError::OwnerStopped)
                        .await;
                    let retirement = self.retire_delivery().await;
                    let failure_paused = if let Some(binding) =
                        binding.filter(|_| !retirement.joined || retirement.failure.is_some())
                    {
                        self.ensure_worker_failure_pause(binding).await
                    } else {
                        true
                    };
                    self.account = None;
                    if retirement.joined && failure_paused {
                        self.publish(LastFmRuntimePhase::Stopped, None);
                        return Ok(LastFmRuntimeShutdownReason::Drained);
                    }
                    self.publish(
                        LastFmRuntimePhase::Failed,
                        Some(LastFmRuntimeCommandError::OwnerStopped),
                    );
                    return Err(LastFmRuntimeShutdownError);
                }
                #[cfg(test)]
                Command::PanicForQuiescenceTest => {
                    panic!("redacted Last.fm actor panic test");
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
        let binding = self
            .account
            .as_ref()
            .filter(|account| !account.queue_purged)
            .map(|account| account.binding);
        self.retire_now_playing(LastFmRuntimeCommandError::OwnerStopped)
            .await;
        let _retirement = self.retire_delivery().await;
        if let Some(binding) = binding {
            let _ = self.ensure_worker_failure_pause(binding).await;
        }
        self.account = None;
        Err(LastFmRuntimeShutdownError)
    }

    async fn quiesce_after_actor_panic(&mut self) {
        let unpurged_binding = self
            .account
            .as_ref()
            .filter(|account| !account.queue_purged)
            .map(|account| account.binding);
        if let Ok(mut ingress) = self.ingress.lock() {
            ingress.phase = IngressPhase::Closed;
            ingress.queue_admission_open = false;
            ingress.shutdown_queued = true;
            cancel_gate_delivery(&mut ingress);
            cancel_gate_now_playing(&mut ingress);
        }
        self.commands.close();
        self.retire_now_playing(LastFmRuntimeCommandError::OwnerStopped)
            .await;
        let _ = self.retire_delivery().await;
        if let Some(binding) = unpurged_binding {
            let _ = self.ensure_worker_failure_pause(binding).await;
        }
        self.account = None;
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

    async fn start_now_playing(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmNowPlayingGeneration,
        now_playing: LastFmNowPlaying,
        completion: oneshot::Sender<Result<LastFmNowPlayingOutcome, LastFmRuntimeCommandError>>,
    ) {
        if !self.account_matches(binding, epoch) {
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        }

        self.retire_now_playing(LastFmRuntimeCommandError::Superseded)
            .await;
        if !self.now_playing_command_is_latest(binding, epoch, generation) {
            let _ = completion.send(Err(LastFmRuntimeCommandError::Superseded));
            return;
        }

        let Some((session, vault_lease)) = self
            .account
            .as_ref()
            .and_then(|account| {
                (account.last_now_playing_generation.0 < generation.0).then_some(account)
            })
            .and_then(|account| {
                account
                    .session
                    .clone()
                    .zip(account.vault_lease.as_ref().map(Arc::clone))
            })
        else {
            let _ = completion.send(Err(LastFmRuntimeCommandError::DeliveryCapability));
            return;
        };
        let cancellation = CancellationToken::new();
        {
            let Ok(mut ingress) = self.ingress.lock() else {
                self.commands.close();
                let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
                return;
            };
            if !matches!(
                ingress.phase,
                IngressPhase::Active {
                    account_binding,
                    account_epoch,
                } if account_binding == binding && account_epoch == epoch
            ) || ingress.now_playing_generation != generation
                || ingress.now_playing_reauthorization_commit
                || ingress.now_playing_cancellation.is_some()
            {
                let _ = completion.send(Err(LastFmRuntimeCommandError::Superseded));
                return;
            }
            ingress.now_playing_cancellation = Some(cancellation.clone());
        }
        let Some(account) = self
            .account
            .as_mut()
            .filter(|account| account.binding == binding && account.epoch == epoch)
        else {
            if let Ok(mut ingress) = self.ingress.lock() {
                cancel_gate_now_playing(&mut ingress);
            }
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        };
        account.last_now_playing_generation = generation;

        let transport = Arc::clone(&self.transport);
        let task_cancellation = cancellation.clone();
        let track = now_playing.0;
        let request = VaultOwnedNowPlayingRequest {
            request: Box::pin(async move { transport.update_now_playing(&session, &track).await }),
            _vault_lease: vault_lease,
        };
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                () = task_cancellation.cancelled() => NowPlayingTaskExit::Cancelled,
                result = request => {
                    NowPlayingTaskExit::Completed(result)
                }
            }
        });
        self.now_playing = Some(NowPlayingRuntime {
            account_binding: binding,
            account_epoch: epoch,
            generation,
            cancellation,
            task,
            completion: Some(completion),
        });
    }

    async fn handle_now_playing_result(
        &mut self,
        generation: LastFmNowPlayingGeneration,
        result: Result<NowPlayingTaskExit, tokio::task::JoinError>,
    ) {
        let Some(mut now_playing) = self.now_playing.take() else {
            self.commands.close();
            return;
        };
        if now_playing.generation != generation {
            self.now_playing = Some(now_playing);
            return;
        }

        let binding = now_playing.account_binding;
        let epoch = now_playing.account_epoch;

        #[cfg(test)]
        self.wait_at_now_playing_test_gate(false).await;

        let result = match result {
            Ok(NowPlayingTaskExit::Completed(result)) => result,
            Ok(NowPlayingTaskExit::Cancelled) | Err(_) => {
                let current = self.finish_current_now_playing_result(binding, epoch, generation);
                now_playing.complete(Err(if current {
                    LastFmRuntimeCommandError::OwnerStopped
                } else {
                    LastFmRuntimeCommandError::Superseded
                }));
                return;
            }
        };
        if result == Err(LastFmClientError::ReauthenticationRequired) {
            if !self.claim_now_playing_reauthorization(binding, epoch, generation) {
                now_playing.complete(Err(LastFmRuntimeCommandError::Superseded));
                return;
            }
            #[cfg(test)]
            self.wait_at_now_playing_test_gate(true).await;
            let persisted = storage::persist_pause_for_account(
                &self.database,
                binding,
                storage::LastFmDurablePause::ReauthenticationRequired,
            )
            .await
            .is_ok();
            if !self
                .finish_now_playing_reauthorization_commit(binding, epoch, generation, persisted)
            {
                now_playing.complete(Err(LastFmRuntimeCommandError::StaleAccount));
                return;
            }
            self.retire_delivery().await;
            now_playing.complete(Err(if persisted {
                LastFmRuntimeCommandError::ReauthenticationRequired
            } else {
                LastFmRuntimeCommandError::Queue
            }));
            return;
        }

        if !self.finish_current_now_playing_result(binding, epoch, generation) {
            now_playing.complete(Err(LastFmRuntimeCommandError::Superseded));
            return;
        }

        let outcome = match result {
            Ok(SubmissionResult::Accepted { .. }) => LastFmNowPlayingOutcome::Accepted,
            Ok(SubmissionResult::Ignored { .. }) => LastFmNowPlayingOutcome::Ignored,
            Err(error) => match disposition_for_client_error(error) {
                LastFmDeliveryDisposition::SettleTerminal => LastFmNowPlayingOutcome::Rejected,
                LastFmDeliveryDisposition::RetryTransient => LastFmNowPlayingOutcome::Unavailable,
                LastFmDeliveryDisposition::QuarantineCompatibility => {
                    LastFmNowPlayingOutcome::Incompatible
                }
                LastFmDeliveryDisposition::PauseCapabilityOrInternal => {
                    LastFmNowPlayingOutcome::CapabilityUnavailable
                }
                LastFmDeliveryDisposition::PauseForReauthentication => {
                    now_playing.complete(Err(LastFmRuntimeCommandError::ReauthenticationRequired));
                    return;
                }
            },
        };
        now_playing.complete(Ok(outcome));
    }

    async fn clear_now_playing(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmNowPlayingGeneration,
        completion: oneshot::Sender<Result<(), LastFmRuntimeCommandError>>,
    ) {
        if !self.account_matches(binding, epoch) {
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        }
        self.retire_now_playing(LastFmRuntimeCommandError::Superseded)
            .await;
        let Some(account) = self
            .account
            .as_mut()
            .filter(|account| account.binding == binding && account.epoch == epoch)
        else {
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        };
        if account.last_now_playing_generation.0 >= generation.0 {
            let _ = completion.send(Err(LastFmRuntimeCommandError::DeliveryCapability));
            return;
        }
        account.last_now_playing_generation = generation;
        let _ = completion.send(Ok(()));
    }

    fn now_playing_command_is_latest(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmNowPlayingGeneration,
    ) -> bool {
        self.account_matches(binding, epoch)
            && matches!(
                self.status.phase,
                LastFmRuntimePhase::Active | LastFmRuntimePhase::BackingOff
            )
            && self.now_playing_ingress_matches(binding, epoch, generation)
    }

    fn now_playing_ingress_matches(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmNowPlayingGeneration,
    ) -> bool {
        let Ok(ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        self.account_matches(binding, epoch)
            && matches!(
                ingress.phase,
                IngressPhase::Active {
                    account_binding,
                    account_epoch,
                } if account_binding == binding && account_epoch == epoch
            )
            && ingress.now_playing_generation == generation
            && !ingress.now_playing_reauthorization_commit
    }

    fn finish_current_now_playing_result(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmNowPlayingGeneration,
    ) -> bool {
        let Ok(mut ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        let current = matches!(
            ingress.phase,
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } if account_binding == binding && account_epoch == epoch
        ) && ingress.now_playing_generation == generation
            && !ingress.now_playing_reauthorization_commit;
        if current {
            cancel_gate_now_playing(&mut ingress);
        }
        current
    }

    fn claim_now_playing_reauthorization(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmNowPlayingGeneration,
    ) -> bool {
        let Ok(mut ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        let current = matches!(
            ingress.phase,
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } if account_binding == binding && account_epoch == epoch
        ) && ingress.now_playing_generation == generation
            && !ingress.now_playing_reauthorization_commit;
        if current {
            cancel_gate_now_playing(&mut ingress);
            ingress.now_playing_reauthorization_commit = true;
        }
        current
    }

    fn finish_now_playing_reauthorization_commit(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        generation: LastFmNowPlayingGeneration,
        persisted: bool,
    ) -> bool {
        let ingress_owner = Arc::clone(&self.ingress);
        let Ok(mut ingress) = ingress_owner.lock() else {
            self.commands.close();
            return false;
        };
        if !ingress.now_playing_reauthorization_commit {
            self.commands.close();
            return false;
        }
        ingress.now_playing_reauthorization_commit = false;
        if !matches!(
            ingress.phase,
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } if account_binding == binding && account_epoch == epoch
        ) || ingress.now_playing_generation != generation
        {
            return false;
        }
        cancel_gate_delivery(&mut ingress);
        ingress.queue_admission_open = persisted;
        if persisted {
            self.publish(
                LastFmRuntimePhase::ReauthenticationRequired,
                Some(LastFmRuntimeCommandError::ReauthenticationRequired),
            );
        } else {
            self.publish(
                LastFmRuntimePhase::CapabilityPaused,
                Some(LastFmRuntimeCommandError::Queue),
            );
        }
        true
    }

    #[cfg(test)]
    async fn wait_at_now_playing_test_gate(&self, after_claim: bool) {
        let gate = self.ingress.lock().ok().and_then(|mut ingress| {
            if after_claim {
                ingress.now_playing_reauthorization_commit_gate.take()
            } else {
                ingress.now_playing_result_gate.take()
            }
        });
        if let Some(gate) = gate {
            let _ = gate.reached.send(()).await;
            let _ = gate.release.recv().await;
        }
    }

    async fn retire_now_playing(&mut self, reason: LastFmRuntimeCommandError) {
        if let Ok(mut ingress) = self.ingress.lock() {
            cancel_gate_now_playing(&mut ingress);
        } else {
            self.commands.close();
        }
        if let Some(now_playing) = self.now_playing.take() {
            now_playing.cancel_and_join(reason).await;
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
        self.retire_now_playing(LastFmRuntimeCommandError::ReauthenticationRequired)
            .await;
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

        if !self.retire_delivery().await.joined {
            self.finish_reauthorization_without_delivery(
                binding,
                epoch,
                true,
                LastFmRuntimePhase::ReauthenticationRequired,
                Some(LastFmRuntimeCommandError::DeliveryCapability),
            );
            let _ = completion.send(Err(LastFmRuntimeCommandError::DeliveryCapability));
            return;
        }
        if let Err(error) = self.save_exact_credential(binding, &renewed).await {
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
        if !self.transition_still_owned(binding, epoch, TransitionState::ReauthorizationInFlight) {
            let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
            return;
        }
        if storage::replace_exact_pause(
            &self.database,
            binding,
            storage::LastFmDurablePause::ReauthenticationRequired,
            storage::LastFmDurablePause::Capability,
        )
        .await
        .is_err()
        {
            self.finish_reauthorization_without_delivery(
                binding,
                epoch,
                true,
                LastFmRuntimePhase::ReauthenticationRequired,
                Some(LastFmRuntimeCommandError::Queue),
            );
            let _ = completion.send(Err(LastFmRuntimeCommandError::Queue));
            return;
        }
        let restart_status = match self
            .delivery_restart_status(binding, storage::LastFmDurablePause::Capability)
            .await
        {
            Ok(status) => status,
            Err(error) => {
                self.finish_reauthorization_without_delivery(
                    binding,
                    epoch,
                    false,
                    LastFmRuntimePhase::CapabilityPaused,
                    Some(error),
                );
                let _ = completion.send(Err(error));
                return;
            }
        };
        match self
            .start_transitioned_delivery(
                binding,
                epoch,
                renewed,
                TransitionDeliveryPlan {
                    transition: TransitionState::ReauthorizationInFlight,
                    require_reauthorization_marker: true,
                    restart_status,
                    expected_pause: storage::LastFmDurablePause::Capability,
                },
            )
            .await
        {
            TransitionDeliveryStart::Started => {
                let _ = completion.send(Ok(()));
            }
            TransitionDeliveryStart::Closed => {
                let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
            }
            TransitionDeliveryStart::Unavailable => {
                let restored = self.ensure_worker_failure_pause(binding).await;
                self.finish_reauthorization_without_delivery(
                    binding,
                    epoch,
                    false,
                    LastFmRuntimePhase::CapabilityPaused,
                    Some(if restored {
                        LastFmRuntimeCommandError::DeliveryCapability
                    } else {
                        LastFmRuntimeCommandError::Queue
                    }),
                );
                let _ = completion.send(Err(if restored {
                    LastFmRuntimeCommandError::DeliveryCapability
                } else {
                    LastFmRuntimeCommandError::Queue
                }));
            }
        }
    }

    async fn resume_after_manual_recovery(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        pause: storage::LastFmDurablePause,
        status_revision: u64,
        completion: oneshot::Sender<Result<(), LastFmRuntimeCommandError>>,
    ) {
        let ready = matches!(
            pause,
            storage::LastFmDurablePause::Compatibility | storage::LastFmDurablePause::Capability
        ) && self.account_matches(binding, epoch)
            && self.status.phase == pause.runtime_phase()
            && self.status.revision == status_revision;
        if !ready {
            self.finish_manual_recovery_without_delivery(binding, epoch, pause);
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        }
        if !self.retire_delivery().await.joined {
            self.finish_manual_recovery_without_delivery(binding, epoch, pause);
            let _ = completion.send(Err(LastFmRuntimeCommandError::DeliveryCapability));
            return;
        }
        if !self.transition_still_owned(binding, epoch, TransitionState::ManualRecoveryInFlight) {
            let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
            return;
        }
        if storage::replace_exact_pause(
            &self.database,
            binding,
            pause,
            storage::LastFmDurablePause::Capability,
        )
        .await
        .is_err()
        {
            self.finish_manual_recovery_without_delivery(binding, epoch, pause);
            let _ = completion.send(Err(LastFmRuntimeCommandError::Queue));
            return;
        }
        let restart_status = match self
            .delivery_restart_status(binding, storage::LastFmDurablePause::Capability)
            .await
        {
            Ok(status) => status,
            Err(error) => {
                self.finish_manual_recovery_without_delivery(
                    binding,
                    epoch,
                    storage::LastFmDurablePause::Capability,
                );
                let _ = completion.send(Err(error));
                return;
            }
        };
        let Some(session) = self
            .account
            .as_ref()
            .and_then(|account| account.session.clone())
        else {
            self.finish_manual_recovery_without_delivery(
                binding,
                epoch,
                storage::LastFmDurablePause::Capability,
            );
            let _ = completion.send(Err(LastFmRuntimeCommandError::StaleAccount));
            return;
        };
        match self
            .start_transitioned_delivery(
                binding,
                epoch,
                session,
                TransitionDeliveryPlan {
                    transition: TransitionState::ManualRecoveryInFlight,
                    require_reauthorization_marker: false,
                    restart_status,
                    expected_pause: storage::LastFmDurablePause::Capability,
                },
            )
            .await
        {
            TransitionDeliveryStart::Started => {
                let _ = completion.send(Ok(()));
            }
            TransitionDeliveryStart::Closed => {
                let _ = completion.send(Err(LastFmRuntimeCommandError::OwnerStopped));
            }
            TransitionDeliveryStart::Unavailable => {
                let restored = self.ensure_worker_failure_pause(binding).await;
                self.finish_manual_recovery_without_delivery(
                    binding,
                    epoch,
                    storage::LastFmDurablePause::Capability,
                );
                let _ = completion.send(Err(if restored {
                    LastFmRuntimeCommandError::DeliveryCapability
                } else {
                    LastFmRuntimeCommandError::Queue
                }));
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

        self.retire_now_playing(LastFmRuntimeCommandError::OwnerStopped)
            .await;
        let delivery_retired = self.retire_delivery().await.joined;

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

        match self.delete_credential_and_clear_cleanup(binding).await {
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
        match storage::validate_account_queue_state(&self.database, binding).await {
            Ok(storage::LastFmValidatedQueueState {
                pending_scrobbles: 0,
                durable_pause: Some(storage::LastFmDurablePause::CredentialCleanupRequired),
                ..
            }) => {}
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
        match self.delete_credential_and_clear_cleanup(binding).await {
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
                            self.pause_delivery_for_receipt(
                                binding,
                                epoch,
                                &receipt,
                                storage::LastFmDurablePause::Capability,
                                acknowledgement,
                            )
                            .await;
                            return;
                        };
                        let Ok(outcome_counts) =
                            TerminalOutcomeCounts::from_result(&result, row_count)
                        else {
                            self.pause_delivery_for_receipt(
                                binding,
                                epoch,
                                &receipt,
                                storage::LastFmDurablePause::Capability,
                                acknowledgement,
                            )
                            .await;
                            return;
                        };
                        match storage::settle_terminal(&self.database, &receipt).await {
                            Ok(()) => {
                                let Some(pending_scrobbles) =
                                    self.status.pending_scrobbles.checked_sub(row_count)
                                else {
                                    self.pause_delivery_for_account(
                                        binding,
                                        epoch,
                                        storage::LastFmDurablePause::Capability,
                                    )
                                    .await;
                                    let _ =
                                        acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
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
                                self.pause_delivery_for_receipt(
                                    binding,
                                    epoch,
                                    &receipt,
                                    storage::LastFmDurablePause::Capability,
                                    acknowledgement,
                                )
                                .await;
                                let _ = error;
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
                            Err(error) => {
                                self.pause_delivery_for_receipt(
                                    binding,
                                    epoch,
                                    &receipt,
                                    storage::LastFmDurablePause::Capability,
                                    acknowledgement,
                                )
                                .await;
                                let _ = error;
                            }
                        }
                    }
                    LastFmDeliveryDisposition::PauseForReauthentication => {
                        self.pause_delivery_for_receipt(
                            binding,
                            epoch,
                            &receipt,
                            storage::LastFmDurablePause::ReauthenticationRequired,
                            acknowledgement,
                        )
                        .await;
                    }
                    LastFmDeliveryDisposition::QuarantineCompatibility => {
                        self.pause_delivery_for_receipt(
                            binding,
                            epoch,
                            &receipt,
                            storage::LastFmDurablePause::Compatibility,
                            acknowledgement,
                        )
                        .await;
                    }
                    LastFmDeliveryDisposition::PauseCapabilityOrInternal => {
                        self.pause_delivery_for_receipt(
                            binding,
                            epoch,
                            &receipt,
                            storage::LastFmDurablePause::Capability,
                            acknowledgement,
                        )
                        .await;
                    }
                }
            }
            LastFmDeliveryEvent::Failed {
                generation,
                failure,
                acknowledgement,
            } => {
                if !self.delivery_matches(binding, epoch, generation) {
                    let _ = acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
                    return;
                }
                let pause = match failure {
                    LastFmDeliveryWorkerFailure::Storage(_) => {
                        storage::LastFmDurablePause::Capability
                    }
                    LastFmDeliveryWorkerFailure::Clock(_)
                    | LastFmDeliveryWorkerFailure::Preparation(_)
                    | LastFmDeliveryWorkerFailure::UnexpectedTaskExit => {
                        storage::LastFmDurablePause::Capability
                    }
                };
                self.pause_delivery_for_account(binding, epoch, pause).await;
                let _ = acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
            }
        }
    }

    async fn pause_delivery_for_receipt(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        receipt: &storage::LastFmBatchReceipt,
        pause: storage::LastFmDurablePause,
        acknowledgement: LastFmDeliveryAcknowledgement,
    ) {
        let persisted = storage::persist_pause_for_receipt(&self.database, receipt, pause)
            .await
            .is_ok();
        self.finish_persisted_pause(binding, epoch, pause, persisted);
        self.retire_now_playing(pause.runtime_failure()).await;
        let _ = acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
    }

    async fn pause_delivery_for_account(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        pause: storage::LastFmDurablePause,
    ) {
        let persisted = storage::persist_pause_for_account(&self.database, binding, pause)
            .await
            .is_ok();
        self.finish_persisted_pause(binding, epoch, pause, persisted);
        self.retire_now_playing(pause.runtime_failure()).await;
    }

    fn finish_persisted_pause(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        pause: storage::LastFmDurablePause,
        persisted: bool,
    ) {
        let queue_admission_open = persisted && pause.queue_admission_open();
        if self.pause_delivery_ingress(binding, epoch, !queue_admission_open) {
            if persisted {
                self.publish(pause.runtime_phase(), Some(pause.runtime_failure()));
            } else {
                self.publish(
                    LastFmRuntimePhase::CapabilityPaused,
                    Some(LastFmRuntimeCommandError::Queue),
                );
            }
        }
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

    async fn retire_delivery(&mut self) -> DeliveryRetirement {
        let delivery = self
            .account
            .as_mut()
            .and_then(|account| account.delivery.take());
        match delivery {
            Some(delivery) => delivery.cancel_and_join().await,
            None => DeliveryRetirement {
                joined: true,
                failure: None,
            },
        }
    }

    async fn ensure_worker_failure_pause(&self, binding: LastFmAccountBinding) -> bool {
        if storage::persist_pause_for_account(
            &self.database,
            binding,
            storage::LastFmDurablePause::Capability,
        )
        .await
        .is_ok()
        {
            return true;
        }
        storage::validate_account_queue_state(&self.database, binding)
            .await
            .is_ok_and(|state| state.durable_pause.is_some())
    }

    async fn start_transitioned_delivery(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        session: StoredSession,
        plan: TransitionDeliveryPlan,
    ) -> TransitionDeliveryStart {
        let TransitionDeliveryPlan {
            transition,
            require_reauthorization_marker,
            restart_status,
            expected_pause,
        } = plan;
        let Some(generation) = self
            .account
            .as_ref()
            .filter(|account| account.binding == binding && account.epoch == epoch)
            .and_then(|account| account.last_delivery_generation.checked_next())
        else {
            return TransitionDeliveryStart::Unavailable;
        };

        let ingress_owner = Arc::clone(&self.ingress);
        let (delivery_sender, delivery_events) = async_channel::bounded(1);
        let (worker, activation) = spawn_lastfm_delivery_worker_suspended(
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
        {
            let Ok(mut ingress) = ingress_owner.lock() else {
                self.commands.close();
                return TransitionDeliveryStart::Unavailable;
            };
            match ingress.phase {
                IngressPhase::Transitioning {
                    account_binding,
                    account_epoch,
                    state,
                } if account_binding == binding
                    && account_epoch == epoch
                    && state == transition
                    && (!require_reauthorization_marker || ingress.reauthorization_queued) => {}
                IngressPhase::Closed => {
                    ingress.reauthorization_queued = false;
                    return TransitionDeliveryStart::Closed;
                }
                IngressPhase::Active { .. } | IngressPhase::Transitioning { .. } => {
                    ingress.phase = IngressPhase::Closed;
                    ingress.queue_admission_open = false;
                    ingress.reauthorization_queued = false;
                    cancel_gate_delivery(&mut ingress);
                    self.commands.close();
                    return TransitionDeliveryStart::Unavailable;
                }
            }
            if !self.account_matches(binding, epoch) {
                ingress.phase = IngressPhase::Closed;
                ingress.queue_admission_open = false;
                ingress.reauthorization_queued = false;
                cancel_gate_delivery(&mut ingress);
                self.commands.close();
                return TransitionDeliveryStart::Unavailable;
            }
            let account = self
                .account
                .as_mut()
                .expect("validated Last.fm account remains actor-owned");
            account.session = Some(session);
            account.last_delivery_generation = generation;
            account.delivery = Some(delivery);
            cancel_gate_delivery(&mut ingress);
            ingress.delivery_cancellation = Some(cancellation);
            ingress.phase = IngressPhase::Transitioning {
                account_binding: binding,
                account_epoch: epoch,
                state: TransitionState::DeliveryRestartCommitted,
            };
        }

        // The staged worker cannot inspect the queue. Clearing is therefore
        // the last fallible durable step before activation; on failure the
        // exact marker remains and the suspended generation is retired.
        #[cfg(test)]
        {
            let gate = ingress_owner
                .lock()
                .ok()
                .and_then(|ingress| ingress.recovery_clear_gate.clone());
            if let Some(gate) = gate {
                let _ = gate.reached.send(()).await;
                let _ = gate.release.recv().await;
            }
        }
        if storage::clear_exact_pause(&self.database, binding, expected_pause)
            .await
            .is_err()
        {
            let mut shutdown_won = false;
            if let Ok(mut ingress) = ingress_owner.lock() {
                if matches!(
                    ingress.phase,
                    IngressPhase::Transitioning {
                        account_binding,
                        account_epoch,
                        state: TransitionState::DeliveryRestartCommitted,
                    } if account_binding == binding && account_epoch == epoch
                ) {
                    shutdown_won = ingress.shutdown_queued;
                    ingress.phase = if shutdown_won {
                        IngressPhase::Closed
                    } else {
                        IngressPhase::Transitioning {
                            account_binding: binding,
                            account_epoch: epoch,
                            state: transition,
                        }
                    };
                    cancel_gate_delivery(&mut ingress);
                }
            } else {
                self.commands.close();
            }
            let _ = self.retire_delivery().await;
            return if shutdown_won {
                TransitionDeliveryStart::Closed
            } else {
                TransitionDeliveryStart::Unavailable
            };
        }

        let Ok(mut ingress) = ingress_owner.lock() else {
            self.commands.close();
            return TransitionDeliveryStart::Unavailable;
        };
        match ingress.phase {
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::DeliveryRestartCommitted,
            } if account_binding == binding && account_epoch == epoch => {}
            IngressPhase::Closed => {
                ingress.reauthorization_queued = false;
                return TransitionDeliveryStart::Closed;
            }
            IngressPhase::Active { .. } | IngressPhase::Transitioning { .. } => {
                ingress.phase = IngressPhase::Closed;
                ingress.queue_admission_open = false;
                ingress.reauthorization_queued = false;
                cancel_gate_delivery(&mut ingress);
                self.commands.close();
                return TransitionDeliveryStart::Unavailable;
            }
        }
        if ingress.shutdown_queued {
            ingress.phase = IngressPhase::Closed;
            cancel_gate_delivery(&mut ingress);
            return TransitionDeliveryStart::Closed;
        }
        if !activation.activate() {
            ingress.phase = IngressPhase::Closed;
            ingress.queue_admission_open = false;
            ingress.reauthorization_queued = false;
            cancel_gate_delivery(&mut ingress);
            self.commands.close();
            return TransitionDeliveryStart::Unavailable;
        }
        ingress.queue_admission_open = true;
        ingress.reauthorization_queued = false;
        ingress.phase = IngressPhase::Active {
            account_binding: binding,
            account_epoch: epoch,
        };
        self.publish(restart_status.0, restart_status.1);
        TransitionDeliveryStart::Started
    }

    async fn delivery_restart_status(
        &self,
        binding: LastFmAccountBinding,
        expected_pause: storage::LastFmDurablePause,
    ) -> Result<(LastFmRuntimePhase, Option<LastFmRuntimeCommandError>), LastFmRuntimeCommandError>
    {
        let queue = storage::validate_account_queue_state(&self.database, binding)
            .await
            .map_err(LastFmRuntimeCommandError::from)?;
        if queue.durable_pause != Some(expected_pause)
            || queue.pending_scrobbles != self.status.pending_scrobbles
        {
            return Err(LastFmRuntimeCommandError::Queue);
        }
        let now = self
            .clock
            .now_unix_ms()
            .map_err(|_| LastFmRuntimeCommandError::DeliveryCapability)?;
        if queue
            .oldest_next_attempt_at_ms
            .is_some_and(|deadline| deadline > now)
        {
            Ok((
                LastFmRuntimePhase::BackingOff,
                Some(LastFmRuntimeCommandError::Delivery),
            ))
        } else {
            Ok((LastFmRuntimePhase::Active, None))
        }
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

    fn finish_manual_recovery_without_delivery(
        &mut self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        pause: storage::LastFmDurablePause,
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
                state: TransitionState::ManualRecoveryInFlight,
            } if account_binding == binding && account_epoch == epoch => {
                ingress.queue_admission_open = pause.queue_admission_open();
                ingress.phase = IngressPhase::Active {
                    account_binding: binding,
                    account_epoch: epoch,
                };
                self.publish(pause.runtime_phase(), Some(pause.runtime_failure()));
                true
            }
            IngressPhase::Closed => false,
            IngressPhase::Active { .. } | IngressPhase::Transitioning { .. } => {
                ingress.phase = IngressPhase::Closed;
                ingress.queue_admission_open = false;
                cancel_gate_delivery(&mut ingress);
                self.commands.close();
                false
            }
        }
    }

    fn transition_still_owned(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
        expected: TransitionState,
    ) -> bool {
        let Ok(ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        matches!(
            ingress.phase,
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state,
            } if account_binding == binding && account_epoch == epoch && state == expected
        )
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

    async fn delete_credential_and_clear_cleanup(
        &mut self,
        binding: LastFmAccountBinding,
    ) -> Result<(), LastFmRuntimeCommandError> {
        self.delete_exact_credential(binding).await?;
        storage::clear_exact_pause(
            &self.database,
            binding,
            storage::LastFmDurablePause::CredentialCleanupRequired,
        )
        .await
        .map_err(LastFmRuntimeCommandError::from)
    }

    fn take_vault_lease(
        &mut self,
        binding: LastFmAccountBinding,
    ) -> Result<SharedVaultLifecycleLease, LastFmRuntimeCommandError> {
        self.account
            .as_mut()
            .filter(|account| account.binding == binding)
            .and_then(|account| account.vault_lease.take())
            .ok_or(LastFmRuntimeCommandError::StaleAccount)
    }

    fn restore_vault_lease(
        &mut self,
        binding: LastFmAccountBinding,
        lease: SharedVaultLifecycleLease,
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
            cancel_gate_now_playing(&mut ingress);
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

    fn manual_recovery_received(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
    ) -> bool {
        let Ok(ingress) = self.ingress.lock() else {
            self.commands.close();
            return false;
        };
        match ingress.phase {
            IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::ManualRecoveryInFlight,
            } => account_binding == binding && account_epoch == epoch,
            // Shutdown may close after admission but before actor receipt. The
            // admitted command drains without clearing state or restarting a
            // worker, then the queued shutdown marker remains authoritative.
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

    fn now_playing_clear_received(
        &self,
        binding: LastFmAccountBinding,
        epoch: LastFmAccountEpoch,
    ) -> Option<LastFmNowPlayingGeneration> {
        let Ok(mut ingress) = self.ingress.lock() else {
            self.commands.close();
            return None;
        };
        if !ingress.now_playing_clear_queued {
            self.commands.close();
            return None;
        }
        ingress.now_playing_clear_queued = false;
        let generation = ingress.now_playing_clear_generation.take()?;
        let belongs_to_account = match ingress.phase {
            IngressPhase::Active {
                account_binding,
                account_epoch,
            }
            | IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                ..
            } => account_binding == binding && account_epoch == epoch,
            IngressPhase::Closed => true,
        };
        belongs_to_account.then_some(generation)
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
    match event {
        LastFmDeliveryEvent::Result(event) => {
            let (_, _, _, acknowledgement) = event.into_parts();
            let _ = acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
        }
        LastFmDeliveryEvent::Failed {
            acknowledgement, ..
        } => {
            let _ = acknowledgement.acknowledge(LastFmDeliveryDirective::Stop);
        }
    }
}

/// Start failure before an active handle can escape.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmRuntimeStartError {
    #[error("Last.fm protected credential store is unavailable")]
    CredentialStore,
    #[error("Last.fm protected credential store has no matching account")]
    CredentialMismatch,
    #[error("Last.fm credential cleanup was already complete")]
    CredentialCleanupCompleted,
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

/// Opaque authority for one explicit, category-bound manual retry of a durable
/// compatibility or local-capability quarantine.
pub struct LastFmManualPauseRecovery {
    runtime: Weak<HandleInner>,
    account_binding: LastFmAccountBinding,
    account_epoch: LastFmAccountEpoch,
    status_revision: u64,
    pause: storage::LastFmDurablePause,
}

impl fmt::Debug for LastFmManualPauseRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmManualPauseRecovery")
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
    let empty_cleanup_tombstone = storage::has_empty_cleanup_tombstone(&database)
        .await
        .map_err(LastFmRuntimeStartError::from)?;
    let credentials_for_load = Arc::clone(&credentials);
    // Move the lease into the blocking operation. If startup itself is
    // cancelled, the detached blocking load still retains this vault
    // generation until it has stopped touching the process-global record.
    let (vault_lease, stored_session) =
        tokio::task::spawn_blocking(move || (vault_lease, credentials_for_load.load()))
            .await
            .map_err(|_| LastFmRuntimeStartError::CredentialStore)?;
    let stored_session = stored_session.map_err(|_| LastFmRuntimeStartError::CredentialStore)?;
    let Some(stored_session) = stored_session else {
        let Some(cleanup_binding) = empty_cleanup_tombstone else {
            return Err(LastFmRuntimeStartError::CredentialMismatch);
        };
        let authority = storage::LastFmClosedAndDrainedQueue::issue_after_barrier();
        storage::clear_empty_cleanup_after_missing_vault(&database, cleanup_binding, &authority)
            .await
            .map_err(LastFmRuntimeStartError::from)?;
        drop(vault_lease);
        return Err(LastFmRuntimeStartError::CredentialCleanupCompleted);
    };
    let binding = stored_session.account_binding();
    let queue_state = storage::validate_account_queue_state(&database, binding)
        .await
        .map_err(LastFmRuntimeStartError::from)?;
    let cleanup_only =
        queue_state.durable_pause == Some(storage::LastFmDurablePause::CredentialCleanupRequired);
    let epoch = LastFmAccountEpoch::INITIAL;
    let (command_sender, command_receiver) = async_channel::bounded(COMMAND_CAPACITY);
    let initial_status = LastFmRuntimeStatus::startup(queue_state, clock.now_unix_ms().ok());
    let (status_sender, status) = watch::channel(initial_status);
    let generation = LastFmDeliveryGeneration::new(1);
    let pending_delivery = if queue_state.durable_pause.is_none() {
        let (delivery_sender, delivery_events) = async_channel::bounded(1);
        let worker = spawn_lastfm_delivery_worker(
            database.clone(),
            stored_session.clone(),
            generation,
            Arc::clone(&transport),
            Arc::clone(&clock),
            delivery_sender,
        );
        Some((worker, delivery_events))
    } else {
        None
    };
    let delivery_cancellation = pending_delivery
        .as_ref()
        .map(|(worker, _)| worker.cancellation_token());
    let ingress_phase = if cleanup_only {
        IngressPhase::Transitioning {
            account_binding: binding,
            account_epoch: epoch,
            state: TransitionState::CredentialCleanupRetry,
        }
    } else {
        IngressPhase::Active {
            account_binding: binding,
            account_epoch: epoch,
        }
    };
    let ingress = Arc::new(Mutex::new(IngressGate {
        phase: ingress_phase,
        queue_admission_open: queue_state
            .durable_pause
            .is_none_or(storage::LastFmDurablePause::queue_admission_open),
        queued_metadata: 0,
        delivery_event_queued: false,
        reauthorization_queued: false,
        now_playing_clear_queued: false,
        now_playing_clear_generation: None,
        now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
        now_playing_reauthorization_commit: false,
        delivery_cancellation,
        now_playing_cancellation: None,
        shutdown_queued: false,
        #[cfg(test)]
        recovery_clear_gate: None,
        #[cfg(test)]
        now_playing_result_gate: None,
        #[cfg(test)]
        now_playing_reauthorization_commit_gate: None,
    }));
    let inner = Arc::new(HandleInner {
        commands: command_sender.clone(),
        ingress: Arc::clone(&ingress),
        status,
    });
    let delivery = pending_delivery.map(|(worker, delivery_events)| {
        let relay = spawn_delivery_relay(
            delivery_events,
            command_sender.clone(),
            Arc::clone(&ingress),
            binding,
            epoch,
        );
        DeliveryRuntime {
            generation,
            worker,
            relay,
        }
    });
    let mut owner = RuntimeOwner {
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
            session: if cleanup_only {
                None
            } else {
                Some(stored_session)
            },
            queue_purged: cleanup_only,
            last_delivery_generation: generation,
            last_now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
            delivery,
            vault_lease: Some(Arc::new(vault_lease)),
        }),
        now_playing: None,
    };
    let (completion_sender, completion) = watch::channel(LastFmRuntimeDrainState::Pending);
    let owner_task = tokio::spawn(async move {
        let mut completion = CompletionGuard {
            sender: completion_sender,
            drained: false,
        };
        let result = match AssertUnwindSafe(owner.run()).catch_unwind().await {
            Ok(result) => result,
            Err(_) => {
                owner.quiesce_after_actor_panic().await;
                Err(LastFmRuntimeShutdownError)
            }
        };
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
    if ingress.shutdown_queued || matches!(ingress.phase, IngressPhase::Closed) {
        return false;
    }
    let restart_committed = matches!(
        ingress.phase,
        IngressPhase::Transitioning {
            state: TransitionState::DeliveryRestartCommitted,
            ..
        }
    );
    ingress.queue_admission_open = false;
    match inner.commands.try_send(Command::Shutdown) {
        Ok(()) => {
            ingress.shutdown_queued = true;
            cancel_gate_now_playing(&mut ingress);
            if !restart_committed {
                ingress.phase = IngressPhase::Closed;
                cancel_gate_delivery(&mut ingress);
            }
            true
        }
        Err(_) => {
            ingress.phase = IngressPhase::Closed;
            cancel_gate_delivery(&mut ingress);
            cancel_gate_now_playing(&mut ingress);
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

fn cancel_gate_now_playing(ingress: &mut IngressGate) {
    if let Some(cancellation) = ingress.now_playing_cancellation.take() {
        cancellation.cancel();
    }
}

fn valid_now_playing_required_text(value: &str) -> bool {
    !value.trim().is_empty() && valid_now_playing_text(value)
}

fn valid_now_playing_optional_text(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.trim().is_empty() || valid_now_playing_text(value))
}

fn valid_now_playing_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NOW_PLAYING_METADATA_BYTES
        && !value.contains('\0')
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
#[path = "runtime_delivery_tests.rs"]
mod runtime_delivery_tests;

#[cfg(test)]
#[path = "runtime_reauthorization_tests.rs"]
mod runtime_reauthorization_tests;

#[cfg(test)]
#[path = "runtime_now_playing_tests.rs"]
mod runtime_now_playing_tests;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier as ThreadBarrier, Condvar, Mutex};

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

    struct BlockingDropTransport {
        started: async_channel::Sender<()>,
        dropping: async_channel::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    struct BlockingDropGuard {
        dropping: async_channel::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Drop for BlockingDropGuard {
        fn drop(&mut self) {
            let _ = self.dropping.try_send(());
            let (released, changed) = &*self.release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
        }
    }

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

    #[async_trait::async_trait]
    impl LastFmTransport for BlockingDropTransport {
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
            let _guard = BlockingDropGuard {
                dropping: self.dropping.clone(),
                release: Arc::clone(&self.release),
            };
            let _ = self.started.send(()).await;
            std::future::pending().await
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

        fn has_session(&self) -> bool {
            self.session.lock().unwrap().is_some()
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
            now_playing_clear_queued: false,
            now_playing_clear_generation: None,
            now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
            now_playing_reauthorization_commit: false,
            delivery_cancellation: Some(cancellation.clone()),
            now_playing_cancellation: None,
            shutdown_queued: false,
            #[cfg(test)]
            recovery_clear_gate: None,
            now_playing_result_gate: None,
            now_playing_reauthorization_commit_gate: None,
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
        let (failure_acknowledgement, _failure_directive) =
            LastFmDeliveryAcknowledgement::testing_pair();
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
                        acknowledgement: failure_acknowledgement,
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
            now_playing_clear_queued: false,
            now_playing_clear_generation: None,
            now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
            now_playing_reauthorization_commit: false,
            delivery_cancellation: None,
            now_playing_cancellation: None,
            shutdown_queued: false,
            #[cfg(test)]
            recovery_clear_gate: None,
            now_playing_result_gate: None,
            now_playing_reauthorization_commit_gate: None,
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
                last_now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
                delivery: None,
                vault_lease: Some(Arc::new(vault_lease)),
            }),
            now_playing: None,
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
            now_playing_clear_queued: false,
            now_playing_clear_generation: None,
            now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
            now_playing_reauthorization_commit: false,
            delivery_cancellation: Some(cancellation),
            now_playing_cancellation: None,
            shutdown_queued: false,
            #[cfg(test)]
            recovery_clear_gate: None,
            now_playing_result_gate: None,
            now_playing_reauthorization_commit_gate: None,
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
                last_now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
                delivery: Some(DeliveryRuntime {
                    generation,
                    worker,
                    relay,
                }),
                vault_lease: Some(Arc::new(vault_lease)),
            }),
            now_playing: None,
            clock: fixed_clock(),
        };

        let (failure_acknowledgement, failure_directive) =
            LastFmDeliveryAcknowledgement::testing_pair();

        owner
            .handle_delivery(
                binding,
                epoch,
                LastFmDeliveryEvent::Failed {
                    generation: LastFmDeliveryGeneration::new(8),
                    failure: LastFmDeliveryWorkerFailure::Clock(
                        LastFmDeliveryPrimitiveError::ClockOutOfRange,
                    ),
                    acknowledgement: failure_acknowledgement,
                },
            )
            .await;

        assert_eq!(
            failure_directive.await.unwrap(),
            LastFmDeliveryDirective::Stop
        );

        assert_eq!(storage::queue_len(&database).await.unwrap(), 1);
        assert_eq!(
            storage::validate_account_queue_state(&database, binding)
                .await
                .unwrap()
                .durable_pause,
            None
        );
        assert_eq!(*status.borrow(), initial_status);
        assert!(matches!(
            ingress.lock().unwrap().phase,
            IngressPhase::Active {
                account_binding,
                account_epoch,
            } if account_binding == binding && account_epoch == epoch
        ));
        assert!(owner.retire_delivery().await.joined);
    }

    #[tokio::test]
    async fn admitted_manual_resume_closed_before_actor_receipt_drains_without_clearing_or_restart()
    {
        let database = database().await;
        let stored_session = session("listener");
        let binding = stored_session.account_binding();
        storage::persist_pause_for_account(
            &database,
            binding,
            storage::LastFmDurablePause::Compatibility,
        )
        .await
        .unwrap();
        let store = Arc::new(TestCredentialStore::new(stored_session));
        let (handle, shutdown) = spawn_lastfm_runtime(
            LastFmRuntimeActivation::issue_after_consent_and_enablement(),
            database.clone(),
            store,
            pending_transport(),
            fixed_clock(),
        )
        .await
        .unwrap();
        let (completion, receiver) = oneshot::channel();
        {
            let mut ingress = handle.inner.ingress.lock().unwrap();
            let IngressPhase::Active {
                account_binding,
                account_epoch,
            } = ingress.phase
            else {
                panic!("durably paused runtime retains active account authority");
            };
            ingress.phase = IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::ManualRecoveryInFlight,
            };
            handle
                .inner
                .commands
                .try_send(Command::ResumeAfterManualRecovery {
                    account_binding,
                    account_epoch,
                    pause: storage::LastFmDurablePause::Compatibility,
                    status_revision: handle.inner.status.borrow().revision,
                    completion,
                })
                .unwrap();
            ingress.phase = IngressPhase::Closed;
            ingress.queue_admission_open = false;
            handle.inner.commands.try_send(Command::Shutdown).unwrap();
        }

        assert_eq!(
            LastFmRuntimeOperation { receiver }.wait().await,
            Err(LastFmRuntimeCommandError::OwnerStopped)
        );
        assert_eq!(
            shutdown.shutdown().await.unwrap(),
            LastFmRuntimeShutdownReason::Drained
        );
        assert_eq!(
            storage::validate_account_queue_state(&database, binding)
                .await
                .unwrap()
                .durable_pause,
            Some(storage::LastFmDurablePause::Compatibility)
        );
    }

    #[tokio::test]
    async fn compatibility_resume_orders_an_admitted_enqueue_behind_pause_clear() {
        let database = database().await;
        let stored_session = session("listener");
        let binding = stored_session.account_binding();
        storage::persist_pause_for_account(
            &database,
            binding,
            storage::LastFmDurablePause::Compatibility,
        )
        .await
        .unwrap();
        let store = Arc::new(TestCredentialStore::new(stored_session));
        let (handle, shutdown) = spawn_lastfm_runtime(
            LastFmRuntimeActivation::issue_after_consent_and_enablement(),
            database.clone(),
            store,
            pending_transport(),
            fixed_clock(),
        )
        .await
        .unwrap();
        let (resume_completion, resume_receiver) = oneshot::channel();
        let (enqueue_completion, enqueue_receiver) = oneshot::channel();
        {
            let mut ingress = handle.inner.ingress.lock().unwrap();
            let IngressPhase::Active {
                account_binding,
                account_epoch,
            } = ingress.phase
            else {
                panic!("durably paused runtime retains active account authority");
            };
            assert!(ingress.queue_admission_open);
            ingress.phase = IngressPhase::Transitioning {
                account_binding,
                account_epoch,
                state: TransitionState::ManualRecoveryInFlight,
            };
            handle
                .inner
                .commands
                .try_send(Command::ResumeAfterManualRecovery {
                    account_binding,
                    account_epoch,
                    pause: storage::LastFmDurablePause::Compatibility,
                    status_revision: handle.inner.status.borrow().revision,
                    completion: resume_completion,
                })
                .unwrap();
            handle
                .inner
                .commands
                .try_send(Command::Enqueue {
                    account_binding,
                    account_epoch,
                    scrobble: scrobble(binding, "Behind manual resume"),
                    completion: enqueue_completion,
                })
                .unwrap();
            ingress.queued_metadata += 1;
        }

        LastFmRuntimeOperation {
            receiver: resume_receiver,
        }
        .wait()
        .await
        .unwrap();
        assert!(matches!(
            LastFmRuntimeOperation {
                receiver: enqueue_receiver,
            }
            .wait()
            .await
            .unwrap(),
            LastFmEnqueueOutcome::Inserted { .. }
        ));
        assert_eq!(storage::queue_len(&database).await.unwrap(), 1);
        assert_eq!(
            storage::validate_account_queue_state(&database, binding)
                .await
                .unwrap()
                .durable_pause,
            None
        );
        shutdown.shutdown().await.unwrap();
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
            now_playing_clear_queued: false,
            now_playing_clear_generation: None,
            now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
            now_playing_reauthorization_commit: false,
            delivery_cancellation: Some(cancellation),
            now_playing_cancellation: None,
            shutdown_queued: false,
            #[cfg(test)]
            recovery_clear_gate: None,
            now_playing_result_gate: None,
            now_playing_reauthorization_commit_gate: None,
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
                last_now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
                delivery: Some(DeliveryRuntime {
                    generation,
                    worker,
                    relay,
                }),
                vault_lease: Some(Arc::new(vault_lease)),
            }),
            now_playing: None,
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
        assert!(owner.retire_delivery().await.joined);
    }

    #[tokio::test]
    async fn enqueue_before_disconnect_commits_then_purges_before_vault_delete() {
        let database = database().await;
        let initial_session = session("listener");
        let binding = initial_session.account_binding();
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
            storage::validate_account_queue_state(&database, binding)
                .await
                .unwrap()
                .durable_pause,
            None
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn actor_panic_joins_predecessor_delivery_before_releasing_vault_generation() {
        let database = database().await;
        let initial_session = session("listener");
        let binding = initial_session.account_binding();
        storage::enqueue(&database, &scrobble(binding, "Retained"))
            .await
            .unwrap();
        let store = Arc::new(TestCredentialStore::new(initial_session));
        let (started, started_events) = async_channel::bounded(1);
        let (dropping, dropping_events) = async_channel::bounded(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let transport: Arc<dyn LastFmTransport> = Arc::new(BlockingDropTransport {
            started,
            dropping,
            release: Arc::clone(&release),
        });
        let (handle, shutdown) = spawn_lastfm_runtime(
            LastFmRuntimeActivation::issue_after_consent_and_enablement(),
            database.clone(),
            store.clone(),
            transport,
            fixed_clock(),
        )
        .await
        .unwrap();
        let barrier = shutdown.barrier();
        started_events.recv().await.unwrap();
        handle
            .inner
            .commands
            .try_send(Command::PanicForQuiescenceTest)
            .unwrap();
        dropping_events.recv().await.unwrap();

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
        {
            let (released, changed) = &*release;
            *released.lock().unwrap() = true;
            changed.notify_all();
        }
        assert_eq!(barrier.wait().await, Err(LastFmRuntimeShutdownError));
        let (successor_handle, successor_shutdown) = successor.await.unwrap().unwrap();
        assert_eq!(
            successor_handle.subscribe_status().borrow().phase,
            LastFmRuntimePhase::CapabilityPaused
        );
        assert_eq!(
            storage::validate_account_queue_state(&database, binding)
                .await
                .unwrap()
                .durable_pause,
            Some(storage::LastFmDurablePause::Capability)
        );
        successor_shutdown.shutdown().await.unwrap();
        assert_eq!(shutdown.shutdown().await, Err(LastFmRuntimeShutdownError));
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
        storage::clear_exact_pause(
            &database,
            rogue_binding,
            storage::LastFmDurablePause::CredentialCleanupRequired,
        )
        .await
        .unwrap();
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
        let binding = initial_session.account_binding();
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
            storage::validate_account_queue_state(&database, binding)
                .await
                .unwrap()
                .durable_pause,
            None
        );
        assert_eq!(
            handle.retry_credential_cleanup().unwrap_err(),
            LastFmRuntimeAdmissionError::NotActive
        );
        shutdown.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_tombstone_survives_restart_and_missing_vault_finishes_exact_cleanup() {
        let database = database().await;
        let initial_session = session("listener");
        let binding = initial_session.account_binding();
        let store = Arc::new(TestCredentialStore::new(initial_session.clone()));
        store.fail_next_deletes(1);
        let (handle, shutdown) = runtime(database.clone(), initial_session, store.clone()).await;
        assert_eq!(
            handle.disconnect_and_purge().unwrap().wait().await,
            Err(LastFmRuntimeCommandError::CredentialStore)
        );
        shutdown.shutdown().await.unwrap();

        let (successor, successor_shutdown) = spawn_lastfm_runtime(
            LastFmRuntimeActivation::issue_after_consent_and_enablement(),
            database.clone(),
            store.clone(),
            pending_transport(),
            fixed_clock(),
        )
        .await
        .unwrap();
        assert_eq!(
            successor.subscribe_status().borrow().phase,
            LastFmRuntimePhase::CredentialCleanup
        );
        assert_eq!(
            successor
                .try_enqueue(unbound_scrobble("refused"))
                .unwrap_err(),
            LastFmRuntimeAdmissionError::Transitioning
        );
        successor
            .retry_credential_cleanup()
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert!(!store.has_session());
        assert_eq!(
            storage::validate_account_queue_state(&database, binding)
                .await
                .unwrap()
                .durable_pause,
            None
        );
        successor_shutdown.shutdown().await.unwrap();

        storage::purge_account(&database, binding).await.unwrap();
        assert_eq!(
            spawn_lastfm_runtime(
                LastFmRuntimeActivation::issue_after_consent_and_enablement(),
                database.clone(),
                store,
                pending_transport(),
                fixed_clock(),
            )
            .await
            .unwrap_err(),
            LastFmRuntimeStartError::CredentialCleanupCompleted
        );
        assert_eq!(
            storage::has_empty_cleanup_tombstone(&database)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn cleanup_retry_refuses_a_wrong_same_account_pause_before_vault_delete() {
        let database = database().await;
        let stored_session = session("listener");
        let binding = stored_session.account_binding();
        storage::persist_pause_for_account(
            &database,
            binding,
            storage::LastFmDurablePause::Compatibility,
        )
        .await
        .unwrap();
        let store = Arc::new(TestCredentialStore::new(stored_session));
        let vault_lease = acquire_vault_lifecycle().await;
        let epoch = LastFmAccountEpoch::INITIAL;
        let ingress = Arc::new(Mutex::new(IngressGate {
            phase: IngressPhase::Transitioning {
                account_binding: binding,
                account_epoch: epoch,
                state: TransitionState::CredentialCleanupInFlight,
            },
            queue_admission_open: false,
            queued_metadata: 0,
            delivery_event_queued: false,
            reauthorization_queued: false,
            now_playing_clear_queued: false,
            now_playing_clear_generation: None,
            now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
            now_playing_reauthorization_commit: false,
            delivery_cancellation: None,
            now_playing_cancellation: None,
            shutdown_queued: false,
            recovery_clear_gate: None,
            now_playing_result_gate: None,
            now_playing_reauthorization_commit_gate: None,
        }));
        let (command_sender, commands) = async_channel::bounded(COMMAND_CAPACITY);
        let mut initial_status = LastFmRuntimeStatus::active(0);
        initial_status.phase = LastFmRuntimePhase::CredentialCleanup;
        let (status_sender, _status) = watch::channel(initial_status);
        let mut owner = RuntimeOwner {
            database: database.clone(),
            credentials: store.clone(),
            command_sender,
            commands,
            ingress,
            status_sender,
            status: initial_status,
            account: Some(ActiveAccount {
                binding,
                epoch,
                session: None,
                queue_purged: true,
                last_delivery_generation: LastFmDeliveryGeneration::new(1),
                last_now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
                delivery: None,
                vault_lease: Some(Arc::new(vault_lease)),
            }),
            now_playing: None,
            transport: pending_transport(),
            clock: fixed_clock(),
        };
        let (completion, result) = oneshot::channel();
        owner.retry_cleanup(binding, epoch, completion).await;
        assert_eq!(result.await.unwrap(), Err(LastFmRuntimeCommandError::Queue));
        assert_eq!(store.delete_attempts(), 0);
        assert_eq!(
            storage::validate_account_queue_state(&database, binding)
                .await
                .unwrap()
                .durable_pause,
            Some(storage::LastFmDurablePause::Compatibility)
        );
    }

    #[tokio::test]
    async fn cleanup_tombstone_refuses_a_different_vault_account_without_mutation() {
        let database = database().await;
        let retired = session("retired");
        let retired_binding = retired.account_binding();
        storage::purge_account(&database, retired_binding)
            .await
            .unwrap();
        let store = Arc::new(TestCredentialStore::new(session("successor")));
        assert_eq!(
            spawn_lastfm_runtime(
                LastFmRuntimeActivation::issue_after_consent_and_enablement(),
                database.clone(),
                store.clone(),
                pending_transport(),
                fixed_clock(),
            )
            .await
            .unwrap_err(),
            LastFmRuntimeStartError::AccountMismatch
        );
        assert_eq!(store.delete_attempts(), 0);
        assert_eq!(
            storage::validate_account_queue_state(&database, retired_binding)
                .await
                .unwrap()
                .durable_pause,
            Some(storage::LastFmDurablePause::CredentialCleanupRequired)
        );
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
