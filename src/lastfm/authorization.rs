//! GTK-free, latest-only Last.fm desktop-authorization lifecycle.
//!
//! The owner retains the request token and staged session behind a bounded,
//! serialized command lane. Presentation receives only a redacted challenge:
//! its browser URL is available through a synchronously revocable, short-lived
//! view, while its opaque finish authority is consumed atomically before
//! `auth.getSession` is first awaited.
//!
//! This module is intentionally an injected internal core. Production consent,
//! global single-owner coordination, and vault installation remain deferred;
//! no build-credential factory wires this owner into the application.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use futures::FutureExt;
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use super::client::{
    DesktopAuthToken, DesktopAuthorizationUrl, DesktopAuthorizedSession, LastFmClient,
    LastFmClientError,
};

const AUTHORIZATION_COMMAND_CAPACITY: usize = 8;
const DESKTOP_TOKEN_LIFETIME: Duration = Duration::from_hours(1);

/// Network boundary used by the desktop-authorization owner.
///
/// The exchange consumes its token. Implementations therefore cannot replay
/// one owned token without manufacturing a second authority outside this API.
#[async_trait::async_trait]
pub(super) trait LastFmAuthorizationTransport: Send + Sync {
    async fn request_auth_token(&self) -> Result<DesktopAuthToken, LastFmClientError>;

    fn authorization_url(
        &self,
        token: &DesktopAuthToken,
    ) -> Result<DesktopAuthorizationUrl, LastFmClientError>;

    async fn exchange_auth_token(
        &self,
        token: DesktopAuthToken,
    ) -> Result<DesktopAuthorizedSession, LastFmClientError>;
}

#[async_trait::async_trait]
impl LastFmAuthorizationTransport for LastFmClient {
    async fn request_auth_token(&self) -> Result<DesktopAuthToken, LastFmClientError> {
        Self::request_auth_token(self).await
    }

    fn authorization_url(
        &self,
        token: &DesktopAuthToken,
    ) -> Result<DesktopAuthorizationUrl, LastFmClientError> {
        Self::authorization_url(self, token)
    }

    async fn exchange_auth_token(
        &self,
        token: DesktopAuthToken,
    ) -> Result<DesktopAuthorizedSession, LastFmClientError> {
        Self::exchange_auth_token(self, token).await
    }
}

/// Monotonic clock boundary for the in-memory request-token lifetime.
///
/// Values are elapsed monotonic process time, never Unix or civil time.
/// `wait_until` must be cancellation-safe and may wake early; the owner
/// rechecks `now` before expiring authority.
#[async_trait::async_trait]
pub(super) trait LastFmAuthorizationClock: Send + Sync {
    fn now(&self) -> Duration;

    async fn wait_until(&self, deadline: Duration);
}

/// Production monotonic clock anchored when the authorization owner starts.
#[derive(Debug)]
pub(super) struct SystemLastFmAuthorizationClock {
    origin: tokio::time::Instant,
}

impl Default for SystemLastFmAuthorizationClock {
    fn default() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
        }
    }
}

#[async_trait::async_trait]
impl LastFmAuthorizationClock for SystemLastFmAuthorizationClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    async fn wait_until(&self, deadline: Duration) {
        let remaining = deadline.saturating_sub(self.now());
        if !remaining.is_zero() {
            tokio::time::sleep(remaining).await;
        }
    }
}

struct FlowSeal(u8);

/// Opaque identity of one exact admitted authorization flow.
///
/// The token contains no provider token, URL, username, or generation value.
#[derive(Clone)]
pub struct LastFmAuthorizationFlow(Arc<FlowSeal>);

impl LastFmAuthorizationFlow {
    fn fresh() -> Self {
        Self(Arc::new(FlowSeal(0)))
    }
}

impl PartialEq for LastFmAuthorizationFlow {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for LastFmAuthorizationFlow {}

impl fmt::Debug for LastFmAuthorizationFlow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.0 .0;
        formatter
            .debug_struct("LastFmAuthorizationFlow")
            .finish_non_exhaustive()
    }
}

struct FinishSeal(u8);

#[derive(Clone)]
struct LastFmAuthorizationFinish(Arc<FinishSeal>);

impl LastFmAuthorizationFinish {
    fn fresh() -> Self {
        Self(Arc::new(FinishSeal(0)))
    }
}

impl PartialEq for LastFmAuthorizationFinish {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for LastFmAuthorizationFinish {}

impl fmt::Debug for LastFmAuthorizationFinish {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.0 .0;
        formatter
            .debug_struct("LastFmAuthorizationFinish")
            .finish_non_exhaustive()
    }
}

struct ChallengeInner {
    generation: u64,
    flow: LastFmAuthorizationFlow,
    finish: LastFmAuthorizationFinish,
    handle: Weak<HandleInner>,
}

/// Browser challenge for one exact in-memory request token.
///
/// Clones share one protected URL allocation. A caller may inspect it
/// repeatedly while the challenge remains current. Successful finish, cancel,
/// supersession, expiry, terminal failure, or shutdown synchronously revokes
/// every clone before the corresponding admission call returns.
#[derive(Clone)]
pub struct LastFmAuthorizationChallenge(Arc<ChallengeInner>);

impl LastFmAuthorizationChallenge {
    /// Inspect the current browser URL without copying it into durable state.
    ///
    /// The callback must be short-lived and must not re-enter an authorization
    /// handle. Revocation waits for an in-progress inspection to finish.
    pub fn with_authorization_url<T>(
        &self,
        inspect: impl FnOnce(&str) -> T,
    ) -> Result<T, LastFmAuthorizationAdmissionError> {
        let handle = self
            .0
            .handle
            .upgrade()
            .ok_or(LastFmAuthorizationAdmissionError::Closed)?;
        let mut ingress = match handle.ingress.lock() {
            Ok(ingress) => ingress,
            Err(poisoned) => {
                let mut ingress = poisoned.into_inner();
                close_ingress(&handle, &mut ingress);
                return Err(LastFmAuthorizationAdmissionError::Closed);
            }
        };
        if !ingress.open {
            return Err(LastFmAuthorizationAdmissionError::Closed);
        }
        let Some(current) = ingress.current.as_ref() else {
            return Err(LastFmAuthorizationAdmissionError::StaleFlow);
        };
        if current.generation != self.0.generation || current.flow != self.0.flow {
            return Err(LastFmAuthorizationAdmissionError::StaleFlow);
        }
        if ingress.expire_current_if_due(self.0.generation, &self.0.flow, handle.clock.now()) {
            return Err(LastFmAuthorizationAdmissionError::FinishUnavailable);
        }
        let current = ingress
            .current
            .as_ref()
            .expect("current challenge remains installed after deadline check");
        if current.finish.as_ref() != Some(&self.0.finish) || current.cancellation.is_cancelled() {
            return Err(LastFmAuthorizationAdmissionError::FinishUnavailable);
        }
        current
            .authorization_url
            .as_ref()
            .map(|url| inspect(url.as_str()))
            .ok_or(LastFmAuthorizationAdmissionError::FinishUnavailable)
    }

    pub fn flow(&self) -> LastFmAuthorizationFlow {
        self.0.flow.clone()
    }
}

