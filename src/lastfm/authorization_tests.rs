use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use super::*;

const API_KEY: &str = "0123456789abcdef0123456789abcdef";
const TOKEN: &str = "fedcba9876543210fedcba9876543210";
const SESSION_KEY: &str = "11111111111111111111111111111111";
const AUTHORIZATION_URL: &str = "https://www.last.fm/api/auth/?api_key=0123456789abcdef0123456789abcdef&token=fedcba9876543210fedcba9876543210";
const TEST_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransportEvent {
    RequestStarted(u64),
    RequestDropped(u64),
    ExchangeStarted(u64),
    ExchangeDropped(u64),
}

#[derive(Clone, Copy)]
enum CallKind {
    Request,
    Exchange,
}

type UrlHook = Box<dyn FnOnce() + Send + 'static>;

struct ActiveCall {
    kind: CallKind,
    id: u64,
    active: Arc<AtomicUsize>,
    events: async_channel::Sender<TransportEvent>,
}

impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        let event = match self.kind {
            CallKind::Request => TransportEvent::RequestDropped(self.id),
            CallKind::Exchange => TransportEvent::ExchangeDropped(self.id),
        };
        let _ = self.events.try_send(event);
    }
}

struct ScriptedTransport {
    requests: async_channel::Receiver<Result<DesktopAuthToken, LastFmClientError>>,
    exchanges: async_channel::Receiver<Result<DesktopAuthorizedSession, LastFmClientError>>,
    url_errors: Mutex<VecDeque<LastFmClientError>>,
    url_hook: Arc<Mutex<Option<UrlHook>>>,
    events: async_channel::Sender<TransportEvent>,
    next_request: AtomicU64,
    next_exchange: AtomicU64,
    active: Arc<AtomicUsize>,
    maximum_active: Arc<AtomicUsize>,
    request_count: Arc<AtomicUsize>,
    exchange_count: Arc<AtomicUsize>,
    url_count: Arc<AtomicUsize>,
}

struct TransportControl {
    requests: async_channel::Sender<Result<DesktopAuthToken, LastFmClientError>>,
    exchanges: async_channel::Sender<Result<DesktopAuthorizedSession, LastFmClientError>>,
    events: async_channel::Receiver<TransportEvent>,
    maximum_active: Arc<AtomicUsize>,
    request_count: Arc<AtomicUsize>,
    exchange_count: Arc<AtomicUsize>,
    url_count: Arc<AtomicUsize>,
    url_hook: Arc<Mutex<Option<UrlHook>>>,
}

impl ScriptedTransport {
    fn new() -> (Arc<Self>, TransportControl) {
        let (request_sender, requests) = async_channel::unbounded();
        let (exchange_sender, exchanges) = async_channel::unbounded();
        let (event_sender, events) = async_channel::unbounded();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum_active = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::new(AtomicUsize::new(0));
        let exchange_count = Arc::new(AtomicUsize::new(0));
        let url_count = Arc::new(AtomicUsize::new(0));
        let url_hook = Arc::new(Mutex::new(None));
        (
            Arc::new(Self {
                requests,
                exchanges,
                url_errors: Mutex::new(VecDeque::new()),
                url_hook: Arc::clone(&url_hook),
                events: event_sender,
                next_request: AtomicU64::new(0),
                next_exchange: AtomicU64::new(0),
                active,
                maximum_active: Arc::clone(&maximum_active),
                request_count: Arc::clone(&request_count),
                exchange_count: Arc::clone(&exchange_count),
                url_count: Arc::clone(&url_count),
            }),
            TransportControl {
                requests: request_sender,
                exchanges: exchange_sender,
                events,
                maximum_active,
                request_count,
                exchange_count,
                url_count,
                url_hook,
            },
        )
    }

    fn begin_call(&self, kind: CallKind) -> ActiveCall {
        let id = match kind {
            CallKind::Request => self.next_request.fetch_add(1, Ordering::SeqCst) + 1,
            CallKind::Exchange => self.next_exchange.fetch_add(1, Ordering::SeqCst) + 1,
        };
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum_active.fetch_max(active, Ordering::SeqCst);
        match kind {
            CallKind::Request => {
                self.request_count.fetch_add(1, Ordering::SeqCst);
                let _ = self.events.try_send(TransportEvent::RequestStarted(id));
            }
            CallKind::Exchange => {
                self.exchange_count.fetch_add(1, Ordering::SeqCst);
                let _ = self.events.try_send(TransportEvent::ExchangeStarted(id));
            }
        }
        ActiveCall {
            kind,
            id,
            active: Arc::clone(&self.active),
            events: self.events.clone(),
        }
    }
}

impl TransportControl {
    fn set_url_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .url_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
    }
}

#[async_trait::async_trait]
impl LastFmAuthorizationTransport for ScriptedTransport {
    async fn request_auth_token(&self) -> Result<DesktopAuthToken, LastFmClientError> {
        let _call = self.begin_call(CallKind::Request);
        self.requests
            .recv()
            .await
            .unwrap_or(Err(LastFmClientError::Transport))
    }

    fn authorization_url(
        &self,
        _token: &DesktopAuthToken,
    ) -> Result<DesktopAuthorizationUrl, LastFmClientError> {
        self.url_count.fetch_add(1, Ordering::SeqCst);
        let hook = self
            .url_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
        let error = {
            self.url_errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        };
        if let Some(error) = error {
            return Err(error);
        }
        DesktopAuthorizationUrl::for_test(AUTHORIZATION_URL)
    }

    async fn exchange_auth_token(
        &self,
        _token: DesktopAuthToken,
    ) -> Result<DesktopAuthorizedSession, LastFmClientError> {
        let _call = self.begin_call(CallKind::Exchange);
        self.exchanges
            .recv()
            .await
            .unwrap_or(Err(LastFmClientError::Transport))
    }
}

#[derive(Default)]
struct ClockState {
    now: Duration,
    waiters: Vec<(Duration, oneshot::Sender<()>)>,
}

#[derive(Default)]
struct ScriptedClock {
    state: Mutex<ClockState>,
}

