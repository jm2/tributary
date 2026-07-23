//! Process-lifetime coordination boundary for Last.fm playback evidence.
//!
//! The coordinator is deliberately dormant until the complete consent,
//! credential, runtime, and source-policy activation boundary exists. Startup
//! nevertheless claims its unique process owner now, so later activation
//! cannot accidentally grow a second playback-evidence owner. Window code
//! receives only an epoch-bound cloneable binding plus the retained,
//! uncloneable owner used for terminal shutdown.
#![allow(clippy::redundant_pub_crate)] // Explicit crate-internal authority boundary.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::architecture::SourceId;
use crate::audio::{PlayerEvent, PlayerEventGeneration};
use crate::source_registry::SourceRegistry;

use super::playback_owner::{
    LastFmAcceptedOutputLoad, LastFmOutputIntent, LastFmPlaybackHandoff, LastFmPlaybackHandoffKind,
    LastFmPlaybackOwner, LastFmPlaybackOwnerError, LastFmPlaybackOwnerUpdate,
    LastFmPlaybackRuntimeOperation,
};
use super::runtime::{
    LastFmPlaybackRuntimeIngress, LastFmRuntimeAdmissionError, LastFmRuntimeCommandError,
};

static PROCESS_OWNER_CLAIMED: AtomicBool = AtomicBool::new(false);
// The bridge must never retain more enqueue receipts than the runtime's
// bounded metadata ingress can admit.
const ENQUEUE_COMPLETION_CAPACITY: usize = 64;

/// Fixed category returned when process ownership has already been consumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmPlaybackCoordinatorClaimError {
    #[error("Last.fm playback coordinator ownership is unavailable")]
    AlreadyClaimed,
}

/// Fixed category returned when a window cannot bind the coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmPlaybackCoordinatorBindError {
    #[error("Last.fm playback coordinator has shut down")]
    Shutdown,
    #[error("Last.fm playback coordinator state is unavailable")]
    Poisoned,
    #[error("Last.fm playback coordinator window identity is exhausted")]
    WindowEpochExhausted,
    #[error("Last.fm playback coordinator could not retire its activation")]
    RetirementFailed,
}

/// Fixed category returned when the sealed headless bridge cannot activate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmPlaybackCoordinatorActivationError {
    #[error("Last.fm playback coordinator already has an active owner")]
    AlreadyActive,
    #[error("Last.fm playback coordinator activation belongs to a stale window")]
    StaleWindow,
    #[error("Last.fm playback coordinator is retiring an activation")]
    Retiring,
    #[error("Last.fm playback coordinator has shut down")]
    Shutdown,
    #[error("Last.fm playback coordinator state is unavailable")]
    Poisoned,
    #[error("Last.fm playback coordinator activation identity is exhausted")]
    ActivationEpochExhausted,
}

/// Fixed, content-free failure of one coordinator ingress operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmPlaybackCoordinatorFailure {
    #[error("Last.fm playback coordinator state is unavailable")]
    Poisoned,
    #[error("Last.fm playback owner state is unavailable")]
    PlaybackOwnerPoisoned,
    #[error("Last.fm playback operation gate is unavailable")]
    OperationGatePoisoned,
    #[error("Last.fm playback operation capacity is exhausted")]
    OperationCapacityExhausted,
    #[error("Last.fm playback retirement gate is unavailable")]
    RetirementGatePoisoned,
    #[error("Last.fm playback evidence failed")]
    Playback(LastFmPlaybackOwnerError),
    #[error("Last.fm runtime playback ingress rejected an operation")]
    Runtime(LastFmRuntimeAdmissionError),
    #[error("Last.fm runtime failed to durably accept playback evidence")]
    RuntimeCommand(LastFmRuntimeCommandError),
}

/// Content-free disposition of one coordinator operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "coordinator disposition must be handled or deliberately ignored"]
pub(crate) enum LastFmPlaybackCoordinatorOutcome {
    /// The process boundary exists but user/runtime activation does not.
    Dormant,
    /// A state transition was applied.
    Applied,
    /// A runtime enqueue was admitted for supervised durable completion.
    PendingDurability,
    /// Exact source authority or opt-in rejected an action at dispatch.
    SourceRejected,
    /// The caller belongs to a superseded window epoch.
    StaleWindow,
    /// Work selected an activation which retired before it could commit.
    StaleActivation,
    /// Process-lifetime shutdown has closed all ingress.
    Shutdown,
    /// The state mutex failed closed and terminally shut down the coordinator.
    Failed(LastFmPlaybackCoordinatorFailure),
}

impl LastFmPlaybackCoordinatorOutcome {
    const fn terminal_environment_failure(self) -> bool {
        matches!(
            self,
            Self::Failed(
                LastFmPlaybackCoordinatorFailure::PlaybackOwnerPoisoned
                    | LastFmPlaybackCoordinatorFailure::OperationGatePoisoned
                    | LastFmPlaybackCoordinatorFailure::OperationCapacityExhausted
                    | LastFmPlaybackCoordinatorFailure::Runtime(
                        LastFmRuntimeAdmissionError::Closed
                    )
                    | LastFmPlaybackCoordinatorFailure::RuntimeCommand(_)
            )
        )
    }
}

/// Typed reason for unconditional retirement of the active occurrence.
///
/// These categories contain no source identity, metadata, generation, output
/// locator, or error text. Application shutdown is intentionally a distinct
/// owner operation because it closes the complete coordinator, not merely the
/// current occurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LastFmPlaybackRetirement {
    Stop,
    SourceRetirement,
    OutputReplacement,
    QueueAbandoned,
    Terminal,
}

enum LastFmPlaybackCoordinatorState {
    Dormant {
        window_epoch: u64,
        activation_epoch: u64,
    },
    Active {
        window_epoch: u64,
        activation_epoch: u64,
        environment: Arc<LastFmActivePlaybackEnvironment>,
    },
    Retiring {
        window_epoch: u64,
        activation_epoch: u64,
        environment: Arc<LastFmActivePlaybackEnvironment>,
    },
    Shutdown,
}

type LastFmEnqueueCompletion =
    Pin<Box<dyn Future<Output = Result<(), LastFmRuntimeCommandError>> + Send + 'static>>;

enum LastFmPlaybackRuntimeDispatch {
    Immediate(LastFmPlaybackCoordinatorOutcome),
    PendingEnqueue(LastFmEnqueueCompletion),
}

trait LastFmPlaybackRuntimePort: Send + Sync {
    fn dispatch(
        &self,
        handoff: LastFmPlaybackHandoff,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
    ) -> LastFmPlaybackRuntimeDispatch;
}

impl LastFmPlaybackRuntimePort for LastFmPlaybackRuntimeIngress {
    fn dispatch(
        &self,
        handoff: LastFmPlaybackHandoff,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
    ) -> LastFmPlaybackRuntimeDispatch {
        match handoff.try_admit(self, registry, enabled_remote_sources) {
            None => LastFmPlaybackRuntimeDispatch::Immediate(
                LastFmPlaybackCoordinatorOutcome::SourceRejected,
            ),
            Some(Ok(LastFmPlaybackRuntimeOperation::Enqueue(operation))) => {
                LastFmPlaybackRuntimeDispatch::PendingEnqueue(Box::pin(async move {
                    operation.wait().await.map(|_| ())
                }))
            }
            Some(Ok(operation)) => {
                // Ephemeral NowPlaying/Clear completion does not establish
                // durable evidence. Runtime status remains its asynchronous
                // reporting boundary, so synchronous admission is the bridge
                // commit and its receipt is deliberately released here.
                drop(operation);
                LastFmPlaybackRuntimeDispatch::Immediate(LastFmPlaybackCoordinatorOutcome::Applied)
            }
            Some(Err(error)) => {
                LastFmPlaybackRuntimeDispatch::Immediate(LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::Runtime(error),
                ))
            }
        }
    }
}

struct LastFmActivePlaybackEnvironment {
    operation_gate: Arc<Mutex<LastFmActivePlaybackGate>>,
    operation_drained: Arc<Condvar>,
    retirement: Mutex<LastFmActivePlaybackRetirement>,
    retirement_completed: Condvar,
    owner: Mutex<LastFmPlaybackOwner>,
    runtime: Box<dyn LastFmPlaybackRuntimePort>,
    completion_runtime: tokio::runtime::Handle,
    enqueue_completion_slots: Arc<tokio::sync::Semaphore>,
    registry: SourceRegistry,
    enabled_remote_sources: HashSet<SourceId>,
}

struct LastFmActivePlaybackGate {
    live: bool,
    in_flight: usize,
    poisoned: bool,
    terminal_failure: Option<LastFmPlaybackCoordinatorOutcome>,
}

#[derive(Clone, Copy)]
enum LastFmActivePlaybackRetirement {
    Pending,
    Running { poisoned: bool },
    Complete(LastFmPlaybackCoordinatorOutcome),
}

struct LastFmActivePlaybackOperation {
    operation_gate: Arc<Mutex<LastFmActivePlaybackGate>>,
    operation_drained: Arc<Condvar>,
    cancellation_failure: Option<LastFmPlaybackCoordinatorOutcome>,
    completed: bool,
}

impl LastFmActivePlaybackOperation {
    fn reserve_child(&self) -> Result<Self, LastFmPlaybackCoordinatorOutcome> {
        let mut gate = match self.operation_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => {
                let mut gate = poisoned.into_inner();
                gate.live = false;
                gate.poisoned = true;
                gate.terminal_failure = Some(LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                ));
                self.operation_gate.clear_poison();
                return Err(gate
                    .terminal_failure
                    .expect("poisoned operation gate has a fixed failure"));
            }
        };
        if let Some(failure) = gate.terminal_failure {
            return Err(failure);
        }
        if gate.poisoned || gate.in_flight == 0 {
            gate.live = false;
            gate.poisoned = true;
            let failure = LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
            );
            gate.terminal_failure = Some(failure);
            return Err(failure);
        }
        let Some(in_flight) = gate.in_flight.checked_add(1) else {
            gate.live = false;
            let failure = LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::OperationCapacityExhausted,
            );
            gate.terminal_failure = Some(failure);
            return Err(failure);
        };
        gate.in_flight = in_flight;
        drop(gate);
        Ok(Self {
            operation_gate: Arc::clone(&self.operation_gate),
            operation_drained: Arc::clone(&self.operation_drained),
            cancellation_failure: None,
            completed: false,
        })
    }

    fn arm_owner_stopped(mut self) -> Self {
        self.cancellation_failure = Some(LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::RuntimeCommand(
                LastFmRuntimeCommandError::OwnerStopped,
            ),
        ));
        self
    }

    fn complete(
        mut self,
        outcome: LastFmPlaybackCoordinatorOutcome,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let outcome = LastFmActivePlaybackEnvironment::finish_operation_gate(
            &self.operation_gate,
            &self.operation_drained,
            Some(outcome),
        )
        .expect("completed operation always returns an outcome");
        self.completed = true;
        outcome
    }

    fn complete_without_outcome(mut self) {
        let _ = LastFmActivePlaybackEnvironment::finish_operation_gate(
            &self.operation_gate,
            &self.operation_drained,
            None,
        );
        self.completed = true;
    }
}

impl Drop for LastFmActivePlaybackOperation {
    fn drop(&mut self) {
        if !self.completed {
            if let Some(LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::RuntimeCommand(error),
            )) = self.cancellation_failure
            {
                tracing::error!(
                    error = %error,
                    "Last.fm durable enqueue completion supervisor stopped"
                );
            }
            let _ = LastFmActivePlaybackEnvironment::finish_operation_gate(
                &self.operation_gate,
                &self.operation_drained,
                self.cancellation_failure,
            );
        }
    }
}

struct LastFmActivePlaybackRetirementExecutor<'a> {
    environment: &'a LastFmActivePlaybackEnvironment,
    completed: bool,
}

impl LastFmActivePlaybackRetirementExecutor<'_> {
    fn complete(
        mut self,
        outcome: LastFmPlaybackCoordinatorOutcome,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let outcome = self.environment.publish_retirement(outcome);
        self.completed = true;
        outcome
    }
}

impl Drop for LastFmActivePlaybackRetirementExecutor<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let _ = self
            .environment
            .publish_retirement(LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::RetirementGatePoisoned,
            ));
    }
}