impl fmt::Debug for LastFmAuthorizationChallenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmAuthorizationChallenge")
            .finish_non_exhaustive()
    }
}

/// Successful one-shot exchange which has not yet been installed in a vault.
///
/// This move-only grant deliberately is not a durable account and contains no
/// opaque account UUID. Only the Last.fm integration may unwrap it for the
/// subsequent serialized vault/runtime transition.
pub struct LastFmAuthorizationGrant(DesktopAuthorizedSession);

impl LastFmAuthorizationGrant {
    pub(in crate::lastfm) fn into_authorized_session(self) -> DesktopAuthorizedSession {
        self.0
    }
}

impl fmt::Debug for LastFmAuthorizationGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmAuthorizationGrant([REDACTED])")
    }
}

/// Immediate refusal before an authorization command crosses bounded ingress.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmAuthorizationAdmissionError {
    #[error("Last.fm authorization command ingress is busy")]
    Busy,
    #[error("Last.fm authorization flow is stale")]
    StaleFlow,
    #[error("Last.fm authorization is not ready to finish")]
    FinishUnavailable,
    #[error("Last.fm authorization owner is closed")]
    Closed,
}

/// Content-free terminal result from an admitted authorization command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmAuthorizationError {
    #[error("Last.fm authorization is unavailable in this build")]
    CapabilityUnavailable,
    #[error("Last.fm authorization is temporarily unavailable")]
    TemporarilyUnavailable,
    #[error("Last.fm rejected authorization")]
    Rejected,
    #[error("Last.fm returned an incompatible authorization response")]
    Incompatible,
    #[error("Last.fm authorization expired")]
    Expired,
    #[error("Last.fm authorization was superseded")]
    Superseded,
    #[error("Last.fm authorization was cancelled")]
    Cancelled,
    #[error("Last.fm authorization owner stopped")]
    OwnerStopped,
}

fn map_client_error(error: LastFmClientError) -> LastFmAuthorizationError {
    if error.is_retryable() {
        return LastFmAuthorizationError::TemporarilyUnavailable;
    }
    match error {
        LastFmClientError::AppCredentialsUnavailable
        | LastFmClientError::ClientConstruction
        | LastFmClientError::InvalidInput => LastFmAuthorizationError::CapabilityUnavailable,
        LastFmClientError::ServiceRejected { .. } => LastFmAuthorizationError::Rejected,
        LastFmClientError::ReauthenticationRequired
        | LastFmClientError::HttpStatus
        | LastFmClientError::BodyLimit
        | LastFmClientError::InvalidResponse => LastFmAuthorizationError::Incompatible,
        LastFmClientError::Timeout
        | LastFmClientError::Transport
        | LastFmClientError::ServiceUnavailable
        | LastFmClientError::RateLimited => LastFmAuthorizationError::TemporarilyUnavailable,
    }
}

/// Content-free authorization lifecycle phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmAuthorizationPhase {
    Idle,
    Requesting,
    AwaitingApproval,
    Exchanging,
    GrantIssued,
    Expired,
    Failed,
    ShuttingDown,
    Stopped,
}

/// Latest privacy-safe authorization owner snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LastFmAuthorizationStatus {
    pub revision: u64,
    pub phase: LastFmAuthorizationPhase,
    pub failure: Option<LastFmAuthorizationError>,
}

impl LastFmAuthorizationStatus {
    const INITIAL: Self = Self {
        revision: 0,
        phase: LastFmAuthorizationPhase::Idle,
        failure: None,
    };
}

/// Eventual content-free result of one command which crossed ingress.
#[must_use = "Last.fm authorization operations should be observed"]
pub struct LastFmAuthorizationOperation<T> {
    receiver: oneshot::Receiver<Result<T, LastFmAuthorizationError>>,
}

impl<T> LastFmAuthorizationOperation<T> {
    pub async fn wait(self) -> Result<T, LastFmAuthorizationError> {
        self.receiver
            .await
            .unwrap_or(Err(LastFmAuthorizationError::OwnerStopped))
    }
}

impl<T> fmt::Debug for LastFmAuthorizationOperation<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmAuthorizationOperation(..)")
    }
}

/// Exact flow identity plus completion of a newly admitted token request.
#[must_use = "Last.fm authorization starts should be observed"]
pub struct LastFmAuthorizationStart {
    flow: LastFmAuthorizationFlow,
    operation: LastFmAuthorizationOperation<LastFmAuthorizationChallenge>,
}

impl LastFmAuthorizationStart {
    pub fn flow(&self) -> LastFmAuthorizationFlow {
        self.flow.clone()
    }

    pub async fn wait(self) -> Result<LastFmAuthorizationChallenge, LastFmAuthorizationError> {
        self.operation.wait().await
    }
}

impl fmt::Debug for LastFmAuthorizationStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmAuthorizationStart")
            .field("flow", &self.flow)
            .finish_non_exhaustive()
    }
}

struct IngressCurrent {
    generation: u64,
    flow: LastFmAuthorizationFlow,
    cancellation: CancellationToken,
    finish: Option<LastFmAuthorizationFinish>,
    authorization_url: Option<DesktopAuthorizationUrl>,
    challenge_deadline: Option<Duration>,
}

impl IngressCurrent {
    fn revoke_challenge(&mut self) {
        self.finish = None;
        self.authorization_url.take();
        self.challenge_deadline = None;
    }
}

struct IngressGate {
    open: bool,
    generation: u64,
    generation_ceiling: u64,
    current: Option<IngressCurrent>,
    terminal: Option<(u64, LastFmAuthorizationError)>,
    status_sender: watch::Sender<LastFmAuthorizationStatus>,
    status: LastFmAuthorizationStatus,
}

impl IngressGate {
    fn current_matches(&self, generation: u64, flow: &LastFmAuthorizationFlow) -> bool {
        self.open
            && self.current.as_ref().is_some_and(|current| {
                current.generation == generation
                    && current.flow == *flow
                    && !current.cancellation.is_cancelled()
            })
    }

    fn reason_for(
        &self,
        generation: u64,
        flow: &LastFmAuthorizationFlow,
    ) -> LastFmAuthorizationError {
        if !self.open {
            LastFmAuthorizationError::OwnerStopped
        } else if self
            .current
            .as_ref()
            .is_some_and(|current| current.generation == generation && current.flow == *flow)
        {
            LastFmAuthorizationError::Cancelled
        } else {
            LastFmAuthorizationError::Superseded
        }
    }

    fn terminal_error(
        &self,
        generation: u64,
        flow: &LastFmAuthorizationFlow,
    ) -> LastFmAuthorizationError {
        self.terminal
            .filter(|(terminal_generation, _)| *terminal_generation == generation)
            .map_or_else(|| self.reason_for(generation, flow), |(_, error)| error)
    }

    fn publish_owner_stopped_unless_terminal(&mut self) {
        if self.status.phase == LastFmAuthorizationPhase::Stopped {
            return;
        }
        self.publish(
            LastFmAuthorizationPhase::Stopped,
            Some(LastFmAuthorizationError::OwnerStopped),
        );
    }

    fn clear_current_exact(
        &mut self,
        generation: u64,
        flow: &LastFmAuthorizationFlow,
        cancel: bool,
    ) -> bool {
        if !self
            .current
            .as_ref()
            .is_some_and(|current| current.generation == generation && current.flow == *flow)
        {
            return false;
        }
        if let Some(mut current) = self.current.take() {
            current.revoke_challenge();
            if cancel {
                current.cancellation.cancel();
            }
        }
        true
    }

