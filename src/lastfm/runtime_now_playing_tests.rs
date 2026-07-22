use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::db::migration::Migrator;
use crate::lastfm::client::{
    IgnoredReason, LastFmClientError, LastFmTrack, Scrobble, ScrobbleBatchResult, SubmissionResult,
};
use crate::lastfm::credentials::{CredentialError, ProtectedString};
use crate::lastfm::delivery::LastFmDeliveryPrimitiveError;

const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";
const WATCHDOG: Duration = Duration::from_secs(3);

#[derive(Debug, Eq, PartialEq)]
struct NowPlayingCall {
    title: String,
    expected_session: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleEvent {
    RequestRetired,
    VaultLeaseAcquired,
}

struct ActiveRequestGuard {
    active: Arc<AtomicUsize>,
    retired: async_channel::Sender<()>,
    lifecycle: async_channel::Sender<LifecycleEvent>,
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        let _ = self.retired.try_send(());
        let _ = self.lifecycle.try_send(LifecycleEvent::RequestRetired);
    }
}

struct ScriptedTransport {
    expected_session: StoredSession,
    calls: async_channel::Sender<NowPlayingCall>,
    responses: async_channel::Receiver<Result<SubmissionResult, LastFmClientError>>,
    retired: async_channel::Sender<()>,
    lifecycle: async_channel::Sender<LifecycleEvent>,
    active: Arc<AtomicUsize>,
    maximum_active: AtomicUsize,
}

type ScriptedTransportFixture = (
    Arc<ScriptedTransport>,
    async_channel::Receiver<NowPlayingCall>,
    async_channel::Sender<Result<SubmissionResult, LastFmClientError>>,
    async_channel::Receiver<()>,
    async_channel::Receiver<LifecycleEvent>,
    Arc<AtomicUsize>,
);

impl ScriptedTransport {
    fn new(expected_session: StoredSession) -> ScriptedTransportFixture {
        let (calls, call_events) = async_channel::unbounded();
        let (response_events, responses) = async_channel::unbounded();
        let (retired, retirement_events) = async_channel::unbounded();
        let (lifecycle, lifecycle_events) = async_channel::unbounded();
        let active = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                expected_session,
                calls,
                responses,
                retired,
                lifecycle,
                active: Arc::clone(&active),
                maximum_active: AtomicUsize::new(0),
            }),
            call_events,
            response_events,
            retirement_events,
            lifecycle_events,
            active,
        )
    }
}

#[async_trait::async_trait]
impl LastFmTransport for ScriptedTransport {
    async fn update_now_playing(
        &self,
        session: &StoredSession,
        track: &LastFmTrack,
    ) -> Result<SubmissionResult, LastFmClientError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum_active.fetch_max(active, Ordering::SeqCst);
        let _guard = ActiveRequestGuard {
            active: Arc::clone(&self.active),
            retired: self.retired.clone(),
            lifecycle: self.lifecycle.clone(),
        };
        let _ = self
            .calls
            .send(NowPlayingCall {
                title: track.title.clone(),
                expected_session: session == &self.expected_session,
            })
            .await;
        self.responses
            .recv()
            .await
            .unwrap_or(Err(LastFmClientError::Transport))
    }

    async fn submit_scrobbles(
        &self,
        _session: &StoredSession,
        _scrobbles: &[Scrobble],
    ) -> Result<ScrobbleBatchResult, LastFmClientError> {
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

struct TestCredentialStore {
    session: Mutex<Option<StoredSession>>,
    active: Arc<AtomicUsize>,
    active_at_delete: Mutex<Vec<usize>>,
}

impl TestCredentialStore {
    fn new(session: StoredSession, active: Arc<AtomicUsize>) -> Self {
        Self {
            session: Mutex::new(Some(session)),
            active,
            active_at_delete: Mutex::new(Vec::new()),
        }
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
            .push(self.active.load(Ordering::SeqCst));
        *self
            .session
            .lock()
            .map_err(|_| CredentialError::Unavailable)? = None;
        Ok(())
    }
}

struct Fixture {
    database: DatabaseConnection,
    binding: LastFmAccountBinding,
    handle: LastFmRuntimeHandle,
    shutdown: LastFmRuntimeShutdown,
    calls: async_channel::Receiver<NowPlayingCall>,
    responses: async_channel::Sender<Result<SubmissionResult, LastFmClientError>>,
    retired: async_channel::Receiver<()>,
    lifecycle: async_channel::Receiver<LifecycleEvent>,
    active: Arc<AtomicUsize>,
    transport: Arc<ScriptedTransport>,
    store: Arc<TestCredentialStore>,
}

async fn fixture() -> Fixture {
    let database = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&database, None).await.unwrap();
    let session = StoredSession::new("listener", ProtectedString::new(SESSION_KEY)).unwrap();
    let binding = session.account_binding();
    let (transport, calls, responses, retired, lifecycle, active) =
        ScriptedTransport::new(session.clone());
    let store = Arc::new(TestCredentialStore::new(session, Arc::clone(&active)));
    let (handle, shutdown) = spawn_lastfm_runtime(
        LastFmRuntimeActivation::issue_after_consent_and_enablement(),
        database.clone(),
        store.clone(),
        transport.clone(),
        Arc::new(FixedClock),
    )
    .await
    .unwrap();
    Fixture {
        database,
        binding,
        handle,
        shutdown,
        calls,
        responses,
        retired,
        lifecycle,
        active,
        transport,
        store,
    }
}