/// Private witness which makes playback-owner construction possible only
/// inside this exact process coordinator module.
pub(super) struct LastFmPlaybackOwnerMint(());

impl LastFmPlaybackOwnerMint {
    fn issue() -> Self {
        Self(())
    }
}

impl LastFmActivePlaybackEnvironment {
    fn new(
        runtime: Box<dyn LastFmPlaybackRuntimePort>,
        completion_runtime: tokio::runtime::Handle,
        registry: SourceRegistry,
        enabled_remote_sources: HashSet<SourceId>,
    ) -> Self {
        Self {
            operation_gate: Arc::new(Mutex::new(LastFmActivePlaybackGate {
                live: true,
                in_flight: 0,
                poisoned: false,
                terminal_failure: None,
            })),
            operation_drained: Arc::new(Condvar::new()),
            retirement: Mutex::new(LastFmActivePlaybackRetirement::Pending),
            retirement_completed: Condvar::new(),
            owner: Mutex::new(LastFmPlaybackOwner::new_for_coordinator(
                LastFmPlaybackOwnerMint::issue(),
            )),
            runtime,
            completion_runtime,
            enqueue_completion_slots: Arc::new(tokio::sync::Semaphore::new(
                ENQUEUE_COMPLETION_CAPACITY,
            )),
            registry,
            enabled_remote_sources,
        }
    }

    fn revoke_admission(&self) {
        match self.operation_gate.lock() {
            Ok(mut gate) => gate.live = false,
            Err(poisoned) => {
                let mut gate = poisoned.into_inner();
                gate.live = false;
                gate.poisoned = true;
                gate.terminal_failure = Some(LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                ));
                self.operation_gate.clear_poison();
            }
        }
    }

    fn begin_operation(
        &self,
    ) -> Result<LastFmActivePlaybackOperation, LastFmPlaybackCoordinatorOutcome> {
        let mut gate = match self.operation_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => {
                let mut gate = poisoned.into_inner();
                gate.live = false;
                gate.poisoned = true;
                gate.terminal_failure = Some(LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                ));
                self.operation_gate.clear_poison();
                return Err(gate
                    .terminal_failure
                    .expect("poisoned operation gate has a fixed failure"));
            }
        };
        if gate.poisoned {
            return Err(gate
                .terminal_failure
                .unwrap_or(LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                )));
        }
        if let Some(failure) = gate.terminal_failure {
            return Err(failure);
        }
        if !gate.live {
            return Err(LastFmPlaybackCoordinatorOutcome::StaleActivation);
        }
        let Some(in_flight) = gate.in_flight.checked_add(1) else {
            gate.live = false;
            let failure = LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::OperationCapacityExhausted,
            );
            gate.terminal_failure = Some(failure);
            return Err(failure);
        };
        gate.in_flight = in_flight;
        drop(gate);
        Ok(LastFmActivePlaybackOperation {
            operation_gate: Arc::clone(&self.operation_gate),
            operation_drained: Arc::clone(&self.operation_drained),
            cancellation_failure: None,
            completed: false,
        })
    }

    fn finish_operation_gate(
        operation_gate: &Mutex<LastFmActivePlaybackGate>,
        operation_drained: &Condvar,
        outcome: Option<LastFmPlaybackCoordinatorOutcome>,
    ) -> Option<LastFmPlaybackCoordinatorOutcome> {
        let mut gate = match operation_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => {
                let mut gate = poisoned.into_inner();
                gate.live = false;
                gate.poisoned = true;
                gate.terminal_failure = Some(LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                ));
                operation_gate.clear_poison();
                gate
            }
        };
        if let Some(outcome) = outcome.filter(|outcome| outcome.terminal_environment_failure()) {
            gate.live = false;
            if gate.terminal_failure.is_none() {
                gate.terminal_failure = Some(outcome);
            }
        }
        if gate.in_flight == 0 {
            gate.live = false;
            gate.poisoned = true;
            gate.terminal_failure = Some(LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
            ));
            let outcome = gate.terminal_failure.or(outcome);
            operation_drained.notify_all();
            return outcome;
        }
        gate.in_flight -= 1;
        let outcome = gate.terminal_failure.or(outcome);
        if gate.in_flight == 0 {
            operation_drained.notify_all();
        }
        outcome
    }

    fn wait_for_operation_drain(&self) -> Option<LastFmPlaybackCoordinatorOutcome> {
        let mut gate = match self.operation_gate.lock() {
            Ok(gate) => gate,
            Err(error) => {
                let mut gate = error.into_inner();
                gate.poisoned = true;
                gate.terminal_failure = Some(LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                ));
                self.operation_gate.clear_poison();
                gate
            }
        };
        gate.live = false;
        while gate.in_flight != 0 {
            gate = match self.operation_drained.wait(gate) {
                Ok(gate) => gate,
                Err(error) => {
                    let mut gate = error.into_inner();
                    gate.poisoned = true;
                    gate.terminal_failure = Some(LastFmPlaybackCoordinatorOutcome::Failed(
                        LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                    ));
                    self.operation_gate.clear_poison();
                    gate
                }
            };
            gate.live = false;
        }
        gate.terminal_failure
    }

    fn lock_owner(
        &self,
    ) -> Result<MutexGuard<'_, LastFmPlaybackOwner>, LastFmPlaybackCoordinatorOutcome> {
        let owner = self.owner.lock().map_err(|_| {
            self.revoke_admission();
            LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::PlaybackOwnerPoisoned,
            )
        })?;
        Ok(owner)
    }

    fn dispatch_handoff(
        &self,
        handoff: LastFmPlaybackHandoff,
        parent: &LastFmActivePlaybackOperation,
    ) -> LastFmPlaybackCoordinatorOutcome {
        // Reserve the child drain lease before runtime admission. A concurrent
        // close may revoke new top-level ingress, but work which already owns
        // its parent lease must never admit an enqueue whose durable receipt
        // can then escape the retirement barrier.
        let completion_reservation = if handoff.kind() == LastFmPlaybackHandoffKind::Enqueue {
            match parent.reserve_child() {
                Ok(lease) => match Arc::clone(&self.enqueue_completion_slots).try_acquire_owned() {
                    Ok(slot) => Some((lease, slot)),
                    Err(_) => {
                        lease.complete_without_outcome();
                        drop(handoff);
                        return LastFmPlaybackCoordinatorOutcome::Failed(
                            LastFmPlaybackCoordinatorFailure::OperationCapacityExhausted,
                        );
                    }
                },
                Err(outcome) => {
                    drop(handoff);
                    return outcome;
                }
            }
        } else {
            None
        };
        match self
            .runtime
            .dispatch(handoff, &self.registry, &self.enabled_remote_sources)
        {
            LastFmPlaybackRuntimeDispatch::Immediate(outcome) => {
                if let Some((lease, slot)) = completion_reservation {
                    lease.complete_without_outcome();
                    drop(slot);
                }
                outcome
            }
            LastFmPlaybackRuntimeDispatch::PendingEnqueue(completion) => {
                let Some((lease, slot)) = completion_reservation else {
                    drop(completion);
                    return LastFmPlaybackCoordinatorOutcome::Failed(
                        LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                    );
                };
                let lease = lease.arm_owner_stopped();
                let task = self.completion_runtime.spawn(async move {
                    match completion.await {
                        Ok(()) => lease.complete_without_outcome(),
                        Err(error) => {
                            tracing::error!(
                                error = %error,
                                "Last.fm durable enqueue completion failed"
                            );
                            let _ = lease.complete(LastFmPlaybackCoordinatorOutcome::Failed(
                                LastFmPlaybackCoordinatorFailure::RuntimeCommand(error),
                            ));
                        }
                    }
                    drop(slot);
                });
                // The owned lease makes cancellation fail closed, while the
                // detached task remains bounded by runtime enqueue admission.
                drop(task);
                LastFmPlaybackCoordinatorOutcome::PendingDurability
            }
        }
    }

    fn dispatch_retirement_handoff(
        &self,
        handoff: LastFmPlaybackHandoff,
    ) -> LastFmPlaybackCoordinatorOutcome {
        match self
            .runtime
            .dispatch(handoff, &self.registry, &self.enabled_remote_sources)
        {
            LastFmPlaybackRuntimeDispatch::Immediate(outcome) => outcome,
            LastFmPlaybackRuntimeDispatch::PendingEnqueue(completion) => {
                drop(completion);
                LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
                )
            }
        }
    }

    fn finish_update(
        &self,
        update: LastFmPlaybackOwnerUpdate,
        base: LastFmPlaybackCoordinatorOutcome,
        operation: &LastFmActivePlaybackOperation,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let (handoff, owner_error) = update.into_parts();
        let dispatch = handoff.map(|handoff| self.dispatch_handoff(handoff, operation));
        if let Some(outcome) = dispatch.filter(|outcome| outcome.terminal_environment_failure()) {
            return outcome;
        }
        if let Some(error) = owner_error {
            return LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::Playback(error),
            );
        }
        match dispatch {
            Some(LastFmPlaybackCoordinatorOutcome::Applied) | None => base,
            Some(outcome) => outcome,
        }
    }

    fn observe_output_intent(
        &self,
        intent: LastFmOutputIntent,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let operation = match self.begin_operation() {
            Ok(operation) => operation,
            Err(outcome) => {
                drop(intent);
                return outcome;
            }
        };
        let update = {
            let mut owner = match self.lock_owner() {
                Ok(owner) => owner,
                Err(outcome) => {
                    drop(intent);
                    return operation.complete(outcome);
                }
            };
            owner.observe_output_intent(intent)
        };
        let outcome = self.finish_update(
            update,
            LastFmPlaybackCoordinatorOutcome::Applied,
            &operation,
        );
        operation.complete(outcome)
    }

    fn accept_output_load_admitted(
        &self,
        load: LastFmAcceptedOutputLoad,
        operation: &LastFmActivePlaybackOperation,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let admission = {
            let mut owner = match self.lock_owner() {
                Ok(owner) => owner,
                Err(outcome) => {
                    load.revoke();
                    return outcome;
                }
            };
            owner.accept_output_load(load, &self.registry, &self.enabled_remote_sources)
        };
        let base = if admission.admitted() {
            LastFmPlaybackCoordinatorOutcome::Applied
        } else if admission.stale() {
            LastFmPlaybackCoordinatorOutcome::StaleActivation
        } else {
            LastFmPlaybackCoordinatorOutcome::SourceRejected
        };
        self.finish_update(admission.into_update(), base, operation)
    }

    fn observe_event(&self, event: &PlayerEvent) -> LastFmPlaybackCoordinatorOutcome {
        let operation = match self.begin_operation() {
            Ok(operation) => operation,
            Err(outcome) => return outcome,
        };
        let update = {
            let mut owner = match self.lock_owner() {
                Ok(owner) => owner,
                Err(outcome) => return operation.complete(outcome),
            };
            owner.observe_event(event)
        };
        let outcome = self.finish_update(
            update,
            LastFmPlaybackCoordinatorOutcome::Applied,
            &operation,
        );
        operation.complete(outcome)
    }

    fn observe_discontinuity(
        &self,
        generation: PlayerEventGeneration,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let operation = match self.begin_operation() {
            Ok(operation) => operation,
            Err(outcome) => return outcome,
        };
        let outcome = {
            let mut owner = match self.lock_owner() {
                Ok(owner) => owner,
                Err(outcome) => return operation.complete(outcome),
            };
            let _ = owner.observe_discontinuity(generation);
            LastFmPlaybackCoordinatorOutcome::Applied
        };
        operation.complete(outcome)
    }

    fn revalidate_active_source(&self) -> LastFmPlaybackCoordinatorOutcome {
        let operation = match self.begin_operation() {
            Ok(operation) => operation,
            Err(outcome) => return outcome,
        };
        let update = {
            let mut owner = match self.lock_owner() {
                Ok(owner) => owner,
                Err(outcome) => return operation.complete(outcome),
            };
            owner.revalidate_active_source(&self.registry, &self.enabled_remote_sources)
        };
        let outcome = self.finish_update(
            update,
            LastFmPlaybackCoordinatorOutcome::Applied,
            &operation,
        );
        operation.complete(outcome)
    }

    fn retire(&self) -> LastFmPlaybackCoordinatorOutcome {
        let operation = match self.begin_operation() {
            Ok(operation) => operation,
            Err(outcome) => return outcome,
        };
        let handoff = {
            let mut owner = match self.lock_owner() {
                Ok(owner) => owner,
                Err(outcome) => return operation.complete(outcome),
            };
            owner.retire()
        };
        let outcome = handoff.map_or(LastFmPlaybackCoordinatorOutcome::Applied, |handoff| {
            self.dispatch_retirement_handoff(handoff)
        });
        operation.complete(outcome)
    }

    fn publish_retirement(
        &self,
        outcome: LastFmPlaybackCoordinatorOutcome,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let mut poisoned_state = false;
        let mut retirement = match self.retirement.lock() {
            Ok(retirement) => retirement,
            Err(poisoned) => {
                poisoned_state = true;
                let retirement = poisoned.into_inner();
                self.retirement.clear_poison();
                retirement
            }
        };
        poisoned_state |= matches!(
            *retirement,
            LastFmActivePlaybackRetirement::Running { poisoned: true }
        );
        let outcome = if poisoned_state {
            LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::RetirementGatePoisoned,
            )
        } else {
            outcome
        };
        *retirement = LastFmActivePlaybackRetirement::Complete(outcome);
        drop(retirement);
        self.retirement_completed.notify_all();
        outcome
    }

    fn retire_after_revocation(&self) -> LastFmPlaybackCoordinatorOutcome {
        loop {
            let (mut retirement, observed_poison) = match self.retirement.lock() {
                Ok(retirement) => (retirement, false),
                Err(poisoned) => {
                    let retirement = poisoned.into_inner();
                    self.retirement.clear_poison();
                    (retirement, true)
                }
            };
            match &mut *retirement {
                LastFmActivePlaybackRetirement::Pending => {
                    *retirement = LastFmActivePlaybackRetirement::Running {
                        poisoned: observed_poison,
                    };
                    break;
                }
                LastFmActivePlaybackRetirement::Running { poisoned } => {
                    *poisoned |= observed_poison;
                    let (next, wait_poisoned) = match self.retirement_completed.wait(retirement) {
                        Ok(retirement) => (retirement, false),
                        Err(poisoned) => {
                            let retirement = poisoned.into_inner();
                            self.retirement.clear_poison();
                            (retirement, true)
                        }
                    };
                    retirement = next;
                    if wait_poisoned {
                        if let LastFmActivePlaybackRetirement::Running { poisoned } =
                            &mut *retirement
                        {
                            *poisoned = true;
                        }
                    }
                    if let LastFmActivePlaybackRetirement::Complete(outcome) = *retirement {
                        return outcome;
                    }
                }
                LastFmActivePlaybackRetirement::Complete(outcome) => {
                    return *outcome;
                }
            }
        }
        let executor = LastFmActivePlaybackRetirementExecutor {
            environment: self,
            completed: false,
        };
        let gate_failure = self.wait_for_operation_drain();
        let mut owner_poisoned = false;
        let handoff = match self.owner.lock() {
            Ok(mut owner) => owner.retire(),
            Err(poisoned) => {
                owner_poisoned = true;
                let mut owner = poisoned.into_inner();
                let handoff = owner.retire();
                self.owner.clear_poison();
                handoff
            }
        };
        let dispatch = handoff.map_or(LastFmPlaybackCoordinatorOutcome::Applied, |handoff| {
            self.dispatch_retirement_handoff(handoff)
        });
        let outcome = if let Some(failure) = gate_failure {
            failure
        } else if owner_poisoned {
            LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::PlaybackOwnerPoisoned,
            )
        } else {
            dispatch
        };
        executor.complete(outcome)
    }
}