    fn expire_current_if_due(
        &mut self,
        generation: u64,
        flow: &LastFmAuthorizationFlow,
        now: Duration,
    ) -> bool {
        let due = self.current.as_ref().is_some_and(|current| {
            current.generation == generation
                && current.flow == *flow
                && current
                    .challenge_deadline
                    .is_some_and(|deadline| now >= deadline)
        });
        if !due {
            return false;
        }
        self.clear_current_exact(generation, flow, true);
        self.terminal = Some((generation, LastFmAuthorizationError::Expired));
        self.publish(
            LastFmAuthorizationPhase::Expired,
            Some(LastFmAuthorizationError::Expired),
        );
        true
    }

    fn publish(
        &mut self,
        phase: LastFmAuthorizationPhase,
        failure: Option<LastFmAuthorizationError>,
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
    clock: Arc<dyn LastFmAuthorizationClock>,
    status: watch::Receiver<LastFmAuthorizationStatus>,
}

/// Cloneable, nonblocking presentation side of the authorization owner.
#[derive(Clone)]
pub struct LastFmAuthorizationHandle {
    inner: Arc<HandleInner>,
}

impl LastFmAuthorizationHandle {
    /// Admit a new latest-only flow.
    ///
    /// Successful admission cancels the predecessor synchronously under the
    /// shared gate. The owner joins and drops it before starting this request.
    pub fn try_begin(&self) -> Result<LastFmAuthorizationStart, LastFmAuthorizationAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        if !ingress.open {
            return Err(LastFmAuthorizationAdmissionError::Closed);
        }
        let Some(generation) = ingress
            .generation
            .checked_add(1)
            .filter(|generation| *generation <= ingress.generation_ceiling)
        else {
            close_ingress(&self.inner, &mut ingress);
            return Err(LastFmAuthorizationAdmissionError::Closed);
        };
        let flow = LastFmAuthorizationFlow::fresh();
        let cancellation = CancellationToken::new();
        let (completion, receiver) = oneshot::channel();
        let (admitted, admission) = oneshot::channel();
        let command = Command::Begin {
            generation,
            flow: flow.clone(),
            cancellation: cancellation.clone(),
            completion,
            admission,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => {
                if let Some(mut previous) = ingress.current.take() {
                    previous.revoke_challenge();
                    previous.cancellation.cancel();
                }
                ingress.generation = generation;
                ingress.terminal = None;
                ingress.current = Some(IngressCurrent {
                    generation,
                    flow: flow.clone(),
                    cancellation,
                    finish: None,
                    authorization_url: None,
                    challenge_deadline: None,
                });
                let _ = admitted.send(());
                Ok(LastFmAuthorizationStart {
                    flow,
                    operation: LastFmAuthorizationOperation { receiver },
                })
            }
            Err(async_channel::TrySendError::Full(_)) => {
                Err(LastFmAuthorizationAdmissionError::Busy)
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                close_ingress(&self.inner, &mut ingress);
                Err(LastFmAuthorizationAdmissionError::Closed)
            }
        }
    }

    /// Consume the exact current finish authority before exchange is awaited.
    ///
    /// Reading or cloning the challenge does not consume it. Capacity failure
    /// also leaves it usable; only successful command admission removes the
    /// matching seal from the ingress gate.
    pub fn try_finish(
        &self,
        challenge: &LastFmAuthorizationChallenge,
    ) -> Result<
        LastFmAuthorizationOperation<LastFmAuthorizationGrant>,
        LastFmAuthorizationAdmissionError,
    > {
        let mut ingress = self.lock_ingress()?;
        if !ingress.open {
            return Err(LastFmAuthorizationAdmissionError::Closed);
        }
        let Some(current) = ingress.current.as_ref() else {
            return Err(LastFmAuthorizationAdmissionError::StaleFlow);
        };
        if current.generation != challenge.0.generation || current.flow != challenge.0.flow {
            return Err(LastFmAuthorizationAdmissionError::StaleFlow);
        }
        if ingress.expire_current_if_due(
            challenge.0.generation,
            &challenge.0.flow,
            self.inner.clock.now(),
        ) {
            return Err(LastFmAuthorizationAdmissionError::FinishUnavailable);
        }
        let current = ingress
            .current
            .as_mut()
            .expect("current challenge remains installed after deadline check");
        if current.finish.as_ref() != Some(&challenge.0.finish) {
            return Err(LastFmAuthorizationAdmissionError::FinishUnavailable);
        }
        let (completion, receiver) = oneshot::channel();
        let (admitted, admission) = oneshot::channel();
        let command = Command::Finish {
            generation: current.generation,
            flow: current.flow.clone(),
            completion,
            admission,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => {
                current.revoke_challenge();
                let _ = admitted.send(());
                Ok(LastFmAuthorizationOperation { receiver })
            }
            Err(async_channel::TrySendError::Full(_)) => {
                Err(LastFmAuthorizationAdmissionError::Busy)
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                close_ingress(&self.inner, &mut ingress);
                Err(LastFmAuthorizationAdmissionError::Closed)
            }
        }
    }

    /// Cancel one exact flow. Stale UI teardown cannot cancel its successor.
    pub fn try_cancel(
        &self,
        flow: &LastFmAuthorizationFlow,
    ) -> Result<LastFmAuthorizationOperation<()>, LastFmAuthorizationAdmissionError> {
        let mut ingress = self.lock_ingress()?;
        if !ingress.open {
            return Err(LastFmAuthorizationAdmissionError::Closed);
        }
        let Some(current) = ingress.current.as_mut() else {
            return Err(LastFmAuthorizationAdmissionError::StaleFlow);
        };
        if current.flow != *flow {
            return Err(LastFmAuthorizationAdmissionError::StaleFlow);
        }
        let (completion, receiver) = oneshot::channel();
        let (admitted, admission) = oneshot::channel();
        let command = Command::Cancel {
            generation: current.generation,
            flow: current.flow.clone(),
            completion,
            admission,
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => {
                current.revoke_challenge();
                current.cancellation.cancel();
                let _ = admitted.send(());
                Ok(LastFmAuthorizationOperation { receiver })
            }
            Err(async_channel::TrySendError::Full(_)) => {
                Err(LastFmAuthorizationAdmissionError::Busy)
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                close_ingress(&self.inner, &mut ingress);
                Err(LastFmAuthorizationAdmissionError::Closed)
            }
        }
    }

    pub fn close_and_flush(&self) -> bool {
        request_close(&self.inner)
    }

    pub fn subscribe_status(&self) -> watch::Receiver<LastFmAuthorizationStatus> {
        self.inner.status.clone()
    }

    fn lock_ingress(
        &self,
    ) -> Result<MutexGuard<'_, IngressGate>, LastFmAuthorizationAdmissionError> {
        match self.inner.ingress.lock() {
            Ok(ingress) => Ok(ingress),
            Err(poisoned) => {
                let mut ingress = poisoned.into_inner();
                close_ingress(&self.inner, &mut ingress);
                Err(LastFmAuthorizationAdmissionError::Closed)
            }
        }
    }
}

