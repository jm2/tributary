//! Latest-request coordination for server-playlist operations.
//!
//! This module deliberately has no GTK, database, backend, or source-registry
//! dependencies. Callers retain those capabilities in an operation closure;
//! the coordinator supplies only cancellation and a final admission point.
//! One owner task serializes request starts and admission decisions so their
//! order is unambiguous. Work on different keys remains concurrent; work on
//! one key waits for an admitted predecessor's task and guard to settle.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{oneshot, watch};
use tokio::task::{AbortHandle, Id, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::architecture::{NativePlaylistId, SourceId};

type BoxedOperationFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
type BoxedOperation =
    Box<dyn FnOnce(ServerPlaylistOperationContext) -> BoxedOperationFuture + Send + 'static>;

/// A source-wide lane, used for listing and one reconnect sweep.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ServerPlaylistSourceKey(SourceId);

impl ServerPlaylistSourceKey {
    pub const fn new(source_id: SourceId) -> Self {
        Self(source_id)
    }
}

impl fmt::Debug for ServerPlaylistSourceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistSourceKey")
            .finish_non_exhaustive()
    }
}

/// One source-scoped native playlist lane, shared by Import Copy and Keep
/// Synced so the two operations cannot race before final admission.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ServerPlaylistRemoteKey {
    source_id: SourceId,
    native_playlist_id: NativePlaylistId,
}

impl ServerPlaylistRemoteKey {
    pub fn new(source_id: SourceId, native_playlist_id: NativePlaylistId) -> Self {
        Self {
            source_id,
            native_playlist_id,
        }
    }
}

impl fmt::Debug for ServerPlaylistRemoteKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistRemoteKey")
            .finish_non_exhaustive()
    }
}

/// One durable local-playlist lane, shared by manual and reconnect recovery
/// operations for the same linked mirror.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ServerPlaylistLocalKey(Arc<str>);

impl ServerPlaylistLocalKey {
    pub fn new(playlist_id: impl Into<Arc<str>>) -> Self {
        Self(playlist_id.into())
    }
}

impl fmt::Debug for ServerPlaylistLocalKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistLocalKey")
            .finish_non_exhaustive()
    }
}

/// The three deliberately disjoint latest-request lanes.
#[derive(Clone, Eq, Hash, PartialEq)]
pub enum ServerPlaylistOperationKey {
    Source(ServerPlaylistSourceKey),
    RemotePlaylist(ServerPlaylistRemoteKey),
    LocalPlaylist(ServerPlaylistLocalKey),
}

impl ServerPlaylistOperationKey {
    pub const fn source(source_id: SourceId) -> Self {
        Self::Source(ServerPlaylistSourceKey::new(source_id))
    }

    pub fn remote_playlist(source_id: SourceId, native_playlist_id: NativePlaylistId) -> Self {
        Self::RemotePlaylist(ServerPlaylistRemoteKey::new(source_id, native_playlist_id))
    }

    pub fn local_playlist(playlist_id: impl Into<Arc<str>>) -> Self {
        Self::LocalPlaylist(ServerPlaylistLocalKey::new(playlist_id))
    }

    pub const fn class(&self) -> ServerPlaylistOperationKeyClass {
        match self {
            Self::Source(_) => ServerPlaylistOperationKeyClass::Source,
            Self::RemotePlaylist(_) => ServerPlaylistOperationKeyClass::RemotePlaylist,
            Self::LocalPlaylist(_) => ServerPlaylistOperationKeyClass::LocalPlaylist,
        }
    }
}

impl From<ServerPlaylistSourceKey> for ServerPlaylistOperationKey {
    fn from(key: ServerPlaylistSourceKey) -> Self {
        Self::Source(key)
    }
}

impl From<ServerPlaylistRemoteKey> for ServerPlaylistOperationKey {
    fn from(key: ServerPlaylistRemoteKey) -> Self {
        Self::RemotePlaylist(key)
    }
}

impl From<ServerPlaylistLocalKey> for ServerPlaylistOperationKey {
    fn from(key: ServerPlaylistLocalKey) -> Self {
        Self::LocalPlaylist(key)
    }
}

impl fmt::Debug for ServerPlaylistOperationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(key) => formatter.debug_tuple("Source").field(key).finish(),
            Self::RemotePlaylist(key) => {
                formatter.debug_tuple("RemotePlaylist").field(key).finish()
            }
            Self::LocalPlaylist(key) => formatter.debug_tuple("LocalPlaylist").field(key).finish(),
        }
    }
}

/// Content-free key classification suitable for diagnostics and metrics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistOperationKeyClass {
    Source,
    RemotePlaylist,
    LocalPlaylist,
}

/// A strictly positive generation scoped to one operation key.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ServerPlaylistOperationGeneration(u64);

impl ServerPlaylistOperationGeneration {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Immediate result of a nonblocking request submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistRequestStatus {
    Queued,
    Closed,
}

struct CoordinatorSeal;

/// Coordinator-scoped start order for a logical request which may fan out
/// only after asynchronous discovery.
///
/// Reserve this before a reconnect list/database step, then pass the same
/// stamp to each [`ServerPlaylistCoordinatorHandle::begin_if_not_newer`]
/// call. A manual request begun meanwhile has a newer stamp and suppresses
/// the delayed reconnect work for an overlapping key.
#[derive(Clone)]
pub struct ServerPlaylistRequestStamp {
    sequence: u64,
    coordinator: Arc<CoordinatorSeal>,
}

impl fmt::Debug for ServerPlaylistRequestStamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistRequestStamp")
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}

/// Closed reasons a logical request stamp could not be reserved.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ServerPlaylistRequestStampError {
    #[error("server playlist coordinator is closed")]
    Closed,
    #[error("server playlist request order is exhausted")]
    Exhausted,
}

struct HandleInner {
    commands: async_channel::Sender<Command>,
    coordinator: Arc<CoordinatorSeal>,
    request_sequence: AtomicU64,
    submissions: Mutex<()>,
    #[cfg(test)]
    after_direct_reserve_hook: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        // Operation contexts hold bare sender clones. Explicitly closing here
        // means dropping the last public handle still closes the lane.
        self.commands.close();
    }
}

/// Cloneable, nonblocking submission side of the coordinator.
#[derive(Clone)]
pub struct ServerPlaylistCoordinatorHandle {
    inner: Arc<HandleInner>,
}