struct LastFmPlaybackCoordinatorCore {
    state: Mutex<LastFmPlaybackCoordinatorState>,
}

impl LastFmPlaybackCoordinatorCore {
    fn new() -> Self {
        Self {
            state: Mutex::new(LastFmPlaybackCoordinatorState::Dormant {
                window_epoch: 0,
                activation_epoch: 0,
            }),
        }
    }

    fn lock_state(
        &self,
    ) -> Result<MutexGuard<'_, LastFmPlaybackCoordinatorState>, LastFmPlaybackCoordinatorFailure>
    {
        match self.state.lock() {
            Ok(state) => Ok(state),
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                let environment =
                    if let LastFmPlaybackCoordinatorState::Active { environment, .. }
                    | LastFmPlaybackCoordinatorState::Retiring { environment, .. } = &*state
                    {
                        environment.revoke_admission();
                        Some(Arc::clone(environment))
                    } else {
                        None
                    };
                *state = LastFmPlaybackCoordinatorState::Shutdown;
                // Clear poison while the recovery guard is still held. A
                // concurrent binding can then observe only the terminal
                // Shutdown state, never a second transient Poisoned result.
                self.state.clear_poison();
                drop(state);
                if let Some(environment) = environment {
                    let _ = environment.retire_after_revocation();
                }
                Err(LastFmPlaybackCoordinatorFailure::Poisoned)
            }
        }
    }

    fn active_environment(
        &self,
        binding_epoch: u64,
    ) -> Result<Arc<LastFmActivePlaybackEnvironment>, LastFmPlaybackCoordinatorOutcome> {
        let state = self
            .lock_state()
            .map_err(LastFmPlaybackCoordinatorOutcome::Failed)?;
        match &*state {
            LastFmPlaybackCoordinatorState::Active {
                window_epoch,
                environment,
                ..
            } if *window_epoch == binding_epoch => Ok(Arc::clone(environment)),
            LastFmPlaybackCoordinatorState::Dormant { window_epoch, .. }
                if *window_epoch == binding_epoch =>
            {
                Err(LastFmPlaybackCoordinatorOutcome::Dormant)
            }
            LastFmPlaybackCoordinatorState::Retiring { window_epoch, .. }
                if *window_epoch == binding_epoch =>
            {
                Err(LastFmPlaybackCoordinatorOutcome::StaleActivation)
            }
            LastFmPlaybackCoordinatorState::Dormant { .. }
            | LastFmPlaybackCoordinatorState::Active { .. }
            | LastFmPlaybackCoordinatorState::Retiring { .. } => {
                Err(LastFmPlaybackCoordinatorOutcome::StaleWindow)
            }
            LastFmPlaybackCoordinatorState::Shutdown => {
                Err(LastFmPlaybackCoordinatorOutcome::Shutdown)
            }
        }
    }

    fn recheck_environment(
        &self,
        binding_epoch: u64,
        environment: &Arc<LastFmActivePlaybackEnvironment>,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                environment.revoke_admission();
                if let LastFmPlaybackCoordinatorState::Active {
                    environment: current,
                    ..
                }
                | LastFmPlaybackCoordinatorState::Retiring {
                    environment: current,
                    ..
                } = &*state
                {
                    current.revoke_admission();
                }
                *state = LastFmPlaybackCoordinatorState::Shutdown;
                self.state.clear_poison();
                drop(state);
                return LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::Poisoned,
                );
            }
        };
        match &*state {
            LastFmPlaybackCoordinatorState::Active {
                window_epoch,
                environment: current,
                ..
            } if *window_epoch == binding_epoch && Arc::ptr_eq(current, environment) => {
                LastFmPlaybackCoordinatorOutcome::Applied
            }
            LastFmPlaybackCoordinatorState::Active { window_epoch, .. }
                if *window_epoch != binding_epoch =>
            {
                LastFmPlaybackCoordinatorOutcome::StaleWindow
            }
            LastFmPlaybackCoordinatorState::Active { window_epoch, .. }
                if *window_epoch == binding_epoch =>
            {
                LastFmPlaybackCoordinatorOutcome::StaleActivation
            }
            LastFmPlaybackCoordinatorState::Dormant { window_epoch, .. }
                if *window_epoch == binding_epoch =>
            {
                LastFmPlaybackCoordinatorOutcome::Dormant
            }
            LastFmPlaybackCoordinatorState::Retiring { window_epoch, .. }
                if *window_epoch == binding_epoch =>
            {
                LastFmPlaybackCoordinatorOutcome::StaleActivation
            }
            LastFmPlaybackCoordinatorState::Dormant { .. }
            | LastFmPlaybackCoordinatorState::Active { .. }
            | LastFmPlaybackCoordinatorState::Retiring { .. } => {
                LastFmPlaybackCoordinatorOutcome::StaleWindow
            }
            LastFmPlaybackCoordinatorState::Shutdown => LastFmPlaybackCoordinatorOutcome::Shutdown,
        }
    }

    fn fail_active_environment(
        &self,
        binding_epoch: u64,
        environment: &Arc<LastFmActivePlaybackEnvironment>,
        cause: LastFmPlaybackCoordinatorOutcome,
    ) -> LastFmPlaybackCoordinatorOutcome {
        if !cause.terminal_environment_failure() {
            return cause;
        }
        let exact = {
            let Ok(mut state) = self.lock_state() else {
                return LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::Poisoned,
                );
            };
            match &*state {
                LastFmPlaybackCoordinatorState::Active {
                    window_epoch,
                    activation_epoch,
                    environment: current,
                } if *window_epoch == binding_epoch && Arc::ptr_eq(current, environment) => {
                    let activation_epoch = *activation_epoch;
                    environment.revoke_admission();
                    *state = LastFmPlaybackCoordinatorState::Retiring {
                        window_epoch: binding_epoch,
                        activation_epoch,
                        environment: Arc::clone(environment),
                    };
                    true
                }
                LastFmPlaybackCoordinatorState::Retiring {
                    window_epoch,
                    environment: current,
                    ..
                } if *window_epoch == binding_epoch && Arc::ptr_eq(current, environment) => true,
                _ => false,
            }
        };
        environment.revoke_admission();
        let _ = environment.retire_after_revocation();
        if exact {
            let Ok(mut state) = self.lock_state() else {
                return LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::Poisoned,
                );
            };
            match &*state {
                LastFmPlaybackCoordinatorState::Active {
                    window_epoch,
                    environment: current,
                    ..
                }
                | LastFmPlaybackCoordinatorState::Retiring {
                    window_epoch,
                    environment: current,
                    ..
                } if *window_epoch == binding_epoch && Arc::ptr_eq(current, environment) => {
                    *state = LastFmPlaybackCoordinatorState::Shutdown;
                }
                _ => {}
            }
        }
        cause
    }

    fn deactivate_exact(
        &self,
        window_epoch: u64,
        activation_epoch: u64,
        activation_environment: &Arc<LastFmActivePlaybackEnvironment>,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let environment = {
            let Ok(mut state) = self.lock_state() else {
                return LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::Poisoned,
                );
            };
            match &*state {
                LastFmPlaybackCoordinatorState::Active {
                    window_epoch: current_window,
                    activation_epoch: current_activation,
                    environment,
                } if *current_window == window_epoch
                    && *current_activation == activation_epoch
                    && Arc::ptr_eq(environment, activation_environment) =>
                {
                    environment.revoke_admission();
                    let environment = Arc::clone(environment);
                    *state = LastFmPlaybackCoordinatorState::Retiring {
                        window_epoch,
                        activation_epoch,
                        environment: Arc::clone(&environment),
                    };
                    environment
                }
                LastFmPlaybackCoordinatorState::Active {
                    window_epoch: current_window,
                    ..
                } if *current_window != window_epoch => {
                    return LastFmPlaybackCoordinatorOutcome::StaleWindow;
                }
                LastFmPlaybackCoordinatorState::Retiring {
                    window_epoch: current_window,
                    activation_epoch: current_activation,
                    environment,
                } if *current_window == window_epoch
                    && *current_activation == activation_epoch
                    && Arc::ptr_eq(environment, activation_environment) =>
                {
                    environment.revoke_admission();
                    Arc::clone(environment)
                }
                LastFmPlaybackCoordinatorState::Active { .. }
                | LastFmPlaybackCoordinatorState::Retiring { .. } => {
                    return LastFmPlaybackCoordinatorOutcome::StaleActivation;
                }
                LastFmPlaybackCoordinatorState::Dormant {
                    window_epoch: current_window,
                    activation_epoch: current_activation,
                } if *current_window == window_epoch && *current_activation == activation_epoch => {
                    activation_environment.revoke_admission();
                    Arc::clone(activation_environment)
                }
                LastFmPlaybackCoordinatorState::Dormant { .. } => {
                    return LastFmPlaybackCoordinatorOutcome::StaleWindow;
                }
                LastFmPlaybackCoordinatorState::Shutdown => {
                    activation_environment.revoke_admission();
                    Arc::clone(activation_environment)
                }
            }
        };

        let retirement = environment.retire_after_revocation();
        {
            let Ok(mut state) = self.lock_state() else {
                return LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::Poisoned,
                );
            };
            match &*state {
                LastFmPlaybackCoordinatorState::Retiring {
                    window_epoch: current_window,
                    activation_epoch: current_activation,
                    environment: current,
                } if *current_window == window_epoch
                    && *current_activation == activation_epoch
                    && Arc::ptr_eq(current, &environment) =>
                {
                    if retirement == LastFmPlaybackCoordinatorOutcome::Applied {
                        *state = LastFmPlaybackCoordinatorState::Dormant {
                            window_epoch,
                            activation_epoch,
                        };
                        LastFmPlaybackCoordinatorOutcome::Applied
                    } else {
                        *state = LastFmPlaybackCoordinatorState::Shutdown;
                        retirement
                    }
                }
                LastFmPlaybackCoordinatorState::Dormant {
                    window_epoch: current_window,
                    activation_epoch: current_activation,
                } if *current_window == window_epoch && *current_activation == activation_epoch => {
                    retirement
                }
                LastFmPlaybackCoordinatorState::Shutdown => retirement,
                _ => LastFmPlaybackCoordinatorOutcome::StaleActivation,
            }
        }
    }

    fn shutdown(&self) -> LastFmPlaybackCoordinatorOutcome {
        let environment = {
            let Ok(mut state) = self.lock_state() else {
                return LastFmPlaybackCoordinatorOutcome::Failed(
                    LastFmPlaybackCoordinatorFailure::Poisoned,
                );
            };
            let previous = std::mem::replace(&mut *state, LastFmPlaybackCoordinatorState::Shutdown);
            match previous {
                LastFmPlaybackCoordinatorState::Active { environment, .. } => {
                    environment.revoke_admission();
                    Some(environment)
                }
                LastFmPlaybackCoordinatorState::Retiring { environment, .. } => {
                    environment.revoke_admission();
                    Some(environment)
                }
                LastFmPlaybackCoordinatorState::Dormant { .. } => None,
                LastFmPlaybackCoordinatorState::Shutdown => {
                    return LastFmPlaybackCoordinatorOutcome::Shutdown;
                }
            }
        };
        match environment {
            Some(environment) => environment.retire_after_revocation(),
            None => LastFmPlaybackCoordinatorOutcome::Applied,
        }
    }

    fn force_shutdown(&self) {
        let environment = match self.state.lock() {
            Ok(mut state) => {
                let previous =
                    std::mem::replace(&mut *state, LastFmPlaybackCoordinatorState::Shutdown);
                match previous {
                    LastFmPlaybackCoordinatorState::Active { environment, .. } => {
                        environment.revoke_admission();
                        Some(environment)
                    }
                    LastFmPlaybackCoordinatorState::Retiring { environment, .. } => {
                        environment.revoke_admission();
                        Some(environment)
                    }
                    _ => None,
                }
            }
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                let environment =
                    if let LastFmPlaybackCoordinatorState::Active { environment, .. }
                    | LastFmPlaybackCoordinatorState::Retiring { environment, .. } = &*state
                    {
                        environment.revoke_admission();
                        Some(Arc::clone(environment))
                    } else {
                        None
                    };
                *state = LastFmPlaybackCoordinatorState::Shutdown;
                self.state.clear_poison();
                drop(state);
                environment
            }
        };
        if let Some(environment) = environment {
            let _ = environment.retire_after_revocation();
        }
    }
}