impl ScriptedClock {
    fn at(now: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ClockState {
                now,
                waiters: Vec::new(),
            }),
        })
    }

    fn set_without_wake(&self, now: Duration) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(now >= state.now, "scripted monotonic clock cannot regress");
        state.now = now;
    }

    fn advance_to(&self, now: Duration) {
        let due = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(now >= state.now, "scripted monotonic clock cannot regress");
            state.now = now;
            let mut pending = Vec::new();
            let mut due = Vec::new();
            for (deadline, waiter) in state.waiters.drain(..) {
                if deadline <= now {
                    due.push(waiter);
                } else {
                    pending.push((deadline, waiter));
                }
            }
            state.waiters = pending;
            due
        };
        for waiter in due {
            let _ = waiter.send(());
        }
    }
}

#[async_trait::async_trait]
impl LastFmAuthorizationClock for ScriptedClock {
    fn now(&self) -> Duration {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .now
    }

    async fn wait_until(&self, deadline: Duration) {
        let receiver = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.now >= deadline {
                return;
            }
            let (sender, receiver) = oneshot::channel();
            state.waiters.push((deadline, sender));
            receiver
        };
        let _ = receiver.await;
    }
}

struct Harness {
    handle: LastFmAuthorizationHandle,
    shutdown: LastFmAuthorizationShutdown,
    control: TransportControl,
    clock: Arc<ScriptedClock>,
}

fn harness(now: Duration) -> Harness {
    harness_with_options(
        now,
        AuthorizationSpawnOptions {
            generation_ceiling: u64::MAX,
            result_gate: None,
        },
    )
}

fn harness_with_options(now: Duration, options: AuthorizationSpawnOptions) -> Harness {
    let (transport, control) = ScriptedTransport::new();
    let clock = ScriptedClock::at(now);
    let transport: Arc<dyn LastFmAuthorizationTransport> = transport;
    let injected_clock: Arc<dyn LastFmAuthorizationClock> = clock.clone();
    let (handle, shutdown) =
        spawn_lastfm_authorization_with_options(transport, injected_clock, options);
    Harness {
        handle,
        shutdown,
        control,
        clock,
    }
}

fn token() -> DesktopAuthToken {
    DesktopAuthToken::for_test(TOKEN).expect("valid token fixture")
}

fn session() -> DesktopAuthorizedSession {
    DesktopAuthorizedSession::for_test("private-listener", SESSION_KEY)
        .expect("valid session fixture")
}

fn authorization_url(
    challenge: &LastFmAuthorizationChallenge,
) -> Result<String, LastFmAuthorizationAdmissionError> {
    challenge.with_authorization_url(str::to_owned)
}

fn assert_url_revoked(challenge: &LastFmAuthorizationChallenge) {
    assert!(authorization_url(challenge).is_err());
}

async fn send_request(
    control: &TransportControl,
    result: Result<DesktopAuthToken, LastFmClientError>,
) {
    control
        .requests
        .send(result)
        .await
        .expect("request response receiver remains live");
}

async fn send_exchange(
    control: &TransportControl,
    result: Result<DesktopAuthorizedSession, LastFmClientError>,
) {
    control
        .exchanges
        .send(result)
        .await
        .expect("exchange response receiver remains live");
}

async fn expect_event(control: &TransportControl, expected: TransportEvent) {
    let observed = tokio::time::timeout(TEST_DEADLINE, control.events.recv())
        .await
        .expect("transport event deadline")
        .expect("transport event channel remains live");
    assert_eq!(observed, expected);
}

async fn wait_for_phase(
    status: &mut watch::Receiver<LastFmAuthorizationStatus>,
    expected: LastFmAuthorizationPhase,
) -> LastFmAuthorizationStatus {
    loop {
        let observed = *status.borrow_and_update();
        if observed.phase == expected {
            return observed;
        }
        tokio::time::timeout(TEST_DEADLINE, status.changed())
            .await
            .expect("status deadline")
            .expect("status sender remains live");
    }
}

async fn ready_challenge(
    harness: &Harness,
) -> (LastFmAuthorizationFlow, LastFmAuthorizationChallenge) {
    let start = harness.handle.try_begin().expect("flow admission");
    let flow = start.flow();
    let request_id = harness.control.request_count.load(Ordering::SeqCst) as u64 + 1;
    expect_event(&harness.control, TransportEvent::RequestStarted(request_id)).await;
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(request_id)).await;
    let challenge = tokio::time::timeout(TEST_DEADLINE, start.wait())
        .await
        .expect("challenge deadline")
        .expect("challenge ready");
    (flow, challenge)
}

async fn shutdown(harness: Harness) {
    assert_eq!(
        tokio::time::timeout(TEST_DEADLINE, harness.shutdown.shutdown())
            .await
            .expect("shutdown deadline"),
        Ok(LastFmAuthorizationShutdownReason::Drained)
    );
}

#[tokio::test]
async fn challenge_is_repeatable_exchange_is_one_shot_and_secrets_are_redacted() {
    let harness = harness(Duration::from_secs(10));
    let mut status = harness.handle.subscribe_status();
    let (_flow, challenge) = ready_challenge(&harness).await;
    let retained = challenge.clone();
    assert_eq!(authorization_url(&challenge).unwrap(), AUTHORIZATION_URL);
    assert_eq!(authorization_url(&challenge).unwrap(), AUTHORIZATION_URL);
    assert_eq!(challenge.flow(), challenge.flow());

    let exchange = harness
        .handle
        .try_finish(&challenge)
        .expect("first finish is admitted");
    assert_url_revoked(&challenge);
    assert_url_revoked(&retained);
    assert_eq!(
        harness.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::FinishUnavailable
    );
    expect_event(&harness.control, TransportEvent::ExchangeStarted(1)).await;
    send_exchange(&harness.control, Ok(session())).await;
    expect_event(&harness.control, TransportEvent::ExchangeDropped(1)).await;
    let grant = exchange.wait().await.expect("exchange succeeds");
    let issued = wait_for_phase(&mut status, LastFmAuthorizationPhase::GrantIssued).await;
    assert_eq!(issued.failure, None);

    for rendered in [
        format!("{challenge:?}"),
        format!("{:?}", challenge.flow()),
        format!("{grant:?}"),
        format!("{:?}", harness.handle),
    ] {
        assert!(!rendered.contains(API_KEY));
        assert!(!rendered.contains(TOKEN));
        assert!(!rendered.contains(SESSION_KEY));
        assert!(!rendered.contains("private-listener"));
    }
    let staged = grant.into_authorized_session();
    assert_eq!(staged.username(), "private-listener");
    let (username, key) = staged.into_parts();
    assert_eq!(username.as_str(), "private-listener");
    assert_eq!(key.expose(), SESSION_KEY);
    assert_eq!(harness.control.exchange_count.load(Ordering::SeqCst), 1);
    assert_eq!(harness.control.maximum_active.load(Ordering::SeqCst), 1);
    shutdown(harness).await;
}