impl fmt::Debug for LastFmAuthorizationHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let open = self
            .inner
            .ingress
            .lock()
            .ok()
            .is_some_and(|ingress| ingress.open);
        formatter
            .debug_struct("LastFmAuthorizationHandle")
            .field("open", &open)
            .finish()
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
    if let Some(mut current) = ingress.current.take() {
        current.revoke_challenge();
        current.cancellation.cancel();
    }
    inner.commands.close();
}

enum Command {
    Begin {
        generation: u64,
        flow: LastFmAuthorizationFlow,
        cancellation: CancellationToken,
        completion: oneshot::Sender<Result<LastFmAuthorizationChallenge, LastFmAuthorizationError>>,
        admission: oneshot::Receiver<()>,
    },
    Finish {
        generation: u64,
        flow: LastFmAuthorizationFlow,
        completion: oneshot::Sender<Result<LastFmAuthorizationGrant, LastFmAuthorizationError>>,
        admission: oneshot::Receiver<()>,
    },
    Cancel {
        generation: u64,
        flow: LastFmAuthorizationFlow,
        completion: oneshot::Sender<Result<(), LastFmAuthorizationError>>,
        admission: oneshot::Receiver<()>,
    },
}

enum RequestTaskExit {
    Cancelled,
    Completed {
        result: Result<DesktopAuthToken, LastFmClientError>,
        observed_at: Duration,
    },
}

enum ExpiryTaskExit {
    Cancelled,
    Expired,
}

enum ExchangeTaskExit {
    Cancelled,
    Completed(Result<DesktopAuthorizedSession, LastFmClientError>),
}

struct RequestingFlow {
    task: JoinHandle<RequestTaskExit>,
    completion: oneshot::Sender<Result<LastFmAuthorizationChallenge, LastFmAuthorizationError>>,
}

struct AwaitingFlow {
    token: DesktopAuthToken,
    deadline: Duration,
    expiry_cancellation: CancellationToken,
    expiry_task: JoinHandle<ExpiryTaskExit>,
}

struct ExchangingFlow {
    task: JoinHandle<ExchangeTaskExit>,
    completion: oneshot::Sender<Result<LastFmAuthorizationGrant, LastFmAuthorizationError>>,
}

enum FlowPhase {
    Requesting(RequestingFlow),
    Awaiting(AwaitingFlow),
    Exchanging(ExchangingFlow),
}

struct ActiveFlow {
    generation: u64,
    flow: LastFmAuthorizationFlow,
    cancellation: CancellationToken,
    phase: FlowPhase,
}

enum OwnerEvent {
    Command(Result<Command, async_channel::RecvError>),
    Request(Result<RequestTaskExit, JoinError>),
    Expiry(Result<ExpiryTaskExit, JoinError>),
    Exchange(Result<ExchangeTaskExit, JoinError>),
}

struct AuthorizationOwner {
    commands: async_channel::Receiver<Command>,
    ingress: Arc<Mutex<IngressGate>>,
    handle: Weak<HandleInner>,
    transport: Arc<dyn LastFmAuthorizationTransport>,
    clock: Arc<dyn LastFmAuthorizationClock>,
    active: Option<ActiveFlow>,
    #[cfg(test)]
    result_gate: Option<AuthorizationResultGate>,
}

#[cfg(test)]
#[derive(Clone)]
struct AuthorizationResultGate {
    kind: AuthorizationResultGateKind,
    reached: async_channel::Sender<()>,
    release: async_channel::Receiver<()>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum AuthorizationResultGateKind {
    Begin,
    Finish,
    Cancel,
    Request,
    Expiry,
    Exchange,
}

impl AuthorizationOwner {
    async fn run(
        &mut self,
    ) -> Result<LastFmAuthorizationShutdownReason, LastFmAuthorizationShutdownError> {
        loop {
            let event = self.next_event().await;
            #[cfg(test)]
            self.pause_ready_result(&event).await;
            match event {
                OwnerEvent::Command(Ok(command)) => {
                    self.handle_command(command).await?;
                }
                OwnerEvent::Command(Err(_)) => {
                    self.publish_global(LastFmAuthorizationPhase::ShuttingDown, None)?;
                    let joined = self
                        .retire_active(LastFmAuthorizationError::OwnerStopped)
                        .await;
                    self.reject_queued(LastFmAuthorizationError::OwnerStopped);
                    if joined {
                        self.publish_global(LastFmAuthorizationPhase::Stopped, None)?;
                        return Ok(LastFmAuthorizationShutdownReason::Drained);
                    }
                    self.publish_global(
                        LastFmAuthorizationPhase::Stopped,
                        Some(LastFmAuthorizationError::OwnerStopped),
                    )?;
                    return Err(LastFmAuthorizationShutdownError);
                }
                OwnerEvent::Request(result) => self.finish_request(result)?,
                OwnerEvent::Expiry(result) => self.finish_expiry(result)?,
                OwnerEvent::Exchange(result) => self.finish_exchange(result)?,
            }
        }
    }

    #[cfg(test)]
    async fn pause_ready_result(&mut self, event: &OwnerEvent) {
        let matches = self.result_gate.as_ref().is_some_and(|gate| {
            matches!(
                (gate.kind, event),
                (
                    AuthorizationResultGateKind::Begin,
                    OwnerEvent::Command(Ok(Command::Begin { .. }))
                ) | (
                    AuthorizationResultGateKind::Finish,
                    OwnerEvent::Command(Ok(Command::Finish { .. }))
                ) | (
                    AuthorizationResultGateKind::Cancel,
                    OwnerEvent::Command(Ok(Command::Cancel { .. }))
                ) | (AuthorizationResultGateKind::Request, OwnerEvent::Request(_))
                    | (AuthorizationResultGateKind::Expiry, OwnerEvent::Expiry(_))
                    | (
                        AuthorizationResultGateKind::Exchange,
                        OwnerEvent::Exchange(_)
                    )
            )
        });
        if !matches {
            return;
        }
        let Some(gate) = self.result_gate.take() else {
            return;
        };
        let _ = gate.reached.send(()).await;
        let _ = gate.release.recv().await;
    }

    async fn next_event(&mut self) -> OwnerEvent {
        let commands = &self.commands;
        match self.active.as_mut().map(|active| &mut active.phase) {
            Some(FlowPhase::Requesting(requesting)) => {
                tokio::select! {
                    biased;
                    result = &mut requesting.task => OwnerEvent::Request(result),
                    command = commands.recv() => OwnerEvent::Command(command),
                }
            }
            Some(FlowPhase::Awaiting(awaiting)) => {
                tokio::select! {
                    biased;
                    result = &mut awaiting.expiry_task => OwnerEvent::Expiry(result),
                    command = commands.recv() => OwnerEvent::Command(command),
                }
            }
            Some(FlowPhase::Exchanging(exchanging)) => {
                tokio::select! {
                    biased;
                    result = &mut exchanging.task => OwnerEvent::Exchange(result),
                    command = commands.recv() => OwnerEvent::Command(command),
                }
            }
            None => OwnerEvent::Command(commands.recv().await),
        }
    }

