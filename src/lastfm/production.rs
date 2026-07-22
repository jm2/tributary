//! Process-lifetime production owner for Last.fm activation.
//!
//! This boundary composes the native credential store, the durable runtime,
//! and the exact-window playback coordinator without letting any one of those
//! pieces infer user consent.  The owner starts dormant, accepts exactly one
//! database attachment, and requires a move-only authority issued only after
//! explicit consent and enablement.  One activation freezes its remote-source
//! policy for the complete runtime generation.
#![allow(clippy::redundant_pub_crate)] // Explicit crate-internal lifecycle authority boundary.

use std::collections::HashSet;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::FutureExt;
use sea_orm::DatabaseConnection;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::architecture::SourceId;

use super::client::{AppCredentials, LastFmClient};
use super::credentials::{OsSessionCredentialStore, SessionCredentialStore};
use super::delivery::{LastFmClock, LastFmTransport, SystemLastFmClock};
use super::playback_coordinator::{
    LastFmPlaybackCoordinatorActivation, LastFmPlaybackCoordinatorBinding,
    LastFmPlaybackCoordinatorOutcome,
};
use super::runtime::{
    spawn_lastfm_runtime, LastFmRuntimeActivation, LastFmRuntimeBarrier, LastFmRuntimeHandle,
    LastFmRuntimeShutdown,
};

const APPLICATION_COMMAND_CAPACITY: usize = 2;
const MAX_ENABLED_REMOTE_SOURCES: usize = 256;
static APPLICATION_OWNER_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Fixed category returned after process ownership has already been consumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Last.fm application ownership is unavailable")]
pub(crate) struct LastFmApplicationOwnerClaimError;

/// Privacy-safe phase of the production composition owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LastFmApplicationPhase {
    UnavailableBuild,
    AwaitingDatabase,
    AwaitingConsent,
    Starting,
    Active,
    ShuttingDown,
    Stopped,
    Failed,
}

/// Latest content-free application-owner snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LastFmApplicationStatus {
    pub(crate) revision: u64,
    pub(crate) phase: LastFmApplicationPhase,
    pub(crate) failure: Option<LastFmApplicationCommandError>,
}

impl LastFmApplicationStatus {
    const fn initial(build_available: bool) -> Self {
        Self {
            revision: 0,
            phase: if build_available {
                LastFmApplicationPhase::AwaitingDatabase
            } else {
                LastFmApplicationPhase::UnavailableBuild
            },
            failure: None,
        }
    }
}

/// Immediate reason a production-owner command was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmApplicationAdmissionError {
    #[error("Last.fm is unavailable in this build")]
    BuildUnavailable,
    #[error("Last.fm application command ingress is busy")]
    Busy,
    #[error("Last.fm application owner is closed")]
    Closed,
    #[error("Last.fm database is already attached")]
    DatabaseAlreadyAttached,
    #[error("Last.fm database is not attached")]
    DatabaseRequired,
    #[error("Last.fm activation has already been consumed")]
    ActivationConsumed,
    #[error("Last.fm remote-source policy is invalid")]
    InvalidSourcePolicy,
}

/// Content-free failure of an admitted production-owner command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmApplicationCommandError {
    #[error("Last.fm application owner stopped")]
    OwnerStopped,
    #[error("Last.fm runtime could not start")]
    RuntimeStart,
    #[error("Last.fm playback ingress is unavailable")]
    PlaybackIngress,
    #[error("Last.fm playback coordinator could not activate")]
    CoordinatorActivation,
    #[error("Last.fm runtime terminated unexpectedly")]
    RuntimeTerminated,
    #[error("Last.fm application generation did not drain")]
    Drain,
}

/// Move-only authority issued after explicit consent and enablement.
///
/// The exact remote-source policy is immutable after construction.  Changing
/// it requires retiring the complete generation and issuing a successor.
#[must_use = "Last.fm activation authority must be consumed or explicitly discarded"]
pub(crate) struct LastFmApplicationActivation {
    enabled_remote_sources: HashSet<SourceId>,
}

impl LastFmApplicationActivation {
    pub(in crate::lastfm) fn issue_after_explicit_consent_and_enablement(
        enabled_remote_sources: HashSet<SourceId>,
    ) -> Result<Self, LastFmApplicationAdmissionError> {
        if enabled_remote_sources.len() > MAX_ENABLED_REMOTE_SOURCES
            || enabled_remote_sources
                .iter()
                .any(|source_id| source_id.is_reserved_remote())
        {
            return Err(LastFmApplicationAdmissionError::InvalidSourcePolicy);
        }
        Ok(Self {
            enabled_remote_sources,
        })
    }
}

impl fmt::Debug for LastFmApplicationActivation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmApplicationActivation(<redacted>)")
    }
}

#[allow(clippy::struct_excessive_bools)] // Independent one-shot admission proofs.
struct IngressGate {
    open: bool,
    build_available: bool,
    database_admitted: bool,
    activation_admitted: bool,
    status_sender: watch::Sender<LastFmApplicationStatus>,
    status: LastFmApplicationStatus,
}

impl IngressGate {
    fn publish(
        &mut self,
        phase: LastFmApplicationPhase,
        failure: Option<LastFmApplicationCommandError>,
    ) {
        self.status.revision = self.status.revision.saturating_add(1);
        self.status.phase = phase;
        self.status.failure = failure;
        self.status_sender.send_replace(self.status);
    }
}

struct HandleInner {
    commands: async_channel::Sender<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    status: watch::Receiver<LastFmApplicationStatus>,
}

/// Cloneable, nonblocking control and status surface.
#[derive(Clone)]
pub(crate) struct LastFmApplicationHandle {
    inner: Arc<HandleInner>,
}