#[tokio::test]
async fn validity_is_strictly_before_response_time_plus_exactly_one_hour() {
    let before = harness(Duration::from_secs(100));
    let (_flow, challenge) = ready_challenge(&before).await;
    before.clock.set_without_wake(Duration::from_secs(3_699));
    let exchange = before.handle.try_finish(&challenge).unwrap();
    expect_event(&before.control, TransportEvent::ExchangeStarted(1)).await;
    send_exchange(&before.control, Ok(session())).await;
    expect_event(&before.control, TransportEvent::ExchangeDropped(1)).await;
    assert!(exchange.wait().await.is_ok());
    shutdown(before).await;

    let equality = harness(Duration::from_secs(100));
    let mut status = equality.handle.subscribe_status();
    let (_flow, challenge) = ready_challenge(&equality).await;
    equality.clock.set_without_wake(Duration::from_secs(3_700));
    assert_eq!(
        equality.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::FinishUnavailable
    );
    let expired = wait_for_phase(&mut status, LastFmAuthorizationPhase::Expired).await;
    assert_eq!(expired.failure, Some(LastFmAuthorizationError::Expired));
    assert_url_revoked(&challenge);
    assert_eq!(equality.control.exchange_count.load(Ordering::SeqCst), 0);
    shutdown(equality).await;
}