impl ServerPlaylistCoordinatorHandle {
    /// Reserve the start order of one logical request without waiting for the
    /// owner. Request-order exhaustion closes the entire coordinator rather
    /// than permitting an ambiguous comparison.
    pub fn reserve_request_stamp(
        &self,
    ) -> Result<ServerPlaylistRequestStamp, ServerPlaylistRequestStampError> {
        let Some(_submission) = self.lock_submission() else {
            return Err(ServerPlaylistRequestStampError::Closed);
        };
        self.reserve_request_stamp_locked()
    }

    fn reserve_request_stamp_locked(
        &self,
    ) -> Result<ServerPlaylistRequestStamp, ServerPlaylistRequestStampError> {
        if self.inner.commands.is_closed() {
            return Err(ServerPlaylistRequestStampError::Closed);
        }
        let sequence = self
            .inner
            .request_sequence
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                self.inner.commands.close();
                ServerPlaylistRequestStampError::Exhausted
            })?
            + 1;
        if self.inner.commands.is_closed() {
            return Err(ServerPlaylistRequestStampError::Closed);
        }
        Ok(ServerPlaylistRequestStamp {
            sequence,
            coordinator: Arc::clone(&self.inner.coordinator),
        })
    }

    /// Queue one operation without waiting for owner, backend, or database
    /// work. The operation future starts only if the owner accepts a fresh
    /// per-key generation.
    pub fn request<F, Fut>(
        &self,
        key: impl Into<ServerPlaylistOperationKey>,
        operation: F,
    ) -> ServerPlaylistRequestStatus
    where
        F: FnOnce(ServerPlaylistOperationContext) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let key = key.into();
        let operation: BoxedOperation =
            Box::new(move |context| Box::pin(operation(context)) as BoxedOperationFuture);
        let Some(submission) = self.lock_submission() else {
            return ServerPlaylistRequestStatus::Closed;
        };
        let Ok(stamp) = self.reserve_request_stamp_locked() else {
            drop(submission);
            drop(operation);
            return ServerPlaylistRequestStatus::Closed;
        };
        #[cfg(test)]
        self.run_after_direct_reserve_hook();
        let (status, rejected) = self.try_send_start_locked(Command::Start {
            key,
            request_sequence: stamp.sequence,
            operation,
        });
        drop(submission);
        drop(rejected);
        status
    }

    /// Queue work with a previously reserved logical-request order.
    ///
    /// The owner suppresses this work if it has already observed an equal or
    /// newer stamp for the key. Equal stamps are idempotently accepted at
    /// most once per key, which also closes accidental duplicate fan-out.
    pub fn begin_if_not_newer<F, Fut>(
        &self,
        key: impl Into<ServerPlaylistOperationKey>,
        stamp: &ServerPlaylistRequestStamp,
        operation: F,
    ) -> ServerPlaylistRequestStatus
    where
        F: FnOnce(ServerPlaylistOperationContext) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if !Arc::ptr_eq(&self.inner.coordinator, &stamp.coordinator) {
            // A foreign stamp has no ordering meaning in this owner. Treating
            // it as fresh could admit stale work, so close the lane.
            self.inner.commands.close();
            return ServerPlaylistRequestStatus::Closed;
        }
        let key = key.into();
        let operation: BoxedOperation =
            Box::new(move |context| Box::pin(operation(context)) as BoxedOperationFuture);
        let command = Command::Start {
            key,
            request_sequence: stamp.sequence,
            operation,
        };
        let Some(submission) = self.lock_submission() else {
            drop(command);
            return ServerPlaylistRequestStatus::Closed;
        };
        let (status, rejected) = self.try_send_start_locked(command);
        drop(submission);
        drop(rejected);
        status
    }

    fn try_send_start_locked(
        &self,
        command: Command,
    ) -> (ServerPlaylistRequestStatus, Option<Command>) {
        match self.inner.commands.try_send(command) {
            Ok(()) => (ServerPlaylistRequestStatus::Queued, None),
            Err(async_channel::TrySendError::Closed(command)) => {
                (ServerPlaylistRequestStatus::Closed, Some(command))
            }
            Err(async_channel::TrySendError::Full(command)) => {
                // The command channel is unbounded. If its implementation ever
                // reports Full, continuing would lose latest-request ordering.
                self.inner.commands.close();
                (ServerPlaylistRequestStatus::Closed, Some(command))
            }
        }
    }

    fn lock_submission(&self) -> Option<MutexGuard<'_, ()>> {
        match self.inner.submissions.lock() {
            Ok(submission) => Some(submission),
            Err(_) => {
                // A poisoned ordering gate can no longer prove reserve/send
                // atomicity, so fail the entire coordinator closed.
                self.inner.commands.close();
                None
            }
        }
    }

    #[cfg(test)]
    fn install_after_direct_reserve_hook(&self, hook: impl FnOnce() + Send + 'static) {
        let mut installed = self
            .inner
            .after_direct_reserve_hook
            .lock()
            .expect("direct-reserve test hook lock");
        assert!(installed.replace(Box::new(hook)).is_none());
    }

    #[cfg(test)]
    fn run_after_direct_reserve_hook(&self) {
        let hook = match self.inner.after_direct_reserve_hook.lock() {
            Ok(mut installed) => installed.take(),
            Err(_) => {
                self.inner.commands.close();
                None
            }
        };
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Close admission immediately for every clone. Already queued or active
    /// pre-admission work is cancelled by the owner; admitted work is drained.
    pub fn close(&self) -> bool {
        self.inner.commands.close()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.commands.is_closed()
    }
}

impl fmt::Debug for ServerPlaylistCoordinatorHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistCoordinatorHandle")
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// Pre-admission authority supplied to one operation future.
///
/// Network and staging work should select on [`Self::cancelled`]. The context
/// is consumed by [`Self::admit`], preventing one operation from attempting
/// final admission more than once.
pub struct ServerPlaylistOperationContext {
    job_id: JobId,
    key_class: ServerPlaylistOperationKeyClass,
    generation: ServerPlaylistOperationGeneration,
    cancellation: CancellationToken,
    commands: async_channel::Sender<Command>,
}

impl ServerPlaylistOperationContext {
    pub const fn key_class(&self) -> ServerPlaylistOperationKeyClass {
        self.key_class
    }