impl LastFmApplicationHandle {
    /// Attach the one database connection this process generation may use.
    pub(crate) fn try_attach_database(
        &self,
        database: DatabaseConnection,
    ) -> Result<LastFmApplicationOperation<()>, LastFmApplicationAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        if !ingress.build_available {
            return Err(LastFmApplicationAdmissionError::BuildUnavailable);
        }
        if ingress.database_admitted {
            return Err(LastFmApplicationAdmissionError::DatabaseAlreadyAttached);
        }
        let (completion, receiver) = oneshot::channel();
        match self.commands_try_send(Command::AttachDatabase {
            database,
            completion,
        }) {
            Ok(()) => ingress.database_admitted = true,
            Err(error) => return Err(error),
        }
        Ok(LastFmApplicationOperation { receiver })
    }

    /// Consume one explicit activation authority.
    pub(crate) fn try_activate(
        &self,
        activation: LastFmApplicationActivation,
    ) -> Result<LastFmApplicationOperation<()>, LastFmApplicationAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        if !ingress.build_available {
            return Err(LastFmApplicationAdmissionError::BuildUnavailable);
        }
        if !ingress.database_admitted {
            return Err(LastFmApplicationAdmissionError::DatabaseRequired);
        }
        if ingress.activation_admitted {
            return Err(LastFmApplicationAdmissionError::ActivationConsumed);
        }
        let (completion, receiver) = oneshot::channel();
        match self.commands_try_send(Command::Activate {
            activation,
            completion,
        }) {
            Ok(()) => ingress.activation_admitted = true,
            Err(error) => return Err(error),
        }
        Ok(LastFmApplicationOperation { receiver })
    }

    /// Close ingress. The retained shutdown owner proves the ordered drain.
    pub(crate) fn close_and_flush(&self) -> bool {
        request_close(&self.inner)
    }

    pub(crate) fn subscribe_status(&self) -> watch::Receiver<LastFmApplicationStatus> {
        self.inner.status.clone()
    }

    fn lock_ingress(&self) -> Result<MutexGuard<'_, IngressGate>, LastFmApplicationAdmissionError> {
        match self.inner.ingress.lock() {
            Ok(ingress) if ingress.open => Ok(ingress),
            Ok(_) => Err(LastFmApplicationAdmissionError::Closed),
            Err(poisoned) => {
                let mut ingress = poisoned.into_inner();
                close_ingress(&self.inner, &mut ingress);
                Err(LastFmApplicationAdmissionError::Closed)
            }
        }
    }

    fn commands_try_send(&self, command: Command) -> Result<(), LastFmApplicationAdmissionError> {
        match self.inner.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(async_channel::TrySendError::Full(_)) => Err(LastFmApplicationAdmissionError::Busy),
            Err(async_channel::TrySendError::Closed(_)) => {
                Err(LastFmApplicationAdmissionError::Closed)
            }
        }
    }
}

impl fmt::Debug for LastFmApplicationHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let phase = self.inner.status.borrow().phase;
        formatter
            .debug_struct("LastFmApplicationHandle")
            .field("phase", &phase)
            .finish_non_exhaustive()
    }
}

/// Completion receipt for one admitted owner command.
pub(crate) struct LastFmApplicationOperation<T> {
    receiver: oneshot::Receiver<Result<T, LastFmApplicationCommandError>>,
}

impl<T> LastFmApplicationOperation<T> {
    pub(crate) async fn wait(self) -> Result<T, LastFmApplicationCommandError> {
        self.receiver
            .await
            .unwrap_or(Err(LastFmApplicationCommandError::OwnerStopped))
    }
}

impl<T> fmt::Debug for LastFmApplicationOperation<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmApplicationOperation(..)")
    }
}

enum Command {
    AttachDatabase {
        database: DatabaseConnection,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    },
    Activate {
        activation: LastFmApplicationActivation,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    },
    #[cfg(test)]
    StopRuntimeForTest,
    #[cfg(test)]
    PanicForTest,
}

struct ActiveGeneration {
    coordinator: LastFmPlaybackCoordinatorActivation,
    runtime_handle: LastFmRuntimeHandle,
    runtime_barrier: LastFmRuntimeBarrier,
    runtime_shutdown: LastFmRuntimeShutdown,
}

struct ApplicationOwner {
    commands: async_channel::Receiver<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    coordinator: LastFmPlaybackCoordinatorBinding,
    completion_runtime: tokio::runtime::Handle,
    credentials: Arc<dyn SessionCredentialStore>,
    transport: Option<Arc<dyn LastFmTransport>>,
    clock: Arc<dyn LastFmClock>,
    database: Option<DatabaseConnection>,
    generation: Option<ActiveGeneration>,
    #[cfg(test)]
    attachment_publish_gate: Option<AttachmentPublishGate>,
    #[cfg(test)]
    activation_start_gate: Option<ActivationStartGate>,
    #[cfg(test)]
    runtime_exit_gate: Option<RuntimeExitGate>,
    #[cfg(test)]
    panic_cleanup_gate: Option<PanicCleanupGate>,
}

impl ApplicationOwner {
    async fn run(
        &mut self,
    ) -> Result<LastFmApplicationShutdownReason, LastFmApplicationShutdownError> {
        loop {
            let command = if let Some(generation) = self.generation.as_ref() {
                let runtime_barrier = generation.runtime_barrier.clone();
                tokio::select! {
                    biased;
                    command = self.commands.recv() => command,
                    _ = runtime_barrier.wait() => {
                        return self.fail_after_unexpected_runtime_exit().await;
                    }
                }
            } else {
                self.commands.recv().await
            };
            let Ok(command) = command else {
                break;
            };
            match command {
                Command::AttachDatabase {
                    database,
                    completion,
                } => {
                    #[cfg(test)]
                    if let Some(gate) = self.attachment_publish_gate.take() {
                        let _ = gate.reached.send(()).await;
                        let _ = gate.release.recv().await;
                    }
                    self.attach_database(database, completion)?;
                }
                Command::Activate {
                    activation,
                    completion,
                } => {
                    if let Err(error) = self.activate(activation, completion).await {
                        self.reject_queued();
                        let _ = self.publish(
                            LastFmApplicationPhase::Failed,
                            Some(LastFmApplicationCommandError::Drain),
                        );
                        return Err(error);
                    }
                }
                #[cfg(test)]
                Command::StopRuntimeForTest => {
                    if let Some(generation) = self.generation.as_ref() {
                        generation.runtime_handle.close_and_flush();
                    }
                }
                #[cfg(test)]
                Command::PanicForTest => panic!("injected application-owner panic"),
            }
        }

        // A start/claim/activation failure is terminal but not itself a
        // failed drain. Preserve that diagnosis while completing the empty
        // generation and persistent barrier normally.
        if self.phase()? == LastFmApplicationPhase::Failed {
            self.reject_queued();
            return if self.close_generation().await.is_ok() {
                Ok(LastFmApplicationShutdownReason::Drained)
            } else {
                let _ = self.publish(
                    LastFmApplicationPhase::Failed,
                    Some(LastFmApplicationCommandError::Drain),
                );
                Err(LastFmApplicationShutdownError)
            };
        }

        self.publish(LastFmApplicationPhase::ShuttingDown, None)?;
        self.reject_queued();
        let drained = self.close_generation().await.is_ok();
        if drained {
            self.publish(LastFmApplicationPhase::Stopped, None)?;
            Ok(LastFmApplicationShutdownReason::Drained)
        } else {
            let _ = self.publish(
                LastFmApplicationPhase::Failed,
                Some(LastFmApplicationCommandError::Drain),
            );
            Err(LastFmApplicationShutdownError)
        }
    }