fn track(title: impl Into<String>) -> LastFmTrack {
    LastFmTrack {
        artist: "Private Artist".to_owned(),
        title: title.into(),
        album: Some("Private Album".to_owned()),
        album_artist: Some("Private Album Artist".to_owned()),
        track_number: Some(7),
        duration_seconds: 240,
    }
}

fn now_playing(title: impl Into<String>) -> LastFmNowPlaying {
    LastFmNowPlaying::try_new(track(title)).unwrap()
}

async fn receive<T>(receiver: &async_channel::Receiver<T>) -> T {
    tokio::time::timeout(WATCHDOG, receiver.recv())
        .await
        .expect("fixture event before watchdog")
        .expect("fixture channel remains open")
}

async fn wait<T>(operation: LastFmRuntimeOperation<T>) -> Result<T, LastFmRuntimeCommandError> {
    tokio::time::timeout(WATCHDOG, operation.wait())
        .await
        .expect("runtime operation before watchdog")
}

async fn wait_for_phase(
    status: &mut watch::Receiver<LastFmRuntimeStatus>,
    phase: LastFmRuntimePhase,
) {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if status.borrow_and_update().phase == phase {
                return;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .expect("runtime phase before watchdog");
}

fn install_result_gate(
    handle: &LastFmRuntimeHandle,
    after_claim: bool,
) -> (async_channel::Receiver<()>, async_channel::Sender<()>) {
    let (reached, reached_events) = async_channel::bounded(1);
    let (release, release_events) = async_channel::bounded(1);
    let gate = RecoveryClearGate {
        reached,
        release: release_events,
    };
    let mut ingress = handle.inner.ingress.lock().unwrap();
    if after_claim {
        ingress.now_playing_reauthorization_commit_gate = Some(gate);
    } else {
        ingress.now_playing_result_gate = Some(gate);
    }
    (reached_events, release)
}

#[test]
fn validated_payload_and_all_public_debug_are_content_free() {
    let private = "unique-private-now-playing-sentinel";
    let payload = now_playing(private);
    let rendered = format!("{payload:?}");
    assert_eq!(rendered, "LastFmNowPlaying([REDACTED])");
    assert!(!rendered.contains(private));

    for invalid in [
        LastFmTrack {
            artist: " ".to_owned(),
            ..track("valid")
        },
        LastFmTrack {
            title: "line\nbreak".to_owned(),
            ..track("valid")
        },
        LastFmTrack {
            album: Some("x".repeat(MAX_NOW_PLAYING_METADATA_BYTES + 1)),
            ..track("valid")
        },
        LastFmTrack {
            album: Some(" ".repeat(MAX_NOW_PLAYING_METADATA_BYTES + 1)),
            ..track("valid")
        },
        LastFmTrack {
            album_artist: Some("line\nbreak".to_owned()),
            ..track("valid")
        },
        LastFmTrack {
            track_number: Some(0),
            ..track("valid")
        },
        LastFmTrack {
            duration_seconds: 30,
            ..track("valid")
        },
    ] {
        assert_eq!(
            LastFmNowPlaying::try_from(invalid).unwrap_err(),
            LastFmNowPlayingInputError
        );
    }

    let canonical = LastFmNowPlaying::try_new(LastFmTrack {
        album: Some(" ".repeat(MAX_NOW_PLAYING_METADATA_BYTES)),
        album_artist: Some("   ".to_owned()),
        ..track("valid")
    })
    .unwrap();
    assert_eq!(canonical.0.album, None);
    assert_eq!(canonical.0.album_artist, None);
    assert_eq!(
        format!("{:?}", LastFmRuntimeCommandError::Superseded),
        "Superseded"
    );
}

#[tokio::test]
async fn newer_occurrence_and_explicit_clear_cancel_join_before_successor() {
    let fixture = fixture().await;
    let initial_status = *fixture.handle.subscribe_status().borrow();

    let first = fixture
        .handle
        .try_update_now_playing(now_playing("First"))
        .unwrap();
    assert_eq!(
        receive(&fixture.calls).await,
        NowPlayingCall {
            title: "First".to_owned(),
            expected_session: true,
        }
    );
    let second = fixture
        .handle
        .try_update_now_playing(now_playing("Second"))
        .unwrap();
    receive(&fixture.retired).await;
    assert_eq!(
        wait(first).await,
        Err(LastFmRuntimeCommandError::Superseded)
    );
    assert_eq!(
        receive(&fixture.calls).await,
        NowPlayingCall {
            title: "Second".to_owned(),
            expected_session: true,
        }
    );
    assert_eq!(fixture.transport.maximum_active.load(Ordering::SeqCst), 1);

    let clear = fixture.handle.try_clear_now_playing().unwrap();
    receive(&fixture.retired).await;
    assert_eq!(
        wait(second).await,
        Err(LastFmRuntimeCommandError::Superseded)
    );
    assert_eq!(wait(clear).await, Ok(()));
    assert_eq!(*fixture.handle.subscribe_status().borrow(), initial_status);
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
}

#[tokio::test]
async fn ready_now_playing_result_precedes_an_already_queued_metadata_command() {
    let database = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&database, None).await.unwrap();
    let session = StoredSession::new("listener", ProtectedString::new(SESSION_KEY)).unwrap();
    let binding = session.account_binding();
    let epoch = LastFmAccountEpoch::INITIAL;
    let generation = LastFmNowPlayingGeneration(1);
    let (command_sender, commands) = async_channel::bounded(COMMAND_CAPACITY);
    let ingress = Arc::new(Mutex::new(IngressGate {
        phase: IngressPhase::Active {
            account_binding: binding,
            account_epoch: epoch,
        },
        playback_ingress_claimed: false,
        queue_admission_open: true,
        queued_metadata: 1,
        delivery_event_queued: false,
        reauthorization_queued: false,
        now_playing_clear_queued: false,
        now_playing_clear_generation: None,
        now_playing_generation: LastFmNowPlayingGeneration(2),
        now_playing_reauthorization_commit: false,
        delivery_cancellation: None,
        now_playing_cancellation: None,
        shutdown_queued: false,
        recovery_clear_gate: None,
        now_playing_result_gate: None,
        now_playing_reauthorization_commit_gate: None,
    }));
    let initial_status = LastFmRuntimeStatus::active(0);
    let (status_sender, _status) = watch::channel(initial_status);
    let active = Arc::new(AtomicUsize::new(0));
    let credentials: Arc<dyn SessionCredentialStore> = Arc::new(TestCredentialStore::new(
        session.clone(),
        Arc::clone(&active),
    ));
    let (transport, _, _, _, _, _) = ScriptedTransport::new(session);

    let (ready, ready_event) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = ready.send(());
        NowPlayingTaskExit::Completed(Ok(SubmissionResult::Accepted { corrected: false }))
    });
    ready_event.await.unwrap();
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }

    let (current_completion, _current_operation) = oneshot::channel();
    let (queued_completion, _queued_operation) = oneshot::channel();
    command_sender
        .try_send(Command::NowPlaying {
            account_binding: binding,
            account_epoch: epoch,
            generation: LastFmNowPlayingGeneration(2),
            now_playing: now_playing("Backlog"),
            completion: queued_completion,
        })
        .unwrap();
    let mut owner = RuntimeOwner {
        database,
        credentials,
        command_sender: command_sender.clone(),
        commands,
        ingress,
        status_sender,
        status: initial_status,
        now_playing: Some(NowPlayingRuntime {
            account_binding: binding,
            account_epoch: epoch,
            generation,
            cancellation: CancellationToken::new(),
            task,
            completion: Some(current_completion),
        }),
        account: None,
        transport,
        clock: Arc::new(FixedClock),
    };

    match owner.next_event().await {
        RuntimeEvent::NowPlaying {
            generation: observed,
            result:
                Ok(NowPlayingTaskExit::Completed(Ok(SubmissionResult::Accepted { corrected: false }))),
        } => assert_eq!(observed.0, generation.0),
        RuntimeEvent::NowPlaying { .. } | RuntimeEvent::Command(_) => {
            panic!("ready now-playing result must win biased arbitration")
        }
    }
    assert_eq!(
        owner.commands.len(),
        1,
        "the already queued metadata command remains behind the ready result"
    );
}