/// Unique, non-cloneable process owner of Last.fm playback coordination.
///
/// Dropping this value terminally closes its bindings. The process-global
/// claim remains consumed after drop, so neither a failed first window build
/// nor a later application activation can recreate ownership.
pub(crate) struct LastFmPlaybackCoordinatorOwner {
    core: Arc<LastFmPlaybackCoordinatorCore>,
    next_window_epoch: u64,
    shutdown_started: bool,
}

impl LastFmPlaybackCoordinatorOwner {
    /// Claim the sole coordinator owner for this process.
    pub(crate) fn claim_process() -> Result<Self, LastFmPlaybackCoordinatorClaimError> {
        PROCESS_OWNER_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| LastFmPlaybackCoordinatorClaimError::AlreadyClaimed)?;
        Ok(Self {
            core: Arc::new(LastFmPlaybackCoordinatorCore::new()),
            next_window_epoch: 0,
            shutdown_started: false,
        })
    }

    /// Bind one window environment after its source registry exists.
    ///
    /// Rebinding advances a checked epoch and makes every older binding inert.
    /// The registry is retained only by the returned binding; dormant and
    /// shutdown coordinator states retain no source or remote policy.
    pub(crate) fn bind_window(
        &mut self,
        source_registry: SourceRegistry,
    ) -> Result<LastFmPlaybackCoordinatorBinding, LastFmPlaybackCoordinatorBindError> {
        if self.shutdown_started {
            return Err(LastFmPlaybackCoordinatorBindError::Shutdown);
        }
        loop {
            let retirement = {
                let Ok(mut state) = self.core.lock_state() else {
                    self.shutdown_started = true;
                    return Err(LastFmPlaybackCoordinatorBindError::Poisoned);
                };
                match &*state {
                    LastFmPlaybackCoordinatorState::Dormant {
                        activation_epoch, ..
                    } => {
                        let activation_epoch = *activation_epoch;
                        let Some(window_epoch) = self.next_window_epoch.checked_add(1) else {
                            *state = LastFmPlaybackCoordinatorState::Shutdown;
                            self.shutdown_started = true;
                            return Err(LastFmPlaybackCoordinatorBindError::WindowEpochExhausted);
                        };
                        self.next_window_epoch = window_epoch;
                        *state = LastFmPlaybackCoordinatorState::Dormant {
                            window_epoch,
                            activation_epoch,
                        };
                        return Ok(LastFmPlaybackCoordinatorBinding {
                            core: Arc::clone(&self.core),
                            source_registry,
                            window_epoch,
                        });
                    }
                    LastFmPlaybackCoordinatorState::Active {
                        window_epoch,
                        activation_epoch,
                        environment,
                    } => {
                        let window_epoch = *window_epoch;
                        let activation_epoch = *activation_epoch;
                        environment.revoke_admission();
                        let environment = Arc::clone(environment);
                        *state = LastFmPlaybackCoordinatorState::Retiring {
                            window_epoch,
                            activation_epoch,
                            environment: Arc::clone(&environment),
                        };
                        Some((window_epoch, activation_epoch, environment))
                    }
                    LastFmPlaybackCoordinatorState::Retiring {
                        window_epoch,
                        activation_epoch,
                        environment,
                    } => Some((*window_epoch, *activation_epoch, Arc::clone(environment))),
                    LastFmPlaybackCoordinatorState::Shutdown => {
                        self.shutdown_started = true;
                        return Err(LastFmPlaybackCoordinatorBindError::Shutdown);
                    }
                }
            };

            let Some((window_epoch, activation_epoch, environment)) = retirement else {
                unreachable!("dormant binding returns before retirement")
            };
            let retirement_outcome = environment.retire_after_revocation();
            let Ok(mut state) = self.core.lock_state() else {
                self.shutdown_started = true;
                return Err(LastFmPlaybackCoordinatorBindError::Poisoned);
            };
            if retirement_outcome != LastFmPlaybackCoordinatorOutcome::Applied {
                *state = LastFmPlaybackCoordinatorState::Shutdown;
                self.shutdown_started = true;
                return Err(LastFmPlaybackCoordinatorBindError::RetirementFailed);
            }
            match &*state {
                LastFmPlaybackCoordinatorState::Retiring {
                    window_epoch: current_window,
                    activation_epoch: current_activation,
                    ..
                } if *current_window == window_epoch && *current_activation == activation_epoch => {
                    *state = LastFmPlaybackCoordinatorState::Dormant {
                        window_epoch,
                        activation_epoch,
                    };
                }
                LastFmPlaybackCoordinatorState::Dormant {
                    window_epoch: current_window,
                    activation_epoch: current_activation,
                } if *current_window == window_epoch && *current_activation == activation_epoch => {
                    // Another exact retirement owner completed while this
                    // rebind waited on the serialized drain. Continue and
                    // install the successor window from that Dormant state.
                }
                LastFmPlaybackCoordinatorState::Shutdown => {
                    self.shutdown_started = true;
                    return Err(LastFmPlaybackCoordinatorBindError::Shutdown);
                }
                _ => {}
            }
        }
    }

    /// Terminally close process-lifetime coordinator admission.
    pub(crate) fn shutdown(&mut self) -> LastFmPlaybackCoordinatorOutcome {
        self.shutdown_started = true;
        self.core.shutdown()
    }

    #[cfg(test)]
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn isolated_for_test() -> Self {
        Self {
            core: Arc::new(LastFmPlaybackCoordinatorCore::new()),
            next_window_epoch: 0,
            shutdown_started: false,
        }
    }
}

impl Drop for LastFmPlaybackCoordinatorOwner {
    fn drop(&mut self) {
        self.shutdown_started = true;
        self.core.force_shutdown();
    }
}

impl fmt::Debug for LastFmPlaybackCoordinatorOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPlaybackCoordinatorOwner(<redacted>)")
    }
}

/// Cloneable, epoch-bound window ingress for Last.fm playback coordination.
///
/// The source registry is the future active environment for synchronous
/// attribution revalidation. It is never copied into dormant/shutdown state,
/// and no method holds the coordinator mutex while invoking caller code.
#[derive(Clone)]
pub(crate) struct LastFmPlaybackCoordinatorBinding {
    core: Arc<LastFmPlaybackCoordinatorCore>,
    source_registry: SourceRegistry,
    window_epoch: u64,
}

impl LastFmPlaybackCoordinatorBinding {
    fn finish_environment_outcome(
        &self,
        environment: &Arc<LastFmActivePlaybackEnvironment>,
        outcome: LastFmPlaybackCoordinatorOutcome,
    ) -> LastFmPlaybackCoordinatorOutcome {
        if outcome
            == LastFmPlaybackCoordinatorOutcome::Failed(LastFmPlaybackCoordinatorFailure::Poisoned)
        {
            environment.revoke_admission();
            let _ = environment.retire_after_revocation();
            return outcome;
        }
        self.core
            .fail_active_environment(self.window_epoch, environment, outcome)
    }

    /// Activate the sealed headless bridge with playback-only runtime
    /// authority which has already been claimed from a successfully started
    /// Last.fm runtime. The supplied executor must remain independently
    /// driven while GTK callers synchronously wait in close, rebind, or
    /// shutdown; those waits must never run on its only worker. No production
    /// caller issues this activation yet.
    pub(crate) fn activate(
        &self,
        runtime: LastFmPlaybackRuntimeIngress,
        completion_runtime: tokio::runtime::Handle,
        enabled_remote_sources: HashSet<SourceId>,
    ) -> Result<LastFmPlaybackCoordinatorActivation, LastFmPlaybackCoordinatorActivationError> {
        self.activate_with_runtime_port(
            Box::new(runtime),
            completion_runtime,
            enabled_remote_sources,
        )
    }