    async fn handle_command(
        &mut self,
        command: Command,
    ) -> Result<(), LastFmAuthorizationShutdownError> {
        match command {
            Command::Begin {
                generation,
                flow,
                cancellation,
                completion,
                admission,
            } => {
                if admission.await.is_err() {
                    let _ = completion.send(Err(LastFmAuthorizationError::OwnerStopped));
                    return Ok(());
                }
                if !self
                    .retire_active(LastFmAuthorizationError::Superseded)
                    .await
                {
                    let _ = completion.send(Err(LastFmAuthorizationError::OwnerStopped));
                    return Err(LastFmAuthorizationShutdownError);
                }
                let ingress = Arc::clone(&self.ingress);
                let mut ingress = ingress
                    .lock()
                    .map_err(|_| LastFmAuthorizationShutdownError)?;
                if !ingress.current_matches(generation, &flow) {
                    let error = ingress.reason_for(generation, &flow);
                    let _ = completion.send(Err(error));
                    return Ok(());
                }
                let task = spawn_request_task(
                    Arc::clone(&self.transport),
                    Arc::clone(&self.clock),
                    cancellation.clone(),
                );
                self.active = Some(ActiveFlow {
                    generation,
                    flow,
                    cancellation,
                    phase: FlowPhase::Requesting(RequestingFlow { task, completion }),
                });
                ingress.publish(LastFmAuthorizationPhase::Requesting, None);
            }
            Command::Finish {
                generation,
                flow,
                completion,
                admission,
            } => {
                if admission.await.is_err() {
                    let _ = completion.send(Err(LastFmAuthorizationError::OwnerStopped));
                    return Ok(());
                }
                self.begin_exchange(generation, flow, completion).await?;
            }
            Command::Cancel {
                generation,
                flow,
                completion,
                admission,
            } => {
                if admission.await.is_err() {
                    let _ = completion.send(Err(LastFmAuthorizationError::OwnerStopped));
                    return Ok(());
                }
                let exact_active = self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.generation == generation && active.flow == flow);
                let joined = if exact_active {
                    self.retire_active(LastFmAuthorizationError::Cancelled)
                        .await
                } else {
                    true
                };
                if !joined {
                    let _ = completion.send(Err(LastFmAuthorizationError::OwnerStopped));
                    return Err(LastFmAuthorizationShutdownError);
                }
                let ingress = Arc::clone(&self.ingress);
                let mut ingress = ingress
                    .lock()
                    .map_err(|_| LastFmAuthorizationShutdownError)?;
                let current_is_exact = ingress.current.as_ref().is_some_and(|current| {
                    current.generation == generation && current.flow == flow
                });
                if current_is_exact {
                    ingress.clear_current_exact(generation, &flow, true);
                    ingress.terminal = Some((generation, LastFmAuthorizationError::Cancelled));
                    ingress.publish(LastFmAuthorizationPhase::Idle, None);
                }
                let _ = completion.send(Ok(()));
            }
        }
        Ok(())
    }

    fn finish_request(
        &mut self,
        result: Result<RequestTaskExit, JoinError>,
    ) -> Result<(), LastFmAuthorizationShutdownError> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        let ActiveFlow {
            generation,
            flow,
            cancellation,
            phase,
        } = active;
        let FlowPhase::Requesting(requesting) = phase else {
            return Err(LastFmAuthorizationShutdownError);
        };
        let completion = requesting.completion;
        let Ok(result) = result else {
            let ingress = Arc::clone(&self.ingress);
            let mut ingress = ingress
                .lock()
                .map_err(|_| LastFmAuthorizationShutdownError)?;
            if ingress.current_matches(generation, &flow) {
                ingress.clear_current_exact(generation, &flow, true);
                ingress.terminal = Some((generation, LastFmAuthorizationError::OwnerStopped));
                ingress.publish(
                    LastFmAuthorizationPhase::Failed,
                    Some(LastFmAuthorizationError::OwnerStopped),
                );
                let _ = completion.send(Err(LastFmAuthorizationError::OwnerStopped));
            } else {
                let error = ingress.reason_for(generation, &flow);
                let _ = completion.send(Err(error));
            }
            return Err(LastFmAuthorizationShutdownError);
        };
        let (token, observed_at) = match result {
            RequestTaskExit::Completed {
                result: Ok(token),
                observed_at,
            } => (token, observed_at),
            RequestTaskExit::Completed {
                result: Err(error), ..
            } => {
                let mapped = map_client_error(error);
                let ingress = Arc::clone(&self.ingress);
                let mut ingress = ingress
                    .lock()
                    .map_err(|_| LastFmAuthorizationShutdownError)?;
                if ingress.current_matches(generation, &flow) {
                    ingress.clear_current_exact(generation, &flow, true);
                    ingress.terminal = Some((generation, mapped));
                    ingress.publish(LastFmAuthorizationPhase::Failed, Some(mapped));
                    let _ = completion.send(Err(mapped));
                } else {
                    let reason = ingress.reason_for(generation, &flow);
                    let _ = completion.send(Err(reason));
                }
                return Ok(());
            }
            RequestTaskExit::Cancelled => {
                let ingress = Arc::clone(&self.ingress);
                let ingress = ingress
                    .lock()
                    .map_err(|_| LastFmAuthorizationShutdownError)?;
                let reason = ingress.reason_for(generation, &flow);
                let _ = completion.send(Err(reason));
                return Ok(());
            }
        };