#[tokio::test]
async fn duplicate_clear_coalesces_past_an_intervening_update_without_starting_it() {
    let fixture = fixture().await;
    let playing = fixture
        .handle
        .try_update_now_playing(now_playing("Playing"))
        .unwrap();
    receive(&fixture.calls).await;

    let first_clear = fixture.handle.try_clear_now_playing().unwrap();
    let intervening = fixture
        .handle
        .try_update_now_playing(now_playing("Must not start"))
        .unwrap();
    assert_eq!(
        fixture.handle.try_clear_now_playing().unwrap_err(),
        LastFmRuntimeAdmissionError::Busy,
        "the existing reserved command coalesces the newer clear"
    );

    receive(&fixture.retired).await;
    assert_eq!(
        wait(playing).await,
        Err(LastFmRuntimeCommandError::Superseded)
    );
    assert_eq!(wait(first_clear).await, Ok(()));
    assert_eq!(
        wait(intervening).await,
        Err(LastFmRuntimeCommandError::Superseded)
    );
    assert!(
        fixture.calls.try_recv().is_err(),
        "the update between coalesced clears never reaches transport"
    );
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
}

#[tokio::test]
async fn terminal_and_transient_results_are_one_shot_and_leave_delivery_status_untouched() {
    let fixture = fixture().await;
    let initial_status = *fixture.handle.subscribe_status().borrow();
    let cases = [
        (
            Ok(SubmissionResult::Accepted { corrected: true }),
            LastFmNowPlayingOutcome::Accepted,
        ),
        (
            Ok(SubmissionResult::Ignored {
                reason: IgnoredReason::Artist,
            }),
            LastFmNowPlayingOutcome::Ignored,
        ),
        (
            Err(LastFmClientError::ServiceRejected { code: 13 }),
            LastFmNowPlayingOutcome::Rejected,
        ),
        (
            Err(LastFmClientError::Timeout),
            LastFmNowPlayingOutcome::Unavailable,
        ),
        (
            Err(LastFmClientError::Transport),
            LastFmNowPlayingOutcome::Unavailable,
        ),
        (
            Err(LastFmClientError::ServiceUnavailable),
            LastFmNowPlayingOutcome::Unavailable,
        ),
        (
            Err(LastFmClientError::RateLimited),
            LastFmNowPlayingOutcome::Unavailable,
        ),
        (
            Err(LastFmClientError::HttpStatus),
            LastFmNowPlayingOutcome::Incompatible,
        ),
        (
            Err(LastFmClientError::BodyLimit),
            LastFmNowPlayingOutcome::Incompatible,
        ),
        (
            Err(LastFmClientError::InvalidResponse),
            LastFmNowPlayingOutcome::Incompatible,
        ),
        (
            Err(LastFmClientError::AppCredentialsUnavailable),
            LastFmNowPlayingOutcome::CapabilityUnavailable,
        ),
        (
            Err(LastFmClientError::ClientConstruction),
            LastFmNowPlayingOutcome::CapabilityUnavailable,
        ),
        (
            Err(LastFmClientError::InvalidInput),
            LastFmNowPlayingOutcome::CapabilityUnavailable,
        ),
    ];
    for (index, (response, expected)) in cases.into_iter().enumerate() {
        let operation = fixture
            .handle
            .try_update_now_playing(now_playing(format!("Case {index}")))
            .unwrap();
        assert!(receive(&fixture.calls).await.expected_session);
        fixture.responses.send(response).await.unwrap();
        assert_eq!(wait(operation).await, Ok(expected));
        receive(&fixture.retired).await;
        assert_eq!(*fixture.handle.subscribe_status().borrow(), initial_status);
        assert!(
            fixture.calls.try_recv().is_err(),
            "now-playing never retries"
        );
    }
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
}