    fn activate_with_runtime_port(
        &self,
        runtime: Box<dyn LastFmPlaybackRuntimePort>,
        completion_runtime: tokio::runtime::Handle,
        enabled_remote_sources: HashSet<SourceId>,
    ) -> Result<LastFmPlaybackCoordinatorActivation, LastFmPlaybackCoordinatorActivationError> {
        let environment = Arc::new(LastFmActivePlaybackEnvironment::new(
            runtime,
            completion_runtime,
            self.source_registry.clone(),
            enabled_remote_sources,
        ));
        let mut state = self
            .core
            .lock_state()
            .map_err(|_| LastFmPlaybackCoordinatorActivationError::Poisoned)?;
        let activation_epoch = match &*state {
            LastFmPlaybackCoordinatorState::Dormant {
                window_epoch,
                activation_epoch,
            } if *window_epoch == self.window_epoch => {
                activation_epoch.checked_add(1).ok_or_else(|| {
                    *state = LastFmPlaybackCoordinatorState::Shutdown;
                    LastFmPlaybackCoordinatorActivationError::ActivationEpochExhausted
                })?
            }
            LastFmPlaybackCoordinatorState::Dormant { .. } => {
                return Err(LastFmPlaybackCoordinatorActivationError::StaleWindow);
            }
            LastFmPlaybackCoordinatorState::Active { window_epoch, .. }
                if *window_epoch == self.window_epoch =>
            {
                return Err(LastFmPlaybackCoordinatorActivationError::AlreadyActive);
            }
            LastFmPlaybackCoordinatorState::Active { .. } => {
                return Err(LastFmPlaybackCoordinatorActivationError::StaleWindow);
            }
            LastFmPlaybackCoordinatorState::Retiring { window_epoch, .. }
                if *window_epoch == self.window_epoch =>
            {
                return Err(LastFmPlaybackCoordinatorActivationError::Retiring);
            }
            LastFmPlaybackCoordinatorState::Retiring { .. } => {
                return Err(LastFmPlaybackCoordinatorActivationError::StaleWindow);
            }
            LastFmPlaybackCoordinatorState::Shutdown => {
                return Err(LastFmPlaybackCoordinatorActivationError::Shutdown);
            }
        };
        *state = LastFmPlaybackCoordinatorState::Active {
            window_epoch: self.window_epoch,
            activation_epoch,
            environment: Arc::clone(&environment),
        };
        drop(state);
        Ok(LastFmPlaybackCoordinatorActivation {
            core: Arc::clone(&self.core),
            window_epoch: self.window_epoch,
            activation_epoch,
            environment,
            closed: false,
        })
    }

    /// Consume an output transition intent before Stop/Load touches output.
    pub(crate) fn observe_output_intent(
        &self,
        intent: LastFmOutputIntent,
    ) -> LastFmPlaybackCoordinatorOutcome {
        match self.core.active_environment(self.window_epoch) {
            Ok(environment) => {
                let outcome = environment.observe_output_intent(intent);
                self.finish_environment_outcome(&environment, outcome)
            }
            Err(outcome) => {
                drop(intent);
                outcome
            }
        }
    }

    /// Consume one accepted generation without constructing metadata while
    /// the coordinator is dormant, stale, failed, or shut down.
    ///
    /// The metadata-bearing extractor and the metadata-free exact discard are
    /// separate closures by design. This dormant implementation always drops
    /// `build` uncalled and then invokes `discard` exactly once. The state lock
    /// is released before either closure is dropped or invoked. An admitted
    /// `build` executes inside the lifecycle drain barrier and therefore must
    /// be bounded and must not synchronously call activation close, owner
    /// rebind/shutdown, or another API which waits for that same barrier.
    pub(crate) fn accept_output_load_lazy<Build, Discard>(
        &self,
        _generation: PlayerEventGeneration,
        build: Build,
        discard: Discard,
    ) -> LastFmPlaybackCoordinatorOutcome
    where
        Build: FnOnce() -> Option<LastFmAcceptedOutputLoad>,
        Discard: FnOnce(),
    {
        let environment = match self.core.active_environment(self.window_epoch) {
            Ok(environment) => environment,
            Err(outcome) => {
                drop(build);
                discard();
                return outcome;
            }
        };
        let operation = match environment.begin_operation() {
            Ok(operation) => operation,
            Err(outcome) => {
                drop(build);
                discard();
                return self.finish_environment_outcome(&environment, outcome);
            }
        };
        drop(discard);
        let load = build();
        let Some(load) = load else {
            let outcome = self
                .core
                .recheck_environment(self.window_epoch, &environment);
            let outcome = operation.complete(outcome);
            return self.finish_environment_outcome(&environment, outcome);
        };
        let recheck = self
            .core
            .recheck_environment(self.window_epoch, &environment);
        if recheck != LastFmPlaybackCoordinatorOutcome::Applied {
            load.revoke();
            let outcome = operation.complete(recheck);
            return self.finish_environment_outcome(&environment, outcome);
        }
        let outcome = environment.accept_output_load_admitted(load, &operation);
        let outcome = operation.complete(outcome);
        self.finish_environment_outcome(&environment, outcome)
    }

    /// Observe an already generation-gated output event.
    pub(crate) fn observe_event(&self, event: &PlayerEvent) -> LastFmPlaybackCoordinatorOutcome {
        match self.core.active_environment(self.window_epoch) {
            Ok(environment) => {
                let outcome = environment.observe_event(event);
                self.finish_environment_outcome(&environment, outcome)
            }
            Err(outcome) => outcome,
        }
    }

    /// Re-anchor evidence after a seek, restart, or same-output resume.
    pub(crate) fn observe_discontinuity(
        &self,
        generation: PlayerEventGeneration,
    ) -> LastFmPlaybackCoordinatorOutcome {
        match self.core.active_environment(self.window_epoch) {
            Ok(environment) => {
                let outcome = environment.observe_discontinuity(generation);
                self.finish_environment_outcome(&environment, outcome)
            }
            Err(outcome) => outcome,
        }
    }

    /// Revalidate the active occurrence against this window's registry.
    pub(crate) fn revalidate_active_authority(&self) -> LastFmPlaybackCoordinatorOutcome {
        match self.core.active_environment(self.window_epoch) {
            Ok(environment) => {
                let outcome = environment.revalidate_active_source();
                self.finish_environment_outcome(&environment, outcome)
            }
            Err(outcome) => outcome,
        }
    }

    /// Unconditionally retire the active occurrence for a typed global cause.
    pub(crate) fn retire(
        &self,
        _reason: LastFmPlaybackRetirement,
    ) -> LastFmPlaybackCoordinatorOutcome {
        match self.core.active_environment(self.window_epoch) {
            Ok(environment) => {
                let outcome = environment.retire();
                self.finish_environment_outcome(&environment, outcome)
            }
            Err(outcome) => outcome,
        }
    }
}

/// Non-cloneable lease for one exact headless playback activation.
///
/// Explicit close or drop synchronously revokes admission, retires the owner,
/// dispatches any required clear, and only then exposes Dormant for a
/// successor activation.
pub(crate) struct LastFmPlaybackCoordinatorActivation {
    core: Arc<LastFmPlaybackCoordinatorCore>,
    window_epoch: u64,
    activation_epoch: u64,
    environment: Arc<LastFmActivePlaybackEnvironment>,
    closed: bool,
}

impl LastFmPlaybackCoordinatorActivation {
    pub(crate) fn close(mut self) -> LastFmPlaybackCoordinatorOutcome {
        self.closed = true;
        self.core
            .deactivate_exact(self.window_epoch, self.activation_epoch, &self.environment)
    }
}

impl Drop for LastFmPlaybackCoordinatorActivation {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ =
            self.core
                .deactivate_exact(self.window_epoch, self.activation_epoch, &self.environment);
    }
}

impl fmt::Debug for LastFmPlaybackCoordinatorActivation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPlaybackCoordinatorActivation(<redacted>)")
    }
}

