use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use uuid::Uuid;

use super::*;
use crate::db::migration::Migrator;
use crate::lastfm::client::{
    LastFmClientError, LastFmTrack, Scrobble, ScrobbleBatchResult, SubmissionResult,
};
use crate::lastfm::credentials::{CredentialError, ProtectedString};
use crate::lastfm::delivery::LastFmDeliveryPrimitiveError;

const ORIGINAL_KEY: &str = "0123456789abcdef0123456789abcdef";
const RENEWED_KEY: &str = "fedcba9876543210fedcba9876543210";
const WATCHDOG: Duration = Duration::from_secs(3);

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

struct ObservedTransport {
    calls: async_channel::Sender<bool>,
    responses: async_channel::Receiver<Result<ScrobbleBatchResult, LastFmClientError>>,
}

impl ObservedTransport {
    fn new() -> (
        Arc<Self>,
        async_channel::Receiver<bool>,
        async_channel::Sender<Result<ScrobbleBatchResult, LastFmClientError>>,
    ) {
        let (calls, call_events) = async_channel::unbounded();
        let (response_events, responses) = async_channel::unbounded();
        (
            Arc::new(Self { calls, responses }),
            call_events,
            response_events,
        )
    }
}

#[async_trait::async_trait]
impl LastFmTransport for ObservedTransport {
    async fn update_now_playing(
        &self,
        _session: &StoredSession,
        _track: &LastFmTrack,
    ) -> Result<SubmissionResult, LastFmClientError> {
        Err(LastFmClientError::InvalidInput)
    }

    async fn submit_scrobbles(
        &self,
        session: &StoredSession,
        _scrobbles: &[Scrobble],
    ) -> Result<ScrobbleBatchResult, LastFmClientError> {
        let _ = self.calls.send(session.key().expose() == RENEWED_KEY).await;
        self.responses
            .recv()
            .await
            .unwrap_or(Err(LastFmClientError::Transport))
    }
}

struct GatedCredentialStore {
    session: Mutex<Option<StoredSession>>,
    save_started: async_channel::Sender<()>,
    save_release: Mutex<mpsc::Receiver<()>>,
    deletes: AtomicUsize,
}

impl GatedCredentialStore {
    fn new(session: StoredSession) -> (Arc<Self>, async_channel::Receiver<()>, mpsc::Sender<()>) {
        let (save_started, save_events) = async_channel::bounded(1);
        let (release, save_release) = mpsc::channel();
        (
            Arc::new(Self {
                session: Mutex::new(Some(session)),
                save_started,
                save_release: Mutex::new(save_release),
                deletes: AtomicUsize::new(0),
            }),
            save_events,
            release,
        )
    }

    fn has_renewed_session(&self) -> bool {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|session| session.key().expose() == RENEWED_KEY)
    }
}

impl SessionCredentialStore for GatedCredentialStore {
    fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
        self.session
            .lock()
            .map(|session| session.clone())
            .map_err(|_| CredentialError::Unavailable)
    }

    fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
        self.save_started
            .try_send(())
            .map_err(|_| CredentialError::Unavailable)?;
        self.save_release
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .recv_timeout(WATCHDOG)
            .map_err(|_| CredentialError::Unavailable)?;
        *self
            .session
            .lock()
            .map_err(|_| CredentialError::Unavailable)? = Some(session.clone());
        Ok(())
    }

    fn delete(&self) -> Result<(), CredentialError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        *self
            .session
            .lock()
            .map_err(|_| CredentialError::Unavailable)? = None;
        Ok(())
    }
}

struct PausedRuntime {
    database: DatabaseConnection,
    binding: LastFmAccountBinding,
    handle: LastFmRuntimeHandle,
    shutdown: LastFmRuntimeShutdown,
    status: watch::Receiver<LastFmRuntimeStatus>,
    store: Arc<GatedCredentialStore>,
    calls: async_channel::Receiver<bool>,
    save_events: async_channel::Receiver<()>,
    save_release: mpsc::Sender<()>,
}

async fn paused_runtime() -> PausedRuntime {
    let database = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&database, None).await.unwrap();
    let session = StoredSession::new("listener", ProtectedString::new(ORIGINAL_KEY)).unwrap();
    let binding = session.account_binding();
    let pending = PendingLastFmScrobble::try_new(
        Uuid::new_v4(),
        binding,
        "Artist".to_owned(),
        "Track".to_owned(),
        Some("Album".to_owned()),
        None,
        Some(1),
        180,
        1_700_000_000,
    )
    .unwrap();
    assert!(matches!(
        storage::enqueue(&database, &pending).await.unwrap(),
        LastFmEnqueueOutcome::Inserted { .. }
    ));

    let (store, save_events, save_release) = GatedCredentialStore::new(session);
    let (transport, calls, responses) = ObservedTransport::new();
    let (handle, shutdown) = spawn_lastfm_runtime(
        LastFmRuntimeActivation::issue_after_consent_and_enablement(),
        database.clone(),
        store.clone(),
        transport,
        Arc::new(FixedClock),
    )
    .await
    .unwrap();
    let mut status = handle.subscribe_status();
    assert!(
        !receive(&calls).await,
        "initial delivery uses the original key"
    );
    responses
        .send(Err(LastFmClientError::ReauthenticationRequired))
        .await
        .unwrap();
    wait_for_status(&mut status, LastFmRuntimePhase::ReauthenticationRequired).await;

    PausedRuntime {
        database,
        binding,
        handle,
        shutdown,
        status,
        store,
        calls,
        save_events,
        save_release,
    }
}

