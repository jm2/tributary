//! Process-lifetime production owner for Last.fm activation.
//!
//! This boundary composes the native credential store, the durable runtime,
//! and the exact-window playback coordinator without letting any one of those
//! pieces infer user consent.  The owner starts dormant, accepts exactly one
//! database attachment, and requires a move-only authority issued only after
//! explicit consent and enablement.  One activation freezes its remote-source
//! policy for its complete runtime generation.  A successor generation is
//! admitted only after the predecessor has been fully drained, so every
//! runtime and playback owner keeps one-shot activation semantics while the
//! owner sequences generations.  While a generation is active the owner
//! relays its typed runtime status into the application snapshot and forwards
//! the disconnect-and-purge, same-account reauthorization, and manual-pause
//! recovery controls to the exact active runtime.
//!
//! A capable build also owns the one desktop-authorization owner. Its grant
//! comes back here: with no generation active, `Connect` installs it as a new
//! vault account and activates it; with a generation active, reauthorization
//! and disconnect go through the runtime, which holds the vault lease for its
//! whole life. The owner itself takes that lease only while no generation
//! exists, so no vault operation can wait on a running runtime.
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

use super::authorization::{
    spawn_lastfm_authorization, LastFmAuthorizationGrant, LastFmAuthorizationHandle,
    LastFmAuthorizationShutdown, LastFmAuthorizationTransport, SystemLastFmAuthorizationClock,
};
use super::client::{AppCredentials, LastFmClient};
use super::credentials::{
    CredentialError, LastFmAccountBinding, OsSessionCredentialStore, SessionCredentialStore,
    StoredSession,
};
use super::delivery::{LastFmClock, LastFmTransport, SystemLastFmClock};
use super::lifecycle::{
    acquire_vault_lifecycle, recover_quarantined_lastfm_queue, LastFmQuarantinedQueueRecoveryError,
};
use super::playback_coordinator::{
    LastFmPlaybackCoordinatorActivation, LastFmPlaybackCoordinatorBinding,
    LastFmPlaybackCoordinatorOutcome,
};
use super::policy::{LastFmLivePolicy, LastFmPolicyGeneration};
use super::runtime::{
    spawn_lastfm_runtime, LastFmManualPauseRecovery, LastFmRuntimeActivation,
    LastFmRuntimeAdmissionError, LastFmRuntimeBarrier, LastFmRuntimeCommandError,
    LastFmRuntimeHandle, LastFmRuntimeOperation, LastFmRuntimeShutdown, LastFmRuntimeStatus,
};
use super::storage::{self, LastFmClosedAndDrainedQueue, LastFmQueueError};

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
///
/// `runtime` carries the active generation's latest typed runtime status.
/// It is `None` while no runtime generation is installed: before the first
/// activation, after a completed disconnect drain, and after the runtime
/// status publisher has shut down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LastFmApplicationStatus {
    pub(crate) revision: u64,
    pub(crate) phase: LastFmApplicationPhase,
    pub(crate) failure: Option<LastFmApplicationCommandError>,
    pub(crate) runtime: Option<LastFmRuntimeStatus>,
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
            runtime: None,
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
    #[error("Last.fm application generation is already active")]
    GenerationActive,
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
    #[error("Last.fm application generation is already active")]
    GenerationActive,
    #[error("Last.fm consent and enablement are required")]
    ConsentRequired,
    #[error("Last.fm protected credential store is unavailable")]
    CredentialStore,
    #[error("a Last.fm account is already stored")]
    AccountPresent,
    #[error("Last.fm queue belongs to an account that can no longer be loaded")]
    QuarantinedQueue,
}

/// Typed failure of an admitted disconnect-and-purge.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmApplicationDisconnectError {
    #[error("Last.fm application owner stopped")]
    OwnerStopped,
    #[error("Last.fm application generation is not active")]
    GenerationInactive,
    #[error("Last.fm runtime refused disconnect: {0}")]
    RuntimeRefused(LastFmRuntimeAdmissionError),
    #[error("Last.fm runtime stopped before disconnect completed")]
    RuntimeStopped,
    #[error("Last.fm disconnect did not complete and can be retried")]
    Incomplete,
}

/// Move-only authority issued after explicit consent and enablement.
///
/// The exact remote-source policy is immutable after construction.  Changing
/// it requires retiring the complete generation and issuing a successor, so
/// the authority freezes the identity of the generation whose consent issued
/// it: once the live slot moves past that generation, the authority is spent,
/// even when the successor is itself consented and enabled.
#[must_use = "Last.fm activation authority must be consumed or explicitly discarded"]
pub(crate) struct LastFmApplicationActivation {
    policy_generation: u64,
    #[cfg(test)]
    enabled_remote_sources: HashSet<SourceId>,
}

impl LastFmApplicationActivation {
    #[cfg(test)]
    pub(in crate::lastfm) fn issue_after_explicit_consent_and_enablement(
        policy_generation: u64,
        enabled_remote_sources: HashSet<SourceId>,
    ) -> Result<Self, LastFmApplicationAdmissionError> {
        Self::validated(policy_generation, enabled_remote_sources)
    }

    /// Consume the current live policy generation as one activation authority.
    ///
    /// The exact remote-source set is taken from the generation's immutable
    /// `activation_remote_sources` view, so the same generation that gates
    /// queue capture also issues dispatch authority. A generation without
    /// current consent and enablement grants no activation authority and is
    /// refused here rather than inferring consent from a build credential,
    /// vault record, queued row, or discoverable account.
    pub(crate) fn issue_from_policy_generation(
        generation: &LastFmPolicyGeneration,
    ) -> Result<Self, LastFmApplicationAdmissionError> {
        let Some(enabled_remote_sources) = generation.activation_remote_sources() else {
            return Err(LastFmApplicationAdmissionError::InvalidSourcePolicy);
        };
        Self::validated(generation.generation(), enabled_remote_sources.clone())
    }

    fn validated(
        policy_generation: u64,
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
            policy_generation,
            #[cfg(test)]
            enabled_remote_sources,
        })
    }

    /// The identity of the generation whose consent issued this authority.
    pub(in crate::lastfm) const fn policy_generation(&self) -> u64 {
        self.policy_generation
    }

    /// The exact remote-source set this authority freezes. Test-only so the
    /// activation remains a move-only, content-free authority in production.
    #[cfg(test)]
    pub(in crate::lastfm) fn enabled_remote_sources_for_test(&self) -> &HashSet<SourceId> {
        &self.enabled_remote_sources
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
    activation_pending: bool,
    generation_active: bool,
    runtime_generation: u64,
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

    /// Open the runtime status relay: advance the generation tag, install
    /// the first typed snapshot, and return the tag the relay task must
    /// present on every later update.
    fn open_runtime_relay(&mut self, runtime: LastFmRuntimeStatus) -> u64 {
        self.runtime_generation = self.runtime_generation.saturating_add(1);
        self.status.revision = self.status.revision.saturating_add(1);
        self.status.runtime = Some(runtime);
        self.status_sender.send_replace(self.status);
        self.runtime_generation
    }

    /// Relay one runtime snapshot. Stale relays observe the generation tag
    /// and exit without touching the successor's snapshot, so a predecessor
    /// can never resurrect a cleared status or overwrite its successor's.
    fn relay_runtime(&mut self, generation: u64, runtime: LastFmRuntimeStatus) {
        if self.runtime_generation != generation || self.status.runtime == Some(runtime) {
            return;
        }
        self.status.revision = self.status.revision.saturating_add(1);
        self.status.runtime = Some(runtime);
        self.status_sender.send_replace(self.status);
    }

    /// Close the runtime status relay: advance the generation tag so any
    /// live relay task exits on its next observation, and clear the relayed
    /// snapshot.
    fn close_runtime_relay(&mut self) {
        self.runtime_generation = self.runtime_generation.saturating_add(1);
        if self.status.runtime.take().is_some() {
            self.status.revision = self.status.revision.saturating_add(1);
            self.status_sender.send_replace(self.status);
        }
    }
}