impl fmt::Debug for LastFmPlaybackCoordinatorBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPlaybackCoordinatorBinding(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::fs::File;
    use std::future::pending;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{mpsc, OnceLock};
    use std::thread;
    use std::time::Duration;

    use async_trait::async_trait;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    use crate::architecture::{MediaKey, TrackId};
    use crate::audio::PlayerState;
    use crate::db::migration::Migrator;
    use crate::external_file::ExternalFileHint;
    use crate::lastfm::client::{
        LastFmClientError, LastFmTrack, Scrobble, ScrobbleBatchResult, SubmissionResult,
    };
    use crate::lastfm::credentials::{
        CredentialError, ProtectedString, SessionCredentialStore, StoredSession,
    };
    use crate::lastfm::delivery::{LastFmClock, LastFmDeliveryPrimitiveError, LastFmTransport};
    use crate::lastfm::playback_owner::{
        LastFmAcceptedOutputFreshness, LastFmAcceptedPlayback, LastFmPlaybackHandoffKind,
        LastFmPlaybackOccurrenceIdentity, LastFmPlaybackSource,
    };
    use crate::lastfm::runtime::{
        spawn_lastfm_runtime, LastFmRuntimeActivation, LastFmRuntimeShutdownReason,
    };
    use crate::lastfm::storage;
    use crate::source_registry::PlaybackSourceReference;
    use crate::ui::playback::LastFmAcceptedOutputMint;

    use super::*;

    type LockProbe = Arc<dyn Fn() + Send + Sync>;

    const TEST_SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";

    struct PendingRuntimeTransport;

    #[async_trait]
    impl LastFmTransport for PendingRuntimeTransport {
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

    struct FixedRuntimeClock;

    #[async_trait]
    impl LastFmClock for FixedRuntimeClock {
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

    struct RuntimeCredentialStore(Mutex<Option<StoredSession>>);

    impl SessionCredentialStore for RuntimeCredentialStore {
        fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
            self.0
                .lock()
                .map(|session| session.clone())
                .map_err(|_| CredentialError::Unavailable)
        }

        fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
            *self.0.lock().map_err(|_| CredentialError::Unavailable)? = Some(session.clone());
            Ok(())
        }

        fn delete(&self) -> Result<(), CredentialError> {
            *self.0.lock().map_err(|_| CredentialError::Unavailable)? = None;
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingRuntimePort {
        calls: Arc<Mutex<Vec<LastFmPlaybackHandoffKind>>>,
        scripted: Arc<Mutex<VecDeque<LastFmPlaybackRuntimeDispatch>>>,
        rejected_kind: Arc<Mutex<Option<LastFmPlaybackHandoffKind>>>,
        lock_probe: Arc<Mutex<Option<LockProbe>>>,
        dispatch_barrier: Arc<(Mutex<DispatchBarrier>, Condvar)>,
    }

    #[derive(Default)]
    struct DispatchBarrier {
        blocked_kind: Option<LastFmPlaybackHandoffKind>,
        entered: bool,
        released: bool,
    }

    impl RecordingRuntimePort {
        fn calls(&self) -> Vec<LastFmPlaybackHandoffKind> {
            self.calls.lock().expect("lock recorded calls").clone()
        }

        fn script(&self, outcome: LastFmPlaybackCoordinatorOutcome) {
            self.scripted
                .lock()
                .expect("lock scripted outcomes")
                .push_back(LastFmPlaybackRuntimeDispatch::Immediate(outcome));
        }

        fn script_pending_enqueue(
            &self,
        ) -> tokio::sync::oneshot::Sender<Result<(), LastFmRuntimeCommandError>> {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            self.scripted
                .lock()
                .expect("lock scripted outcomes")
                .push_back(LastFmPlaybackRuntimeDispatch::PendingEnqueue(Box::pin(
                    async move {
                        receiver
                            .await
                            .unwrap_or(Err(LastFmRuntimeCommandError::OwnerStopped))
                    },
                )));
            sender
        }

        fn reject_next(&self, kind: LastFmPlaybackHandoffKind) {
            *self.rejected_kind.lock().expect("lock scripted rejection") = Some(kind);
        }

        fn set_lock_probe(&self, probe: LockProbe) {
            *self.lock_probe.lock().expect("lock probe") = Some(probe);
        }

        fn block_clear_dispatch(&self) {
            self.block_dispatch(LastFmPlaybackHandoffKind::ClearNowPlaying);
        }

        fn block_now_playing_dispatch(&self) {
            self.block_dispatch(LastFmPlaybackHandoffKind::NowPlaying);
        }

        fn block_dispatch(&self, kind: LastFmPlaybackHandoffKind) {
            let (barrier, _) = &*self.dispatch_barrier;
            *barrier.lock().expect("lock dispatch barrier") = DispatchBarrier {
                blocked_kind: Some(kind),
                entered: false,
                released: false,
            };
        }

        fn wait_for_blocked_dispatch(&self) {
            let (barrier, changed) = &*self.dispatch_barrier;
            let mut barrier = barrier.lock().expect("lock dispatch barrier");
            while !barrier.entered {
                barrier = changed.wait(barrier).expect("wait for blocked dispatch");
            }
        }

        fn release_blocked_dispatch(&self) {
            let (barrier, changed) = &*self.dispatch_barrier;
            let mut barrier = barrier.lock().expect("lock dispatch barrier");
            barrier.released = true;
            changed.notify_all();
        }
    }

    impl LastFmPlaybackRuntimePort for RecordingRuntimePort {
        fn dispatch(
            &self,
            handoff: LastFmPlaybackHandoff,
            registry: &SourceRegistry,
            enabled_remote_sources: &HashSet<SourceId>,
        ) -> LastFmPlaybackRuntimeDispatch {
            let kind = handoff.kind();
            let rejected = {
                let mut rejected_kind = self.rejected_kind.lock().expect("lock scripted rejection");
                if *rejected_kind == Some(kind) {
                    rejected_kind.take();
                    true
                } else {
                    false
                }
            };
            if rejected {
                drop(handoff);
                return LastFmPlaybackRuntimeDispatch::Immediate(
                    LastFmPlaybackCoordinatorOutcome::SourceRejected,
                );
            }
            let admitted = handoff.try_admit_with_callbacks_for_test(
                registry,
                enabled_remote_sources,
                |_| LastFmPlaybackHandoffKind::NowPlaying,
                |_| LastFmPlaybackHandoffKind::Enqueue,
                || LastFmPlaybackHandoffKind::ClearNowPlaying,
            );
            let Some(kind) = admitted else {
                return LastFmPlaybackRuntimeDispatch::Immediate(
                    LastFmPlaybackCoordinatorOutcome::SourceRejected,
                );
            };
            self.calls.lock().expect("lock recorded calls").push(kind);
            let probe = self.lock_probe.lock().expect("lock probe").clone();
            if let Some(probe) = probe {
                probe();
            }
            {
                let (barrier, changed) = &*self.dispatch_barrier;
                let mut barrier = barrier.lock().expect("lock dispatch barrier");
                if barrier.blocked_kind == Some(kind) {
                    barrier.entered = true;
                    changed.notify_all();
                    while !barrier.released {
                        barrier = changed.wait(barrier).expect("wait to release dispatch");
                    }
                }
            }
            self.scripted
                .lock()
                .expect("lock scripted outcomes")
                .pop_front()
                .unwrap_or(LastFmPlaybackRuntimeDispatch::Immediate(
                    LastFmPlaybackCoordinatorOutcome::Applied,
                ))
        }
    }

    fn registry() -> (tokio::runtime::Runtime, SourceRegistry) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build coordinator test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        (runtime, registry)
    }

    fn test_completion_runtime() -> tokio::runtime::Handle {
        static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .expect("build coordinator completion runtime")
            })
            .handle()
            .clone()
    }

    fn accepted_local_load(
        generation: PlayerEventGeneration,
        track_id: &str,
    ) -> LastFmAcceptedOutputLoad {
        accepted_local_load_with_identity(
            generation,
            track_id,
            LastFmPlaybackOccurrenceIdentity::fresh(),
        )
    }

    fn accepted_local_load_with_identity(
        generation: PlayerEventGeneration,
        track_id: &str,
        identity: LastFmPlaybackOccurrenceIdentity,
    ) -> LastFmAcceptedOutputLoad {
        let source = LastFmPlaybackSource::local(MediaKey::new(
            SourceId::local(),
            TrackId::new(track_id).expect("valid local track id"),
        ))
        .expect("local playback source");
        let accepted = LastFmAcceptedPlayback::try_new(
            identity,
            source,
            "artist-private".to_owned(),
            "title-private".to_owned(),
            Some("album-private".to_owned()),
            Some("album-artist-private".to_owned()),
            Some(7),
            Some(100),
        )
        .expect("valid local accepted playback");
        LastFmAcceptedOutputLoad::eligible(
            LastFmAcceptedOutputMint::for_test(),
            generation,
            LastFmAcceptedOutputFreshness::fresh(),
            accepted,
        )
    }

    fn rejected_remote_load(
        generation: PlayerEventGeneration,
        source_id: SourceId,
    ) -> LastFmAcceptedOutputLoad {
        let source = LastFmPlaybackSource::managed(
            PlaybackSourceReference::session(
                MediaKey::new(
                    source_id,
                    TrackId::remote("remote-private").expect("valid remote track id"),
                ),
                7,
            )
            .expect("test managed reference"),
        );
        let accepted = LastFmAcceptedPlayback::try_new(
            LastFmPlaybackOccurrenceIdentity::fresh(),
            source,
            "artist-private".to_owned(),
            "title-private".to_owned(),
            Some("album-private".to_owned()),
            Some("album-artist-private".to_owned()),
            Some(7),
            Some(100),
        )
        .expect("valid remote accepted playback");
        LastFmAcceptedOutputLoad::eligible(
            LastFmAcceptedOutputMint::for_test(),
            generation,
            LastFmAcceptedOutputFreshness::fresh(),
            accepted,
        )
    }

    fn accepted_managed_load(
        generation: PlayerEventGeneration,
        reference: PlaybackSourceReference,
    ) -> LastFmAcceptedOutputLoad {
        let profile = reference.profile();
        let artist = profile.artist().to_owned();
        let title = profile.title().to_owned();
        let album = profile.album().map(str::to_owned);
        let album_artist = profile.album_artist().map(str::to_owned);
        let track_number = profile.track_number();
        let duration_secs = profile.duration_secs();
        let accepted = LastFmAcceptedPlayback::try_new(
            LastFmPlaybackOccurrenceIdentity::fresh(),
            LastFmPlaybackSource::managed(reference),
            artist,
            title,
            album,
            album_artist,
            track_number,
            duration_secs,
        )
        .expect("exact managed profile is valid playback metadata");
        LastFmAcceptedOutputLoad::eligible(
            LastFmAcceptedOutputMint::for_test(),
            generation,
            LastFmAcceptedOutputFreshness::fresh(),
            accepted,
        )
    }

    fn append_riff_info_field(info: &mut Vec<u8>, id: &[u8; 4], value: &str) {
        info.extend_from_slice(id);
        let size = u32::try_from(value.len() + 1).expect("bounded RIFF fixture field");
        info.extend_from_slice(&size.to_le_bytes());
        info.extend_from_slice(value.as_bytes());
        info.push(0);
        if size % 2 != 0 {
            info.push(0);
        }
    }

    fn tagged_wav_bytes() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 8_000;
        const DURATION_SECONDS: u32 = 31;
        let data_size = SAMPLE_RATE * DURATION_SECONDS;
        let mut info = b"INFO".to_vec();
        append_riff_info_field(&mut info, b"INAM", "Managed Fixture Title");
        append_riff_info_field(&mut info, b"IART", "Managed Fixture Artist");

        let mut bytes = Vec::with_capacity(data_size as usize + info.len() + 64);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.resize(bytes.len() + data_size as usize, 0x80);
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(
            &u32::try_from(info.len())
                .expect("bounded RIFF INFO fixture")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&info);
        let riff_size = u32::try_from(bytes.len() - 8).expect("bounded WAV fixture");
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        bytes
    }

    fn activate_for_test(
        binding: &LastFmPlaybackCoordinatorBinding,
        port: &RecordingRuntimePort,
        enabled_remote_sources: HashSet<SourceId>,
    ) -> LastFmPlaybackCoordinatorActivation {
        binding
            .activate_with_runtime_port(
                Box::new(port.clone()),
                test_completion_runtime(),
                enabled_remote_sources,
            )
            .expect("activate test playback bridge")
    }

    fn prime_local_scrobble(
        binding: &LastFmPlaybackCoordinatorBinding,
        generation: PlayerEventGeneration,
        track_id: &str,
    ) {
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || Some(accepted_local_load(generation, track_id)),
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 1_000, 100_000)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
    }

    #[test]
    fn process_claim_is_exactly_once_even_after_owner_drop() {
        let owner = LastFmPlaybackCoordinatorOwner::claim_process()
            .expect("first process claim must succeed");
        drop(owner);
        assert!(matches!(
            LastFmPlaybackCoordinatorOwner::claim_process(),
            Err(LastFmPlaybackCoordinatorClaimError::AlreadyClaimed)
        ));
    }

    #[test]
    fn dormant_binding_never_invokes_metadata_extractor_and_discards_once() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind dormant owner");
        let discarded = Cell::new(0);
        let core = Arc::clone(&binding.core);

        let outcome = binding.accept_output_load_lazy(
            PlayerEventGeneration::from_raw(7),
            || -> Option<LastFmAcceptedOutputLoad> {
                panic!("dormant coordinator must not extract playback metadata")
            },
            || {
                assert!(
                    core.state.try_lock().is_ok(),
                    "coordinator lock must be released before exact discard"
                );
                discarded.set(discarded.get() + 1);
            },
        );

        assert_eq!(outcome, LastFmPlaybackCoordinatorOutcome::Dormant);
        assert_eq!(discarded.get(), 1);
    }

    #[test]
    fn rebinding_makes_old_window_inert_without_moving_registry_into_state() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let old = owner
            .bind_window(registry.clone())
            .expect("bind first window");
        let current = owner.bind_window(registry).expect("bind second window");

        assert_eq!(
            old.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::StaleWindow
        );
        assert_eq!(
            current.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
    }

    #[test]
    fn stale_and_shutdown_bindings_still_discard_without_extracting() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let stale = owner
            .bind_window(registry.clone())
            .expect("bind first window");
        let current = owner.bind_window(registry).expect("bind second window");
        let stale_discarded = Cell::new(0);

        assert_eq!(
            stale.accept_output_load_lazy(
                PlayerEventGeneration::from_raw(3),
                || -> Option<LastFmAcceptedOutputLoad> {
                    panic!("stale binding must not extract playback metadata")
                },
                || stale_discarded.set(stale_discarded.get() + 1),
            ),
            LastFmPlaybackCoordinatorOutcome::StaleWindow
        );
        assert_eq!(stale_discarded.get(), 1);

        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Applied);
        let shutdown_discarded = Cell::new(0);
        assert_eq!(
            current.accept_output_load_lazy(
                PlayerEventGeneration::from_raw(4),
                || -> Option<LastFmAcceptedOutputLoad> {
                    panic!("shutdown binding must not extract playback metadata")
                },
                || shutdown_discarded.set(shutdown_discarded.get() + 1),
            ),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(shutdown_discarded.get(), 1);
    }

    #[test]
    fn window_epoch_exhaustion_terminally_closes_owner() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        owner.next_window_epoch = u64::MAX;

        assert!(matches!(
            owner.bind_window(registry),
            Err(LastFmPlaybackCoordinatorBindError::WindowEpochExhausted)
        ));
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Shutdown);
    }

    #[test]
    fn all_dormant_non_metadata_ingress_is_inert() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind dormant owner");
        let generation = PlayerEventGeneration::from_raw(11);
        let event = PlayerEvent::state(generation, crate::audio::PlayerState::Playing);
        let intent =
            LastFmOutputIntent::for_test(PlayerEventGeneration::from_raw(10), generation, None);

        assert_eq!(
            binding.observe_output_intent(intent),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
        assert_eq!(
            binding.observe_event(&event),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
        assert_eq!(
            binding.observe_discontinuity(generation),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
        for reason in [
            LastFmPlaybackRetirement::Stop,
            LastFmPlaybackRetirement::SourceRetirement,
            LastFmPlaybackRetirement::OutputReplacement,
            LastFmPlaybackRetirement::QueueAbandoned,
            LastFmPlaybackRetirement::Terminal,
        ] {
            assert_eq!(
                binding.retire(reason),
                LastFmPlaybackCoordinatorOutcome::Dormant
            );
        }
    }

    #[test]
    fn sealed_activation_is_exact_window_scoped_and_reusable_only_after_retirement() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry.clone()).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());

        assert!(matches!(
            binding.activate_with_runtime_port(
                Box::new(RecordingRuntimePort::default()),
                test_completion_runtime(),
                HashSet::new(),
            ),
            Err(LastFmPlaybackCoordinatorActivationError::AlreadyActive)
        ));
        assert_eq!(
            format!("{activation:?}"),
            "LastFmPlaybackCoordinatorActivation(<redacted>)"
        );
        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );

        let successor = activate_for_test(&binding, &port, HashSet::new());
        drop(successor);
        let current = owner.bind_window(registry).expect("rebind window");
        assert!(matches!(
            binding.activate_with_runtime_port(
                Box::new(port.clone()),
                test_completion_runtime(),
                HashSet::new(),
            ),
            Err(LastFmPlaybackCoordinatorActivationError::StaleWindow)
        ));
        assert_eq!(
            current.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
    }

    #[test]
    fn active_bridge_extracts_once_dispatches_in_order_and_releases_locks() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let environment = binding
            .core
            .active_environment(binding.window_epoch)
            .expect("active environment");
        let core = Arc::downgrade(&binding.core);
        let active = Arc::downgrade(&environment);
        port.set_lock_probe(Arc::new(move || {
            let core = core.upgrade().expect("live coordinator core");
            let active = active.upgrade().expect("live active environment");
            assert!(
                core.state.try_lock().is_ok(),
                "coordinator lock must be free before runtime dispatch"
            );
            assert!(
                active.owner.try_lock().is_ok(),
                "playback-owner lock must be free before runtime dispatch"
            );
        }));

        let generation = PlayerEventGeneration::from_raw(20);
        let extracted = Cell::new(0);
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || {
                    extracted.set(extracted.get() + 1);
                    Some(accepted_local_load(generation, "active-private"))
                },
                || panic!("active bridge must not invoke metadata-free discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(extracted.get(), 1);
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 1_000, 100_000)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 51_000, 100_000)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.retire(LastFmPlaybackRetirement::Stop),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::Enqueue,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, u64::MAX, u64::MAX)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(port.calls().len(), 3);
        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
    }

    #[test]
    fn lazy_build_losing_activation_is_revoked_without_dispatch_or_discard() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let (start_close, close_requested) = mpsc::channel();
        let closer = thread::spawn(move || {
            close_requested.recv().expect("builder requests close");
            activation.close()
        });
        let generation = PlayerEventGeneration::from_raw(30);
        let extracted = Cell::new(0);
        let discarded = Cell::new(0);
        let core = Arc::clone(&binding.core);

        let outcome = binding.accept_output_load_lazy(
            generation,
            || {
                extracted.set(extracted.get() + 1);
                start_close.send(()).expect("start concurrent close");
                loop {
                    let retiring = matches!(
                        &*core.state.lock().expect("inspect coordinator state"),
                        LastFmPlaybackCoordinatorState::Retiring { .. }
                            | LastFmPlaybackCoordinatorState::Shutdown
                    );
                    if retiring {
                        break;
                    }
                    thread::yield_now();
                }
                Some(accepted_local_load(generation, "raced-private"))
            },
            || discarded.set(discarded.get() + 1),
        );

        assert_eq!(outcome, LastFmPlaybackCoordinatorOutcome::StaleActivation);
        assert_eq!(
            closer.join().expect("join concurrent close"),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(extracted.get(), 1);
        assert_eq!(discarded.get(), 0);
        assert!(port.calls().is_empty());
    }

    #[test]
    fn fixed_source_and_runtime_rejections_do_not_reopen_one_shot_latches() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let remote_generation = PlayerEventGeneration::from_raw(40);
        assert_eq!(
            binding.accept_output_load_lazy(
                remote_generation,
                || Some(rejected_remote_load(remote_generation, SourceId::random())),
                || panic!("active bridge must not discard before extraction"),
            ),
            LastFmPlaybackCoordinatorOutcome::SourceRejected
        );
        assert!(port.calls().is_empty());

        let local_generation = PlayerEventGeneration::from_raw(41);
        assert_eq!(
            binding.accept_output_load_lazy(
                local_generation,
                || Some(accepted_local_load(
                    local_generation,
                    "runtime-busy-private"
                )),
                || panic!("active bridge must not discard before extraction"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        port.script(LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::Runtime(LastFmRuntimeAdmissionError::Busy),
        ));
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(local_generation, PlayerState::Playing,)),
            LastFmPlaybackCoordinatorOutcome::Failed(LastFmPlaybackCoordinatorFailure::Runtime(
                LastFmRuntimeAdmissionError::Busy
            ))
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(local_generation, PlayerState::Playing,)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(port.calls(), vec![LastFmPlaybackHandoffKind::NowPlaying]);
        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
    }

    #[test]
    fn closed_runtime_wins_over_simultaneous_owner_error_and_fails_closed() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(45);
        let identity = LastFmPlaybackOccurrenceIdentity::fresh();
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || {
                    Some(accepted_local_load_with_identity(
                        generation,
                        "closed-runtime-private",
                        identity.clone(),
                    ))
                },
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );

        port.script(LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::Runtime(LastFmRuntimeAdmissionError::Closed),
        ));
        assert_eq!(
            binding.observe_output_intent(LastFmOutputIntent::for_test(
                PlayerEventGeneration::from_raw(44),
                generation,
                Some(identity),
            )),
            LastFmPlaybackCoordinatorOutcome::Failed(LastFmPlaybackCoordinatorFailure::Runtime(
                LastFmRuntimeAdmissionError::Closed
            ))
        );
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Failed(LastFmPlaybackCoordinatorFailure::Runtime(
                LastFmRuntimeAdmissionError::Closed
            ))
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
    }

    #[test]
    fn in_flight_closed_runtime_makes_waiting_close_fail_terminally() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(46);
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || Some(accepted_local_load(generation, "in-flight-closed-private")),
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        let failure = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::Runtime(LastFmRuntimeAdmissionError::Closed),
        );
        port.script(failure);
        port.block_now_playing_dispatch();
        let event_binding = binding.clone();
        let event = thread::spawn(move || {
            event_binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing))
        });
        port.wait_for_blocked_dispatch();
        let core = Arc::clone(&binding.core);
        let closer = thread::spawn(move || activation.close());
        loop {
            if matches!(
                &*core.state.lock().expect("inspect retiring state"),
                LastFmPlaybackCoordinatorState::Retiring { .. }
            ) {
                break;
            }
            thread::yield_now();
        }

        port.release_blocked_dispatch();
        assert_eq!(event.join().expect("join player event"), failure);
        assert_eq!(closer.join().expect("join activation close"), failure);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
    }

    #[test]
    fn pending_enqueue_blocks_rebind_until_durable_success_before_successor_activation() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner
            .bind_window(registry.clone())
            .expect("bind predecessor window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(47);
        prime_local_scrobble(&binding, generation, "pending-success-private");
        let completion = port.script_pending_enqueue();
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 51_000, 100_000)),
            LastFmPlaybackCoordinatorOutcome::PendingDurability
        );

        let core = Arc::clone(&binding.core);
        let (finished, completion_observed) = mpsc::channel();
        let rebinder = thread::spawn(move || {
            let result = owner.bind_window(registry);
            finished.send(()).expect("report completed rebind");
            (owner, result)
        });
        loop {
            if matches!(
                &*core.state.lock().expect("inspect retiring state"),
                LastFmPlaybackCoordinatorState::Retiring { .. }
            ) {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(
            completion_observed.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "successor window must remain unavailable before durable completion"
        );

        completion
            .send(Ok(()))
            .expect("complete durable enqueue successfully");
        completion_observed
            .recv_timeout(Duration::from_secs(2))
            .expect("rebind completes after durable receipt");
        let (mut owner, successor) = rebinder.join().expect("join rebind");
        let successor = successor.expect("bind successor after durable completion");
        let successor_activation = activate_for_test(&successor, &port, HashSet::new());
        drop(activation);
        assert_eq!(
            successor.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            successor_activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Applied);
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::Enqueue,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
    }

    #[test]
    fn delayed_enqueue_failure_is_sticky_and_fails_waiting_close() {
        for error in [
            LastFmRuntimeCommandError::QueueFull,
            LastFmRuntimeCommandError::Queue,
            LastFmRuntimeCommandError::StaleAccount,
            LastFmRuntimeCommandError::OwnerStopped,
        ] {
            let (_runtime, registry) = registry();
            let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
            let binding = owner.bind_window(registry).expect("bind window");
            let port = RecordingRuntimePort::default();
            let activation = activate_for_test(&binding, &port, HashSet::new());
            let generation = PlayerEventGeneration::from_raw(48);
            prime_local_scrobble(&binding, generation, "pending-failure-private");
            let completion = port.script_pending_enqueue();
            assert_eq!(
                binding.observe_event(&PlayerEvent::position(generation, 51_000, 100_000)),
                LastFmPlaybackCoordinatorOutcome::PendingDurability
            );
            completion
                .send(Err(error))
                .expect("complete durable enqueue with fixed failure");

            let failure = LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::RuntimeCommand(error),
            );
            assert_eq!(activation.close(), failure);
            assert_eq!(
                binding.revalidate_active_authority(),
                LastFmPlaybackCoordinatorOutcome::Shutdown
            );
            assert!(matches!(
                owner.bind_window(binding.source_registry.clone()),
                Err(LastFmPlaybackCoordinatorBindError::Shutdown)
            ));
        }
    }

    #[test]
    fn rejected_enqueue_admission_releases_neutral_completion_reservation() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(49);
        prime_local_scrobble(&binding, generation, "rejected-enqueue-private");
        let rejection = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::Runtime(LastFmRuntimeAdmissionError::Busy),
        );
        port.script(rejection);
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 51_000, 100_000)),
            rejection
        );
        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied,
            "retirement must not wait on a receipt which runtime never admitted"
        );
    }

    #[test]
    fn source_rejected_enqueue_releases_neutral_completion_reservation() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(50);
        prime_local_scrobble(&binding, generation, "source-rejected-private");
        port.reject_next(LastFmPlaybackHandoffKind::Enqueue);
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 51_000, 100_000)),
            LastFmPlaybackCoordinatorOutcome::SourceRejected
        );
        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied,
            "source rejection must not leave a phantom durable receipt"
        );
    }

    #[test]
    fn cancelled_enqueue_supervisor_latches_owner_stopped_via_raii() {
        let (_runtime, registry) = registry();
        let completion_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build cancellable completion runtime");
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = binding
            .activate_with_runtime_port(
                Box::new(port.clone()),
                completion_runtime.handle().clone(),
                HashSet::new(),
            )
            .expect("activate with cancellable completion runtime");
        let generation = PlayerEventGeneration::from_raw(51);
        prime_local_scrobble(&binding, generation, "cancelled-receipt-private");
        let _completion = port.script_pending_enqueue();
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 51_000, 100_000)),
            LastFmPlaybackCoordinatorOutcome::PendingDurability
        );

        drop(completion_runtime);
        let failure = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::RuntimeCommand(
                LastFmRuntimeCommandError::OwnerStopped,
            ),
        );
        assert_eq!(activation.close(), failure);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
    }

    #[test]
    fn rebind_retires_and_clears_before_successor_activation() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let old = owner
            .bind_window(registry.clone())
            .expect("bind first window");
        let port = RecordingRuntimePort::default();
        let old_activation = activate_for_test(&old, &port, HashSet::new());
        let first_generation = PlayerEventGeneration::from_raw(50);
        assert_eq!(
            old.accept_output_load_lazy(
                first_generation,
                || Some(accepted_local_load(first_generation, "old-private")),
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            old.observe_event(&PlayerEvent::state(first_generation, PlayerState::Playing,)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );

        let current = owner.bind_window(registry).expect("rebind active owner");
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
        assert_eq!(
            old_activation.close(),
            LastFmPlaybackCoordinatorOutcome::StaleWindow
        );
        let current_activation = activate_for_test(&current, &port, HashSet::new());
        let second_generation = PlayerEventGeneration::from_raw(51);
        assert_eq!(
            current.accept_output_load_lazy(
                second_generation,
                || Some(accepted_local_load(second_generation, "new-private")),
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            current.observe_event(&PlayerEvent::state(second_generation, PlayerState::Playing,)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
                LastFmPlaybackHandoffKind::NowPlaying,
            ]
        );
        drop(current_activation);
    }

    #[test]
    fn concurrent_close_and_shutdown_share_one_retirement_completion() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(60);
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || Some(accepted_local_load(generation, "retirement-private")),
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );

        port.block_clear_dispatch();
        let closer = thread::spawn(move || activation.close());
        port.wait_for_blocked_dispatch();
        let core = Arc::clone(&binding.core);
        let (shutdown_completed, completed) = mpsc::channel();
        let shutdown = thread::spawn(move || {
            let outcome = owner.shutdown();
            shutdown_completed
                .send(outcome)
                .expect("report shutdown completion");
            outcome
        });
        loop {
            if matches!(
                &*core.state.lock().expect("inspect shutdown state"),
                LastFmPlaybackCoordinatorState::Shutdown
            ) {
                break;
            }
            thread::yield_now();
        }
        assert!(matches!(
            completed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        port.release_blocked_dispatch();
        assert_eq!(
            closer.join().expect("join activation close"),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            shutdown.join().expect("join owner shutdown"),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            completed.recv().expect("receive shutdown outcome"),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
    }

    #[test]
    fn concurrent_retirement_failure_is_shared_and_denies_successor_window() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry.clone()).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(61);
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || Some(accepted_local_load(generation, "failure-private")),
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        let failure = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::Runtime(LastFmRuntimeAdmissionError::Closed),
        );
        port.script(failure);
        port.block_clear_dispatch();
        let closer = thread::spawn(move || activation.close());
        port.wait_for_blocked_dispatch();
        let core = Arc::clone(&binding.core);
        let shutdown = thread::spawn(move || {
            let outcome = owner.shutdown();
            (owner, outcome)
        });
        loop {
            if matches!(
                &*core.state.lock().expect("inspect shutdown state"),
                LastFmPlaybackCoordinatorState::Shutdown
            ) {
                break;
            }
            thread::yield_now();
        }

        port.release_blocked_dispatch();
        assert_eq!(closer.join().expect("join activation close"), failure);
        let (mut owner, shutdown_outcome) = shutdown.join().expect("join owner shutdown");
        assert_eq!(shutdown_outcome, failure);
        assert!(matches!(
            owner.bind_window(registry),
            Err(LastFmPlaybackCoordinatorBindError::Shutdown)
        ));
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
    }

    #[test]
    fn shutdown_first_close_joins_shared_success_and_failure() {
        let closed = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::Runtime(LastFmRuntimeAdmissionError::Closed),
        );
        for expected in [LastFmPlaybackCoordinatorOutcome::Applied, closed] {
            let (_runtime, registry) = registry();
            let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
            let binding = owner.bind_window(registry.clone()).expect("bind window");
            let port = RecordingRuntimePort::default();
            let activation = activate_for_test(&binding, &port, HashSet::new());
            let generation = PlayerEventGeneration::from_raw(62);
            assert_eq!(
                binding.accept_output_load_lazy(
                    generation,
                    || Some(accepted_local_load(generation, "shutdown-first-private")),
                    || panic!("active bridge must not discard"),
                ),
                LastFmPlaybackCoordinatorOutcome::Applied
            );
            assert_eq!(
                binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
                LastFmPlaybackCoordinatorOutcome::Applied
            );
            if expected != LastFmPlaybackCoordinatorOutcome::Applied {
                port.script(expected);
            }
            port.block_clear_dispatch();
            let shutdown = thread::spawn(move || {
                let outcome = owner.shutdown();
                (owner, outcome)
            });
            port.wait_for_blocked_dispatch();
            let closer = thread::spawn(move || activation.close());

            port.release_blocked_dispatch();
            let (mut owner, shutdown_outcome) = shutdown.join().expect("join owner shutdown");
            assert_eq!(shutdown_outcome, expected);
            assert_eq!(closer.join().expect("join activation close"), expected);
            assert!(matches!(
                owner.bind_window(registry),
                Err(LastFmPlaybackCoordinatorBindError::Shutdown)
            ));
            assert_eq!(
                binding.revalidate_active_authority(),
                LastFmPlaybackCoordinatorOutcome::Shutdown
            );
            assert_eq!(
                port.calls(),
                vec![
                    LastFmPlaybackHandoffKind::NowPlaying,
                    LastFmPlaybackHandoffKind::ClearNowPlaying,
                ]
            );
        }
    }

    #[test]
    fn operation_gate_poison_before_guard_drop_is_latched_and_terminal() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let environment = binding
            .core
            .active_environment(binding.window_epoch)
            .expect("active environment");
        let operation = environment.begin_operation().expect("begin operation");
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _gate = environment
                .operation_gate
                .lock()
                .expect("lock operation gate to poison");
            panic!("poison operation gate for regression");
        }));
        assert!(poisoned.is_err());

        let failure = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::OperationGatePoisoned,
        );
        let outcome = operation.complete(LastFmPlaybackCoordinatorOutcome::Applied);
        assert_eq!(outcome, failure);
        assert_eq!(
            binding.finish_environment_outcome(&environment, outcome),
            failure
        );
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(activation.close(), failure);
        assert!(port.calls().is_empty());
    }

    #[test]
    fn pending_retirement_gate_poison_is_shared_and_terminal() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let environment = binding
            .core
            .active_environment(binding.window_epoch)
            .expect("active environment");
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _retirement = environment
                .retirement
                .lock()
                .expect("lock pending retirement to poison");
            panic!("poison pending retirement gate for regression");
        }));
        assert!(poisoned.is_err());

        let failure = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::RetirementGatePoisoned,
        );
        assert_eq!(activation.close(), failure);
        assert_eq!(environment.retire_after_revocation(), failure);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
    }

    #[test]
    fn running_retirement_gate_poison_is_shared_and_terminal() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(63);
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || Some(accepted_local_load(generation, "retirement-poison-private")),
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        let environment = binding
            .core
            .active_environment(binding.window_epoch)
            .expect("active environment");
        port.block_clear_dispatch();
        let closer = thread::spawn(move || activation.close());
        port.wait_for_blocked_dispatch();
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _retirement = environment
                .retirement
                .lock()
                .expect("lock running retirement to poison");
            panic!("poison running retirement gate for regression");
        }));
        assert!(poisoned.is_err());

        port.release_blocked_dispatch();
        let failure = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::RetirementGatePoisoned,
        );
        assert_eq!(closer.join().expect("join activation close"), failure);
        assert_eq!(environment.retire_after_revocation(), failure);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
    }

    #[test]
    fn completed_retirement_result_is_immutable_after_later_gate_poison() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let environment = binding
            .core
            .active_environment(binding.window_epoch)
            .expect("active environment");
        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _retirement = environment
                .retirement
                .lock()
                .expect("lock completed retirement to poison");
            panic!("poison completed retirement gate for regression");
        }));
        assert!(poisoned.is_err());

        assert_eq!(
            environment.retire_after_revocation(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            environment.retire_after_revocation(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
        assert!(port.calls().is_empty());
    }

    #[test]
    fn poisoned_active_owner_clears_once_and_terminally_shuts_down() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, HashSet::new());
        let generation = PlayerEventGeneration::from_raw(62);
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || Some(accepted_local_load(generation, "poison-private")),
                || panic!("active bridge must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        let environment = binding
            .core
            .active_environment(binding.window_epoch)
            .expect("active environment");
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _owner = environment.owner.lock().expect("lock owner to poison");
            panic!("poison active playback owner for regression");
        }));
        assert!(poisoned.is_err());

        let failure = LastFmPlaybackCoordinatorOutcome::Failed(
            LastFmPlaybackCoordinatorFailure::PlaybackOwnerPoisoned,
        );
        assert_eq!(binding.revalidate_active_authority(), failure);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(activation.close(), failure);
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );
    }

    #[test]
    fn shutdown_and_owner_drop_make_every_binding_terminal() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner
            .bind_window(registry.clone())
            .expect("bind first owner");
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Applied);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Shutdown);

        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind second owner");
        drop(owner);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
    }

    #[test]
    fn poisoned_state_fails_once_then_remains_terminally_shutdown() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind dormant owner");
        let core = Arc::clone(&owner.core);
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _state = core.state.lock().expect("lock coordinator state");
            panic!("poison coordinator state for regression");
        }));
        assert!(poisoned.is_err());

        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Failed(LastFmPlaybackCoordinatorFailure::Poisoned)
        );
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_runtime_ingress_persists_coordinator_enqueue() {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("connect coordinator runtime database");
        Migrator::up(&database, None)
            .await
            .expect("migrate coordinator runtime database");
        let session = StoredSession::new(
            "coordinator-listener",
            ProtectedString::new(TEST_SESSION_KEY),
        )
        .expect("valid coordinator runtime session");
        let credentials: Arc<dyn SessionCredentialStore> =
            Arc::new(RuntimeCredentialStore(Mutex::new(Some(session))));
        let (handle, shutdown) = spawn_lastfm_runtime(
            LastFmRuntimeActivation::issue_after_consent_and_enablement(),
            database.clone(),
            credentials,
            Arc::new(PendingRuntimeTransport),
            Arc::new(FixedRuntimeClock),
        )
        .await
        .expect("start real Last.fm runtime actor");
        let ingress = handle
            .try_claim_playback_ingress()
            .expect("claim the runtime's unique playback ingress");
        assert!(
            handle.try_claim_playback_ingress().is_err(),
            "coordinator receives the sole playback capability"
        );

        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner
            .bind_window(source_registry.clone())
            .expect("bind coordinator window");
        let activation = binding
            .activate(ingress, tokio::runtime::Handle::current(), HashSet::new())
            .expect("activate with genuine claimed runtime ingress");
        let generation = PlayerEventGeneration::from_raw(80);
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || Some(accepted_local_load(generation, "real-runtime-private")),
                || panic!("active coordinator must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 1_000, 100_000)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::position(generation, 51_000, 100_000)),
            LastFmPlaybackCoordinatorOutcome::PendingDurability
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if storage::queue_len(&database)
                    .await
                    .expect("read durable Last.fm queue")
                    == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("coordinator enqueue reached durable runtime storage");

        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Applied);
        assert_eq!(
            shutdown.shutdown().await.expect("drain real runtime actor"),
            LastFmRuntimeShutdownReason::Drained
        );
        source_registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn exact_managed_source_revocation_clears_once_and_denies_stale_reference() {
        let source_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let directory = tempfile::tempdir().expect("managed source fixture directory");
        let path = directory.path().join("managed.wav");
        std::fs::write(&path, tagged_wav_bytes()).expect("write tagged managed WAV");
        let session = source_registry
            .adopt_external_file_if_current(
                File::open(&path).expect("open tagged managed WAV"),
                ExternalFileHint::new("managed.wav", Some("wav"))
                    .expect("safe managed fixture hint"),
                || true,
            )
            .expect("adopt exact managed source");
        let reference = session
            .playback_source()
            .cloned()
            .expect("tagged managed source mints exact playback reference");
        assert_eq!(reference.profile().duration_secs(), Some(31));
        let enabled_remote_sources = HashSet::from([session.source_id()]);

        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner
            .bind_window(source_registry.clone())
            .expect("bind coordinator window");
        let port = RecordingRuntimePort::default();
        let activation = activate_for_test(&binding, &port, enabled_remote_sources);
        let generation = PlayerEventGeneration::from_raw(81);
        assert_eq!(
            binding.accept_output_load_lazy(
                generation,
                || Some(accepted_managed_load(generation, reference.clone())),
                || panic!("active coordinator must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(port.calls(), vec![LastFmPlaybackHandoffKind::NowPlaying]);

        source_registry
            .retire_external(session.source_id())
            .expect("revoke exact managed source")
            .wait()
            .await;
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(
            port.calls(),
            vec![
                LastFmPlaybackHandoffKind::NowPlaying,
                LastFmPlaybackHandoffKind::ClearNowPlaying,
            ]
        );

        let stale_generation = PlayerEventGeneration::from_raw(82);
        assert_eq!(
            binding.accept_output_load_lazy(
                stale_generation,
                || Some(accepted_managed_load(stale_generation, reference)),
                || panic!("active coordinator must not discard"),
            ),
            LastFmPlaybackCoordinatorOutcome::SourceRejected
        );
        assert_eq!(
            binding.observe_event(&PlayerEvent::state(stale_generation, PlayerState::Playing)),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(port.calls().len(), 2, "revoked source admits no more work");

        assert_eq!(
            activation.close(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Applied);
        source_registry.shutdown().wait().await;
    }

    #[test]
    fn diagnostics_are_fixed_and_redacted() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind dormant owner");

        assert_eq!(
            format!("{owner:?}"),
            "LastFmPlaybackCoordinatorOwner(<redacted>)"
        );
        assert_eq!(
            format!("{binding:?}"),
            "LastFmPlaybackCoordinatorBinding(<redacted>)"
        );
    }
}