        let Some(deadline) = observed_at.checked_add(DESKTOP_TOKEN_LIFETIME) else {
            let error = LastFmAuthorizationError::CapabilityUnavailable;
            let ingress = Arc::clone(&self.ingress);
            let mut ingress = ingress
                .lock()
                .map_err(|_| LastFmAuthorizationShutdownError)?;
            if ingress.current_matches(generation, &flow) {
                ingress.clear_current_exact(generation, &flow, true);
                ingress.terminal = Some((generation, error));
                ingress.publish(LastFmAuthorizationPhase::Failed, Some(error));
                let _ = completion.send(Err(error));
            } else {
                let reason = ingress.reason_for(generation, &flow);
                let _ = completion.send(Err(reason));
            }
            return Ok(());
        };
        if self.clock.now() >= deadline {
            let error = LastFmAuthorizationError::Expired;
            let ingress = Arc::clone(&self.ingress);
            let mut ingress = ingress
                .lock()
                .map_err(|_| LastFmAuthorizationShutdownError)?;
            if ingress.current_matches(generation, &flow) {
                ingress.clear_current_exact(generation, &flow, true);
                ingress.terminal = Some((generation, error));
                ingress.publish(LastFmAuthorizationPhase::Expired, Some(error));
                let _ = completion.send(Err(error));
            } else {
                let reason = ingress.reason_for(generation, &flow);
                let _ = completion.send(Err(reason));
            }
            return Ok(());
        }
        let authorization_url = match self.transport.authorization_url(&token) {
            Ok(url) => url,
            Err(error) => {
                let mapped = map_client_error(error);
                let ingress = Arc::clone(&self.ingress);
                let mut ingress = ingress
                    .lock()
                    .map_err(|_| LastFmAuthorizationShutdownError)?;
                if ingress.current_matches(generation, &flow) {
                    ingress.clear_current_exact(generation, &flow, true);
                    ingress.terminal = Some((generation, mapped));
                    ingress.publish(LastFmAuthorizationPhase::Failed, Some(mapped));
                    let _ = completion.send(Err(mapped));
                } else {
                    let reason = ingress.reason_for(generation, &flow);
                    let _ = completion.send(Err(reason));
                }
                return Ok(());
            }
        };
        let finish = LastFmAuthorizationFinish::fresh();
        let challenge = LastFmAuthorizationChallenge(Arc::new(ChallengeInner {
            generation,
            flow: flow.clone(),
            finish: finish.clone(),
            handle: self.handle.clone(),
        }));
        let ingress = Arc::clone(&self.ingress);
        let mut ingress = ingress
            .lock()
            .map_err(|_| LastFmAuthorizationShutdownError)?;
        if !ingress.current_matches(generation, &flow) {
            let reason = ingress.reason_for(generation, &flow);
            let _ = completion.send(Err(reason));
            return Ok(());
        }
        if self.clock.now() >= deadline {
            let error = LastFmAuthorizationError::Expired;
            ingress.clear_current_exact(generation, &flow, true);
            ingress.terminal = Some((generation, error));
            ingress.publish(LastFmAuthorizationPhase::Expired, Some(error));
            let _ = completion.send(Err(error));
            return Ok(());
        }
        let current = ingress
            .current
            .as_mut()
            .expect("exact current checked under ingress gate");
        current.finish = Some(finish);
        current.authorization_url = Some(authorization_url);
        current.challenge_deadline = Some(deadline);
        if completion.send(Ok(challenge)).is_ok() {
            let expiry_cancellation = CancellationToken::new();
            let expiry_task = spawn_expiry_task(
                Arc::clone(&self.clock),
                deadline,
                cancellation.clone(),
                expiry_cancellation.clone(),
            );
            self.active = Some(ActiveFlow {
                generation,
                flow,
                cancellation,
                phase: FlowPhase::Awaiting(AwaitingFlow {
                    token,
                    deadline,
                    expiry_cancellation,
                    expiry_task,
                }),
            });
            ingress.publish(LastFmAuthorizationPhase::AwaitingApproval, None);
        } else {
            ingress.clear_current_exact(generation, &flow, true);
            ingress.terminal = Some((generation, LastFmAuthorizationError::Cancelled));
            ingress.publish(LastFmAuthorizationPhase::Idle, None);
        }
        Ok(())
    }

    async fn begin_exchange(
        &mut self,
        generation: u64,
        flow: LastFmAuthorizationFlow,
        completion: oneshot::Sender<Result<LastFmAuthorizationGrant, LastFmAuthorizationError>>,
    ) -> Result<(), LastFmAuthorizationShutdownError> {
        let Some(active) = self.active.take() else {
            let ingress = Arc::clone(&self.ingress);
            let ingress = ingress
                .lock()
                .map_err(|_| LastFmAuthorizationShutdownError)?;
            let error = ingress.terminal_error(generation, &flow);
            let _ = completion.send(Err(error));
            return Ok(());
        };
        if active.generation != generation || active.flow != flow {
            self.active = Some(active);
            let ingress = Arc::clone(&self.ingress);
            let ingress = ingress
                .lock()
                .map_err(|_| LastFmAuthorizationShutdownError)?;
            let error = ingress.reason_for(generation, &flow);
            let _ = completion.send(Err(error));
            return Ok(());
        }
        let ActiveFlow {
            generation,
            flow,
            cancellation,
            phase,
        } = active;
        let FlowPhase::Awaiting(awaiting) = phase else {
            self.active = Some(ActiveFlow {
                generation,
                flow: flow.clone(),
                cancellation,
                phase,
            });
            let ingress = Arc::clone(&self.ingress);
            let ingress = ingress
                .lock()
                .map_err(|_| LastFmAuthorizationShutdownError)?;
            let error = ingress.reason_for(generation, &flow);
            let _ = completion.send(Err(error));
            return Ok(());
        };
        awaiting.expiry_cancellation.cancel();
        if awaiting.expiry_task.await.is_err() {
            let ingress = Arc::clone(&self.ingress);
            let mut ingress = ingress
                .lock()
                .map_err(|_| LastFmAuthorizationShutdownError)?;
            if ingress.current_matches(generation, &flow) {
                ingress.clear_current_exact(generation, &flow, true);
                ingress.terminal = Some((generation, LastFmAuthorizationError::OwnerStopped));
                ingress.publish(
                    LastFmAuthorizationPhase::Failed,
                    Some(LastFmAuthorizationError::OwnerStopped),
                );
                let _ = completion.send(Err(LastFmAuthorizationError::OwnerStopped));
            } else {
                let error = ingress.reason_for(generation, &flow);
                let _ = completion.send(Err(error));
            }
            return Err(LastFmAuthorizationShutdownError);
        }
        let ingress = Arc::clone(&self.ingress);
        let mut ingress = ingress
            .lock()
            .map_err(|_| LastFmAuthorizationShutdownError)?;
        if !ingress.current_matches(generation, &flow) {
            let reason = ingress.reason_for(generation, &flow);
            let _ = completion.send(Err(reason));
            return Ok(());
        }
        if self.clock.now() >= awaiting.deadline {
            ingress.clear_current_exact(generation, &flow, true);
            ingress.terminal = Some((generation, LastFmAuthorizationError::Expired));
            ingress.publish(
                LastFmAuthorizationPhase::Expired,
                Some(LastFmAuthorizationError::Expired),
            );
            let _ = completion.send(Err(LastFmAuthorizationError::Expired));
            return Ok(());
        }
        let task = spawn_exchange_task(
            Arc::clone(&self.transport),
            awaiting.token,
            cancellation.clone(),
        );
        self.active = Some(ActiveFlow {
            generation,
            flow,
            cancellation,
            phase: FlowPhase::Exchanging(ExchangingFlow { task, completion }),
        });
        ingress.publish(LastFmAuthorizationPhase::Exchanging, None);
        Ok(())
    }

    fn finish_expiry(
        &mut self,
        result: Result<ExpiryTaskExit, JoinError>,
    ) -> Result<(), LastFmAuthorizationShutdownError> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        let ActiveFlow {
            generation,
            flow,
            cancellation,
            phase,
        } = active;
        let FlowPhase::Awaiting(awaiting) = phase else {
            return Err(LastFmAuthorizationShutdownError);
        };
        match result {
            Ok(ExpiryTaskExit::Expired) => {
                let ingress = Arc::clone(&self.ingress);
                let mut ingress = ingress
                    .lock()
                    .map_err(|_| LastFmAuthorizationShutdownError)?;
                if !ingress.current_matches(generation, &flow) {
                    return Ok(());
                }
                if self.clock.now() < awaiting.deadline {
                    let expiry_cancellation = CancellationToken::new();
                    let expiry_task = spawn_expiry_task(
                        Arc::clone(&self.clock),
                        awaiting.deadline,
                        cancellation.clone(),
                        expiry_cancellation.clone(),
                    );
                    self.active = Some(ActiveFlow {
                        generation,
                        flow,
                        cancellation,
                        phase: FlowPhase::Awaiting(AwaitingFlow {
                            token: awaiting.token,
                            deadline: awaiting.deadline,
                            expiry_cancellation,
                            expiry_task,
                        }),
                    });
                } else {
                    ingress.clear_current_exact(generation, &flow, true);
                    ingress.terminal = Some((generation, LastFmAuthorizationError::Expired));
                    ingress.publish(
                        LastFmAuthorizationPhase::Expired,
                        Some(LastFmAuthorizationError::Expired),
                    );
                }
            }
            Ok(ExpiryTaskExit::Cancelled) => {}
            Err(_) => {
                let ingress = Arc::clone(&self.ingress);
                let mut ingress = ingress
                    .lock()
                    .map_err(|_| LastFmAuthorizationShutdownError)?;
                if ingress.current_matches(generation, &flow) {
                    ingress.clear_current_exact(generation, &flow, true);
                    ingress.terminal = Some((generation, LastFmAuthorizationError::OwnerStopped));
                    ingress.publish(
                        LastFmAuthorizationPhase::Failed,
                        Some(LastFmAuthorizationError::OwnerStopped),
                    );
                }
                return Err(LastFmAuthorizationShutdownError);
            }
        }
        Ok(())
    }

    fn finish_exchange(
        &mut self,
        result: Result<ExchangeTaskExit, JoinError>,
    ) -> Result<(), LastFmAuthorizationShutdownError> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        let ActiveFlow {
            generation,
            flow,
            cancellation: _,
            phase,
        } = active;
        let FlowPhase::Exchanging(exchanging) = phase else {
            return Err(LastFmAuthorizationShutdownError);
        };
        let completion = exchanging.completion;
        let Ok(result) = result else {
            let ingress = Arc::clone(&self.ingress);
            let mut ingress = ingress
                .lock()
                .map_err(|_| LastFmAuthorizationShutdownError)?;
            if ingress.current_matches(generation, &flow) {
                ingress.clear_current_exact(generation, &flow, true);
                ingress.terminal = Some((generation, LastFmAuthorizationError::OwnerStopped));
                ingress.publish(
                    LastFmAuthorizationPhase::Failed,
                    Some(LastFmAuthorizationError::OwnerStopped),
                );
                let _ = completion.send(Err(LastFmAuthorizationError::OwnerStopped));
            } else {
                let error = ingress.reason_for(generation, &flow);
                let _ = completion.send(Err(error));
            }
            return Err(LastFmAuthorizationShutdownError);
        };
        let ingress = Arc::clone(&self.ingress);
        let mut ingress = ingress
            .lock()
            .map_err(|_| LastFmAuthorizationShutdownError)?;
        if !ingress.current_matches(generation, &flow) {
            let reason = ingress.reason_for(generation, &flow);
            let _ = completion.send(Err(reason));
            return Ok(());
        }
        match result {
            ExchangeTaskExit::Completed(Ok(session)) => {
                if completion
                    .send(Ok(LastFmAuthorizationGrant(session)))
                    .is_ok()
                {
                    ingress.clear_current_exact(generation, &flow, false);
                    ingress.terminal = None;
                    ingress.publish(LastFmAuthorizationPhase::GrantIssued, None);
                } else {
                    ingress.clear_current_exact(generation, &flow, true);
                    ingress.terminal = Some((generation, LastFmAuthorizationError::Cancelled));
                    ingress.publish(LastFmAuthorizationPhase::Idle, None);
                }
            }
            ExchangeTaskExit::Cancelled => {
                let reason = ingress.reason_for(generation, &flow);
                let _ = completion.send(Err(reason));
            }
            ExchangeTaskExit::Completed(Err(error)) => {
                let mapped = map_client_error(error);
                ingress.clear_current_exact(generation, &flow, true);
                ingress.terminal = Some((generation, mapped));
                ingress.publish(LastFmAuthorizationPhase::Failed, Some(mapped));
                let _ = completion.send(Err(mapped));
            }
        }
        Ok(())
    }

    async fn retire_active(&mut self, reason: LastFmAuthorizationError) -> bool {
        let Some(active) = self.active.take() else {
            return true;
        };
        active.cancellation.cancel();
        match active.phase {
            FlowPhase::Requesting(requesting) => {
                let joined = requesting.task.await.is_ok();
                let _ = requesting.completion.send(Err(reason));
                joined
            }
            FlowPhase::Awaiting(awaiting) => {
                awaiting.expiry_cancellation.cancel();
                awaiting.expiry_task.await.is_ok()
            }
            FlowPhase::Exchanging(exchanging) => {
                let joined = exchanging.task.await.is_ok();
                let _ = exchanging.completion.send(Err(reason));
                joined
            }
        }
    }

    fn reject_queued(&self, error: LastFmAuthorizationError) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Begin { completion, .. } => {
                    let _ = completion.send(Err(error));
                }
                Command::Finish { completion, .. } => {
                    let _ = completion.send(Err(error));
                }
                Command::Cancel { completion, .. } => {
                    let _ = completion.send(Err(error));
                }
            }
        }
    }

    fn publish_global(
        &self,
        phase: LastFmAuthorizationPhase,
        failure: Option<LastFmAuthorizationError>,
    ) -> Result<(), LastFmAuthorizationShutdownError> {
        let mut ingress = self
            .ingress
            .lock()
            .map_err(|_| LastFmAuthorizationShutdownError)?;
        ingress.publish(phase, failure);
        Ok(())
    }

    async fn quiesce_after_panic(&mut self) {
        {
            let mut ingress = self
                .ingress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ingress.open = false;
            if let Some(mut current) = ingress.current.take() {
                current.revoke_challenge();
                current.cancellation.cancel();
            }
            ingress.publish_owner_stopped_unless_terminal();
        }
        self.commands.close();
        let _ = self
            .retire_active(LastFmAuthorizationError::OwnerStopped)
            .await;
        self.reject_queued(LastFmAuthorizationError::OwnerStopped);
    }
}