    pub const fn generation(&self) -> ServerPlaylistOperationGeneration {
        self.generation
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    /// Request final admission from the owner. The owner processes this on
    /// the same command lane as new starts: whichever command it observes
    /// first is the linearization winner.
    pub async fn admit(self) -> Result<ServerPlaylistAdmissionGuard, ServerPlaylistAdmissionError> {
        if self.cancellation.is_cancelled() {
            return Err(ServerPlaylistAdmissionError::Superseded);
        }

        let (response, receiver) = oneshot::channel();
        let command = Command::Admit {
            job_id: self.job_id,
            response,
        };
        match self.commands.try_send(command) {
            Ok(()) => {}
            Err(async_channel::TrySendError::Closed(_)) => {
                return Err(ServerPlaylistAdmissionError::Closed);
            }
            Err(async_channel::TrySendError::Full(_)) => {
                self.commands.close();
                return Err(ServerPlaylistAdmissionError::Closed);
            }
        }
        receiver
            .await
            .unwrap_or(Err(ServerPlaylistAdmissionError::Closed))
    }
}

impl fmt::Debug for ServerPlaylistOperationContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistOperationContext")
            .field("key_class", &self.key_class)
            .field("generation", &self.generation)
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Closed reasons final admission was denied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ServerPlaylistAdmissionError {
    #[error("server playlist operation was superseded before admission")]
    Superseded,
    #[error("server playlist operation admission is closed")]
    Closed,
}

/// Move-only proof that one generation crossed the coordinator's final
/// admission point.
///
/// Persistence should retain this value through commit or rollback. Its
/// lifetime is independently tracked, so shutdown still drains correctly if
/// the guard temporarily moves into a detached commit worker.
#[must_use = "the admission guard must be retained through commit or rollback"]
pub struct ServerPlaylistAdmissionGuard {
    job_id: Option<JobId>,
    releases: tokio::sync::mpsc::UnboundedSender<JobId>,
}

impl Drop for ServerPlaylistAdmissionGuard {
    fn drop(&mut self) {
        if let Some(job_id) = self.job_id.take() {
            let _ = self.releases.send(job_id);
        }
    }
}

impl fmt::Debug for ServerPlaylistAdmissionGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistAdmissionGuard")
            .field("admitted", &self.job_id.is_some())
            .finish_non_exhaustive()
    }
}

/// Why the owner stopped accepting work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistCoordinatorShutdownReason {
    Closed,
    GenerationExhausted,
}

/// Closed failure returned if the owner task itself was externally aborted or
/// panicked. Raw task diagnostics are deliberately not retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("server playlist coordinator owner stopped unexpectedly")]
pub struct ServerPlaylistCoordinatorShutdownError;

/// Lifecycle-owned join side of the coordinator.
pub struct ServerPlaylistCoordinatorShutdown {
    commands: async_channel::Sender<Command>,
    owner: Option<JoinHandle<ServerPlaylistCoordinatorShutdownReason>>,
    completion: watch::Receiver<bool>,
}

impl ServerPlaylistCoordinatorShutdown {
    /// Cloneable persistent completion signal for observers which do not own
    /// the sole owner-task join handle.
    pub fn barrier(&self) -> ServerPlaylistCoordinatorBarrier {
        ServerPlaylistCoordinatorBarrier {
            completion: self.completion.clone(),
        }
    }

    /// Close the public gate, cancel all tracked pre-admission work, and wait
    /// for every admitted operation future and admission guard to finish.
    pub async fn shutdown(
        mut self,
    ) -> Result<ServerPlaylistCoordinatorShutdownReason, ServerPlaylistCoordinatorShutdownError>
    {
        self.commands.close();
        self.join_owner().await
    }

    async fn join_owner(
        &mut self,
    ) -> Result<ServerPlaylistCoordinatorShutdownReason, ServerPlaylistCoordinatorShutdownError>
    {
        let owner = self
            .owner
            .take()
            .ok_or(ServerPlaylistCoordinatorShutdownError)?;
        owner
            .await
            .map_err(|_| ServerPlaylistCoordinatorShutdownError)
    }
}

impl Drop for ServerPlaylistCoordinatorShutdown {
    fn drop(&mut self) {
        // A dropped join side must never leave admission open. The owner task
        // remains detached long enough to drain already-admitted work.
        self.commands.close();
    }
}

impl fmt::Debug for ServerPlaylistCoordinatorShutdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistCoordinatorShutdown")
            .field("closed", &self.commands.is_closed())
            .field(
                "owner_finished",
                &self.owner.as_ref().is_none_or(JoinHandle::is_finished),
            )
            .finish_non_exhaustive()
    }
}

/// Cloneable persistent signal that completes once the owner task exits.
///
/// Each wait uses a private watch receiver, so repeated and concurrent calls
/// on the same value are independent. A dropped completion sender is also
/// terminal: an aborted or panicking owner can never strand window shutdown.
#[derive(Clone)]
pub struct ServerPlaylistCoordinatorBarrier {
    completion: watch::Receiver<bool>,
}

impl ServerPlaylistCoordinatorBarrier {
    pub fn is_complete(&self) -> bool {
        *self.completion.borrow() || self.completion.has_changed().is_err()
    }

    pub async fn wait(&self) {
        if self.is_complete() {
            return;
        }
        let mut completion = self.completion.clone();
        while !*completion.borrow() {
            if completion.changed().await.is_err() {
                return;
            }
        }
    }
}

impl fmt::Debug for ServerPlaylistCoordinatorBarrier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistCoordinatorBarrier")
            .field("complete", &self.is_complete())
            .finish()
    }
}

/// Start the sole owner task and return its nonblocking handle and lifecycle
/// join side. This must be called from a Tokio runtime.
pub fn spawn_server_playlist_coordinator() -> (
    ServerPlaylistCoordinatorHandle,
    ServerPlaylistCoordinatorShutdown,
) {
    spawn_server_playlist_coordinator_with_generation_ceiling(u64::MAX)
}

fn spawn_server_playlist_coordinator_with_generation_ceiling(
    generation_ceiling: u64,
) -> (
    ServerPlaylistCoordinatorHandle,
    ServerPlaylistCoordinatorShutdown,
) {
    let (command_sender, command_receiver) = async_channel::unbounded();
    let (release_sender, release_receiver) = tokio::sync::mpsc::unbounded_channel();
    let handle = ServerPlaylistCoordinatorHandle {
        inner: Arc::new(HandleInner {
            commands: command_sender.clone(),
            coordinator: Arc::new(CoordinatorSeal),
            request_sequence: AtomicU64::new(0),
            submissions: Mutex::new(()),
            #[cfg(test)]
            after_direct_reserve_hook: Mutex::new(None),
        }),
    };
    let owner = CoordinatorOwner {
        command_sender: command_sender.clone(),
        command_receiver,
        release_sender,
        release_receiver,
        generations: HashMap::new(),
        jobs: HashMap::new(),
        task_jobs: HashMap::new(),
        tasks: JoinSet::new(),
        generation_ceiling,
    };
    let (completion_sender, completion) = watch::channel(false);
    let owner = tokio::spawn(async move {
        let reason = owner.run().await;
        let _ = completion_sender.send(true);
        reason
    });
    (
        handle,
        ServerPlaylistCoordinatorShutdown {
            commands: command_sender,
            owner: Some(owner),
            completion,
        },
    )
}