struct HandleInner {
    commands: async_channel::Sender<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    status: watch::Receiver<LastFmApplicationStatus>,
    authorization: Option<LastFmAuthorizationHandle>,
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
    ///
    /// Activation stays one-shot per generation: while an activation is
    /// queued or a generation is active, a further activation is refused.
    /// A successor is admitted only after the active generation has been
    /// fully drained by a completed disconnect or the ordered shutdown.
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
        if ingress.generation_active || ingress.activation_pending {
            return Err(LastFmApplicationAdmissionError::GenerationActive);
        }
        let (completion, receiver) = oneshot::channel();
        match self.commands_try_send(Command::Activate {
            activation,
            completion,
        }) {
            Ok(()) => ingress.activation_pending = true,
            Err(error) => return Err(error),
        }
        Ok(LastFmApplicationOperation { receiver })
    }

    /// Install a freshly authorized account and activate it.
    ///
    /// Admitted only while no generation is active or starting. The owner
    /// requires current consent and enablement, stores the grant as a new
    /// vault account (refusing if a readable account is already stored or if
    /// queued rows belong to an account that can no longer be loaded), and
    /// then runs the ordinary activation for the live policy generation.
    pub(crate) fn try_connect(
        &self,
        grant: LastFmAuthorizationGrant,
    ) -> Result<LastFmApplicationOperation<()>, LastFmApplicationAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        if !ingress.build_available {
            return Err(LastFmApplicationAdmissionError::BuildUnavailable);
        }
        if !ingress.database_admitted {
            return Err(LastFmApplicationAdmissionError::DatabaseRequired);
        }
        if ingress.generation_active || ingress.activation_pending {
            return Err(LastFmApplicationAdmissionError::GenerationActive);
        }
        let (completion, receiver) = oneshot::channel();
        self.commands_try_send(Command::Connect { grant, completion })?;
        ingress.activation_pending = true;
        Ok(LastFmApplicationOperation { receiver })
    }

    /// Disconnect and purge the stored account.
    ///
    /// With a generation active, the runtime owns the destructive purge
    /// ordering (close admissions, retire delivery, drain and transactionally
    /// purge the durable queue, wipe the session, delete the exact vault
    /// record, compare-and-delete the cleanup marker). Once that completes the
    /// owner drains the generation in the bridge-before-runtime order and
    /// returns to `AwaitingConsent`, where a successor activation may be
    /// admitted. If the runtime could not finish, the generation is retained
    /// and a repeat disconnect retries: the purge again, or only the
    /// credential deletion once the purge has committed.
    ///
    /// With no generation active this is the explicit discard of a queue whose
    /// account can no longer be loaded; a readable stored account is refused
    /// with `GenerationInactive`.
    pub(crate) fn try_disconnect_and_purge(
        &self,
    ) -> Result<
        LastFmApplicationOperation<u64, LastFmApplicationDisconnectError>,
        LastFmApplicationAdmissionError,
    > {
        let ingress = self.lock_ingress()?;
        if !ingress.build_available {
            return Err(LastFmApplicationAdmissionError::BuildUnavailable);
        }
        drop(ingress);
        let (completion, receiver) = oneshot::channel();
        self.commands_try_send(Command::DisconnectAndPurge { completion })?;
        Ok(LastFmApplicationOperation { receiver })
    }

    /// Forward one same-account reauthorization grant to the active runtime.
    ///
    /// The admitted command resolves to the runtime's own admission result:
    /// `Ok` carries the typed runtime operation to await, and the inner
    /// `Err` carries the runtime's typed refusal (`NotActive` when no
    /// generation is installed). No secret crosses the completion channel;
    /// the grant is delivered to the exact active runtime or dropped. The
    /// runtime saves the renewed key under its own vault lease and clears
    /// the durable reauthentication pause before delivery resumes.
    pub(crate) fn try_reauthorize_same_account(
        &self,
        grant: LastFmAuthorizationGrant,
    ) -> Result<
        LastFmApplicationOperation<Result<LastFmRuntimeOperation<()>, LastFmRuntimeAdmissionError>>,
        LastFmApplicationAdmissionError,
    > {
        drop(self.lock_ingress()?);
        let (completion, receiver) = oneshot::channel();
        self.commands_try_send(Command::ReauthorizeSameAccount { grant, completion })?;
        Ok(LastFmApplicationOperation { receiver })
    }

    /// Capture one manual-pause recovery authority from the active runtime.
    ///
    /// The admitted command resolves to the runtime's own admission result;
    /// `NotActive` reports that no generation is installed.
    #[allow(dead_code, reason = "no settings control issues manual recovery yet")]
    pub(crate) fn try_issue_manual_pause_recovery(
        &self,
    ) -> Result<
        LastFmApplicationOperation<Result<LastFmManualPauseRecovery, LastFmRuntimeAdmissionError>>,
        LastFmApplicationAdmissionError,
    > {
        drop(self.lock_ingress()?);
        let (completion, receiver) = oneshot::channel();
        self.commands_try_send(Command::IssueManualPauseRecovery { completion })?;
        Ok(LastFmApplicationOperation { receiver })
    }

    /// Resume one exact paused runtime generation with its captured
    /// recovery authority.
    ///
    /// The admitted command resolves to the runtime's own admission result;
    /// a foreign or stale authority is refused by the runtime itself.
    #[allow(dead_code, reason = "no settings control issues manual recovery yet")]
    pub(crate) fn try_resume_after_manual_recovery(
        &self,
        recovery: LastFmManualPauseRecovery,
    ) -> Result<
        LastFmApplicationOperation<Result<LastFmRuntimeOperation<()>, LastFmRuntimeAdmissionError>>,
        LastFmApplicationAdmissionError,
    > {
        drop(self.lock_ingress()?);
        let (completion, receiver) = oneshot::channel();
        self.commands_try_send(Command::ResumeAfterManualRecovery {
            recovery,
            completion,
        })?;
        Ok(LastFmApplicationOperation { receiver })
    }

    /// Close ingress. The retained shutdown owner proves the ordered drain.
    pub(crate) fn close_and_flush(&self) -> bool {
        request_close(&self.inner)
    }

    pub(crate) fn subscribe_status(&self) -> watch::Receiver<LastFmApplicationStatus> {
        self.inner.status.clone()
    }

    /// The process desktop-authorization owner; `None` in unavailable builds.
    pub(crate) fn authorization(&self) -> Option<LastFmAuthorizationHandle> {
        self.inner.authorization.clone()
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
pub(crate) struct LastFmApplicationOperation<T, E = LastFmApplicationCommandError> {
    receiver: oneshot::Receiver<Result<T, E>>,
}

impl<T> LastFmApplicationOperation<T, LastFmApplicationCommandError> {
    pub(crate) async fn wait(self) -> Result<T, LastFmApplicationCommandError> {
        self.receiver
            .await
            .unwrap_or(Err(LastFmApplicationCommandError::OwnerStopped))
    }
}

impl<T> LastFmApplicationOperation<T, LastFmApplicationDisconnectError> {
    pub(crate) async fn wait(self) -> Result<T, LastFmApplicationDisconnectError> {
        self.receiver
            .await
            .unwrap_or(Err(LastFmApplicationDisconnectError::OwnerStopped))
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
    Connect {
        grant: LastFmAuthorizationGrant,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    },
    DisconnectAndPurge {
        completion: oneshot::Sender<Result<u64, LastFmApplicationDisconnectError>>,
    },
    ReauthorizeSameAccount {
        grant: LastFmAuthorizationGrant,
        completion: oneshot::Sender<
            Result<
                Result<LastFmRuntimeOperation<()>, LastFmRuntimeAdmissionError>,
                LastFmApplicationCommandError,
            >,
        >,
    },
    #[allow(dead_code, reason = "no settings control issues manual recovery yet")]
    IssueManualPauseRecovery {
        completion: oneshot::Sender<
            Result<
                Result<LastFmManualPauseRecovery, LastFmRuntimeAdmissionError>,
                LastFmApplicationCommandError,
            >,
        >,
    },
    #[allow(dead_code, reason = "no settings control issues manual recovery yet")]
    ResumeAfterManualRecovery {
        recovery: LastFmManualPauseRecovery,
        completion: oneshot::Sender<
            Result<
                Result<LastFmRuntimeOperation<()>, LastFmRuntimeAdmissionError>,
                LastFmApplicationCommandError,
            >,
        >,
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
    relay: JoinHandle<()>,
}

struct ApplicationOwner {
    commands: async_channel::Receiver<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    coordinator: LastFmPlaybackCoordinatorBinding,
    completion_runtime: tokio::runtime::Handle,
    credentials: Arc<dyn SessionCredentialStore>,
    transport: Option<Arc<dyn LastFmTransport>>,
    clock: Arc<dyn LastFmClock>,
    live_policy: LastFmLivePolicy,
    database: Option<DatabaseConnection>,
    generation: Option<ActiveGeneration>,
    authorization: Option<LastFmAuthorizationShutdown>,
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
                Command::Connect { grant, completion } => {
                    if let Err(error) = self.connect(grant, completion).await {
                        self.reject_queued();
                        let _ = self.publish(
                            LastFmApplicationPhase::Failed,
                            Some(LastFmApplicationCommandError::Drain),
                        );
                        return Err(error);
                    }
                }
                Command::DisconnectAndPurge { completion } => {
                    if let Err(error) = self.disconnect_and_purge(completion).await {
                        self.reject_queued();
                        let _ = self.publish(
                            LastFmApplicationPhase::Failed,
                            Some(LastFmApplicationCommandError::Drain),
                        );
                        return Err(error);
                    }
                }
                Command::ReauthorizeSameAccount { grant, completion } => {
                    let result = match self.generation.as_ref() {
                        Some(generation) => {
                            let (username, key) = grant.into_authorized_session().into_parts();
                            generation
                                .runtime_handle
                                .reauthorize_same_account(username.as_str().to_owned(), key)
                        }
                        None => Err(LastFmRuntimeAdmissionError::NotActive),
                    };
                    let _ = completion.send(Ok(result));
                }
                Command::IssueManualPauseRecovery { completion } => {
                    let result = match self.generation.as_ref() {
                        Some(generation) => generation
                            .runtime_handle
                            .issue_manual_pause_recovery_after_explicit_user_action(),
                        None => Err(LastFmRuntimeAdmissionError::NotActive),
                    };
                    let _ = completion.send(Ok(result));
                }
                Command::ResumeAfterManualRecovery {
                    recovery,
                    completion,
                } => {
                    let result = match self.generation.as_ref() {
                        Some(generation) => generation
                            .runtime_handle
                            .resume_after_manual_recovery(recovery),
                        None => Err(LastFmRuntimeAdmissionError::NotActive),
                    };
                    let _ = completion.send(Ok(result));
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

        // A playback-ingress claim or coordinator-activation failure is
        // terminal but not itself a failed drain. Preserve that diagnosis while completing the empty
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
        // Consumed as the move-only proof of explicit consent only: its
        // frozen source set is deliberately never read, because dispatch
        // authority is re-derived from the live policy generation at every
        // dispatch below. Its frozen issuing generation IS read below: a
        // replacement generation — even a still-consented, enabled successor
        // — retires the predecessor's authority, so consuming a superseded
        // activation must never mint or start anything.
        activation: LastFmApplicationActivation,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    ) -> Result<(), LastFmApplicationShutdownError> {
        // A duplicate activation can never mint a second runtime or playback
        // owner. The gate holds `activation_pending` from admission until
        // the generation installs (or the owner goes terminal), so admission
        // itself refuses the whole span; this guard is defense-in-depth for
        // a generation that is already active. The refusal is not terminal
        // for the owner: the duplicate authority is spent, and the active
        // generation keeps running.
        {
            let ingress = self.lock_ingress()?;
            if ingress.generation_active {
                drop(ingress);
                let _ = completion.send(Err(LastFmApplicationCommandError::GenerationActive));
                return Ok(());
            }
        }
        // The database attachment and transport stay installed across
        // generations: one attachment backs every generation, and each
        // runtime receives its own handle to the same pool.
        let Some(database) = self.database.clone() else {
            let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
            self.reset_activation_pending()?;
            return Ok(());
        };
        let Some(transport) = self.transport.clone() else {
            let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
            self.reset_activation_pending()?;
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

        // The queued activation freezes the generation whose consent issued
        // it. The live slot must still be that exact generation when the
        // command is processed: any successor — including an enabled-to-
        // enabled replacement — retires the predecessor's authority, and
        // minting from the live slot would silently start a runtime under
        // the successor's source set from the predecessor's consent. Refuse
        // a mismatch before the runtime activation is minted and before any
        // runtime, storage, or credential work, exactly like the
        // superseded-generation refusals below.
        if self.live_policy.snapshot().generation() != activation.policy_generation() {
            return self.refuse_activation(LastFmApplicationCommandError::RuntimeStart, completion);
        }
        let Some(activation) =
            LastFmRuntimeActivation::issue_after_consent_and_enablement(&self.live_policy)
        else {
            // The live policy stopped granting activation authority between
            // command acceptance and processing (revoked consent, disabled
            // integration, or a replaced policy). Refuse before any runtime
            // start, exactly as a superseded generation would at spawn time.
            return self.refuse_activation(LastFmApplicationCommandError::RuntimeStart, completion);
        };
        let started = spawn_lastfm_runtime(
            activation,
            &self.live_policy,
            database,
            Arc::clone(&self.credentials),
            transport,
            Arc::clone(&self.clock),
        )
        .await;
        // A refused start (no stored account, locked vault, quarantined
        // queue, superseded policy) never created a runtime, so the owner
        // stays dormant and a later activation can be admitted.
        let Ok((runtime_handle, runtime_shutdown)) = started else {
            return self.refuse_activation(LastFmApplicationCommandError::RuntimeStart, completion);
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
            self.fail_terminal_before_completion(failure, completion)?;
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

        // Dispatch authority stays LIVE: the coordinator re-derives the
        // current generation's source set at every dispatch instead of
        // freezing this activation's set.
        let Ok(coordinator) = self.coordinator.activate(
            runtime_ingress,
            self.completion_runtime.clone(),
            self.live_policy.clone(),
        ) else {
            let drained = runtime_shutdown.shutdown().await.is_ok();
            let failure = if drained {
                LastFmApplicationCommandError::CoordinatorActivation
            } else {
                LastFmApplicationCommandError::Drain
            };
            self.fail_terminal_before_completion(failure, completion)?;
            return if drained {
                Ok(())
            } else {
                Err(LastFmApplicationShutdownError)
            };
        };

        // Subscribe before publishing Active: activation completion is a
        // status-observation boundary, so the relayed runtime snapshot must
        // already be installed inside the Active publication.
        let mut runtime_status = runtime_handle.subscribe_status();

        // Publishing Active and a concurrent close linearize on this gate.
        let publish_active = {
            let mut ingress = self.lock_ingress()?;
            if ingress.open {
                ingress.generation_active = true;
                ingress.activation_pending = false;
                let relay_generation =
                    ingress.open_runtime_relay(*runtime_status.borrow_and_update());
                ingress.publish(LastFmApplicationPhase::Active, None);
                Some(relay_generation)
            } else {
                None
            }
        };
        let Some(relay_generation) = publish_active else {
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
        };

        self.generation = Some(ActiveGeneration {
            coordinator,
            runtime_handle,
            runtime_barrier,
            runtime_shutdown,
            relay: tokio::spawn(spawn_runtime_status_relay(
                Arc::clone(&self.ingress),
                runtime_status,
                relay_generation,
            )),
        });
        let _ = completion.send(Ok(()));
        Ok(())
    }

    async fn disconnect_and_purge(
        &mut self,
        completion: oneshot::Sender<Result<u64, LastFmApplicationDisconnectError>>,
    ) -> Result<(), LastFmApplicationShutdownError> {
        let Some(generation) = self.generation.as_ref() else {
            let _ = completion.send(self.discard_quarantined_queue().await);
            return Ok(());
        };
        // The runtime owns the destructive purge ordering. The await is
        // deliberately inside the owner: purge completion is the point at
        // which the generation must drain, so the owner must observe it
        // before any successor activation is processed. Once the purge has
        // committed the runtime refuses a second purge; a repeat disconnect
        // then retries only the credential deletion.
        let runtime = &generation.runtime_handle;
        let outcome = match runtime.disconnect_and_purge() {
            Ok(purge) => purge.wait().await.map_err(disconnect_failure),
            Err(LastFmRuntimeAdmissionError::CredentialCleanupRequired) => {
                match runtime.retry_credential_cleanup() {
                    Ok(retry) => retry.wait().await.map(|()| 0).map_err(disconnect_failure),
                    Err(admission) => {
                        Err(LastFmApplicationDisconnectError::RuntimeRefused(admission))
                    }
                }
            }
            Err(admission) => Err(LastFmApplicationDisconnectError::RuntimeRefused(admission)),
        };
        let purged_scrobbles = match outcome {
            Ok(purged_scrobbles) => purged_scrobbles,
            Err(failure) => {
                let _ = completion.send(Err(failure));
                return Ok(());
            }
        };

        // Clean disconnect: drain the generation in the bridge-before-
        // runtime order and return to AwaitingConsent. A concurrent close
        // linearizes on the open check after the drain: whichever side
        // observes the closed gate owns the terminal outcome.
        self.close_generation().await?;
        if !self.is_open()? {
            let _ = completion.send(Err(LastFmApplicationDisconnectError::OwnerStopped));
            return Ok(());
        }
        self.publish(LastFmApplicationPhase::AwaitingConsent, None)?;
        let _ = completion.send(Ok(purged_scrobbles));
        Ok(())
    }

    /// Explicitly discard queued rows whose account can no longer be loaded
    /// (missing or corrupt vault record). A readable account is refused: it
    /// is disconnected through its own runtime generation instead.
    async fn discard_quarantined_queue(&self) -> Result<u64, LastFmApplicationDisconnectError> {
        let Some(database) = self.database.clone() else {
            return Err(LastFmApplicationDisconnectError::GenerationInactive);
        };
        let recovered =
            recover_quarantined_lastfm_queue(database, Arc::clone(&self.credentials)).await;
        match recovered {
            Ok(recovery) => {
                // Clear a recorded quarantine refusal so the surface reads
                // the discarded state; a racing close keeps its own phase.
                let mut ingress = self
                    .lock_ingress()
                    .map_err(|_| LastFmApplicationDisconnectError::OwnerStopped)?;
                if ingress.open {
                    ingress.publish(LastFmApplicationPhase::AwaitingConsent, None);
                }
                Ok(recovery.purged_scrobbles())
            }
            Err(LastFmQuarantinedQueueRecoveryError::ValidSessionPresent) => {
                Err(LastFmApplicationDisconnectError::GenerationInactive)
            }
            Err(_) => Err(LastFmApplicationDisconnectError::Incomplete),
        }
    }

    /// Install a freshly authorized account, then activate it.
    ///
    /// Consent is checked before the vault is touched, so a grant can never
    /// be stored without it. Admission guaranteed that no generation is
    /// active, so no runtime holds the vault lease the install waits for.
    async fn connect(
        &mut self,
        grant: LastFmAuthorizationGrant,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    ) -> Result<(), LastFmApplicationShutdownError> {
        let Ok(activation) =
            LastFmApplicationActivation::issue_from_policy_generation(&self.live_policy.snapshot())
        else {
            return self
                .refuse_activation(LastFmApplicationCommandError::ConsentRequired, completion);
        };
        let Some(database) = self.database.clone() else {
            return self.refuse_activation(LastFmApplicationCommandError::OwnerStopped, completion);
        };
        if let Err(failure) =
            install_new_account(&database, Arc::clone(&self.credentials), grant).await
        {
            return self.refuse_activation(failure, completion);
        }
        self.activate(activation, completion).await
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
            relay,
        } = generation;
        let coordinator_drained = close_coordinator(coordinator).await;
        drop(runtime_handle);
        let runtime_drained = runtime_shutdown.shutdown().await.is_ok();
        let relay_drained = relay.await.is_ok();
        // Retire the relayed runtime snapshot with the generation. Any relay
        // task that lost the race observes a stale generation tag and exits
        // without touching the snapshot.
        {
            let mut ingress = self.ingress.lock().unwrap_or_else(PoisonError::into_inner);
            ingress.generation_active = false;
            ingress.close_runtime_relay();
        }
        self.ingress.clear_poison();
        if coordinator_drained && runtime_drained && relay_drained {
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

    fn fail_terminal_before_completion(
        &self,
        failure: LastFmApplicationCommandError,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    ) -> Result<(), LastFmApplicationShutdownError> {
        // The watch and completion channels are independent. Publish the
        // terminal snapshot synchronously before waking a waiter so command
        // completion is a reliable status-observation boundary. If the status
        // gate is poisoned, still preserve the original command error before
        // propagating the terminal shutdown failure.
        let terminal_status = self.fail_terminal(failure);
        let _ = completion.send(Err(failure));
        terminal_status
    }

    fn lock_ingress(&self) -> Result<MutexGuard<'_, IngressGate>, LastFmApplicationShutdownError> {
        self.ingress
            .lock()
            .map_err(|_| LastFmApplicationShutdownError)
    }

    /// Release the activation span after a non-terminal refusal so a later
    /// activation can be admitted. Terminal paths close the gate instead,
    /// where the pending flag no longer gates anything.
    fn reset_activation_pending(&self) -> Result<(), LastFmApplicationShutdownError> {
        self.lock_ingress()?.activation_pending = false;
        Ok(())
    }

    /// Refuse an admitted activation or connect before any runtime exists.
    /// The owner returns to dormant with the fixed failure recorded, then
    /// wakes the waiter, so completion stays a status-observation boundary.
    fn refuse_activation(
        &self,
        failure: LastFmApplicationCommandError,
        completion: oneshot::Sender<Result<(), LastFmApplicationCommandError>>,
    ) -> Result<(), LastFmApplicationShutdownError> {
        {
            let mut ingress = self.lock_ingress()?;
            ingress.activation_pending = false;
            if ingress.open {
                ingress.publish(LastFmApplicationPhase::AwaitingConsent, Some(failure));
            }
        }
        let _ = completion.send(Err(failure));
        Ok(())
    }

    fn reject_queued(&self) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::AttachDatabase { completion, .. }
                | Command::Activate { completion, .. }
                | Command::Connect { completion, .. } => {
                    let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
                }
                Command::DisconnectAndPurge { completion } => {
                    let _ = completion.send(Err(LastFmApplicationDisconnectError::OwnerStopped));
                }
                Command::ReauthorizeSameAccount { completion, .. }
                | Command::ResumeAfterManualRecovery { completion, .. } => {
                    let _ = completion.send(Err(LastFmApplicationCommandError::OwnerStopped));
                }
                Command::IssueManualPauseRecovery { completion } => {
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

/// Classify a failed runtime disconnect step. A stopped runtime can no longer
/// report its outcome (the barrier path owns that terminal failure); any
/// other failure left the generation in its purge-retry or credential-cleanup
/// state, where the next disconnect resumes.
fn disconnect_failure(failure: LastFmRuntimeCommandError) -> LastFmApplicationDisconnectError {
    if failure == LastFmRuntimeCommandError::OwnerStopped {
        LastFmApplicationDisconnectError::RuntimeStopped
    } else {
        LastFmApplicationDisconnectError::Incomplete
    }
}

/// Store a freshly authorized account in an empty vault under the lease.
///
/// A readable stored account is never replaced here (disconnect first). A
/// corrupt record is overwritten, and an empty cleanup tombstone left by a
/// completed credential deletion is cleared. Queued rows bound to any other
/// account are a quarantine that only the explicit discard may remove, so
/// the install is refused before the vault is written.
async fn install_new_account(
    database: &DatabaseConnection,
    credentials: Arc<dyn SessionCredentialStore>,
    grant: LastFmAuthorizationGrant,
) -> Result<(), LastFmApplicationCommandError> {
    use LastFmApplicationCommandError as Error;

    let (username, key) = grant.into_authorized_session().into_parts();
    let session = StoredSession::new(username.as_str(), key).map_err(|_| Error::CredentialStore)?;
    let lease = acquire_vault_lifecycle().await;
    let loader = Arc::clone(&credentials);
    let (lease, loaded) = tokio::task::spawn_blocking(move || (lease, loader.load()))
        .await
        .map_err(|_| Error::CredentialStore)?;
    match loaded {
        Ok(None) | Err(CredentialError::InvalidData) => {}
        Ok(Some(_)) => return Err(Error::AccountPresent),
        Err(_) => return Err(Error::CredentialStore),
    }
    prepare_queue_for_new_account(database, session.account_binding()).await?;
    let (_lease, saved) = tokio::task::spawn_blocking(move || {
        let saved = credentials.save(&session);
        (lease, saved)
    })
    .await
    .map_err(|_| Error::CredentialStore)?;
    saved.map_err(|_| Error::CredentialStore)
}

/// Clear an empty cleanup marker left by a completed credential deletion,
/// then require that nothing in the queue belongs to another account.
async fn prepare_queue_for_new_account(
    database: &DatabaseConnection,
    binding: LastFmAccountBinding,
) -> Result<(), LastFmApplicationCommandError> {
    if let Some(previous) = storage::has_empty_cleanup_tombstone(database)
        .await
        .map_err(queue_failure)?
    {
        let authority = LastFmClosedAndDrainedQueue::issue_after_barrier();
        storage::clear_empty_cleanup_after_missing_vault(database, previous, &authority)
            .await
            .map_err(queue_failure)?;
    }
    storage::validate_account_queue_state(database, binding)
        .await
        .map(|_| ())
        .map_err(queue_failure)
}

/// Rows or markers bound to another account are a quarantine; anything else
/// is a transient storage failure the user can retry.
const fn queue_failure(error: LastFmQueueError) -> LastFmApplicationCommandError {
    match error {
        LastFmQueueError::AccountMismatch | LastFmQueueError::CorruptStorage => {
            LastFmApplicationCommandError::QuarantinedQueue
        }
        _ => LastFmApplicationCommandError::RuntimeStart,
    }
}

async fn close_coordinator(activation: LastFmPlaybackCoordinatorActivation) -> bool {
    tokio::task::spawn_blocking(move || activation.close())
        .await
        .is_ok_and(|outcome| outcome == LastFmPlaybackCoordinatorOutcome::Applied)
}

/// Relay one active generation's typed runtime status into the application
/// snapshot.
///
/// The relay presents the generation tag captured at install time on every
/// update. Once the runtime's status publisher shuts down, or once a newer
/// generation owns the snapshot, the relay exits without touching state.
/// Poison-tolerant locking keeps a poisoned gate from wedging the relay:
/// the poison is cleared after each healed access.
async fn spawn_runtime_status_relay(
    ingress: Arc<Mutex<IngressGate>>,
    mut runtime_status: watch::Receiver<LastFmRuntimeStatus>,
    generation: u64,
) {
    loop {
        if runtime_status.changed().await.is_err() {
            break;
        }
        let snapshot = *runtime_status.borrow_and_update();
        {
            let mut gate = ingress.lock().unwrap_or_else(PoisonError::into_inner);
            if gate.runtime_generation != generation {
                break;
            }
            gate.relay_runtime(generation, snapshot);
        }
        ingress.clear_poison();
    }
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
    #[cfg(test)]
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
#[cfg(test)]
pub(crate) struct LastFmApplicationBarrier {
    completion: watch::Receiver<LastFmApplicationDrainState>,
}

#[cfg(test)]
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

#[cfg(test)]
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
/// fail-closed owner whose database ingress rejects without retaining input
/// and which has no authorization owner. A capable build also spawns the one
/// desktop-authorization owner over the same client; its drain joins this
/// owner's.
pub(crate) fn spawn_lastfm_application_owner(
    coordinator: LastFmPlaybackCoordinatorBinding,
    completion_runtime: tokio::runtime::Handle,
    live_policy: LastFmLivePolicy,
) -> Result<(LastFmApplicationHandle, LastFmApplicationShutdown), LastFmApplicationOwnerClaimError>
{
    APPLICATION_OWNER_CLAIMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| LastFmApplicationOwnerClaimError)?;
    let client = AppCredentials::from_build()
        .and_then(LastFmClient::new)
        .ok()
        .map(Arc::new);
    let authorization = client.as_ref().map(|client| {
        // The window composes this owner on the GTK thread, outside any
        // runtime context; the authorization owner spawns onto the ambient one.
        let _runtime = completion_runtime.enter();
        spawn_lastfm_authorization(
            Arc::clone(client) as Arc<dyn LastFmAuthorizationTransport>,
            Arc::new(SystemLastFmAuthorizationClock::default()),
        )
    });
    Ok(spawn_with_options(
        coordinator,
        completion_runtime,
        Arc::new(OsSessionCredentialStore),
        client.map(|client| client as Arc<dyn LastFmTransport>),
        Arc::new(SystemLastFmClock),
        live_policy,
        ApplicationSpawnOptions {
            authorization,
            #[cfg(test)]
            attachment_publish_gate: None,
            #[cfg(test)]
            activation_start_gate: None,
            #[cfg(test)]
            runtime_exit_gate: None,
            #[cfg(test)]
            panic_cleanup_gate: None,
        },
    ))
}

#[cfg(test)]
fn spawn_with_dependencies(
    coordinator: LastFmPlaybackCoordinatorBinding,
    completion_runtime: tokio::runtime::Handle,
    credentials: Arc<dyn SessionCredentialStore>,
    transport: Option<Arc<dyn LastFmTransport>>,
    clock: Arc<dyn LastFmClock>,
    live_policy: LastFmLivePolicy,
) -> (LastFmApplicationHandle, LastFmApplicationShutdown) {
    spawn_with_options(
        coordinator,
        completion_runtime,
        credentials,
        transport,
        clock,
        live_policy,
        ApplicationSpawnOptions {
            authorization: None,
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
    authorization: Option<(LastFmAuthorizationHandle, LastFmAuthorizationShutdown)>,
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
    live_policy: LastFmLivePolicy,
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
        activation_pending: false,
        generation_active: false,
        runtime_generation: 0,
        status_sender,
        status: initial_status,
    }));
    let (authorization, authorization_shutdown) = options.authorization.unzip();
    let inner = Arc::new(HandleInner {
        commands,
        ingress: Arc::clone(&ingress),
        status,
        authorization,
    });
    let mut owner = ApplicationOwner {
        commands: receiver,
        ingress,
        coordinator,
        completion_runtime,
        credentials,
        transport,
        clock,
        live_policy,
        database: None,
        generation: None,
        authorization: authorization_shutdown,
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
        let mut result = match AssertUnwindSafe(owner.run()).catch_unwind().await {
            Ok(result) => result,
            Err(_) => {
                owner.quiesce_after_panic().await;
                Err(LastFmApplicationShutdownError)
            }
        };
        // Join the authorization owner last: an in-flight flow can outlive
        // no part of the application drain.
        if let Some(authorization) = owner.authorization.take() {
            if authorization.shutdown().await.is_err() {
                result = Err(LastFmApplicationShutdownError);
            }
        }
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
    use crate::lastfm::policy::LastFmPolicyGeneration;
    use crate::lastfm::runtime::LastFmRuntimePhase;

    /// One live policy slot publishing generation 1 with the same empty
    /// opt-in set the test activations carry.
    pub(super) fn live_policy_for_test() -> LastFmLivePolicy {
        let live = LastFmLivePolicy::default();
        live.publish(LastFmPolicyGeneration::for_test(1, HashSet::new()));
        live
    }
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

    pub(super) struct PendingTransport;

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

    pub(super) struct FixedClock;

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

    pub(super) async fn migrated_database() -> DatabaseConnection {
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

    /// Spawns an application owner over a migrated in-memory database, a
    /// stored vault session, the pending transport, the fixed clock, and the
    /// given live policy, with the source registry left running for the
    /// caller to shut down: the shared fixture for the disconnect/successor
    /// and runtime-control compositions below.
    async fn spawn_owner_with_stored_session_and_database_attached(
        live: &LastFmLivePolicy,
    ) -> (
        LastFmApplicationHandle,
        LastFmApplicationShutdown,
        Arc<FixedCredentials>,
        SourceRegistry,
        LastFmPlaybackCoordinatorOwner,
    ) {
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
            live.clone(),
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        (
            handle,
            shutdown,
            credentials,
            source_registry,
            coordinator_owner,
        )
    }

    /// Asserts a forwarded runtime control resolved to the typed `NotActive`
    /// refusal a dormant owner must surface instead of executing.
    fn assert_not_active_refusal<T: std::fmt::Debug>(
        result: Result<T, LastFmRuntimeAdmissionError>,
        unexpected: &str,
        refusal: &str,
    ) {
        match result {
            Err(LastFmRuntimeAdmissionError::NotActive) => {}
            Err(admission) => panic!("{unexpected}: {admission:?}"),
            Ok(_) => panic!("{refusal}"),
        }
    }

    fn assert_forwarded_refusal<T: std::fmt::Debug>(
        result: Result<T, LastFmRuntimeAdmissionError>,
        expected: LastFmRuntimeAdmissionError,
        unexpected: &str,
        refusal: &str,
    ) {
        match result {
            Err(admission) if admission == expected => {}
            Err(admission) => panic!("{unexpected}: {admission:?}"),
            Ok(_) => panic!("{refusal}"),
        }
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
            live_policy_for_test(),
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
            live_policy_for_test(),
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
            live_policy_for_test(),
            ApplicationSpawnOptions {
                authorization: None,
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
        let (_handle, shutdown) = spawn_lastfm_application_owner(
            first_binding,
            tokio::runtime::Handle::current(),
            live_policy_for_test(),
        )
        .expect("first production owner claim");

        let second_registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let mut second_coordinator_owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let second_binding = second_coordinator_owner
            .bind_window(second_registry.clone())
            .expect("second window binding");
        assert_eq!(
            spawn_lastfm_application_owner(
                second_binding,
                tokio::runtime::Handle::current(),
                live_policy_for_test(),
            )
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
            live_policy_for_test(),
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");

        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            1,
            HashSet::new(),
        )
        .expect("local-only activation policy");
        let activated = handle
            .try_activate(activation)
            .expect("activation admitted");
        let duplicate = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            1,
            HashSet::new(),
        )
        .expect("second well-formed activation policy");
        // A second activation is refused while the first is admitted, not
        // terminal for the owner: the first generation still completes.
        assert_eq!(
            handle.try_activate(duplicate).unwrap_err(),
            LastFmApplicationAdmissionError::GenerationActive
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
            live_policy_for_test(),
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            1,
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

    /// LF2 composition: a real generation activates with the typed runtime
    /// snapshot relayed into the application status, and a completed
    /// disconnect-and-purge drains that generation in the bridge-before-
    /// runtime order back to AwaitingConsent with the relayed snapshot
    /// cleared.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_drains_generation_and_clears_the_relayed_runtime() {
        let live = live_policy_for_test();
        let (handle, shutdown, credentials, source_registry, mut coordinator_owner) =
            spawn_owner_with_stored_session_and_database_attached(&live).await;

        // First generation activates and the relay publishes its runtime
        // snapshot inside the Active status before completion resolves.
        let generation = live.snapshot();
        handle
            .try_activate(
                LastFmApplicationActivation::issue_from_policy_generation(&generation)
                    .expect("generation 1 grants activation authority"),
            )
            .expect("activation admitted")
            .wait()
            .await
            .expect("first generation activates");
        let status = *handle.subscribe_status().borrow();
        assert_eq!(status.phase, LastFmApplicationPhase::Active);
        assert_eq!(
            status.runtime.map(|runtime| runtime.phase),
            Some(LastFmRuntimePhase::Active)
        );

        // Disconnect-and-purge completes, drains the generation, and leaves
        // the owner ready for a successor: the clean-disconnect path clears
        // the relayed snapshot and republishes AwaitingConsent before the
        // command resolves.
        let disconnect = handle
            .try_disconnect_and_purge()
            .expect("disconnect admitted");
        let outcome = tokio::time::timeout(Duration::from_secs(2), disconnect.wait())
            .await
            .expect("disconnect deadline")
            .expect("disconnect purged and drained");
        assert_eq!(outcome, 0);
        let status = *handle.subscribe_status().borrow();
        assert_eq!(status.phase, LastFmApplicationPhase::AwaitingConsent);
        assert_eq!(status.runtime, None);
        // Two vault loads: the first runtime start and the disconnect purge
        // reading the exact record it deletes.
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 2);

        assert_owner_drains(shutdown, &mut coordinator_owner).await;
        source_registry.shutdown().wait().await;
    }

    /// LF2 composition: after a disconnect-and-purge drains the generation,
    /// a successor activation — once the reconnect flow has restored the
    /// vault session the runtime purge deleted — re-activates a fresh
    /// runtime generation under the same live policy generation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn successor_activation_reactivates_after_disconnect_drains() {
        let live = live_policy_for_test();
        let (handle, shutdown, credentials, source_registry, mut coordinator_owner) =
            spawn_owner_with_stored_session_and_database_attached(&live).await;

        // The first generation runs and is then drained by the completed
        // disconnect-and-purge, whose deadline is respected.
        let generation = live.snapshot();
        handle
            .try_activate(
                LastFmApplicationActivation::issue_from_policy_generation(&generation)
                    .expect("generation 1 grants activation authority"),
            )
            .expect("activation admitted")
            .wait()
            .await
            .expect("first generation activates");
        let disconnect = handle
            .try_disconnect_and_purge()
            .expect("disconnect admitted");
        tokio::time::timeout(Duration::from_secs(2), disconnect.wait())
            .await
            .expect("disconnect deadline")
            .expect("disconnect purged and drained");

        // The runtime purge deleted the vault record, so the reconnect flow
        // stores a fresh session before the successor runtime can start.
        credentials
            .save(&stored_session())
            .expect("reconnect stores a fresh session");
        handle
            .try_activate(
                LastFmApplicationActivation::issue_from_policy_generation(&generation)
                    .expect("the still-live generation grants successor authority"),
            )
            .expect("successor activation admitted")
            .wait()
            .await
            .expect("successor generation activates");
        let status = *handle.subscribe_status().borrow();
        assert_eq!(status.phase, LastFmApplicationPhase::Active);
        assert_eq!(
            status.runtime.map(|runtime| runtime.phase),
            Some(LastFmRuntimePhase::Active)
        );
        // Three vault loads: the first runtime start, the disconnect purge
        // reading the exact record it deletes, and the successor runtime
        // start.
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 3);

        assert_owner_drains(shutdown, &mut coordinator_owner).await;
        source_registry.shutdown().wait().await;
    }

    /// LF2 composition: a dormant owner with a database attached refuses
    /// every runtime control without touching a runtime — disconnect with
    /// the typed GenerationInactive, and reauthorization, recovery capture,
    /// and resume with the runtime NotActive refusal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_controls_refuse_on_a_dormant_owner_without_a_runtime() {
        let live = live_policy_for_test();
        let (handle, shutdown, _credentials, source_registry, mut coordinator_owner) =
            spawn_owner_with_stored_session_and_database_attached(&live).await;

        // Dormant owner: every control refuses without touching a runtime.
        let disconnect = handle
            .try_disconnect_and_purge()
            .expect("disconnect admitted");
        assert_eq!(
            disconnect.wait().await,
            Err(LastFmApplicationDisconnectError::GenerationInactive)
        );
        let reauthorization = handle
            .try_reauthorize_same_account(LastFmAuthorizationGrant::for_test(
                "reauthorization-listener",
                "fedcba9876543210fedcba9876543210",
            ))
            .expect("reauthorization admitted");
        assert_not_active_refusal(
            reauthorization.wait().await.expect("forward admitted"),
            "unexpected reauthorization refusal",
            "reauthorization must be refused while dormant",
        );
        let recovery = handle
            .try_issue_manual_pause_recovery()
            .expect("recovery capture admitted");
        assert_not_active_refusal(
            recovery.wait().await.expect("forward admitted"),
            "unexpected recovery refusal",
            "recovery capture must be refused while dormant",
        );
        let resume = handle
            .try_resume_after_manual_recovery(LastFmManualPauseRecovery::dangling_for_test(
                stored_session().account_binding(),
            ))
            .expect("resume admitted");
        assert_not_active_refusal(
            resume.wait().await.expect("forward admitted"),
            "unexpected resume refusal",
            "resume must be refused while dormant",
        );

        assert_owner_drains(shutdown, &mut coordinator_owner).await;
        source_registry.shutdown().wait().await;
    }

    /// LF2 composition: activate the owner's first published policy
    /// generation through the public activation path.
    async fn activate_first_policy_generation(
        handle: &LastFmApplicationHandle,
        live: &LastFmLivePolicy,
    ) {
        let generation = live.snapshot();
        handle
            .try_activate(
                LastFmApplicationActivation::issue_from_policy_generation(&generation)
                    .expect("generation 1 grants activation authority"),
            )
            .expect("activation admitted")
            .wait()
            .await
            .expect("generation activates");
    }

    /// LF2 composition: with an active generation, reauthorization and
    /// manual-pause recovery capture forward to the exact runtime and
    /// surface its own typed refusals for the not-ready states.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reauthorization_and_recovery_forward_to_the_active_generation_and_surface_its_typed_refusals(
    ) {
        let live = live_policy_for_test();
        let (handle, shutdown, _credentials, source_registry, mut coordinator_owner) =
            spawn_owner_with_stored_session_and_database_attached(&live).await;
        activate_first_policy_generation(&handle, &live).await;

        let reauthorization = handle
            .try_reauthorize_same_account(LastFmAuthorizationGrant::for_test(
                "reauthorization-listener",
                "fedcba9876543210fedcba9876543210",
            ))
            .expect("reauthorization admitted");
        assert_forwarded_refusal(
            reauthorization
                .wait()
                .await
                .expect("forwarded to the active runtime"),
            LastFmRuntimeAdmissionError::NotReadyForReauthorization,
            "unexpected reauthorization refusal",
            "reauthorization requires a code-9 paused runtime",
        );
        let recovery = handle
            .try_issue_manual_pause_recovery()
            .expect("recovery capture admitted");
        assert_forwarded_refusal(
            recovery
                .wait()
                .await
                .expect("forwarded to the active runtime"),
            LastFmRuntimeAdmissionError::NotReadyForManualRecovery,
            "unexpected recovery refusal",
            "recovery capture requires a paused runtime",
        );

        assert_owner_drains(shutdown, &mut coordinator_owner).await;
        source_registry.shutdown().wait().await;
    }

    /// LF2 composition: with an active generation, resume-after-recovery
    /// forwards to the exact runtime, which refuses a foreign recovery
    /// authority with its typed refusal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_forward_to_the_active_generation_and_refuses_foreign_recovery_authority() {
        let live = live_policy_for_test();
        let (handle, shutdown, _credentials, source_registry, mut coordinator_owner) =
            spawn_owner_with_stored_session_and_database_attached(&live).await;
        activate_first_policy_generation(&handle, &live).await;

        let resume = handle
            .try_resume_after_manual_recovery(LastFmManualPauseRecovery::dangling_for_test(
                stored_session().account_binding(),
            ))
            .expect("resume admitted");
        assert_forwarded_refusal(
            resume
                .wait()
                .await
                .expect("forwarded to the active runtime"),
            LastFmRuntimeAdmissionError::NotReadyForManualRecovery,
            "unexpected resume refusal",
            "resume must refuse a foreign recovery authority",
        );

        assert_owner_drains(shutdown, &mut coordinator_owner).await;
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
            live_policy_for_test(),
            ApplicationSpawnOptions {
                authorization: None,
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
            1,
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
            live_policy_for_test(),
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            1,
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
            1,
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
            live_policy_for_test(),
            ApplicationSpawnOptions {
                authorization: None,
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
            1,
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
            live_policy_for_test(),
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
            1,
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
        // Completion is the observation boundary: the owner must publish the
        // terminal snapshot before waking this waiter.
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
            live_policy_for_test(),
            ApplicationSpawnOptions {
                authorization: None,
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
            1,
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
            1,
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
            live_policy_for_test(),
        );

        // Current-thread scheduling makes both commands cross admission
        // before the owner can observe either one. Close then wins the shared
        // gate before startup can begin.
        let attachment = handle
            .try_attach_database(database)
            .expect("database command queued");
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            1,
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
            1,
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
            LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(1, reserved)
                .unwrap_err(),
            LastFmApplicationAdmissionError::InvalidSourcePolicy
        );
        let mut oversized = HashSet::new();
        while oversized.len() <= MAX_ENABLED_REMOTE_SOURCES {
            oversized.insert(SourceId::random());
        }
        assert_eq!(
            LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(1, oversized)
                .unwrap_err(),
            LastFmApplicationAdmissionError::InvalidSourcePolicy
        );
        let activation = LastFmApplicationActivation::issue_after_explicit_consent_and_enablement(
            1,
            HashSet::new(),
        )
        .expect("empty local-only policy is valid");
        assert_eq!(
            format!("{activation:?}"),
            "LastFmApplicationActivation(<redacted>)"
        );
    }

    /// The activation authority must freeze exactly the source set the live
    /// policy generation exposes to queue capture, so dispatch cannot admit a
    /// source that capture never observed.
    #[test]
    fn activation_from_policy_generation_freezes_the_capture_set() {
        let enabled_source = SourceId::random();
        let generation = LastFmPolicyGeneration::for_test(4, HashSet::from([enabled_source]));
        let activation = LastFmApplicationActivation::issue_from_policy_generation(&generation)
            .expect("consented generation grants activation authority");
        assert_eq!(
            activation.enabled_remote_sources_for_test(),
            generation.queue_capture_remote_sources()
        );
        assert_eq!(activation.policy_generation(), 4);
    }

    /// A generation without current consent and enablement has no activation
    /// basis. Issuance must refuse rather than silently produce a local-only
    /// activation from a closed or disabled policy.
    #[test]
    fn activation_refuses_a_generation_without_current_consent() {
        assert_eq!(
            LastFmApplicationActivation::issue_from_policy_generation(
                &LastFmPolicyGeneration::default()
            )
            .unwrap_err(),
            LastFmApplicationAdmissionError::InvalidSourcePolicy
        );
    }

    /// Spawns a dormant application owner over unused vault credentials with
    /// an attached in-memory database: the shared fixture for the queued-
    /// activation refusal regressions below.
    async fn spawn_owner_with_unused_vault_and_database_attached() -> (
        LastFmLivePolicy,
        LastFmPlaybackCoordinatorOwner,
        LastFmApplicationHandle,
        LastFmApplicationShutdown,
    ) {
        let live = live_policy_for_test();
        let (coordinator_owner, coordinator) = binding();
        let (handle, shutdown) = spawn_with_dependencies(
            coordinator,
            tokio::runtime::Handle::current(),
            Arc::new(UnusedCredentials),
            Some(Arc::new(PendingTransport)),
            Arc::new(FixedClock),
            live.clone(),
        );
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory database");
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");
        (live, coordinator_owner, handle, shutdown)
    }

    /// Asserts the owner published the runtime-start refusal and stayed
    /// dormant, able to admit a later activation. Completion is the
    /// observation boundary: the snapshot is published before the waiter
    /// wakes.
    fn assert_dormant_runtime_start_refusal(handle: &LastFmApplicationHandle) {
        let status = *handle.subscribe_status().borrow();
        assert_eq!(status.phase, LastFmApplicationPhase::AwaitingConsent);
        assert_eq!(
            status.failure,
            Some(LastFmApplicationCommandError::RuntimeStart)
        );
    }

    /// Drains the owner and asserts a clean shutdown: the drain deadline is
    /// met, the barrier drains fully, and the playback coordinator observed
    /// the final state.
    pub(super) async fn assert_owner_drains(
        shutdown: LastFmApplicationShutdown,
        coordinator_owner: &mut LastFmPlaybackCoordinatorOwner,
    ) {
        let barrier = shutdown.barrier();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), shutdown.shutdown())
                .await
                .expect("application drain deadline"),
            Ok(LastFmApplicationShutdownReason::Drained)
        );
        assert_eq!(barrier.wait().await, Ok(()));
        assert_eq!(barrier.state(), LastFmApplicationDrainState::Drained);
        assert_eq!(
            coordinator_owner.shutdown(),
            LastFmPlaybackCoordinatorOutcome::Applied
        );
    }

    /// A queued application activation freezes its issuing generation. When
    /// the live policy is replaced by a still-consented, enabled successor
    /// before the owner processes the command, the activation is spent: the
    /// owner must refuse before minting a runtime activation, so no
    /// runtime starts and the vault is never read — the successor's source
    /// set must never be governed under the predecessor's consent.
    #[tokio::test]
    async fn superseded_generation_refuses_queued_activation_before_runtime_start() {
        let (live, mut coordinator_owner, handle, shutdown) =
            spawn_owner_with_unused_vault_and_database_attached().await;
        let activation =
            LastFmApplicationActivation::issue_from_policy_generation(&live.snapshot())
                .expect("generation 1 grants activation authority");
        let queued = handle
            .try_activate(activation)
            .expect("activation admitted while generation 1 is live");

        // Enabled-to-enabled replacement inside the queue-processing window:
        // generation 2 is consented and enabled, but the queued authority
        // was issued by generation 1.
        live.publish(LastFmPolicyGeneration::for_test(2, HashSet::new()));

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), queued.wait())
                .await
                .expect("superseded activation deadline"),
            Err(LastFmApplicationCommandError::RuntimeStart)
        );
        assert_dormant_runtime_start_refusal(&handle);
        assert_owner_drains(shutdown, &mut coordinator_owner).await;
    }

    /// The refusal is by generation identity, not by the successor's
    /// consent state: a revoked or disabled replacement goes through the
    /// same refusal, and the vault still is never read.
    #[tokio::test]
    async fn revoked_generation_refuses_queued_activation_before_runtime_start() {
        let (live, mut coordinator_owner, handle, shutdown) =
            spawn_owner_with_unused_vault_and_database_attached().await;
        let activation =
            LastFmApplicationActivation::issue_from_policy_generation(&live.snapshot())
                .expect("generation 1 grants activation authority");
        let queued = handle
            .try_activate(activation)
            .expect("activation admitted while generation 1 is live");

        // Consent revoked: the closed successor grants no activation
        // authority and no longer matches the queued generation.
        live.publish(LastFmPolicyGeneration::default());

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), queued.wait())
                .await
                .expect("revoked activation deadline"),
            Err(LastFmApplicationCommandError::RuntimeStart)
        );
        assert_dormant_runtime_start_refusal(&handle);
        assert_owner_drains(shutdown, &mut coordinator_owner).await;
    }

    /// The generation gate refuses only superseded authorities: a queued
    /// activation whose issuing generation is still live activates the real
    /// runtime and coordinator exactly as before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_generation_queued_activation_still_activates() {
        let live = live_policy_for_test();
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
            live.clone(),
        );
        handle
            .try_attach_database(database)
            .expect("database admitted")
            .wait()
            .await
            .expect("database attached");

        let activation =
            LastFmApplicationActivation::issue_from_policy_generation(&live.snapshot())
                .expect("generation 1 grants activation authority");
        handle
            .try_activate(activation)
            .expect("activation admitted")
            .wait()
            .await
            .expect("same-generation activation starts the runtime");
        assert_eq!(
            handle.subscribe_status().borrow().phase,
            LastFmApplicationPhase::Active
        );
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 1);

        assert_owner_drains(shutdown, &mut coordinator_owner).await;
        source_registry.shutdown().wait().await;
    }
}

#[cfg(test)]
#[path = "production_account_tests.rs"]
mod account_tests;