    async fn fail_after_unexpected_runtime_exit(
        &mut self,
    ) -> Result<LastFmApplicationShutdownReason, LastFmApplicationShutdownError> {
        #[cfg(test)]
        if let Some(gate) = self.runtime_exit_gate.take() {
            let _ = gate.reached.send(()).await;
            let _ = gate.release.recv().await;
        }

        let unexpected = {
            let mut ingress = self.ingress.lock().unwrap_or_else(PoisonError::into_inner);
            if ingress.open {
                ingress.open = false;
                ingress.publish(LastFmApplicationPhase::ShuttingDown, None);
                self.commands.close();
                true
            } else {
                false
            }
        };
        self.ingress.clear_poison();
        self.reject_queued();

        // Whichever event closes ingress owns the outcome. An application
        // close that won the gate remains a normal ordered drain; otherwise
        // the runtime barrier is an unexpected terminal generation failure.
        let drained = self.close_generation().await.is_ok();
        let mut ingress = self.ingress.lock().unwrap_or_else(PoisonError::into_inner);
        ingress.open = false;
        if unexpected {
            ingress.publish(
                LastFmApplicationPhase::Failed,
                Some(LastFmApplicationCommandError::RuntimeTerminated),
            );
        } else if drained {
            ingress.publish(LastFmApplicationPhase::Stopped, None);
        } else {
            ingress.publish(
                LastFmApplicationPhase::Failed,
                Some(LastFmApplicationCommandError::Drain),
            );
        }
        drop(ingress);
        self.ingress.clear_poison();
        if unexpected || !drained {
            Err(LastFmApplicationShutdownError)
        } else {
            Ok(LastFmApplicationShutdownReason::Drained)
        }
    }

    fn attach_database(
        &mut self,
        database: DatabaseConnection,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    ) -> Result<(), LastFmApplicationShutdownError> {
        if self.transport.is_none() {
            let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
            return Ok(());
        }
        if self.database.is_some() {
            let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
            return Ok(());
        }

        // Database attachment and a concurrent close linearize on one gate,
        // so AwaitingConsent can never overwrite ShuttingDown.
        let attach = {
            let mut ingress = self.lock_ingress()?;
            if ingress.open {
                ingress.publish(LastFmApplicationPhase::AwaitingConsent, None);
                true
            } else {
                false
            }
        };
        if !attach {
            let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
            return Ok(());
        }
        self.database = Some(database);
        let _ = completion.send(Ok(()));
        Ok(())
    }

    async fn activate(
        &mut self,
        activation: LastFmApplicationActivation,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    ) -> Result<(), LastFmApplicationShutdownError> {
        let Some(database) = self.database.take() else {
            let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
            return Ok(());
        };
        let Some(transport) = self.transport.take() else {
            let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
            return Ok(());
        };

        #[cfg(test)]
        if let Some(gate) = self.activation_start_gate.take() {
            let _ = gate.reached.send(()).await;
            let _ = gate.release.recv().await;
        }

        // Starting and a concurrent close linearize on the same gate. Close
        // can therefore never be overwritten by a stale Starting snapshot or
        // followed by an unnecessary vault/runtime start.
        let start = {
            let mut ingress = self.lock_ingress()?;
            if ingress.open {
                ingress.publish(LastFmApplicationPhase::Starting, None);
                true
            } else {
                false
            }
        };
        if !start {
            let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
            return Ok(());
        }

        let started = spawn_lastfm_runtime(
            LastFmRuntimeActivation::issue_after_consent_and_enablement(),
            database,
            Arc::clone(&self.credentials),
            transport,
            Arc::clone(&self.clock),
        )
        .await;
        let Ok((runtime_handle, runtime_shutdown)) = started else {
            let _ = completion.send(Err(LastFmApplicationCommandError::RuntimeStart));
            self.fail_terminal(LastFmApplicationCommandError::RuntimeStart)?;
            return Ok(());
        };
        let runtime_barrier = runtime_shutdown.barrier();

        if !self.is_open()? {
            let drained = runtime_shutdown.shutdown().await.is_ok();
            let result = if drained {
                Err(LastFmApplicationCommandError::OwnerStopped)
            } else {
                Err(LastFmApplicationCommandError::Drain)
            };
            let _ = completion.send(result);
            return if drained {
                Ok(())
            } else {
                Err(LastFmApplicationShutdownError)
            };
        }

        let Ok(runtime_ingress) = runtime_handle.try_claim_playback_ingress() else {
            let drained = runtime_shutdown.shutdown().await.is_ok();
            let failure = if drained {
                LastFmApplicationCommandError::PlaybackIngress
            } else {
                LastFmApplicationCommandError::Drain
            };
            let _ = completion.send(Err(failure));
            self.fail_terminal(failure)?;
            return if drained {
                Ok(())
            } else {
                Err(LastFmApplicationShutdownError)
            };
        };

        if !self.is_open()? {
            drop(runtime_ingress);
            let drained = runtime_shutdown.shutdown().await.is_ok();
            let _ = completion.send(Err(if drained {
                LastFmApplicationCommandError::OwnerStopped
            } else {
                LastFmApplicationCommandError::Drain
            }));
            return if drained {
                Ok(())
            } else {
                Err(LastFmApplicationShutdownError)
            };
        }

        let Ok(coordinator) = self.coordinator.activate(
            runtime_ingress,
            self.completion_runtime.clone(),
            activation.enabled_remote_sources,
        ) else {
            let drained = runtime_shutdown.shutdown().await.is_ok();
            let failure = if drained {
                LastFmApplicationCommandError::CoordinatorActivation
            } else {
                LastFmApplicationCommandError::Drain
            };
            let _ = completion.send(Err(failure));
            self.fail_terminal(failure)?;
            return if drained {
                Ok(())
            } else {
                Err(LastFmApplicationShutdownError)
            };
        };

        // Publishing Active and a concurrent close linearize on this gate.
        let publish_active = {
            let mut ingress = self.lock_ingress()?;
            if ingress.open {
                ingress.publish(LastFmApplicationPhase::Active, None);
                true
            } else {
                false
            }
        };
        if !publish_active {
            let coordinator_drained = close_coordinator(coordinator).await;
            let runtime_drained = runtime_shutdown.shutdown().await.is_ok();
            let drained = coordinator_drained && runtime_drained;
            let _ = completion.send(Err(if drained {
                LastFmApplicationCommandError::OwnerStopped
            } else {
                LastFmApplicationCommandError::Drain
            }));
            return if drained {
                Ok(())
            } else {
                Err(LastFmApplicationShutdownError)
            };
        }

        self.generation = Some(ActiveGeneration {
            coordinator,
            runtime_handle,
            runtime_barrier,
            runtime_shutdown,
        });
        let _ = completion.send(Ok(()));
        Ok(())
    }