enum Command {
    Start {
        key: ServerPlaylistOperationKey,
        request_sequence: u64,
        operation: BoxedOperation,
    },
    Admit {
        job_id: JobId,
        response:
            oneshot::Sender<Result<ServerPlaylistAdmissionGuard, ServerPlaylistAdmissionError>>,
    },
}

struct JobSeal;

#[derive(Clone)]
struct JobId(Arc<JobSeal>);

impl JobId {
    fn new() -> Self {
        Self(Arc::new(JobSeal))
    }
}

impl PartialEq for JobId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for JobId {}

impl Hash for JobId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

struct KeyState {
    request_sequence: u64,
    generation: u64,
    pre_admission: Option<JobId>,
    admitted: Option<JobId>,
    pending: Option<PendingStart>,
}

struct PendingStart {
    generation: ServerPlaylistOperationGeneration,
    operation: BoxedOperation,
}

enum JobPhase {
    PreAdmission(CancellationToken),
    Superseded,
    Admitted {
        task_finished: bool,
        guard_released: bool,
    },
}

struct JobState {
    key: ServerPlaylistOperationKey,
    generation: ServerPlaylistOperationGeneration,
    phase: JobPhase,
    abort: AbortHandle,
}

struct CoordinatorOwner {
    command_sender: async_channel::Sender<Command>,
    command_receiver: async_channel::Receiver<Command>,
    release_sender: tokio::sync::mpsc::UnboundedSender<JobId>,
    release_receiver: tokio::sync::mpsc::UnboundedReceiver<JobId>,
    generations: HashMap<ServerPlaylistOperationKey, KeyState>,
    jobs: HashMap<JobId, JobState>,
    task_jobs: HashMap<Id, JobId>,
    tasks: JoinSet<JobId>,
    generation_ceiling: u64,
}

impl CoordinatorOwner {
    async fn run(mut self) -> ServerPlaylistCoordinatorShutdownReason {
        let reason = loop {
            if self.command_receiver.is_closed() {
                break ServerPlaylistCoordinatorShutdownReason::Closed;
            }

            tokio::select! {
                command = self.command_receiver.recv() => {
                    match command {
                        Ok(Command::Start {
                            key,
                            request_sequence,
                            operation,
                        }) => {
                            if !self.start(key, request_sequence, operation) {
                                break ServerPlaylistCoordinatorShutdownReason::GenerationExhausted;
                            }
                        }
                        Ok(Command::Admit { job_id, response }) => {
                            self.admit(&job_id, response);
                        }
                        Err(_) => break ServerPlaylistCoordinatorShutdownReason::Closed,
                    }
                }
                Some(job_id) = self.release_receiver.recv() => {
                    self.release_guard(&job_id);
                }
                joined = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    if let Some(joined) = joined {
                        self.finish_task(joined);
                    }
                }
            }
        };