#[tokio::test]
async fn automatic_expiry_synchronously_revokes_url_and_finish() {
    let harness = harness(Duration::from_secs(20));
    let mut status = harness.handle.subscribe_status();
    let (flow, challenge) = ready_challenge(&harness).await;
    let retained = challenge.clone();
    harness.clock.advance_to(Duration::from_secs(3_620));
    let observed = wait_for_phase(&mut status, LastFmAuthorizationPhase::Expired).await;
    assert_eq!(observed.failure, Some(LastFmAuthorizationError::Expired));
    assert_url_revoked(&challenge);
    assert_url_revoked(&retained);
    assert_eq!(
        harness.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    assert_eq!(
        harness.handle.try_cancel(&flow).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    assert_eq!(harness.control.exchange_count.load(Ordering::SeqCst), 0);
    shutdown(harness).await;
}

#[tokio::test]
async fn unrepresentable_deadline_fails_before_url_or_exchange() {
    let now = Duration::MAX
        .checked_sub(Duration::from_secs(3_599))
        .unwrap();
    let harness = harness(now);
    let mut status = harness.handle.subscribe_status();
    let start = harness.handle.try_begin().unwrap();
    expect_event(&harness.control, TransportEvent::RequestStarted(1)).await;
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(1)).await;
    assert_eq!(
        start.wait().await.unwrap_err(),
        LastFmAuthorizationError::CapabilityUnavailable
    );
    let observed = wait_for_phase(&mut status, LastFmAuthorizationPhase::Failed).await;
    assert_eq!(
        observed.failure,
        Some(LastFmAuthorizationError::CapabilityUnavailable)
    );
    assert_eq!(harness.control.url_count.load(Ordering::SeqCst), 0);
    assert_eq!(harness.control.exchange_count.load(Ordering::SeqCst), 0);
    shutdown(harness).await;
}

#[tokio::test]
async fn request_supersession_cancels_and_joins_before_successor_starts() {
    let harness = harness(Duration::ZERO);
    let first = harness.handle.try_begin().unwrap();
    let first_flow = first.flow();
    expect_event(&harness.control, TransportEvent::RequestStarted(1)).await;
    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    expect_event(&harness.control, TransportEvent::RequestDropped(1)).await;
    assert_eq!(
        first.wait().await.unwrap_err(),
        LastFmAuthorizationError::Superseded
    );
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    assert_eq!(
        harness.handle.try_cancel(&first_flow).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let challenge = second.wait().await.unwrap();
    let cancelled = harness.handle.try_cancel(&second_flow).unwrap();
    assert_eq!(cancelled.wait().await, Ok(()));
    assert_eq!(
        harness.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    assert_eq!(harness.control.maximum_active.load(Ordering::SeqCst), 1);
    shutdown(harness).await;
}

#[tokio::test]
async fn approval_supersession_synchronously_revokes_old_url_and_actions() {
    let harness = harness(Duration::ZERO);
    let (first_flow, first_challenge) = ready_challenge(&harness).await;
    let retained = first_challenge.clone();
    assert_eq!(
        authorization_url(&first_challenge).unwrap(),
        AUTHORIZATION_URL
    );
    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    assert_url_revoked(&first_challenge);
    assert_url_revoked(&retained);
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    assert_eq!(
        harness.handle.try_finish(&first_challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    assert_eq!(
        harness.handle.try_cancel(&first_flow).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let second_challenge = second.wait().await.unwrap();
    assert_eq!(
        harness
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_eq!(
        harness.handle.try_finish(&second_challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    shutdown(harness).await;
}

#[tokio::test]
async fn exchange_supersession_joins_before_successor_request_and_drops_result() {
    let harness = harness(Duration::ZERO);
    let (_first_flow, first_challenge) = ready_challenge(&harness).await;
    let first_exchange = harness.handle.try_finish(&first_challenge).unwrap();
    expect_event(&harness.control, TransportEvent::ExchangeStarted(1)).await;
    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    expect_event(&harness.control, TransportEvent::ExchangeDropped(1)).await;
    assert_eq!(
        first_exchange.wait().await.unwrap_err(),
        LastFmAuthorizationError::Superseded
    );
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let second_challenge = second.wait().await.unwrap();
    assert_eq!(
        harness
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_eq!(
        harness.handle.try_finish(&first_challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    assert_eq!(
        harness.handle.try_finish(&second_challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    assert_eq!(harness.control.maximum_active.load(Ordering::SeqCst), 1);
    shutdown(harness).await;
}

#[tokio::test]
async fn exact_flow_cancel_is_joined_in_request_approval_and_exchange_phases() {
    let requesting = harness(Duration::ZERO);
    let start = requesting.handle.try_begin().unwrap();
    let flow = start.flow();
    expect_event(&requesting.control, TransportEvent::RequestStarted(1)).await;
    let cancel = requesting.handle.try_cancel(&flow).unwrap();
    expect_event(&requesting.control, TransportEvent::RequestDropped(1)).await;
    assert_eq!(
        start.wait().await.unwrap_err(),
        LastFmAuthorizationError::Cancelled
    );
    assert_eq!(cancel.wait().await, Ok(()));
    shutdown(requesting).await;

    let awaiting = harness(Duration::ZERO);
    let (flow, challenge) = ready_challenge(&awaiting).await;
    let retained = challenge.clone();
    let cancel = awaiting.handle.try_cancel(&flow).unwrap();
    assert_url_revoked(&challenge);
    assert_url_revoked(&retained);
    assert_eq!(cancel.wait().await, Ok(()));
    assert_eq!(
        awaiting.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    shutdown(awaiting).await;

    let exchanging = harness(Duration::ZERO);
    let (flow, challenge) = ready_challenge(&exchanging).await;
    let finish = exchanging.handle.try_finish(&challenge).unwrap();
    expect_event(&exchanging.control, TransportEvent::ExchangeStarted(1)).await;
    let cancel = exchanging.handle.try_cancel(&flow).unwrap();
    expect_event(&exchanging.control, TransportEvent::ExchangeDropped(1)).await;
    assert_eq!(
        finish.wait().await.unwrap_err(),
        LastFmAuthorizationError::Cancelled
    );
    assert_eq!(cancel.wait().await, Ok(()));
    shutdown(exchanging).await;
}

#[tokio::test]
async fn admitted_finish_uses_exact_cancel_or_close_reason_before_exchange_starts() {
    let (cancelled, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Finish);
    let (flow, challenge) = ready_challenge(&cancelled).await;
    let _stale = enqueue_stale_finish(&cancelled.handle);
    wait_for_gate(&reached).await;
    let finish = cancelled.handle.try_finish(&challenge).unwrap();
    let cancel = cancelled.handle.try_cancel(&flow).unwrap();
    tokio::task::yield_now().await;
    release.send(()).await.unwrap();
    assert_eq!(
        finish.wait().await.unwrap_err(),
        LastFmAuthorizationError::Cancelled
    );
    assert_eq!(cancel.wait().await, Ok(()));
    assert_eq!(cancelled.control.exchange_count.load(Ordering::SeqCst), 0);
    shutdown(cancelled).await;

    let (closed, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Finish);
    let (_flow, challenge) = ready_challenge(&closed).await;
    let _stale = enqueue_stale_finish(&closed.handle);
    wait_for_gate(&reached).await;
    let finish = closed.handle.try_finish(&challenge).unwrap();
    assert!(closed.handle.close_and_flush());
    tokio::task::yield_now().await;
    release.send(()).await.unwrap();
    assert_eq!(
        finish.wait().await.unwrap_err(),
        LastFmAuthorizationError::OwnerStopped
    );
    assert_eq!(closed.control.exchange_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        closed.shutdown.shutdown().await,
        Ok(LastFmAuthorizationShutdownReason::Drained)
    );
}

fn result_gate(
    kind: AuthorizationResultGateKind,
) -> (
    AuthorizationResultGate,
    async_channel::Receiver<()>,
    async_channel::Sender<()>,
) {
    let (reached, observations) = async_channel::bounded(1);
    let (release, released) = async_channel::bounded(1);
    (
        AuthorizationResultGate {
            kind,
            reached,
            release: released,
        },
        observations,
        release,
    )
}

fn gated_harness(
    now: Duration,
    kind: AuthorizationResultGateKind,
) -> (
    Harness,
    async_channel::Receiver<()>,
    async_channel::Sender<()>,
) {
    let (gate, reached, release) = result_gate(kind);
    (
        harness_with_options(
            now,
            AuthorizationSpawnOptions {
                generation_ceiling: u64::MAX,
                result_gate: Some(gate),
            },
        ),
        reached,
        release,
    )
}

async fn wait_for_gate(reached: &async_channel::Receiver<()>) {
    tokio::time::timeout(TEST_DEADLINE, reached.recv())
        .await
        .expect("transition gate deadline")
        .expect("transition gate remains live");
}

fn enqueue_stale_finish(
    handle: &LastFmAuthorizationHandle,
) -> oneshot::Receiver<Result<LastFmAuthorizationGrant, LastFmAuthorizationError>> {
    let (completion, result) = oneshot::channel();
    let (admitted, admission) = oneshot::channel();
    let _ = admitted.send(());
    assert!(handle
        .inner
        .commands
        .try_send(Command::Finish {
            generation: 0,
            flow: LastFmAuthorizationFlow::fresh(),
            completion,
            admission,
        })
        .is_ok());
    result
}

#[tokio::test]
async fn request_child_observation_anchors_lifetime_across_owner_delay() {
    let (before, reached, release) = gated_harness(
        Duration::from_secs(100),
        AuthorizationResultGateKind::Request,
    );
    let start = before.handle.try_begin().unwrap();
    expect_event(&before.control, TransportEvent::RequestStarted(1)).await;
    send_request(&before.control, Ok(token())).await;
    expect_event(&before.control, TransportEvent::RequestDropped(1)).await;
    wait_for_gate(&reached).await;
    before.clock.advance_to(Duration::from_secs(3_699));
    release.send(()).await.unwrap();
    let challenge = start.wait().await.expect("one second remains");
    let exchange = before.handle.try_finish(&challenge).unwrap();
    expect_event(&before.control, TransportEvent::ExchangeStarted(1)).await;
    send_exchange(&before.control, Ok(session())).await;
    expect_event(&before.control, TransportEvent::ExchangeDropped(1)).await;
    assert!(exchange.wait().await.is_ok());
    shutdown(before).await;

    let (equality, reached, release) = gated_harness(
        Duration::from_secs(100),
        AuthorizationResultGateKind::Request,
    );
    let mut status = equality.handle.subscribe_status();
    let start = equality.handle.try_begin().unwrap();
    expect_event(&equality.control, TransportEvent::RequestStarted(1)).await;
    send_request(&equality.control, Ok(token())).await;
    expect_event(&equality.control, TransportEvent::RequestDropped(1)).await;
    wait_for_gate(&reached).await;
    equality.clock.advance_to(Duration::from_secs(3_700));
    release.send(()).await.unwrap();
    assert_eq!(
        start.wait().await.unwrap_err(),
        LastFmAuthorizationError::Expired
    );
    let expired = wait_for_phase(&mut status, LastFmAuthorizationPhase::Expired).await;
    assert_eq!(expired.failure, Some(LastFmAuthorizationError::Expired));
    assert_eq!(equality.control.url_count.load(Ordering::SeqCst), 0);
    shutdown(equality).await;
}

#[tokio::test]
async fn url_construction_crossing_deadline_cannot_issue_an_expired_challenge() {
    let harness = harness(Duration::from_secs(100));
    let mut status = harness.handle.subscribe_status();
    let clock = Arc::clone(&harness.clock);
    harness.control.set_url_hook(move || {
        clock.advance_to(Duration::from_secs(3_700));
    });
    let start = harness.handle.try_begin().unwrap();
    expect_event(&harness.control, TransportEvent::RequestStarted(1)).await;
    let requesting = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(1)).await;
    assert_eq!(
        start.wait().await.unwrap_err(),
        LastFmAuthorizationError::Expired
    );
    let expired = wait_for_phase(&mut status, LastFmAuthorizationPhase::Expired).await;
    assert_eq!(expired.revision, requesting.revision + 1);
    assert_eq!(expired.failure, Some(LastFmAuthorizationError::Expired));
    assert_eq!(harness.control.url_count.load(Ordering::SeqCst), 1);
    assert_eq!(harness.control.exchange_count.load(Ordering::SeqCst), 0);
    shutdown(harness).await;
}

#[tokio::test]
async fn successor_winning_ready_request_success_suppresses_stale_challenge_and_status() {
    let (harness, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Request);
    let mut status = harness.handle.subscribe_status();
    let first = harness.handle.try_begin().unwrap();
    expect_event(&harness.control, TransportEvent::RequestStarted(1)).await;
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(1)).await;
    wait_for_gate(&reached).await;
    let predecessor = *status.borrow_and_update();
    assert_eq!(predecessor.phase, LastFmAuthorizationPhase::Requesting);

    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    release.send(()).await.unwrap();
    assert_eq!(
        first.wait().await.unwrap_err(),
        LastFmAuthorizationError::Superseded
    );
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    let successor = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    assert_eq!(successor.revision, predecessor.revision + 1);
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let challenge = second.wait().await.unwrap();
    assert_eq!(
        harness
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_url_revoked(&challenge);
    shutdown(harness).await;
}

#[tokio::test]
async fn successor_winning_ready_expiry_suppresses_stale_terminal_status() {
    let (harness, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Expiry);
    let mut status = harness.handle.subscribe_status();
    let (_first_flow, first_challenge) = ready_challenge(&harness).await;
    let predecessor = *status.borrow_and_update();
    assert_eq!(
        predecessor.phase,
        LastFmAuthorizationPhase::AwaitingApproval
    );
    harness.clock.advance_to(DESKTOP_TOKEN_LIFETIME);
    wait_for_gate(&reached).await;
    assert_eq!(
        authorization_url(&first_challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::FinishUnavailable
    );
    let expired = wait_for_phase(&mut status, LastFmAuthorizationPhase::Expired).await;
    assert_eq!(expired.revision, predecessor.revision + 1);
    assert_eq!(expired.failure, Some(LastFmAuthorizationError::Expired));
    assert_eq!(
        harness.handle.try_finish(&first_challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );

    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    assert_url_revoked(&first_challenge);
    release.send(()).await.unwrap();
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    let successor = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    assert_eq!(successor.revision, expired.revision + 1);
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let second_challenge = second.wait().await.unwrap();
    assert_eq!(
        harness
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_url_revoked(&second_challenge);
    shutdown(harness).await;
}

#[tokio::test]
async fn successor_winning_ready_exchange_success_cannot_receive_grant_or_publish_it() {
    let (harness, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Exchange);
    let mut status = harness.handle.subscribe_status();
    let (_flow, challenge) = ready_challenge(&harness).await;
    let exchange = harness.handle.try_finish(&challenge).unwrap();
    expect_event(&harness.control, TransportEvent::ExchangeStarted(1)).await;
    send_exchange(&harness.control, Ok(session())).await;
    expect_event(&harness.control, TransportEvent::ExchangeDropped(1)).await;
    wait_for_gate(&reached).await;
    let predecessor = *status.borrow_and_update();
    assert_eq!(predecessor.phase, LastFmAuthorizationPhase::Exchanging);

    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    release.send(()).await.unwrap();
    assert_eq!(
        exchange.wait().await.unwrap_err(),
        LastFmAuthorizationError::Superseded
    );
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    let successor = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    assert_eq!(successor.revision, predecessor.revision + 1);
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let second_challenge = second.wait().await.unwrap();
    assert_eq!(
        harness
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_url_revoked(&second_challenge);
    shutdown(harness).await;
}

#[tokio::test]
async fn command_transitions_publish_only_for_the_exact_latest_flow() {
    let (beginning, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Begin);
    let mut status = beginning.handle.subscribe_status();
    let first = beginning.handle.try_begin().unwrap();
    wait_for_gate(&reached).await;
    let second = beginning.handle.try_begin().unwrap();
    let second_flow = second.flow();
    release.send(()).await.unwrap();
    assert_eq!(
        first.wait().await.unwrap_err(),
        LastFmAuthorizationError::Superseded
    );
    expect_event(&beginning.control, TransportEvent::RequestStarted(1)).await;
    let requesting = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    assert_eq!(requesting.revision, 1);
    send_request(&beginning.control, Ok(token())).await;
    expect_event(&beginning.control, TransportEvent::RequestDropped(1)).await;
    let challenge = second.wait().await.unwrap();
    assert_eq!(
        beginning
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_url_revoked(&challenge);
    shutdown(beginning).await;

    let (finishing, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Finish);
    let mut status = finishing.handle.subscribe_status();
    let (_first_flow, first_challenge) = ready_challenge(&finishing).await;
    let predecessor = *status.borrow_and_update();
    let finish = finishing.handle.try_finish(&first_challenge).unwrap();
    assert_url_revoked(&first_challenge);
    wait_for_gate(&reached).await;
    let second = finishing.handle.try_begin().unwrap();
    let second_flow = second.flow();
    release.send(()).await.unwrap();
    assert_eq!(
        finish.wait().await.unwrap_err(),
        LastFmAuthorizationError::Superseded
    );
    assert_eq!(finishing.control.exchange_count.load(Ordering::SeqCst), 0);
    expect_event(&finishing.control, TransportEvent::RequestStarted(2)).await;
    let successor = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    assert_eq!(successor.revision, predecessor.revision + 1);
    send_request(&finishing.control, Ok(token())).await;
    expect_event(&finishing.control, TransportEvent::RequestDropped(2)).await;
    let challenge = second.wait().await.unwrap();
    assert_eq!(
        finishing
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_url_revoked(&challenge);
    shutdown(finishing).await;
}

#[tokio::test]
async fn full_bounded_fifo_preserves_finish_authority_and_cannot_block_shutdown() {
    let (harness, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Finish);
    let (_flow, challenge) = ready_challenge(&harness).await;
    let stale_flow = LastFmAuthorizationFlow::fresh();

    let (gate_completion, _gate_result) = oneshot::channel();
    let (gate_admitted, gate_admission) = oneshot::channel();
    let _ = gate_admitted.send(());
    assert!(harness
        .handle
        .inner
        .commands
        .try_send(Command::Finish {
            generation: 0,
            flow: stale_flow.clone(),
            completion: gate_completion,
            admission: gate_admission,
        })
        .is_ok());
    wait_for_gate(&reached).await;

    let mut queued_results = Vec::with_capacity(AUTHORIZATION_COMMAND_CAPACITY);
    for _ in 0..AUTHORIZATION_COMMAND_CAPACITY {
        let (completion, result) = oneshot::channel();
        let (admitted, admission) = oneshot::channel();
        let _ = admitted.send(());
        assert!(harness
            .handle
            .inner
            .commands
            .try_send(Command::Cancel {
                generation: 0,
                flow: stale_flow.clone(),
                completion,
                admission,
            })
            .is_ok());
        queued_results.push(result);
    }

    assert_eq!(
        harness.handle.try_begin().unwrap_err(),
        LastFmAuthorizationAdmissionError::Busy
    );
    assert_eq!(
        harness.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::Busy
    );
    assert_eq!(authorization_url(&challenge).unwrap(), AUTHORIZATION_URL);

    assert!(harness.handle.close_and_flush());
    assert_eq!(
        authorization_url(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::Closed
    );
    release.send(()).await.unwrap();
    assert_eq!(
        harness.shutdown.shutdown().await,
        Ok(LastFmAuthorizationShutdownReason::Drained)
    );
    drop(queued_results);
}

#[tokio::test]
async fn admitted_cancel_completes_after_successor_wins_without_clobbering_status() {
    let (harness, reached, release) =
        gated_harness(Duration::ZERO, AuthorizationResultGateKind::Cancel);
    let mut status = harness.handle.subscribe_status();
    let (_first_flow, first_challenge) = ready_challenge(&harness).await;
    let predecessor = *status.borrow_and_update();
    let cancel = harness.handle.try_cancel(&first_challenge.flow()).unwrap();
    assert_url_revoked(&first_challenge);
    wait_for_gate(&reached).await;

    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    release.send(()).await.unwrap();
    assert_eq!(cancel.wait().await, Ok(()));
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    let successor = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    assert_eq!(successor.revision, predecessor.revision + 1);
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let second_challenge = second.wait().await.unwrap();
    assert_eq!(
        harness
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_url_revoked(&second_challenge);
    shutdown(harness).await;
}

#[tokio::test]
async fn dropped_grant_receiver_returns_owner_to_idle_without_issuing_grant_status() {
    let harness = harness(Duration::ZERO);
    let mut status = harness.handle.subscribe_status();
    let (_flow, challenge) = ready_challenge(&harness).await;
    let exchange = harness.handle.try_finish(&challenge).unwrap();
    expect_event(&harness.control, TransportEvent::ExchangeStarted(1)).await;
    let exchanging = wait_for_phase(&mut status, LastFmAuthorizationPhase::Exchanging).await;
    drop(exchange);
    send_exchange(&harness.control, Ok(session())).await;
    expect_event(&harness.control, TransportEvent::ExchangeDropped(1)).await;
    let idle = wait_for_phase(&mut status, LastFmAuthorizationPhase::Idle).await;
    assert_eq!(idle.revision, exchanging.revision + 1);
    assert_eq!(idle.failure, None);
    assert_url_revoked(&challenge);
    shutdown(harness).await;
}

#[tokio::test]
async fn dropped_start_receiver_clears_new_url_authority_and_returns_idle() {
    let harness = harness(Duration::ZERO);
    let mut status = harness.handle.subscribe_status();
    let start = harness.handle.try_begin().unwrap();
    let flow = start.flow();
    expect_event(&harness.control, TransportEvent::RequestStarted(1)).await;
    let requesting = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    drop(start);
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(1)).await;
    let idle = wait_for_phase(&mut status, LastFmAuthorizationPhase::Idle).await;
    assert_eq!(idle.revision, requesting.revision + 1);
    assert_eq!(idle.failure, None);
    assert_eq!(harness.control.url_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.handle.try_cancel(&flow).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    shutdown(harness).await;
}

#[tokio::test]
async fn terminal_exchange_failure_keeps_every_challenge_clone_revoked() {
    let harness = harness(Duration::ZERO);
    let mut status = harness.handle.subscribe_status();
    let (_flow, challenge) = ready_challenge(&harness).await;
    let retained = challenge.clone();
    let exchange = harness.handle.try_finish(&challenge).unwrap();
    assert_url_revoked(&challenge);
    assert_url_revoked(&retained);
    expect_event(&harness.control, TransportEvent::ExchangeStarted(1)).await;
    send_exchange(&harness.control, Err(LastFmClientError::Transport)).await;
    expect_event(&harness.control, TransportEvent::ExchangeDropped(1)).await;
    assert_eq!(
        exchange.wait().await.unwrap_err(),
        LastFmAuthorizationError::TemporarilyUnavailable
    );
    let failed = wait_for_phase(&mut status, LastFmAuthorizationPhase::Failed).await;
    assert_eq!(
        failed.failure,
        Some(LastFmAuthorizationError::TemporarilyUnavailable)
    );
    assert_url_revoked(&challenge);
    assert_url_revoked(&retained);
    shutdown(harness).await;
}

#[tokio::test]
async fn panicking_url_inspection_poison_fails_the_entire_ingress_closed() {
    let harness = harness(Duration::ZERO);
    let mut status = harness.handle.subscribe_status();
    let barrier = harness.shutdown.barrier();
    let (_flow, challenge) = ready_challenge(&harness).await;
    let inspection = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = challenge.with_authorization_url::<()>(|_| panic!("inspection panic"));
    }));
    assert!(inspection.is_err());
    assert_eq!(
        challenge.with_authorization_url(str::to_owned).unwrap_err(),
        LastFmAuthorizationAdmissionError::Closed
    );
    assert_eq!(
        harness.handle.try_begin().unwrap_err(),
        LastFmAuthorizationAdmissionError::Closed
    );
    assert_eq!(
        harness.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::Closed
    );
    assert_url_revoked(&challenge);
    let stopped = wait_for_phase(&mut status, LastFmAuthorizationPhase::Stopped).await;
    assert_eq!(
        stopped.failure,
        Some(LastFmAuthorizationError::OwnerStopped)
    );
    assert_eq!(barrier.wait().await, Err(LastFmAuthorizationShutdownError));
    assert_eq!(
        harness.shutdown.shutdown().await,
        Err(LastFmAuthorizationShutdownError)
    );
}

#[tokio::test]
async fn retained_challenge_cannot_keep_handle_or_url_authority_alive_after_shutdown() {
    let harness = harness(Duration::ZERO);
    let (_flow, challenge) = ready_challenge(&harness).await;
    assert!(challenge.0.handle.upgrade().is_some());
    shutdown(harness).await;
    assert!(challenge.0.handle.upgrade().is_none());
    assert_eq!(
        challenge.with_authorization_url(str::to_owned).unwrap_err(),
        LastFmAuthorizationAdmissionError::Closed
    );
}

#[tokio::test]
async fn ready_stale_request_error_cannot_publish_failure_into_successor() {
    let (gate, reached, release) = result_gate(AuthorizationResultGateKind::Request);
    let harness = harness_with_options(
        Duration::ZERO,
        AuthorizationSpawnOptions {
            generation_ceiling: u64::MAX,
            result_gate: Some(gate),
        },
    );
    let mut status = harness.handle.subscribe_status();
    let first = harness.handle.try_begin().unwrap();
    expect_event(&harness.control, TransportEvent::RequestStarted(1)).await;
    send_request(&harness.control, Err(LastFmClientError::Transport)).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(1)).await;
    wait_for_gate(&reached).await;
    let predecessor = *status.borrow_and_update();
    assert_eq!(predecessor.phase, LastFmAuthorizationPhase::Requesting);
    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    release.send(()).await.unwrap();
    assert_eq!(
        first.wait().await.unwrap_err(),
        LastFmAuthorizationError::Superseded
    );
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    let observed = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    assert_eq!(observed.revision, predecessor.revision + 1);
    assert_eq!(observed.failure, None);
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let challenge = second.wait().await.unwrap();
    assert_eq!(
        harness
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    assert_eq!(
        harness.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::StaleFlow
    );
    shutdown(harness).await;
}

#[tokio::test]
async fn ready_stale_exchange_error_cannot_publish_failure_into_successor() {
    let (gate, reached, release) = result_gate(AuthorizationResultGateKind::Exchange);
    let harness = harness_with_options(
        Duration::ZERO,
        AuthorizationSpawnOptions {
            generation_ceiling: u64::MAX,
            result_gate: Some(gate),
        },
    );
    let mut status = harness.handle.subscribe_status();
    let (_flow, challenge) = ready_challenge(&harness).await;
    let exchange = harness.handle.try_finish(&challenge).unwrap();
    expect_event(&harness.control, TransportEvent::ExchangeStarted(1)).await;
    send_exchange(&harness.control, Err(LastFmClientError::Transport)).await;
    expect_event(&harness.control, TransportEvent::ExchangeDropped(1)).await;
    wait_for_gate(&reached).await;
    let predecessor = *status.borrow_and_update();
    assert_eq!(predecessor.phase, LastFmAuthorizationPhase::Exchanging);
    let second = harness.handle.try_begin().unwrap();
    let second_flow = second.flow();
    release.send(()).await.unwrap();
    assert_eq!(
        exchange.wait().await.unwrap_err(),
        LastFmAuthorizationError::Superseded
    );
    expect_event(&harness.control, TransportEvent::RequestStarted(2)).await;
    let observed = wait_for_phase(&mut status, LastFmAuthorizationPhase::Requesting).await;
    assert_eq!(observed.revision, predecessor.revision + 1);
    assert_eq!(observed.failure, None);
    send_request(&harness.control, Ok(token())).await;
    expect_event(&harness.control, TransportEvent::RequestDropped(2)).await;
    let _second_challenge = second.wait().await.unwrap();
    assert_eq!(
        harness
            .handle
            .try_cancel(&second_flow)
            .unwrap()
            .wait()
            .await,
        Ok(())
    );
    shutdown(harness).await;
}

#[test]
fn every_client_error_maps_to_one_fixed_authorization_category() {
    for (client, expected) in [
        (
            LastFmClientError::AppCredentialsUnavailable,
            LastFmAuthorizationError::CapabilityUnavailable,
        ),
        (
            LastFmClientError::ClientConstruction,
            LastFmAuthorizationError::CapabilityUnavailable,
        ),
        (
            LastFmClientError::InvalidInput,
            LastFmAuthorizationError::CapabilityUnavailable,
        ),
        (
            LastFmClientError::Timeout,
            LastFmAuthorizationError::TemporarilyUnavailable,
        ),
        (
            LastFmClientError::Transport,
            LastFmAuthorizationError::TemporarilyUnavailable,
        ),
        (
            LastFmClientError::ServiceUnavailable,
            LastFmAuthorizationError::TemporarilyUnavailable,
        ),
        (
            LastFmClientError::RateLimited,
            LastFmAuthorizationError::TemporarilyUnavailable,
        ),
        (
            LastFmClientError::ServiceRejected { code: 14 },
            LastFmAuthorizationError::Rejected,
        ),
        (
            LastFmClientError::ReauthenticationRequired,
            LastFmAuthorizationError::Incompatible,
        ),
        (
            LastFmClientError::HttpStatus,
            LastFmAuthorizationError::Incompatible,
        ),
        (
            LastFmClientError::BodyLimit,
            LastFmAuthorizationError::Incompatible,
        ),
        (
            LastFmClientError::InvalidResponse,
            LastFmAuthorizationError::Incompatible,
        ),
    ] {
        assert_eq!(map_client_error(client), expected);
    }
}

#[tokio::test]
async fn normal_shutdown_joins_each_network_phase_and_completes_barrier() {
    let requesting = harness(Duration::ZERO);
    let start = requesting.handle.try_begin().unwrap();
    expect_event(&requesting.control, TransportEvent::RequestStarted(1)).await;
    let barrier = requesting.shutdown.barrier();
    requesting.handle.close_and_flush();
    expect_event(&requesting.control, TransportEvent::RequestDropped(1)).await;
    assert_eq!(
        start.wait().await.unwrap_err(),
        LastFmAuthorizationError::OwnerStopped
    );
    assert_eq!(
        requesting.shutdown.shutdown().await,
        Ok(LastFmAuthorizationShutdownReason::Drained)
    );
    assert_eq!(barrier.wait().await, Ok(()));

    let awaiting = harness(Duration::ZERO);
    let (_flow, challenge) = ready_challenge(&awaiting).await;
    let retained = challenge.clone();
    awaiting.handle.close_and_flush();
    assert_url_revoked(&challenge);
    assert_url_revoked(&retained);
    assert_eq!(
        awaiting.shutdown.shutdown().await,
        Ok(LastFmAuthorizationShutdownReason::Drained)
    );
    assert_eq!(
        awaiting.handle.try_finish(&challenge).unwrap_err(),
        LastFmAuthorizationAdmissionError::Closed
    );

    let exchanging = harness(Duration::ZERO);
    let (_flow, challenge) = ready_challenge(&exchanging).await;
    let exchange = exchanging.handle.try_finish(&challenge).unwrap();
    expect_event(&exchanging.control, TransportEvent::ExchangeStarted(1)).await;
    exchanging.handle.close_and_flush();
    expect_event(&exchanging.control, TransportEvent::ExchangeDropped(1)).await;
    assert_eq!(
        exchange.wait().await.unwrap_err(),
        LastFmAuthorizationError::OwnerStopped
    );
    assert_eq!(
        exchanging.shutdown.shutdown().await,
        Ok(LastFmAuthorizationShutdownReason::Drained)
    );
}

#[tokio::test]
async fn hard_owner_abort_fails_barrier_and_drops_secret_bearing_child() {
    let harness = harness(Duration::ZERO);
    let mut status = harness.handle.subscribe_status();
    let start = harness.handle.try_begin().unwrap();
    expect_event(&harness.control, TransportEvent::RequestStarted(1)).await;
    let barrier = harness.shutdown.barrier();
    harness.shutdown.abort_owner_for_test();
    assert_eq!(
        tokio::time::timeout(TEST_DEADLINE, barrier.wait())
            .await
            .expect("failed barrier deadline"),
        Err(LastFmAuthorizationShutdownError)
    );
    expect_event(&harness.control, TransportEvent::RequestDropped(1)).await;
    let stopped = wait_for_phase(&mut status, LastFmAuthorizationPhase::Stopped).await;
    assert_eq!(
        stopped.failure,
        Some(LastFmAuthorizationError::OwnerStopped)
    );
    assert_eq!(
        start.wait().await.unwrap_err(),
        LastFmAuthorizationError::OwnerStopped
    );
    assert_eq!(
        harness.handle.try_begin().unwrap_err(),
        LastFmAuthorizationAdmissionError::Closed
    );
    assert_eq!(
        harness.shutdown.shutdown().await,
        Err(LastFmAuthorizationShutdownError)
    );
}

#[tokio::test]
async fn generation_exhaustion_closes_ingress_and_drains_predecessor() {
    let harness = harness_with_options(
        Duration::ZERO,
        AuthorizationSpawnOptions {
            generation_ceiling: 1,
            result_gate: None,
        },
    );
    let start = harness.handle.try_begin().unwrap();
    expect_event(&harness.control, TransportEvent::RequestStarted(1)).await;
    assert_eq!(
        harness.handle.try_begin().unwrap_err(),
        LastFmAuthorizationAdmissionError::Closed
    );
    expect_event(&harness.control, TransportEvent::RequestDropped(1)).await;
    assert_eq!(
        start.wait().await.unwrap_err(),
        LastFmAuthorizationError::OwnerStopped
    );
    assert_eq!(
        harness.shutdown.shutdown().await,
        Ok(LastFmAuthorizationShutdownReason::Drained)
    );
}