    async fn close_generation(&mut self) -> Result<(), LastFmApplicationShutdownError> {
        let Some(generation) = self.generation.take() else {
            return Ok(());
        };
        let ActiveGeneration {
            coordinator,
            runtime_handle,
            runtime_barrier: _,
            runtime_shutdown,
        } = generation;
        let coordinator_drained = close_coordinator(coordinator).await;
        drop(runtime_handle);
        let runtime_drained = runtime_shutdown.shutdown().await.is_ok();
        if coordinator_drained && runtime_drained {
            Ok(())
        } else {
            Err(LastFmApplicationShutdownError)
        }
    }

    fn is_open(&self) -> Result<bool, LastFmApplicationShutdownError> {
        Ok(self.lock_ingress()?.open)
    }

    fn phase(&self) -> Result<LastFmApplicationPhase, LastFmApplicationShutdownError> {
        Ok(self.lock_ingress()?.status.phase)
    }

    fn publish(
        &self,
        phase: LastFmApplicationPhase,
        failure: Option<LastFmApplicationCommandError>,
    ) -> Result<(), LastFmApplicationShutdownError> {
        self.lock_ingress()?.publish(phase, failure);
        Ok(())
    }

    fn fail_terminal(
        &self,
        failure: LastFmApplicationCommandError,
    ) -> Result<(), LastFmApplicationShutdownError> {
        let mut ingress = self.lock_ingress()?;
        ingress.open = false;
        ingress.publish(LastFmApplicationPhase::Failed, Some(failure));
        self.commands.close();
        Ok(())
    }

    fn lock_ingress(&self) -> Result<MutexGuard<'_, IngressGate>, LastFmApplicationShutdownError> {
        self.ingress
            .lock()
            .map_err(|_| LastFmApplicationShutdownError)
    }

    fn reject_queued(&self) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::AttachDatabase { completion, .. }
                | Command::Activate { completion, .. } => {
                    let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
                }
                #[cfg(test)]
                Command::StopRuntimeForTest => {}
                #[cfg(test)]
                Command::PanicForTest => {}
            }
        }
    }

    /// Fail closed after an unexpected unwind while retaining this complete
    /// owner value. The persistent barrier must not settle until the exact
    /// coordinator activation has retired and its runtime has joined.
    async fn quiesce_after_panic(&mut self) {
        {
            let mut ingress = self.ingress.lock().unwrap_or_else(PoisonError::into_inner);
            ingress.open = false;
            ingress.publish(LastFmApplicationPhase::ShuttingDown, None);
            self.commands.close();
        }
        self.ingress.clear_poison();
        self.reject_queued();

        #[cfg(test)]
        if let Some(gate) = self.panic_cleanup_gate.take() {
            let _ = gate.reached.send(()).await;
            let _ = gate.release.recv().await;
        }

        let _ = self.close_generation().await;
        let mut ingress = self.ingress.lock().unwrap_or_else(PoisonError::into_inner);
        ingress.open = false;
        ingress.publish(
            LastFmApplicationPhase::Failed,
            Some(LastFmApplicationCommandError::Drain),
        );
        drop(ingress);
        self.ingress.clear_poison();
    }
}

#[cfg(test)]
#[derive(Clone)]
struct PanicCleanupGate {
    reached: async_channel::Sender<()>,
    release: async_channel::Receiver<()>,
}

#[cfg(test)]
struct ActivationStartGate {
    reached: async_channel::Sender<()>,
    release: async_channel::Receiver<()>,
}

#[cfg(test)]
struct AttachmentPublishGate {
    reached: async_channel::Sender<()>,
    release: async_channel::Receiver<()>,
}

#[cfg(test)]
struct RuntimeExitGate {
    reached: async_channel::Sender<()>,
    release: async_channel::Receiver<()>,
}

async fn close_coordinator(activation: LastFmPlaybackCoordinatorActivation) -> bool {
    tokio::task::spawn_blocking(move || activation.close())
        .await
        .is_ok_and(|outcome| outcome == LastFmPlaybackCoordinatorOutcome::Applied)
}

fn request_close(inner: &Arc<HandleInner>) -> bool {
    let mut ingress = inner
        .ingress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !ingress.open {
        return false;
    }
    close_ingress(inner, &mut ingress);
    true
}

fn close_ingress(inner: &Arc<HandleInner>, ingress: &mut IngressGate) {
    ingress.open = false;
    ingress.publish(LastFmApplicationPhase::ShuttingDown, None);
    inner.commands.close();
}

/// Why the explicit application-owner drain completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LastFmApplicationShutdownReason {
    Drained,
}

/// Fixed failure when the activation generation did not drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Last.fm application owner did not drain")]
pub(crate) struct LastFmApplicationShutdownError;

/// Persistent state of the application-owner shutdown proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LastFmApplicationDrainState {
    Pending,
    Drained,
    Failed,
}

struct CompletionGuard {
    sender: watch::Sender<LastFmApplicationDrainState>,
    drained: bool,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.sender.send_replace(if self.drained {
            LastFmApplicationDrainState::Drained
        } else {
            LastFmApplicationDrainState::Failed
        });
    }
}

/// Sole join side for the process-lifetime production owner.
pub(crate) struct LastFmApplicationShutdown {
    inner: Arc<HandleInner>,
    owner:
        Option<JoinHandle<Result<LastFmApplicationShutdownReason, LastFmApplicationShutdownError>>>,
    completion: watch::Receiver<LastFmApplicationDrainState>,
}

impl LastFmApplicationShutdown {
    pub(crate) fn barrier(&self) -> LastFmApplicationBarrier {
        LastFmApplicationBarrier {
            completion: self.completion.clone(),
        }
    }

    pub(crate) async fn shutdown(
        mut self,
    ) -> Result<LastFmApplicationShutdownReason, LastFmApplicationShutdownError> {
        request_close(&self.inner);
        let owner = self.owner.take().ok_or(LastFmApplicationShutdownError)?;
        owner.await.map_err(|_| LastFmApplicationShutdownError)?
    }
}

impl Drop for LastFmApplicationShutdown {
    fn drop(&mut self) {
        request_close(&self.inner);
    }
}