async fn receive<T>(receiver: &async_channel::Receiver<T>) -> T {
    tokio::time::timeout(WATCHDOG, receiver.recv())
        .await
        .expect("fixture event arrived before watchdog")
        .expect("fixture sender remained active")
}

async fn wait_for_status(
    status: &mut watch::Receiver<LastFmRuntimeStatus>,
    phase: LastFmRuntimePhase,
) -> LastFmRuntimeStatus {
    loop {
        let snapshot = *status.borrow_and_update();
        if snapshot.phase == phase {
            return snapshot;
        }
        tokio::time::timeout(WATCHDOG, status.changed())
            .await
            .expect("runtime status changed before watchdog")
            .expect("runtime owner remained active");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reauthorization_claim_serializes_disconnect_and_retains_single_flight_until_save_finishes()
{
    let runtime = paused_runtime().await;
    let operation = runtime
        .handle
        .reauthorize_same_account("listener".to_owned(), ProtectedString::new(RENEWED_KEY))
        .unwrap();
    receive(&runtime.save_events).await;

    {
        let ingress = runtime
            .handle
            .inner
            .ingress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(matches!(
            ingress.phase,
            IngressPhase::Transitioning {
                state: TransitionState::ReauthorizationInFlight,
                ..
            }
        ));
        assert!(ingress.reauthorization_queued);
    }
    assert_eq!(
        runtime
            .handle
            .reauthorize_same_account("listener".to_owned(), ProtectedString::new(RENEWED_KEY))
            .unwrap_err(),
        LastFmRuntimeAdmissionError::ReauthorizationPending
    );
    assert_eq!(
        runtime.handle.disconnect_and_purge().unwrap_err(),
        LastFmRuntimeAdmissionError::Transitioning
    );
    assert_eq!(storage::queue_len(&runtime.database).await.unwrap(), 1);
    assert_eq!(runtime.store.deletes.load(Ordering::SeqCst), 0);

    runtime.save_release.send(()).unwrap();
    operation.wait().await.unwrap();
    assert!(
        receive(&runtime.calls).await,
        "renewed delivery uses the saved key"
    );
    assert!(runtime.store.has_renewed_session());

    assert_eq!(
        runtime
            .handle
            .disconnect_and_purge()
            .unwrap()
            .wait()
            .await
            .unwrap(),
        1
    );
    assert_eq!(storage::queue_len(&runtime.database).await.unwrap(), 0);
    assert_eq!(runtime.store.deletes.load(Ordering::SeqCst), 1);
    runtime.shutdown.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_reauthorization_save_never_restarts_or_publishes_active_after_close() {
    let runtime = paused_runtime().await;
    let operation = runtime
        .handle
        .reauthorize_same_account("listener".to_owned(), ProtectedString::new(RENEWED_KEY))
        .unwrap();
    receive(&runtime.save_events).await;
    let revision_at_close = runtime.status.borrow().revision;

    assert!(runtime.handle.close_and_flush());
    assert_eq!(
        runtime.handle.disconnect_and_purge().unwrap_err(),
        LastFmRuntimeAdmissionError::Closed
    );
    assert_eq!(
        runtime
            .handle
            .reauthorize_same_account("listener".to_owned(), ProtectedString::new(RENEWED_KEY))
            .unwrap_err(),
        LastFmRuntimeAdmissionError::Closed
    );

    runtime.save_release.send(()).unwrap();
    assert_eq!(
        operation.wait().await,
        Err(LastFmRuntimeCommandError::OwnerStopped)
    );
    assert_eq!(
        runtime.shutdown.shutdown().await.unwrap(),
        LastFmRuntimeShutdownReason::Drained
    );

    let final_status = *runtime.status.borrow();
    assert_eq!(final_status.phase, LastFmRuntimePhase::Stopped);
    assert_eq!(final_status.revision, revision_at_close + 2);
    assert!(
        runtime.calls.try_recv().is_err(),
        "no renewed worker was started"
    );
    assert!(
        runtime.store.has_renewed_session(),
        "admitted vault save drained"
    );
    assert_eq!(runtime.store.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(storage::queue_len(&runtime.database).await.unwrap(), 1);
    let _ = runtime.binding;
}