        self.command_receiver.close();
        self.cancel_for_shutdown();
        self.drain().await;
        reason
    }

    /// Returns false only when accepting this start would reuse a generation.
    fn start(
        &mut self,
        key: ServerPlaylistOperationKey,
        request_sequence: u64,
        operation: BoxedOperation,
    ) -> bool {
        let key_state = self.generations.entry(key.clone()).or_insert(KeyState {
            request_sequence: 0,
            generation: 0,
            pre_admission: None,
            admitted: None,
            pending: None,
        });
        if request_sequence <= key_state.request_sequence {
            return true;
        }
        let Some(next_generation) = key_state.generation.checked_add(1) else {
            return false;
        };
        if next_generation > self.generation_ceiling {
            return false;
        }

        key_state.request_sequence = request_sequence;
        key_state.generation = next_generation;
        let generation = ServerPlaylistOperationGeneration(next_generation);

        // Admission is the irrevocable persistence edge. A newer request can
        // cancel a predecessor which has not crossed it, but it must wait for
        // both the admitted task and its move-only guard to settle before it
        // prepares state from the same durable key. Retain only the newest
        // such deferred start; its request and generation still consume their
        // monotonic order immediately.
        if key_state.admitted.is_some() {
            key_state.pending = Some(PendingStart {
                generation,
                operation,
            });
            return true;
        }

        let previous = key_state.pre_admission.take();
        if let Some(previous) = previous {
            if let Some(previous) = self.jobs.get_mut(&previous) {
                if let JobPhase::PreAdmission(cancellation) = &previous.phase {
                    cancellation.cancel();
                    previous.phase = JobPhase::Superseded;
                }
            }
        }

        self.launch(key, generation, operation);
        true
    }

    fn launch(
        &mut self,
        key: ServerPlaylistOperationKey,
        generation: ServerPlaylistOperationGeneration,
        operation: BoxedOperation,
    ) {
        let cancellation = CancellationToken::new();
        let job_id = JobId::new();
        let context = ServerPlaylistOperationContext {
            job_id: job_id.clone(),
            key_class: key.class(),
            generation,
            cancellation: cancellation.clone(),
            commands: self.command_sender.clone(),
        };
        let returned_job_id = job_id.clone();
        let abort = self.tasks.spawn(async move {
            operation(context).await;
            returned_job_id
        });
        self.task_jobs.insert(abort.id(), job_id.clone());
        self.generations
            .get_mut(&key)
            .expect("launched key state exists")
            .pre_admission = Some(job_id.clone());
        self.jobs.insert(
            job_id.clone(),
            JobState {
                key,
                generation,
                phase: JobPhase::PreAdmission(cancellation),
                abort,
            },
        );
    }

    fn admit(
        &mut self,
        job_id: &JobId,
        response: oneshot::Sender<
            Result<ServerPlaylistAdmissionGuard, ServerPlaylistAdmissionError>,
        >,
    ) {
        let admitted = self.jobs.get(job_id).is_some_and(|job| {
            matches!(job.phase, JobPhase::PreAdmission(_))
                && self.generations.get(&job.key).is_some_and(|state| {
                    state.generation == job.generation.get()
                        && state.pre_admission.as_ref() == Some(job_id)
                })
        });
        if !admitted {
            let _ = response.send(Err(ServerPlaylistAdmissionError::Superseded));
            return;
        }

        let job = self.jobs.get_mut(job_id).expect("admitted job exists");
        if let Some(state) = self.generations.get_mut(&job.key) {
            if state.pre_admission.as_ref() == Some(job_id) {
                state.pre_admission = None;
            }
            debug_assert!(state.admitted.is_none());
            state.admitted = Some(job_id.clone());
        }
        job.phase = JobPhase::Admitted {
            task_finished: false,
            guard_released: false,
        };
        let guard = ServerPlaylistAdmissionGuard {
            job_id: Some(job_id.clone()),
            releases: self.release_sender.clone(),
        };
        // If the operation disappeared after requesting admission, dropping
        // the unsent guard releases the admitted lifetime immediately.
        let _ = response.send(Ok(guard));
    }

    fn release_guard(&mut self, job_id: &JobId) {
        let remove = if let Some(job) = self.jobs.get_mut(job_id) {
            if let JobPhase::Admitted {
                task_finished,
                guard_released,
            } = &mut job.phase
            {
                *guard_released = true;
                *task_finished
            } else {
                false
            }
        } else {
            false
        };
        if remove {
            self.remove_settled_job(job_id);
        }
    }

    fn finish_task(&mut self, joined: Result<(Id, JobId), tokio::task::JoinError>) {
        let (task_id, job_id) = match joined {
            Ok((task_id, job_id)) => (task_id, Some(job_id)),
            Err(error) => {
                let task_id = error.id();
                (task_id, self.task_jobs.get(&task_id).cloned())
            }
        };
        self.task_jobs.remove(&task_id);
        let Some(job_id) = job_id else {
            return;
        };

        let remove = match self.jobs.get_mut(&job_id) {
            Some(JobState {
                phase:
                    JobPhase::Admitted {
                        task_finished,
                        guard_released,
                    },
                ..
            }) => {
                *task_finished = true;
                *guard_released
            }
            Some(_) => true,
            None => false,
        };
        if remove {
            self.remove_settled_job(&job_id);
        }
    }

    fn remove_settled_job(&mut self, job_id: &JobId) {
        let Some(job) = self.jobs.remove(job_id) else {
            return;
        };
        if matches!(job.phase, JobPhase::Admitted { .. }) {
            self.finish_admitted(&job.key, job_id);
        } else {
            self.clear_pre_admission(&job.key, job_id);
        }
    }

    fn finish_admitted(&mut self, key: &ServerPlaylistOperationKey, job_id: &JobId) {
        let accepting = !self.command_receiver.is_closed();
        let pending = {
            let Some(state) = self.generations.get_mut(key) else {
                return;
            };
            if state.admitted.as_ref() != Some(job_id) {
                return;
            }
            state.admitted = None;
            if accepting {
                state.pending.take()
            } else {
                state.pending = None;
                None
            }
        };
        if let Some(pending) = pending {
            self.launch(key.clone(), pending.generation, pending.operation);
        }
    }

    fn clear_pre_admission(&mut self, key: &ServerPlaylistOperationKey, job_id: &JobId) {
        if let Some(state) = self.generations.get_mut(key) {
            if state.pre_admission.as_ref() == Some(job_id) {
                state.pre_admission = None;
            }
        }
    }

    fn cancel_for_shutdown(&mut self) {
        for job in self.jobs.values_mut() {
            match &job.phase {
                JobPhase::PreAdmission(cancellation) => {
                    cancellation.cancel();
                    job.abort.abort();
                    job.phase = JobPhase::Superseded;
                }
                JobPhase::Superseded => job.abort.abort(),
                JobPhase::Admitted { .. } => {}
            }
        }
        for state in self.generations.values_mut() {
            state.pre_admission = None;
            state.pending = None;
        }
    }

    async fn drain(&mut self) {
        while !self.tasks.is_empty() || !self.jobs.is_empty() {
            tokio::select! {
                Some(job_id) = self.release_receiver.recv() => {
                    self.release_guard(&job_id);
                }
                joined = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    if let Some(joined) = joined {
                        self.finish_task(joined);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;
    use uuid::Uuid;

    use super::*;

    fn source(value: u128) -> SourceId {
        SourceId::from_uuid(Uuid::from_u128(value))
    }

    fn native(value: &str) -> NativePlaylistId {
        NativePlaylistId::new(value).expect("native playlist ID")
    }

    fn keys() -> Vec<ServerPlaylistOperationKey> {
        vec![
            ServerPlaylistOperationKey::source(source(1)),
            ServerPlaylistOperationKey::remote_playlist(source(1), native("native-secret")),
            ServerPlaylistOperationKey::local_playlist("local-secret"),
        ]
    }

    async fn wait_until_closed(handle: &ServerPlaylistCoordinatorHandle) {
        while !handle.is_closed() {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn every_key_class_has_an_independent_monotonic_generation() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

        for key in keys() {
            let observed_tx = observed_tx.clone();
            assert_eq!(
                handle.request(key, move |context| async move {
                    observed_tx
                        .send((context.key_class(), context.generation().get()))
                        .expect("observe generation");
                }),
                ServerPlaylistRequestStatus::Queued
            );
        }

        let mut observed = Vec::new();
        for _ in 0..3 {
            observed.push(observed_rx.recv().await.expect("operation started"));
        }
        observed.sort_by_key(|(class, _)| match class {
            ServerPlaylistOperationKeyClass::Source => 0,
            ServerPlaylistOperationKeyClass::RemotePlaylist => 1,
            ServerPlaylistOperationKeyClass::LocalPlaylist => 2,
        });
        assert_eq!(
            observed,
            vec![
                (ServerPlaylistOperationKeyClass::Source, 1),
                (ServerPlaylistOperationKeyClass::RemotePlaylist, 1),
                (ServerPlaylistOperationKeyClass::LocalPlaylist, 1),
            ]
        );

        let (generation_tx, generation_rx) = oneshot::channel();
        assert_eq!(
            handle.request(
                ServerPlaylistOperationKey::source(source(1)),
                move |context| async move {
                    generation_tx
                        .send(context.generation().get())
                        .expect("observe next generation");
                },
            ),
            ServerPlaylistRequestStatus::Queued
        );
        assert_eq!(generation_rx.await.expect("second source operation"), 2);
        assert_eq!(
            shutdown.shutdown().await.expect("orderly shutdown"),
            ServerPlaylistCoordinatorShutdownReason::Closed
        );
    }

    #[tokio::test]
    async fn latest_request_supersedes_pre_admission_work_for_every_key_class() {
        for key in keys() {
            let (handle, shutdown) = spawn_server_playlist_coordinator();
            let (first_started_tx, first_started_rx) = oneshot::channel();
            let (first_result_tx, first_result_rx) = oneshot::channel();
            assert_eq!(
                handle.request(key.clone(), move |context| async move {
                    first_started_tx.send(()).expect("signal first start");
                    context.cancelled().await;
                    first_result_tx
                        .send(context.admit().await.map(|_| ()))
                        .expect("report supersession");
                }),
                ServerPlaylistRequestStatus::Queued
            );
            first_started_rx.await.expect("first operation started");

            let (second_admitted_tx, second_admitted_rx) = oneshot::channel();
            assert_eq!(
                handle.request(key, move |context| async move {
                    let guard = context.admit().await.expect("latest request admitted");
                    second_admitted_tx
                        .send(())
                        .expect("report latest admission");
                    drop(guard);
                }),
                ServerPlaylistRequestStatus::Queued
            );
            assert_eq!(
                first_result_rx.await.expect("first result"),
                Err(ServerPlaylistAdmissionError::Superseded)
            );
            second_admitted_rx.await.expect("second request admitted");
            shutdown.shutdown().await.expect("orderly shutdown");
        }
    }

    #[tokio::test]
    async fn distinct_keys_run_concurrently() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (admitted_tx, mut admitted_rx) = mpsc::unbounded_channel();
        let (release_source_tx, release_source_rx) = oneshot::channel();
        let (release_local_tx, release_local_rx) = oneshot::channel();

        let source_started = started_tx.clone();
        let source_admitted = admitted_tx.clone();
        handle.request(
            ServerPlaylistOperationKey::source(source(10)),
            move |context| async move {
                source_started.send("source").expect("source started");
                release_source_rx.await.expect("release source");
                drop(context.admit().await.expect("admit source"));
                source_admitted.send("source").expect("source admitted");
            },
        );
        handle.request(
            ServerPlaylistOperationKey::local_playlist("playlist-10"),
            move |context| async move {
                started_tx.send("local").expect("local started");
                release_local_rx.await.expect("release local");
                drop(context.admit().await.expect("admit local"));
                admitted_tx.send("local").expect("local admitted");
            },
        );

        let first = started_rx.recv().await.expect("first start");
        let second = started_rx.recv().await.expect("second start");
        assert_ne!(first, second);
        release_source_tx
            .send(())
            .expect("release source operation");
        release_local_tx.send(()).expect("release local operation");
        let first = admitted_rx.recv().await.expect("first admission");
        let second = admitted_rx.recv().await.expect("second admission");
        assert_ne!(first, second);
        shutdown.shutdown().await.expect("orderly shutdown");
    }

    #[tokio::test]
    async fn same_key_successor_waits_for_admitted_task_after_guard_release() {
        let key = ServerPlaylistOperationKey::local_playlist("serialized-task-local-id");
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let (admitted_tx, admitted_rx) = oneshot::channel();
        let (release_guard_tx, release_guard_rx) = oneshot::channel();
        let (guard_released_tx, guard_released_rx) = oneshot::channel();
        let (finish_task_tx, finish_task_rx) = oneshot::channel();
        handle.request(key.clone(), move |context| async move {
            let guard = context.admit().await.expect("first admission wins");
            admitted_tx.send(()).expect("report admission");
            release_guard_rx.await.expect("release first guard");
            drop(guard);
            guard_released_tx.send(()).expect("report guard release");
            finish_task_rx.await.expect("finish first task");
        });
        admitted_rx.await.expect("first admitted");

        let (second_started_tx, mut second_started_rx) = oneshot::channel();
        handle.request(key, move |context| async move {
            let guard = context.admit().await.expect("second admission");
            second_started_tx.send(()).expect("report second start");
            drop(guard);
        });

        release_guard_tx.send(()).expect("release first guard");
        guard_released_rx.await.expect("first guard released");
        assert!(
            timeout(Duration::from_millis(25), &mut second_started_rx)
                .await
                .is_err(),
            "successor started before the admitted task finished"
        );

        finish_task_tx.send(()).expect("finish first task");
        second_started_rx.await.expect("second started after task");
        shutdown.shutdown().await.expect("orderly shutdown");
    }

    #[tokio::test]
    async fn same_key_successor_waits_for_guard_after_admitted_task_finishes() {
        let key = ServerPlaylistOperationKey::local_playlist("serialized-guard-local-id");
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let (guard_tx, guard_rx) = oneshot::channel();
        handle.request(key.clone(), move |context| async move {
            guard_tx
                .send(context.admit().await.expect("first admission wins"))
                .expect("retain guard beyond first task");
        });
        let guard = guard_rx.await.expect("receive retained guard");

        let (second_started_tx, mut second_started_rx) = oneshot::channel();
        handle.request(key, move |context| async move {
            let guard = context.admit().await.expect("second admission");
            second_started_tx.send(()).expect("report second start");
            drop(guard);
        });

        assert!(
            timeout(Duration::from_millis(25), &mut second_started_rx)
                .await
                .is_err(),
            "successor started before the retained admission guard was released"
        );
        drop(guard);
        second_started_rx.await.expect("second started after guard");
        shutdown.shutdown().await.expect("orderly shutdown");
    }

    #[tokio::test]
    async fn admitted_key_retains_only_latest_pending_start() {
        let key = ServerPlaylistOperationKey::local_playlist("latest-pending-local-id");
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let (first_admitted_tx, first_admitted_rx) = oneshot::channel();
        let (release_first_tx, release_first_rx) = oneshot::channel();
        handle.request(key.clone(), move |context| async move {
            let guard = context.admit().await.expect("first admission");
            first_admitted_tx.send(()).expect("report first admission");
            release_first_rx.await.expect("release first operation");
            drop(guard);
        });
        first_admitted_rx.await.expect("first admitted");

        let displaced_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&displaced_ran);
        handle.request(key.clone(), move |_| async move {
            ran.store(true, Ordering::SeqCst);
        });

        let (latest_tx, latest_rx) = oneshot::channel();
        handle.request(key, move |context| async move {
            let generation = context.generation().get();
            let guard = context.admit().await.expect("latest pending admission");
            latest_tx
                .send(generation)
                .expect("report latest pending start");
            drop(guard);
        });
        assert!(!displaced_ran.load(Ordering::SeqCst));

        release_first_tx.send(()).expect("release first operation");
        assert_eq!(latest_rx.await.expect("latest pending ran"), 3);
        shutdown.shutdown().await.expect("orderly shutdown");
        assert!(!displaced_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn close_drops_pending_same_key_start_without_running_it() {
        struct DropSignal(Option<oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let key = ServerPlaylistOperationKey::local_playlist("closed-pending-local-id");
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let (first_admitted_tx, first_admitted_rx) = oneshot::channel();
        let (release_first_tx, release_first_rx) = oneshot::channel();
        handle.request(key.clone(), move |context| async move {
            let guard = context.admit().await.expect("first admission");
            first_admitted_tx.send(()).expect("report first admission");
            release_first_rx.await.expect("release first operation");
            drop(guard);
        });
        first_admitted_rx.await.expect("first admitted");

        let pending_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&pending_ran);
        let (pending_dropped_tx, pending_dropped_rx) = oneshot::channel();
        let drop_signal = DropSignal(Some(pending_dropped_tx));
        handle.request(key, move |_| async move {
            let _drop_signal = drop_signal;
            ran.store(true, Ordering::SeqCst);
        });

        assert!(handle.close());
        pending_dropped_rx
            .await
            .expect("shutdown dropped pending operation");
        assert!(!pending_ran.load(Ordering::SeqCst));
        release_first_tx.send(()).expect("release first operation");
        shutdown.shutdown().await.expect("orderly shutdown");
        assert!(!pending_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn delayed_reconnect_fanout_cannot_supersede_newer_manual_work() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let reconnect_stamp = handle
            .reserve_request_stamp()
            .expect("reserve reconnect start order");
        let local_key = ServerPlaylistOperationKey::local_playlist("shared-local-id");

        let (manual_admitted_tx, manual_admitted_rx) = oneshot::channel();
        assert_eq!(
            handle.request(local_key.clone(), move |context| async move {
                drop(context.admit().await.expect("manual sync admitted"));
                manual_admitted_tx
                    .send(())
                    .expect("report manual admission");
            }),
            ServerPlaylistRequestStatus::Queued
        );
        manual_admitted_rx.await.expect("manual sync ran");

        let reconnect_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&reconnect_ran);
        assert_eq!(
            handle.begin_if_not_newer(local_key, &reconnect_stamp, move |_| async move {
                ran.store(true, Ordering::SeqCst);
            }),
            ServerPlaylistRequestStatus::Queued
        );

        // A later command on the same FIFO submission lane is an owner
        // barrier proving it has already considered the delayed fan-out.
        let (barrier_tx, barrier_rx) = oneshot::channel();
        handle.request(
            ServerPlaylistOperationKey::source(source(11)),
            move |_| async move {
                barrier_tx.send(()).expect("owner barrier");
            },
        );
        barrier_rx.await.expect("owner processed barrier");
        assert!(!reconnect_ran.load(Ordering::SeqCst));
        shutdown.shutdown().await.expect("orderly shutdown");
    }

    #[tokio::test]
    async fn direct_request_reservation_and_enqueue_are_atomic_against_delayed_fanout() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let reconnect_stamp = handle
            .reserve_request_stamp()
            .expect("reserve reconnect start order");
        let local_key = ServerPlaylistOperationKey::local_playlist("inverted-shared-local-id");

        let (manual_reserved_tx, manual_reserved_rx) = std_mpsc::channel();
        let (release_manual_tx, release_manual_rx) = std_mpsc::channel();
        handle.install_after_direct_reserve_hook(move || {
            manual_reserved_tx
                .send(())
                .expect("report direct request reservation");
            release_manual_rx
                .recv()
                .expect("release direct request enqueue");
        });

        let (manual_admitted_tx, manual_admitted_rx) = oneshot::channel();
        let manual_handle = handle.clone();
        let manual_key = local_key.clone();
        let manual_thread = std::thread::spawn(move || {
            manual_handle.request(manual_key, move |context| async move {
                drop(context.admit().await.expect("manual request admitted"));
                manual_admitted_tx
                    .send(())
                    .expect("report manual admission");
            })
        });
        manual_reserved_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("manual request paused after reservation");

        let reconnect_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&reconnect_ran);
        let reconnect_handle = handle.clone();
        let (fanout_attempted_tx, fanout_attempted_rx) = std_mpsc::channel();
        let reconnect_thread = std::thread::spawn(move || {
            fanout_attempted_tx
                .send(())
                .expect("report delayed fanout attempt");
            reconnect_handle.begin_if_not_newer(local_key, &reconnect_stamp, move |_| async move {
                ran.store(true, Ordering::SeqCst);
            })
        });
        fanout_attempted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("delayed fanout attempted while manual request held the gate");

        release_manual_tx
            .send(())
            .expect("release atomic direct request enqueue");
        assert_eq!(
            manual_thread.join().expect("manual submission thread"),
            ServerPlaylistRequestStatus::Queued
        );
        assert_eq!(
            reconnect_thread
                .join()
                .expect("reconnect submission thread"),
            ServerPlaylistRequestStatus::Queued
        );
        manual_admitted_rx.await.expect("manual request ran");

        // A distinct-key request is an owner FIFO barrier proving it has
        // considered both preceding starts.
        let (barrier_tx, barrier_rx) = oneshot::channel();
        handle.request(
            ServerPlaylistOperationKey::source(source(12)),
            move |_| async move {
                barrier_tx.send(()).expect("owner barrier");
            },
        );
        barrier_rx.await.expect("owner processed both starts");
        assert!(!reconnect_ran.load(Ordering::SeqCst));
        shutdown.shutdown().await.expect("orderly shutdown");
    }

    #[tokio::test]
    async fn close_rejects_requests_cancels_pre_admission_and_drains_admission() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let (pre_started_tx, pre_started_rx) = oneshot::channel();
        struct Dropped(Option<oneshot::Sender<()>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let (pre_dropped_tx, pre_dropped_rx) = oneshot::channel();
        handle.request(
            ServerPlaylistOperationKey::source(source(20)),
            move |context| async move {
                let _dropped = Dropped(Some(pre_dropped_tx));
                pre_started_tx.send(()).expect("pre-admission started");
                context.cancelled().await;
                std::future::pending::<()>().await;
            },
        );
        pre_started_rx
            .await
            .expect("pre-admission operation started");

        let (admitted_tx, admitted_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        handle.request(
            ServerPlaylistOperationKey::local_playlist("drained-local-id"),
            move |context| async move {
                let guard = context.admit().await.expect("operation admitted");
                admitted_tx.send(()).expect("report admission");
                release_rx.await.expect("release admitted work");
                drop(guard);
            },
        );
        admitted_rx.await.expect("operation admitted");

        assert!(handle.close());
        assert_eq!(
            handle.request(ServerPlaylistOperationKey::source(source(21)), |_| async {},),
            ServerPlaylistRequestStatus::Closed
        );
        pre_dropped_rx
            .await
            .expect("shutdown aborts pre-admission work");

        let mut shutdown = Box::pin(shutdown.shutdown());
        tokio::select! {
            biased;
            result = &mut shutdown => panic!("shutdown did not drain admission: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        release_tx.send(()).expect("release admitted operation");
        assert_eq!(
            shutdown.await.expect("orderly shutdown"),
            ServerPlaylistCoordinatorShutdownReason::Closed
        );
    }

    #[tokio::test]
    async fn shutdown_tracks_a_guard_moved_beyond_its_operation_future() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let (guard_tx, guard_rx) = oneshot::channel();
        handle.request(
            ServerPlaylistOperationKey::remote_playlist(source(30), native("detached-native-id")),
            move |context| async move {
                guard_tx
                    .send(context.admit().await.expect("operation admitted"))
                    .expect("move guard out of operation");
            },
        );
        let guard = guard_rx.await.expect("receive moved guard");

        let mut shutdown = Box::pin(shutdown.shutdown());
        tokio::select! {
            biased;
            result = &mut shutdown => panic!("shutdown lost moved guard: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        drop(guard);
        shutdown.await.expect("guard release drains shutdown");
        assert!(handle.is_closed());
    }

    #[tokio::test]
    async fn generation_exhaustion_closes_every_key_fail_closed() {
        let (handle, shutdown) = spawn_server_playlist_coordinator_with_generation_ceiling(1);
        let key = ServerPlaylistOperationKey::source(source(40));
        let (first_tx, first_rx) = oneshot::channel();
        handle.request(key.clone(), move |_| async move {
            first_tx.send(()).expect("first generation ran");
        });
        first_rx.await.expect("first generation completed");

        let exhausted_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&exhausted_started);
        assert_eq!(
            handle.request(key, move |_| async move {
                started.store(true, Ordering::SeqCst);
            }),
            ServerPlaylistRequestStatus::Queued
        );
        wait_until_closed(&handle).await;
        assert!(!exhausted_started.load(Ordering::SeqCst));
        assert_eq!(
            handle.request(
                ServerPlaylistOperationKey::local_playlist("other-key"),
                |_| async {},
            ),
            ServerPlaylistRequestStatus::Closed
        );
        assert_eq!(
            shutdown.shutdown().await.expect("owner stopped cleanly"),
            ServerPlaylistCoordinatorShutdownReason::GenerationExhausted
        );
    }

    #[tokio::test]
    async fn dropping_last_handle_closes_the_lane_and_drains() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let clone = handle.clone();
        drop(handle);
        assert!(!clone.is_closed());
        drop(clone);
        assert_eq!(
            shutdown.shutdown().await.expect("last-handle shutdown"),
            ServerPlaylistCoordinatorShutdownReason::Closed
        );
    }

    #[tokio::test]
    async fn completion_barrier_is_persistent_across_repeated_waits() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let barrier = shutdown.barrier();
        assert!(!barrier.is_complete());

        handle.close();
        barrier.wait().await;
        assert!(barrier.is_complete());
        barrier.wait().await;
        barrier.clone().wait().await;

        assert_eq!(
            shutdown.shutdown().await.expect("join completed owner"),
            ServerPlaylistCoordinatorShutdownReason::Closed
        );
    }

    #[tokio::test]
    async fn cloned_completion_barriers_wait_concurrently_for_admitted_drain() {
        let (handle, shutdown) = spawn_server_playlist_coordinator();
        let barrier = shutdown.barrier();
        let (guard_tx, guard_rx) = oneshot::channel();
        handle.request(
            ServerPlaylistOperationKey::local_playlist("barrier-local-id"),
            move |context| async move {
                guard_tx
                    .send(context.admit().await.expect("operation admitted"))
                    .expect("move admission guard");
            },
        );
        let guard = guard_rx.await.expect("receive admission guard");
        handle.close();

        let first_barrier = barrier.clone();
        let second_barrier = barrier.clone();
        let first = tokio::spawn(async move { first_barrier.wait().await });
        let second = tokio::spawn(async move { second_barrier.wait().await });
        tokio::task::yield_now().await;
        assert!(!first.is_finished());
        assert!(!second.is_finished());

        drop(guard);
        first.await.expect("first barrier waiter");
        second.await.expect("second barrier waiter");
        assert!(barrier.is_complete());
        shutdown.shutdown().await.expect("join completed owner");
    }

    #[tokio::test]
    async fn completion_barrier_treats_owner_sender_drop_as_terminal() {
        let (completion_sender, completion) = watch::channel(false);
        let barrier = ServerPlaylistCoordinatorBarrier { completion };
        drop(completion_sender);

        barrier.wait().await;
        assert!(barrier.is_complete());
        barrier.wait().await;
    }

    #[test]
    fn debug_output_is_identity_and_content_redacted() {
        let source_secret = "feedface-feed-face-feed-facefeedface";
        let source_id: SourceId = source_secret.parse().expect("source UUID");
        let native_secret = "native playlist / user@example.test / token=secret";
        let local_secret = "local-playlist-private-uuid";
        let keys = [
            ServerPlaylistOperationKey::source(source_id),
            ServerPlaylistOperationKey::remote_playlist(source_id, native(native_secret)),
            ServerPlaylistOperationKey::local_playlist(local_secret),
        ];
        for key in keys {
            let debug = format!("{key:?}");
            assert!(!debug.contains(source_secret));
            assert!(!debug.contains(native_secret));
            assert!(!debug.contains(local_secret));
        }

        let guard_debug = format!(
            "{:?}",
            ServerPlaylistAdmissionGuard {
                job_id: Some(JobId::new()),
                releases: tokio::sync::mpsc::unbounded_channel().0,
            }
        );
        assert_eq!(
            guard_debug,
            "ServerPlaylistAdmissionGuard { admitted: true, .. }"
        );
        let error_debug = format!("{:?}", ServerPlaylistAdmissionError::Closed);
        assert_eq!(error_debug, "Closed");
    }
}