impl fmt::Debug for LastFmApplicationShutdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmApplicationShutdown")
            .field("drain_state", &*self.completion.borrow())
            .finish_non_exhaustive()
    }
}

/// Cloneable persistent proof of normal drain or abnormal owner loss.
#[derive(Clone)]
pub(crate) struct LastFmApplicationBarrier {
    completion: watch::Receiver<LastFmApplicationDrainState>,
}

impl LastFmApplicationBarrier {
    pub(crate) fn state(&self) -> LastFmApplicationDrainState {
        *self.completion.borrow()
    }

    pub(crate) async fn wait(&self) -> Result<(), LastFmApplicationShutdownError> {
        let mut completion = self.completion.clone();
        loop {
            let state = *completion.borrow_and_update();
            match state {
                LastFmApplicationDrainState::Drained => return Ok(()),
                LastFmApplicationDrainState::Failed => {
                    return Err(LastFmApplicationShutdownError);
                }
                LastFmApplicationDrainState::Pending => {}
            }
            if completion.changed().await.is_err() {
                return Err(LastFmApplicationShutdownError);
            }
        }
    }
}

impl fmt::Debug for LastFmApplicationBarrier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmApplicationBarrier")
            .field("state", &self.state())
            .finish()
    }
}

/// Create the production owner before the application database is available.
///
/// Credential probing constructs no database, vault operation, or network
/// request. Missing or malformed build credentials leave a dormant,
/// fail-closed owner whose database ingress rejects without retaining input.
pub(crate) fn spawn_lastfm_application_owner(
    coordinator: LastFmPlaybackCoordinatorBinding,
    completion_runtime: tokio::runtime::Handle,
) -> Result<(LastFmApplicationHandle, LastFmApplicationShutdown), LastFmApplicationOwnerClaimError>
{
    APPLICATION_OWNER_CLAIMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| LastFmApplicationOwnerClaimError)?;
    let transport = AppCredentials::from_build()
        .and_then(LastFmClient::new)
        .ok()
        .map(|client| Arc::new(client) as Arc<dyn LastFmTransport>);
    Ok(spawn_with_dependencies(
        coordinator,
        completion_runtime,
        Arc::new(OsSessionCredentialStore),
        transport,
        Arc::new(SystemLastFmClock),
    ))
}

fn spawn_with_dependencies(
    coordinator: LastFmPlaybackCoordinatorBinding,
    completion_runtime: tokio::runtime::Handle,
    credentials: Arc<dyn SessionCredentialStore>,
    transport: Option<Arc<dyn LastFmTransport>>,
    clock: Arc<dyn LastFmClock>,
) -> (LastFmApplicationHandle, LastFmApplicationShutdown) {
    spawn_with_options(
        coordinator,
        completion_runtime,
        credentials,
        transport,
        clock,
        ApplicationSpawnOptions {
            #[cfg(test)]
            attachment_publish_gate: None,
            #[cfg(test)]
            activation_start_gate: None,
            #[cfg(test)]
            runtime_exit_gate: None,
            #[cfg(test)]
            panic_cleanup_gate: None,
        },
    )
}

struct ApplicationSpawnOptions {
    #[cfg(test)]
    attachment_publish_gate: Option<AttachmentPublishGate>,
    #[cfg(test)]
    activation_start_gate: Option<ActivationStartGate>,
    #[cfg(test)]
    runtime_exit_gate: Option<RuntimeExitGate>,
    #[cfg(test)]
    panic_cleanup_gate: Option<PanicCleanupGate>,
}