#[tokio::test]
async fn code_nine_persists_exact_reauthorization_pause_and_blocks_later_publication() {
    let fixture = fixture().await;
    let mut status = fixture.handle.subscribe_status();
    let operation = fixture
        .handle
        .try_update_now_playing(now_playing("Needs auth"))
        .unwrap();
    receive(&fixture.calls).await;
    fixture
        .responses
        .send(Err(LastFmClientError::ReauthenticationRequired))
        .await
        .unwrap();
    assert_eq!(
        wait(operation).await,
        Err(LastFmRuntimeCommandError::ReauthenticationRequired)
    );
    wait_for_phase(&mut status, LastFmRuntimePhase::ReauthenticationRequired).await;
    assert_eq!(
        storage::validate_account_queue_state(&fixture.database, fixture.binding)
            .await
            .unwrap()
            .durable_pause,
        Some(storage::LastFmDurablePause::ReauthenticationRequired)
    );
    assert_eq!(
        fixture
            .handle
            .try_update_now_playing(now_playing("Refused"))
            .unwrap_err(),
        LastFmRuntimeAdmissionError::Paused
    );
    assert!(fixture.calls.try_recv().is_err());
    assert_eq!(
        fixture.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
}

#[tokio::test]
async fn successor_admission_before_code_nine_claim_makes_old_result_fully_inert() {
    let fixture = fixture().await;
    let initial_status = *fixture.handle.subscribe_status().borrow();
    let (result_reached, release_result) = install_result_gate(&fixture.handle, false);
    let old = fixture
        .handle
        .try_update_now_playing(now_playing("Old"))
        .unwrap();
    receive(&fixture.calls).await;
    fixture
        .responses
        .send(Err(LastFmClientError::ReauthenticationRequired))
        .await
        .unwrap();
    receive(&result_reached).await;

    let successor = fixture
        .handle
        .try_update_now_playing(now_playing("Successor"))
        .unwrap();
    release_result.send(()).await.unwrap();
    assert_eq!(wait(old).await, Err(LastFmRuntimeCommandError::Superseded));
    assert_eq!(
        storage::validate_account_queue_state(&fixture.database, fixture.binding)
            .await
            .unwrap()
            .durable_pause,
        None
    );
    assert_eq!(*fixture.handle.subscribe_status().borrow(), initial_status);

    assert_eq!(receive(&fixture.calls).await.title, "Successor");
    fixture
        .responses
        .send(Ok(SubmissionResult::Accepted { corrected: false }))
        .await
        .unwrap();
    assert_eq!(wait(successor).await, Ok(LastFmNowPlayingOutcome::Accepted));
    receive(&fixture.retired).await;
    assert_eq!(
        fixture.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
}

#[tokio::test]
async fn code_nine_claim_blocks_successor_through_the_async_persist_window() {
    let fixture = fixture().await;
    let (commit_reached, release_commit) = install_result_gate(&fixture.handle, true);
    let operation = fixture
        .handle
        .try_update_now_playing(now_playing("Claiming"))
        .unwrap();
    receive(&fixture.calls).await;
    fixture
        .responses
        .send(Err(LastFmClientError::ReauthenticationRequired))
        .await
        .unwrap();
    receive(&commit_reached).await;

    assert_eq!(
        fixture
            .handle
            .try_update_now_playing(now_playing("Cannot overtake"))
            .unwrap_err(),
        LastFmRuntimeAdmissionError::Transitioning
    );
    assert_eq!(
        fixture.handle.try_clear_now_playing().unwrap_err(),
        LastFmRuntimeAdmissionError::Transitioning
    );
    assert_eq!(
        storage::validate_account_queue_state(&fixture.database, fixture.binding)
            .await
            .unwrap()
            .durable_pause,
        None,
        "claim itself performs no durable mutation"
    );

    release_commit.send(()).await.unwrap();
    assert_eq!(
        wait(operation).await,
        Err(LastFmRuntimeCommandError::ReauthenticationRequired)
    );
    assert_eq!(
        storage::validate_account_queue_state(&fixture.database, fixture.binding)
            .await
            .unwrap()
            .durable_pause,
        Some(storage::LastFmDurablePause::ReauthenticationRequired)
    );
    assert_eq!(
        fixture.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
}

#[tokio::test]
async fn disconnect_and_shutdown_cancel_join_before_releasing_vault_state() {
    let disconnect_fixture = fixture().await;
    let operation = disconnect_fixture
        .handle
        .try_update_now_playing(now_playing("Disconnect"))
        .unwrap();
    receive(&disconnect_fixture.calls).await;
    let disconnect = disconnect_fixture.handle.disconnect_and_purge().unwrap();
    receive(&disconnect_fixture.retired).await;
    assert_eq!(
        wait(operation).await,
        Err(LastFmRuntimeCommandError::OwnerStopped)
    );
    assert_eq!(wait(disconnect).await, Ok(0));
    assert_eq!(
        *disconnect_fixture.store.active_at_delete.lock().unwrap(),
        vec![0],
        "vault deletion follows request retirement"
    );
    assert_eq!(
        disconnect_fixture.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );

    let fixture = fixture().await;
    let operation = fixture
        .handle
        .try_update_now_playing(now_playing("Shutdown"))
        .unwrap();
    receive(&fixture.calls).await;
    assert!(fixture.handle.close_and_flush());
    receive(&fixture.retired).await;
    assert_eq!(
        wait(operation).await,
        Err(LastFmRuntimeCommandError::OwnerStopped)
    );
    assert_eq!(
        fixture.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn actor_panic_and_owner_loss_retire_task_without_metadata_in_errors() {
    let private = "panic-private-metadata-sentinel";
    let panic_fixture = fixture().await;
    let operation = panic_fixture
        .handle
        .try_update_now_playing(now_playing(private))
        .unwrap();
    receive(&panic_fixture.calls).await;
    assert!(panic_fixture
        .handle
        .inner
        .commands
        .try_send(Command::PanicForQuiescenceTest)
        .is_ok());
    receive(&panic_fixture.retired).await;
    let result = wait(operation).await;
    assert_eq!(result, Err(LastFmRuntimeCommandError::OwnerStopped));
    assert!(!format!("{result:?}").contains(private));
    assert!(panic_fixture.shutdown.shutdown().await.is_err());
    assert_eq!(panic_fixture.active.load(Ordering::SeqCst), 0);

    let fixture = fixture().await;
    let operation = fixture
        .handle
        .try_update_now_playing(now_playing("Owner loss"))
        .unwrap();
    receive(&fixture.calls).await;
    fixture.handle.inner.commands.close();
    receive(&fixture.retired).await;
    assert_eq!(
        wait(operation).await,
        Err(LastFmRuntimeCommandError::OwnerStopped)
    );
    assert!(fixture.shutdown.shutdown().await.is_err());
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn aborting_owner_fails_barrier_and_eventually_retires_now_playing_child() {
    let fixture = fixture().await;
    let operation = fixture
        .handle
        .try_update_now_playing(now_playing("Owner abort"))
        .unwrap();
    receive(&fixture.calls).await;
    let barrier = fixture.shutdown.barrier();
    let lifecycle = fixture.transport.lifecycle.clone();
    let vault_waiter = tokio::spawn(async move {
        let _lease = acquire_vault_lifecycle().await;
        let _ = lifecycle.send(LifecycleEvent::VaultLeaseAcquired).await;
    });
    fixture
        .shutdown
        .owner
        .as_ref()
        .expect("fixture owner exists")
        .abort();

    assert_eq!(
        receive(&fixture.lifecycle).await,
        LifecycleEvent::RequestRetired,
        "the transport future must drop before its vault-lease share"
    );
    assert_eq!(
        receive(&fixture.lifecycle).await,
        LifecycleEvent::VaultLeaseAcquired,
        "a later lifecycle owner may acquire only after request retirement"
    );
    vault_waiter.await.unwrap();
    receive(&fixture.retired).await;
    assert_eq!(
        wait(operation).await,
        Err(LastFmRuntimeCommandError::OwnerStopped)
    );
    assert!(barrier.wait().await.is_err());
    assert!(fixture.shutdown.shutdown().await.is_err());
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
}

#[test]
fn clear_uses_its_reserved_slot_and_cancels_even_when_metadata_ingress_is_full() {
    let session = StoredSession::new("listener", ProtectedString::new(SESSION_KEY)).unwrap();
    let binding = session.account_binding();
    let cancellation = CancellationToken::new();
    let ingress = Arc::new(Mutex::new(IngressGate {
        phase: IngressPhase::Active {
            account_binding: binding,
            account_epoch: LastFmAccountEpoch::INITIAL,
        },
        playback_ingress_claimed: false,
        queue_admission_open: true,
        queued_metadata: 0,
        delivery_event_queued: false,
        reauthorization_queued: false,
        now_playing_clear_queued: false,
        now_playing_clear_generation: None,
        now_playing_generation: LastFmNowPlayingGeneration::INITIAL_PREDECESSOR,
        now_playing_reauthorization_commit: false,
        delivery_cancellation: None,
        now_playing_cancellation: Some(cancellation.clone()),
        shutdown_queued: false,
        recovery_clear_gate: None,
        now_playing_result_gate: None,
        now_playing_reauthorization_commit_gate: None,
    }));
    let (commands, receiver) = async_channel::bounded(COMMAND_CAPACITY);
    let (_status_sender, status) = watch::channel(LastFmRuntimeStatus::active(0));
    let handle = LastFmRuntimeHandle {
        inner: Arc::new(HandleInner {
            commands,
            ingress: Arc::clone(&ingress),
            status,
        }),
    };
    let mut ordinary = Vec::new();
    for index in 0..METADATA_INGRESS_CAPACITY {
        ordinary.push(
            handle
                .try_update_now_playing(now_playing(format!("Backlog {index}")))
                .unwrap(),
        );
    }
    assert_eq!(
        handle
            .try_update_now_playing(now_playing("Busy"))
            .unwrap_err(),
        LastFmRuntimeAdmissionError::Busy
    );

    let clear = handle.try_clear_now_playing().unwrap();
    assert!(cancellation.is_cancelled());
    assert_eq!(receiver.len(), METADATA_INGRESS_CAPACITY + 1);
    assert!(ingress.lock().unwrap().now_playing_clear_queued);
    assert_eq!(
        handle.try_clear_now_playing().unwrap_err(),
        LastFmRuntimeAdmissionError::Busy
    );
    drop(clear);
    drop(ordinary);
}