impl Drop for AuthorizationOwner {
    fn drop(&mut self) {
        {
            let mut ingress = self
                .ingress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ingress.open = false;
            if let Some(mut current) = ingress.current.take() {
                current.revoke_challenge();
                current.cancellation.cancel();
            }
            ingress.publish_owner_stopped_unless_terminal();
        }
        self.commands.close();
        if let Some(active) = &mut self.active {
            active.cancellation.cancel();
            match &mut active.phase {
                FlowPhase::Requesting(requesting) => requesting.task.abort(),
                FlowPhase::Awaiting(awaiting) => {
                    awaiting.expiry_cancellation.cancel();
                    awaiting.expiry_task.abort();
                }
                FlowPhase::Exchanging(exchanging) => exchanging.task.abort(),
            }
        }
    }
}

fn spawn_request_task(
    transport: Arc<dyn LastFmAuthorizationTransport>,
    clock: Arc<dyn LastFmAuthorizationClock>,
    cancellation: CancellationToken,
) -> JoinHandle<RequestTaskExit> {
    tokio::spawn(async move {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => RequestTaskExit::Cancelled,
            result = transport.request_auth_token() => RequestTaskExit::Completed {
                result,
                observed_at: clock.now(),
            },
        }
    })
}

fn spawn_expiry_task(
    clock: Arc<dyn LastFmAuthorizationClock>,
    deadline: Duration,
    flow_cancellation: CancellationToken,
    expiry_cancellation: CancellationToken,
) -> JoinHandle<ExpiryTaskExit> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = flow_cancellation.cancelled() => return ExpiryTaskExit::Cancelled,
                () = expiry_cancellation.cancelled() => return ExpiryTaskExit::Cancelled,
                () = clock.wait_until(deadline) => {
                    if clock.now() >= deadline {
                        return ExpiryTaskExit::Expired;
                    }
                }
            }
        }
    })
}