#[cfg_attr(not(test), allow(unused_variables))]
fn spawn_with_options(
    coordinator: LastFmPlaybackCoordinatorBinding,
    completion_runtime: tokio::runtime::Handle,
    credentials: Arc<dyn SessionCredentialStore>,
    transport: Option<Arc<dyn LastFmTransport>>,
    clock: Arc<dyn LastFmClock>,
    options: ApplicationSpawnOptions,
) -> (LastFmApplicationHandle, LastFmApplicationShutdown) {
    let build_available = transport.is_some();
    let initial_status = LastFmApplicationStatus::initial(build_available);
    let (status_sender, status) = watch::channel(initial_status);
    let (commands, receiver) = async_channel::bounded(APPLICATION_COMMAND_CAPACITY);
    let ingress = Arc::new(Mutex::new(IngressGate {
        open: true,
        build_available,
        database_admitted: false,
        activation_admitted: false,
        status_sender,
        status: initial_status,
    }));
    let inner = Arc::new(HandleInner {
        commands,
        ingress: Arc::clone(&ingress),
        status,
    });
    let mut owner = ApplicationOwner {
        commands: receiver,
        ingress,
        coordinator,
        completion_runtime,
        credentials,
        transport,
        clock,
        database: None,
        generation: None,
        #[cfg(test)]
        attachment_publish_gate: options.attachment_publish_gate,
        #[cfg(test)]
        activation_start_gate: options.activation_start_gate,
        #[cfg(test)]
        runtime_exit_gate: options.runtime_exit_gate,
        #[cfg(test)]
        panic_cleanup_gate: options.panic_cleanup_gate,
    };
    let (completion_sender, completion) = watch::channel(LastFmApplicationDrainState::Pending);
    let owner_runtime = owner.completion_runtime.clone();
    let owner_task = owner_runtime.spawn(async move {
        let mut guard = CompletionGuard {
            sender: completion_sender,
            drained: false,
        };
        let result = match AssertUnwindSafe(owner.run()).catch_unwind().await {
            Ok(result) => result,
            Err(_) => {
                owner.quiesce_after_panic().await;
                Err(LastFmApplicationShutdownError)
            }
        };
        guard.drained = result.is_ok();
        result
    });
    (
        LastFmApplicationHandle {
            inner: Arc::clone(&inner),
        },
        LastFmApplicationShutdown {
            inner,
            owner: Some(owner_task),
            completion,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use async_trait::async_trait;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    use crate::db::migration::Migrator;
    use crate::lastfm::client::{
        LastFmClientError, LastFmTrack, Scrobble, ScrobbleBatchResult, SubmissionResult,
    };
    use crate::lastfm::credentials::{CredentialError, ProtectedString, StoredSession};
    use crate::lastfm::delivery::LastFmDeliveryPrimitiveError;
    use crate::lastfm::playback_coordinator::LastFmPlaybackCoordinatorOwner;
    use crate::source_registry::SourceRegistry;

    use super::*;

    struct UnusedCredentials;

    impl SessionCredentialStore for UnusedCredentials {
        fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
            panic!("dormant application owner must not read the vault")
        }

        fn save(&self, _session: &StoredSession) -> Result<(), CredentialError> {
            panic!("dormant application owner must not write the vault")
        }

        fn delete(&self) -> Result<(), CredentialError> {
            panic!("dormant application owner must not delete the vault")
        }
    }

    struct FixedCredentials {
        session: Mutex<Option<StoredSession>>,
        loads: AtomicUsize,
    }

    impl FixedCredentials {
        fn new(session: StoredSession) -> Self {
            Self {
                session: Mutex::new(Some(session)),
                loads: AtomicUsize::new(0),
            }
        }
    }

    impl SessionCredentialStore for FixedCredentials {
        fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.session
                .lock()
                .map(|session| session.clone())
                .map_err(|_| CredentialError::Unavailable)
        }

        fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
            *self
                .session
                .lock()
                .map_err(|_| CredentialError::Unavailable)? = Some(session.clone());
            Ok(())
        }

        fn delete(&self) -> Result<(), CredentialError> {
            *self
                .session
                .lock()
                .map_err(|_| CredentialError::Unavailable)? = None;
            Ok(())
        }
    }

    struct GatedLoadCredentials {
        session: StoredSession,
        loads: AtomicUsize,
        load_started: async_channel::Sender<()>,
        load_release: Mutex<mpsc::Receiver<()>>,
    }

    impl GatedLoadCredentials {
        fn new(
            session: StoredSession,
        ) -> (Arc<Self>, async_channel::Receiver<()>, mpsc::Sender<()>) {
            let (load_started, load_observations) = async_channel::bounded(1);
            let (load_release, release) = mpsc::channel();
            (
                Arc::new(Self {
                    session,
                    loads: AtomicUsize::new(0),
                    load_started,
                    load_release: Mutex::new(release),
                }),
                load_observations,
                load_release,
            )
        }
    }

    impl SessionCredentialStore for GatedLoadCredentials {
        fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.load_started
                .try_send(())
                .map_err(|_| CredentialError::Unavailable)?;
            self.load_release
                .lock()
                .map_err(|_| CredentialError::Unavailable)?
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| CredentialError::Unavailable)?;
            Ok(Some(self.session.clone()))
        }

        fn save(&self, _session: &StoredSession) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }

        fn delete(&self) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }
    }

    struct PendingTransport;

    #[async_trait]
    impl LastFmTransport for PendingTransport {
        async fn update_now_playing(
            &self,
            _session: &StoredSession,
            _track: &LastFmTrack,
        ) -> Result<SubmissionResult, LastFmClientError> {
            pending().await
        }

        async fn submit_scrobbles(
            &self,
            _session: &StoredSession,
            _scrobbles: &[Scrobble],
        ) -> Result<ScrobbleBatchResult, LastFmClientError> {
            pending().await
        }
    }

    struct FixedClock;

    #[async_trait]
    impl LastFmClock for FixedClock {
        fn now_unix_ms(&self) -> Result<i64, LastFmDeliveryPrimitiveError> {
            Ok(1_700_000_000_000)
        }

        async fn wait_until_unix_ms(
            &self,
            _deadline_unix_ms: i64,
        ) -> Result<(), LastFmDeliveryPrimitiveError> {
            pending().await
        }
    }

    fn binding() -> (
        LastFmPlaybackCoordinatorOwner,
        LastFmPlaybackCoordinatorBinding,
    ) {
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let binding = owner.bind_window(registry).expect("window binding");
        (owner, binding)
    }

    async fn migrated_database() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory database");
        Migrator::up(&database, None)
            .await
            .expect("migrate Last.fm database");
        database
    }

    fn stored_session() -> StoredSession {
        StoredSession::new(
            "application-owner-listener",
            ProtectedString::new("0123456789abcdef0123456789abcdef"),
        )
        .expect("valid test session")
    }

    #[tokio::test]
    async fn unavailable_build_rejects_database_without_touching_runtime_dependencies() {
        let (_coordinator_owner, coordinator) = binding();
        let (handle, shutdown) = spawn_with_dependencies(
            coordinator,
            tokio::runtime::Handle::current(),
            Arc::new(UnusedCredentials),
            None,
            Arc::new(FixedClock),
        );
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::UnavailableBuild
        );
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory database");
        assert_eq!(
            handle.try_attach_database(database).unwrap_err(),
            LastFmApplicationAdmissionError::BuildUnavailable
        );
        let barrier = shutdown.barrier();
        assert_eq!(
            shutdown.shutdown().await,
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(barrier.wait().await, Ok(()));
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Drained);
    }

    #[tokio::test]
    async fn database_attachment_is_one_shot_and_consent_remains_explicit() {
        let (_coordinator_owner, coordinator) = binding();
        let (handle, shutdown) = spawn_with_dependencies(
            coordinator,
            tokio::runtime::Handle::current(),
            Arc::new(UnusedCredentials),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
        );
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory database");
        handle
            .try_attach_database(database.clone())
            .expect("first database admitted")
            .wait()
            .await
            .expect("database attached");
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::AwaitingConsent
        );
        assert_eq!(
            handle.try_attach_database(database).unwrap_err(),
            LastFmApplicationAdmissionError::DatabaseAlreadyAttached
        );
        assert_eq!(
            shutdown.shutdown().await,
            Ok(LastFmApplicationShutdownReason::Drained)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_before_database_publication_never_regresses_or_attaches() {
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let coordinator = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("window binding");
        let (attachment_reached, attachment_observations) = async_channel::bounded(1);
        let (attachment_release, attachment_releases) = async_channel::bounded(1);
        let (handle, shutdown) = spawn_with_options(
            coordinator,
            tokio::runtime::Handle::current(),
            Arc::new(UnusedCredentials),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
            ApplicationSpawnOptions {
                attachment_publish_gate: Some(AttachmentPublishGate {
                    reached: attachment_reached,
                    release: attachment_releases,
                }),
                activation_start_gate: None,
                runtime_exit_gate: None,
                panic_cleanup_gate: None,
            },
        );
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory database");
        let attachment = handle
            .try_attach_database(database)
            .expect("database command admitted");
        tokio::time::timeout(Duration::from_secs(2), attachment_observations.recv())
            .await
            .expect("pre-attachment gate deadline")
            .expect("pre-attachment gate reached");

        let barrier = shutdown.barrier();
        assert!(handle.close_and_flush());
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Pending);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::ShuttingDown
        );
        attachment_release
            .send(())
            .await
            .expect("release pre-attachment gate");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), attachment.wait())
                .await
                .expect("closed attachment deadline"),
            Err(LastFmApplicationCommandError::OwnerStopped)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), shutdown.shutdown())
                .await
                .expect("application drain deadline"),
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(barrier.wait().await, Ok(()));
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Drained);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Stopped
        );
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn production_constructor_is_claimed_exactly_once_per_process() {
        let first_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut first_coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let first_binding = first_coordinator_owner
            .bind_window(first_registry.clone())
            .expect("first window binding");
        let (_handle, shutdown) =
            spawn_lastfm_application_owner(first_binding, tokio::runtime::Handle::current())
                .expect("first production owner claim");

        let second_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut second_coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let second_binding = second_coordinator_owner
            .bind_window(second_registry.clone())
            .expect("second window binding");
        assert_eq!(
            spawn_lastfm_application_owner(second_binding, tokio::runtime::Handle::current(),)
                .unwrap_err(),
            LastFmApplicationOwnerClaimError
        );

        assert_eq!(
            shutdown.shutdown().await,
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(
            first_coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            second_coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        first_registry.shutdown().wait().await;
        second_registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_generation_activates_once_and_drains_bridge_before_runtime() {
        let database = migrated_database().await;
        let credentials = Arc::new(FixedCredentials::new(stored_session()));
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let coordinator = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("window binding");
        let (handle, shutdown) = spawn_with_dependencies(
            coordinator,
            tokio::runtime::Handle::current(),
            credentials.clone(),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");

        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("local-only activation policy");
        let activated = handle
            .try_activate(activation)
            .expect("activation admitted");
        let duplicate = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("second well-formed activation policy");
        assert_eq!(
            handle.try_activate(duplicate).unwrap_err(),
            LastFmApplicationAdmissionError::ActivationConsumed
        );
        tokio::time::timeout(Duration::from_secs(2), activated.wait())
            .await
            .expect("application activation deadline")
            .expect("real runtime and coordinator activated");
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Active
        );
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 1);

        let barrier = shutdown.barrier();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), shutdown.shutdown())
                .await
                .expect("application shutdown deadline"),
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(barrier.wait().await, Ok(()));
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Drained);
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_during_vault_load_joins_late_runtime_before_drained_barrier() {
        let database = migrated_database().await;
        let (credentials, load_started, load_release) = GatedLoadCredentials::new(stored_session());
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let coordinator = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("window binding");
        let (handle, shutdown) = spawn_with_dependencies(
            coordinator,
            tokio::runtime::Handle::current(),
            credentials.clone(),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("local-only activation policy");
        let activation = handle
            .try_activate(activation)
            .expect("activation admitted");
        tokio::time::timeout(Duration::from_secs(2), load_started.recv())
            .await
            .expect("vault load start deadline")
            .expect("vault load started");

        let barrier = shutdown.barrier();
        assert!(handle.close_and_flush());
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Pending);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::ShuttingDown
        );
        load_release.send(()).expect("release vault load");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), activation.wait())
                .await
                .expect("late activation cleanup deadline"),
            Err(LastFmApplicationCommandError::OwnerStopped)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), shutdown.shutdown())
                .await
                .expect("application drain deadline"),
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(barrier.wait().await, Ok(()));
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Drained);
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 1);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Stopped
        );
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_before_start_publication_never_regresses_or_starts_runtime() {
        let database = migrated_database().await;
        let credentials = Arc::new(FixedCredentials::new(stored_session()));
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let coordinator = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("window binding");
        let (start_reached, start_observations) = async_channel::bounded(1);
        let (start_release, start_releases) = async_channel::bounded(1);
        let (handle, shutdown) = spawn_with_options(
            coordinator,
            tokio::runtime::Handle::current(),
            credentials.clone(),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
            ApplicationSpawnOptions {
                attachment_publish_gate: None,
                activation_start_gate: Some(ActivationStartGate {
                    reached: start_reached,
                    release: start_releases,
                }),
                runtime_exit_gate: None,
                panic_cleanup_gate: None,
            },
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("local-only activation policy");
        let activation = handle
            .try_activate(activation)
            .expect("activation admitted");
        tokio::time::timeout(Duration::from_secs(2), start_observations.recv())
            .await
            .expect("pre-start gate deadline")
            .expect("pre-start gate reached");

        let barrier = shutdown.barrier();
        assert!(handle.close_and_flush());
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Pending);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::ShuttingDown
        );
        start_release
            .send(())
            .await
            .expect("release pre-start gate");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), activation.wait())
                .await
                .expect("closed activation deadline"),
            Err(LastFmApplicationCommandError::OwnerStopped)
        );
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 0);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), shutdown.shutdown())
                .await
                .expect("application drain deadline"),
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(barrier.wait().await, Ok(()));
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Drained);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Stopped
        );
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unexpected_runtime_exit_fails_application_after_retiring_generation() {
        let database = migrated_database().await;
        let credentials = Arc::new(FixedCredentials::new(stored_session()));
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let coordinator = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("window binding");
        let (handle, shutdown) = spawn_with_dependencies(
            coordinator,
            tokio::runtime::Handle::current(),
            credentials,
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("local-only activation policy");
        handle
            .try_activate(activation)
            .expect("activation admitted")
            .wait()
            .await
            .expect("real runtime and coordinator activated");
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Active
        );

        let barrier = shutdown.barrier();
        handle
            .inner
            .commands
            .try_send(Command::StopRuntimeForTest)
            .expect("stop retained runtime independently");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), barrier.wait())
                .await
                .expect("unexpected runtime cleanup deadline"),
            Err(LastFmApplicationShutdownError)
        );
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Failed);
        let status = *handle.subscribe_status().borrow();
        assert_eq!(status.phase, LastFmApplicationPhase::Failed);
        assert_eq!(
            status.failure,
            Some(LastFmApplicationCommandError::RuntimeTerminated)
        );
        let late = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("well-formed late activation");
        assert_eq!(
            handle.try_activate(late).unwrap_err(),
            LastFmApplicationAdmissionError::Closed
        );
        assert_eq!(
            shutdown.shutdown().await,
            Err(LastFmApplicationShutdownError)
        );
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn application_close_wins_runtime_exit_classification_and_drains_normally() {
        let database = migrated_database().await;
        let credentials = Arc::new(FixedCredentials::new(stored_session()));
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let coordinator = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("window binding");
        let (exit_reached, exit_observations) = async_channel::bounded(1);
        let (exit_release, exit_releases) = async_channel::bounded(1);
        let (handle, shutdown) = spawn_with_options(
            coordinator,
            tokio::runtime::Handle::current(),
            credentials,
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
            ApplicationSpawnOptions {
                attachment_publish_gate: None,
                activation_start_gate: None,
                runtime_exit_gate: Some(RuntimeExitGate {
                    reached: exit_reached,
                    release: exit_releases,
                }),
                panic_cleanup_gate: None,
            },
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("local-only activation policy");
        handle
            .try_activate(activation)
            .expect("activation admitted")
            .wait()
            .await
            .expect("real runtime and coordinator activated");

        let barrier = shutdown.barrier();
        handle
            .inner
            .commands
            .try_send(Command::StopRuntimeForTest)
            .expect("stop retained runtime independently");
        tokio::time::timeout(Duration::from_secs(2), exit_observations.recv())
            .await
            .expect("runtime-exit classification deadline")
            .expect("runtime-exit classification reached");
        assert!(handle.close_and_flush());
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Pending);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::ShuttingDown
        );
        exit_release
            .send(())
            .await
            .expect("release runtime-exit classification");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), shutdown.shutdown())
                .await
                .expect("application drain deadline"),
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(barrier.wait().await, Ok(()));
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Drained);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Stopped
        );
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_coordinator_rolls_back_claimed_runtime_and_drains_normally() {
        let database = migrated_database().await;
        let credentials = Arc::new(FixedCredentials::new(stored_session()));
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let stale_binding = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("first window binding");
        let (handle, shutdown) = spawn_with_dependencies(
            stale_binding,
            tokio::runtime::Handle::current(),
            credentials.clone(),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
        );
        let _current_binding = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("replacement window binding");
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("local-only activation policy");
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                handle
                    .try_activate(activation)
                    .expect("activation admitted")
                    .wait(),
            )
            .await
            .expect("stale activation rollback deadline"),
            Err(LastFmApplicationCommandError::CoordinatorActivation)
        );
        let status = *handle.subscribe_status().borrow();
        assert_eq!(status.phase, LastFmApplicationPhase::Failed);
        assert_eq!(
            status.failure,
            Some(LastFmApplicationCommandError::CoordinatorActivation)
        );
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 1);
        let barrier = shutdown.barrier();
        assert_eq!(
            shutdown.shutdown().await,
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(barrier.wait().await, Ok(()));
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Drained);
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn caught_owner_panic_closes_ingress_then_drains_before_failed_barrier() {
        let database = migrated_database().await;
        let credentials = Arc::new(FixedCredentials::new(stored_session()));
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let coordinator = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("window binding");
        let (cleanup_reached, cleanup_observations) = async_channel::bounded(1);
        let (cleanup_release, cleanup_releases) = async_channel::bounded(1);
        let (handle, shutdown) = spawn_with_options(
            coordinator,
            tokio::runtime::Handle::current(),
            credentials.clone(),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
            ApplicationSpawnOptions {
                attachment_publish_gate: None,
                activation_start_gate: None,
                runtime_exit_gate: None,
                panic_cleanup_gate: Some(PanicCleanupGate {
                    reached: cleanup_reached,
                    release: cleanup_releases,
                }),
            },
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("local-only activation policy");
        handle
            .try_activate(activation)
            .expect("activation admitted")
            .wait()
            .await
            .expect("real runtime and coordinator activated");
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Active
        );

        let barrier = shutdown.barrier();
        handle
            .inner
            .commands
            .try_send(Command::PanicForTest)
            .expect("inject owner panic");
        tokio::time::timeout(Duration::from_secs(2), cleanup_observations.recv())
            .await
            .expect("panic cleanup reached deadline")
            .expect("panic cleanup reached");

        assert_eq!(barrier.state(), LastFmApplicationDrainState::Pending);
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::ShuttingDown
        );
        let late = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("well-formed late activation");
        assert_eq!(
            handle.try_activate(late).unwrap_err(),
            LastFmApplicationAdmissionError::Closed
        );

        cleanup_release
            .send(())
            .await
            .expect("release panic cleanup");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), barrier.wait())
                .await
                .expect("failed barrier deadline"),
            Err(LastFmApplicationShutdownError)
        );
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Failed);
        let status = *handle.subscribe_status().borrow();
        assert_eq!(status.phase, LastFmApplicationPhase::Failed);
        assert_eq!(status.failure, Some(LastFmApplicationCommandError::Drain));
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 1);
        assert_eq!(
            shutdown.shutdown().await,
            Err(LastFmApplicationShutdownError)
        );
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn close_after_queued_activation_never_starts_or_orphans_a_runtime() {
        let database = migrated_database().await;
        let credentials = Arc::new(FixedCredentials::new(stored_session()));
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let coordinator = coordinator_owner
            .bind_window(source_registry.clone())
            .expect("window binding");
        let (handle, shutdown) = spawn_with_dependencies(
            coordinator,
            tokio::runtime::Handle::current(),
            credentials.clone(),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
        );

        // Current-thread scheduling makes both commands cross admission
        // before the owner can observe either one. Close then wins the shared
        // gate before startup can begin.
        let attachment = handle
            .try_attach_database(database)
            .expect("database command queued");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("local-only activation policy");
        let activation = handle
            .try_activate(activation)
            .expect("activation command queued");
        assert!(handle.close_and_flush());
        assert!(!handle.close_and_flush());
        assert_eq!(
            attachment.wait().await,
            Err(LastFmApplicationCommandError::OwnerStopped)
        );
        assert_eq!(
            activation.wait().await,
            Err(LastFmApplicationCommandError::OwnerStopped)
        );
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 0);
        assert_eq!(
            shutdown.shutdown().await,
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Stopped
        );
        let post_close = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("well-formed post-close request");
        assert_eq!(
            handle.try_activate(post_close).unwrap_err(),
            LastFmApplicationAdmissionError::Closed
        );
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        source_registry.shutdown().wait().await;
    }

    #[test]
    fn activation_policy_is_bounded_exact_and_redacted() {
        let reserved = HashSet::from([SourceId::local()]);
        assert_eq!(
            LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(reserved)
                .unwrap_err(),
            LastFmApplicationAdmissionError::InvalidSourcePolicy
        );
        let mut oversized = HashSet::new();
        while oversized.len() <= MAX_ENABLED_REMOTE_SOURCES {
            oversized.insert(SourceId::random());
        }
        assert_eq!(
            LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(oversized)
                .unwrap_err(),
            LastFmApplicationAdmissionError::InvalidSourcePolicy
        );
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            HashSet::new(),
        )
        .expect("empty local-only policy is valid");
        assert_eq!(
            format!("{activation:?}"),
            "LastFmApplicationActivation(<redacted>)"
        );
    }
}