fn spawn_exchange_task(
    transport: Arc<dyn LastFmAuthorizationTransport>,
    token: DesktopAuthToken,
    cancellation: CancellationToken,
) -> JoinHandle<ExchangeTaskExit> {
    tokio::spawn(async move {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => ExchangeTaskExit::Cancelled,
            result = transport.exchange_auth_token(token) => ExchangeTaskExit::Completed(result),
        }
    })
}

/// Why the explicit owner drain completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmAuthorizationShutdownReason {
    Drained,
}

/// Fixed failure when authorization work did not complete an orderly drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Last.fm authorization owner did not drain")]
pub struct LastFmAuthorizationShutdownError;

/// Persistent state of the authorization shutdown proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmAuthorizationDrainState {
    Pending,
    Drained,
    Failed,
}

struct CompletionGuard {
    sender: watch::Sender<LastFmAuthorizationDrainState>,
    drained: bool,
}

impl CompletionGuard {
    fn mark_drained(&mut self) {
        self.sender
            .send_replace(LastFmAuthorizationDrainState::Drained);
        self.drained = true;
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.drained {
            self.sender
                .send_replace(LastFmAuthorizationDrainState::Failed);
        }
    }
}

/// Sole join side for the authorization owner.
pub struct LastFmAuthorizationShutdown {
    inner: Arc<HandleInner>,
    owner: Option<
        JoinHandle<Result<LastFmAuthorizationShutdownReason, LastFmAuthorizationShutdownError>>,
    >,
    completion: watch::Receiver<LastFmAuthorizationDrainState>,
}

impl LastFmAuthorizationShutdown {
    pub fn barrier(&self) -> LastFmAuthorizationBarrier {
        LastFmAuthorizationBarrier {
            completion: self.completion.clone(),
        }
    }

    pub async fn shutdown(
        mut self,
    ) -> Result<LastFmAuthorizationShutdownReason, LastFmAuthorizationShutdownError> {
        request_close(&self.inner);
        let owner = self.owner.take().ok_or(LastFmAuthorizationShutdownError)?;
        match owner.await {
            Ok(result) => result,
            Err(_) => Err(LastFmAuthorizationShutdownError),
        }
    }

    #[cfg(test)]
    fn abort_owner_for_test(&self) {
        if let Some(owner) = &self.owner {
            owner.abort();
        }
    }
}

impl Drop for LastFmAuthorizationShutdown {
    fn drop(&mut self) {
        request_close(&self.inner);
    }
}

impl fmt::Debug for LastFmAuthorizationShutdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmAuthorizationShutdown")
            .field("drain_state", &*self.completion.borrow())
            .finish_non_exhaustive()
    }
}

/// Cloneable proof of normal drain or abnormal owner loss.
#[derive(Clone)]
pub struct LastFmAuthorizationBarrier {
    completion: watch::Receiver<LastFmAuthorizationDrainState>,
}

impl LastFmAuthorizationBarrier {
    pub fn state(&self) -> LastFmAuthorizationDrainState {
        *self.completion.borrow()
    }

    pub async fn wait(&self) -> Result<(), LastFmAuthorizationShutdownError> {
        let mut completion = self.completion.clone();
        loop {
            let state = *completion.borrow_and_update();
            match state {
                LastFmAuthorizationDrainState::Drained => return Ok(()),
                LastFmAuthorizationDrainState::Failed => {
                    return Err(LastFmAuthorizationShutdownError);
                }
                LastFmAuthorizationDrainState::Pending => {}
            }
            if completion.changed().await.is_err() {
                return Err(LastFmAuthorizationShutdownError);
            }
        }
    }
}

impl fmt::Debug for LastFmAuthorizationBarrier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmAuthorizationBarrier")
            .field("state", &self.state())
            .finish()
    }
}

/// Spawn the internal authorization owner with injected deterministic seams.
pub(super) fn spawn_lastfm_authorization(
    transport: Arc<dyn LastFmAuthorizationTransport>,
    clock: Arc<dyn LastFmAuthorizationClock>,
) -> (LastFmAuthorizationHandle, LastFmAuthorizationShutdown) {
    spawn_lastfm_authorization_with_generation_ceiling(transport, clock, u64::MAX)
}

fn spawn_lastfm_authorization_with_generation_ceiling(
    transport: Arc<dyn LastFmAuthorizationTransport>,
    clock: Arc<dyn LastFmAuthorizationClock>,
    generation_ceiling: u64,
) -> (LastFmAuthorizationHandle, LastFmAuthorizationShutdown) {
    spawn_lastfm_authorization_with_options(
        transport,
        clock,
        AuthorizationSpawnOptions {
            generation_ceiling,
            #[cfg(test)]
            result_gate: None,
        },
    )
}

struct AuthorizationSpawnOptions {
    generation_ceiling: u64,
    #[cfg(test)]
    result_gate: Option<AuthorizationResultGate>,
}

fn spawn_lastfm_authorization_with_options(
    transport: Arc<dyn LastFmAuthorizationTransport>,
    clock: Arc<dyn LastFmAuthorizationClock>,
    options: AuthorizationSpawnOptions,
) -> (LastFmAuthorizationHandle, LastFmAuthorizationShutdown) {
    let (commands, receiver) = async_channel::bounded(AUTHORIZATION_COMMAND_CAPACITY);
    let (status_sender, status) = watch::channel(LastFmAuthorizationStatus::INITIAL);
    let ingress = Arc::new(Mutex::new(IngressGate {
        open: true,
        generation: 0,
        generation_ceiling: options.generation_ceiling,
        current: None,
        terminal: None,
        status_sender,
        status: LastFmAuthorizationStatus::INITIAL,
    }));
    let inner = Arc::new(HandleInner {
        commands,
        ingress: Arc::clone(&ingress),
        clock: Arc::clone(&clock),
        status,
    });
    let mut owner = AuthorizationOwner {
        commands: receiver,
        ingress,
        handle: Arc::downgrade(&inner),
        transport,
        clock,
        active: None,
        #[cfg(test)]
        result_gate: options.result_gate,
    };
    let (completion_sender, completion) = watch::channel(LastFmAuthorizationDrainState::Pending);
    let owner_task = tokio::spawn(async move {
        let mut completion = CompletionGuard {
            sender: completion_sender,
            drained: false,
        };
        let result = match AssertUnwindSafe(owner.run()).catch_unwind().await {
            Ok(result) => result,
            Err(_) => {
                owner.quiesce_after_panic().await;
                Err(LastFmAuthorizationShutdownError)
            }
        };
        if result.is_ok() {
            completion.mark_drained();
        }
        result
    });
    (
        LastFmAuthorizationHandle {
            inner: Arc::clone(&inner),
        },
        LastFmAuthorizationShutdown {
            inner,
            owner: Some(owner_task),
            completion,
        },
    )
}

#[cfg(test)]
#[path = "authorization_tests.rs"]
mod tests;
