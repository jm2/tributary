//! Process-global Last.fm vault lifecycle and explicit quarantine recovery.
//!
//! A normal runtime retains the lifecycle lease for its complete lifetime.
//! Recovery takes the same lease before inspecting the single native-vault
//! record and holds it through every destructive action. This makes the lease
//! both the closed-admission proof for the private FIFO and the barrier which
//! prevents a successor runtime from adopting an account mid-recovery.

use std::fmt;
use std::sync::Arc;

use sea_orm::DatabaseConnection;

use super::credentials::{CredentialError, SessionCredentialStore};
use super::storage::{self, LastFmClosedAndDrainedQueue};

static LASTFM_VAULT_LIFECYCLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) type LastFmVaultLifecycleLease = tokio::sync::MutexGuard<'static, ()>;

/// Acquire the process-global Last.fm vault/account generation.
pub(super) async fn acquire_vault_lifecycle() -> LastFmVaultLifecycleLease {
    LASTFM_VAULT_LIFECYCLE.lock().await
}

/// Successful destructive recovery of a queue whose account cannot be loaded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmQuarantinedQueueRecovery {
    /// No native-vault record existed; only the closed queue snapshot was removed.
    MissingVault { purged_scrobbles: u64 },
    /// An invalid native-vault record was removed after the queue snapshot.
    CorruptVault { purged_scrobbles: u64 },
}

impl LastFmQuarantinedQueueRecovery {
    #[must_use]
    pub const fn purged_scrobbles(self) -> u64 {
        match self {
            Self::MissingVault { purged_scrobbles } | Self::CorruptVault { purged_scrobbles } => {
                purged_scrobbles
            }
        }
    }
}

/// Content-free refusal or failure from explicit quarantined-queue recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmQuarantinedQueueRecoveryError {
    #[error("Last.fm protected credential store is unavailable")]
    CredentialStoreUnavailable,
    #[error("Last.fm still has a valid protected account")]
    ValidSessionPresent,
    #[error("Last.fm quarantined queue storage is unavailable")]
    Queue,
    #[error("Last.fm corrupt protected credential cleanup must be retried")]
    CredentialCleanupRequired,
    #[error("Last.fm quarantined queue recovery stopped unexpectedly")]
    OwnerStopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoverableVaultState {
    Missing,
    Corrupt,
}

/// Explicitly discard a queue which cannot be associated with a loadable
/// native-vault account.
///
/// Calling this function starts a detached lifecycle owner before the first
/// await. Cancelling the caller therefore cannot release the lifecycle lease
/// between vault inspection, SQLite purge, and corrupt-record deletion. A
/// missing record or exact [`CredentialError::InvalidData`] is recoverable;
/// transient vault failure and a valid account leave both stores untouched.
pub async fn recover_quarantined_lastfm_queue(
    database: DatabaseConnection,
    credentials: Arc<dyn SessionCredentialStore>,
) -> Result<LastFmQuarantinedQueueRecovery, LastFmQuarantinedQueueRecoveryError> {
    tokio::spawn(recover_quarantined_lastfm_queue_owned(
        database,
        credentials,
    ))
    .await
    .map_err(|_| LastFmQuarantinedQueueRecoveryError::OwnerStopped)?
}

async fn recover_quarantined_lastfm_queue_owned(
    database: DatabaseConnection,
    credentials: Arc<dyn SessionCredentialStore>,
) -> Result<LastFmQuarantinedQueueRecovery, LastFmQuarantinedQueueRecoveryError> {
    let lease = acquire_vault_lifecycle().await;
    let credentials_for_load = Arc::clone(&credentials);
    // The blocking job owns the lease. Cancellation or panic cannot let a
    // successor inspect the process-global record while this load still runs.
    let (lease, loaded) = tokio::task::spawn_blocking(move || {
        let loaded = credentials_for_load.load();
        (lease, loaded)
    })
    .await
    .map_err(|_| LastFmQuarantinedQueueRecoveryError::CredentialStoreUnavailable)?;

    let vault_state = match loaded {
        Ok(None) => RecoverableVaultState::Missing,
        Err(CredentialError::InvalidData) => RecoverableVaultState::Corrupt,
        Ok(Some(_)) => return Err(LastFmQuarantinedQueueRecoveryError::ValidSessionPresent),
        Err(CredentialError::Unavailable | CredentialError::AccountMismatch) => {
            return Err(LastFmQuarantinedQueueRecoveryError::CredentialStoreUnavailable);
        }
    };

    // The shared lease proves that no runtime admission owner exists and keeps
    // every successor behind this complete destructive transaction sequence.
    let authority = LastFmClosedAndDrainedQueue::issue_after_barrier();
    let purged_scrobbles = storage::purge_quarantined_after_admission_closed(&database, &authority)
        .await
        .map_err(|_| LastFmQuarantinedQueueRecoveryError::Queue)?;

    match vault_state {
        RecoverableVaultState::Missing => {
            drop(lease);
            Ok(LastFmQuarantinedQueueRecovery::MissingVault { purged_scrobbles })
        }
        RecoverableVaultState::Corrupt => {
            let credentials_for_delete = Arc::clone(&credentials);
            // Retain the same lease inside the blocking cleanup just as for the
            // inspection above. On failure the already-purged queue stays empty
            // and a later explicit recovery can retry only this cleanup.
            let (_lease, deleted) = tokio::task::spawn_blocking(move || {
                let deleted = credentials_for_delete.delete();
                (lease, deleted)
            })
            .await
            .map_err(|_| LastFmQuarantinedQueueRecoveryError::CredentialCleanupRequired)?;
            deleted.map_err(|_| LastFmQuarantinedQueueRecoveryError::CredentialCleanupRequired)?;
            Ok(LastFmQuarantinedQueueRecovery::CorruptVault { purged_scrobbles })
        }
    }
}

impl fmt::Display for LastFmQuarantinedQueueRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingVault { .. } => "Last.fm missing-vault queue recovery completed",
            Self::CorruptVault { .. } => "Last.fm corrupt-vault queue recovery completed",
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use uuid::Uuid;

    use super::*;
    use crate::db::migration::Migrator;
    use crate::lastfm::client::{
        LastFmClientError, LastFmTrack, Scrobble, ScrobbleBatchResult, SubmissionResult,
    };
    use crate::lastfm::credentials::{LastFmAccountBinding, ProtectedString, StoredSession};
    use crate::lastfm::delivery::{LastFmClock, LastFmDeliveryPrimitiveError, LastFmTransport};
    use crate::lastfm::runtime::{
        spawn_lastfm_runtime, LastFmRuntimeActivation, LastFmRuntimeStartError,
    };
    use crate::lastfm::storage::{LastFmEnqueueOutcome, PendingLastFmScrobble};

    const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";
    const TEST_DEADLINE: Duration = Duration::from_secs(2);

    #[derive(Clone)]
    enum TestVaultState {
        Missing,
        Corrupt,
        Unavailable,
        Valid(StoredSession),
    }

    struct BlockingGate {
        first: AtomicBool,
        entered: async_channel::Sender<()>,
        released: Mutex<bool>,
        release: Condvar,
    }

    impl BlockingGate {
        fn new() -> (Arc<Self>, async_channel::Receiver<()>) {
            let (entered, observations) = async_channel::bounded(1);
            (
                Arc::new(Self {
                    first: AtomicBool::new(true),
                    entered,
                    released: Mutex::new(false),
                    release: Condvar::new(),
                }),
                observations,
            )
        }

        fn block_first(&self) {
            if !self.first.swap(false, Ordering::SeqCst) {
                return;
            }
            let _ = self.entered.try_send(());
            let mut released = self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = self
                    .release
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        fn release(&self) {
            *self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            self.release.notify_all();
        }
    }

    struct TestCredentialStore {
        state: Mutex<TestVaultState>,
        load_count: AtomicUsize,
        delete_count: AtomicUsize,
        fail_next_delete: AtomicBool,
        load_gate: Option<Arc<BlockingGate>>,
        delete_gate: Option<Arc<BlockingGate>>,
    }

    impl TestCredentialStore {
        fn new(state: TestVaultState) -> Self {
            Self {
                state: Mutex::new(state),
                load_count: AtomicUsize::new(0),
                delete_count: AtomicUsize::new(0),
                fail_next_delete: AtomicBool::new(false),
                load_gate: None,
                delete_gate: None,
            }
        }

        fn with_load_gate(state: TestVaultState, gate: Arc<BlockingGate>) -> Self {
            Self {
                load_gate: Some(gate),
                ..Self::new(state)
            }
        }

        fn with_delete_gate(state: TestVaultState, gate: Arc<BlockingGate>) -> Self {
            Self {
                delete_gate: Some(gate),
                ..Self::new(state)
            }
        }

        fn state(&self) -> TestVaultState {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl SessionCredentialStore for TestCredentialStore {
        fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
            self.load_count.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.load_gate {
                gate.block_first();
            }
            match self.state() {
                TestVaultState::Missing => Ok(None),
                TestVaultState::Corrupt => Err(CredentialError::InvalidData),
                TestVaultState::Unavailable => Err(CredentialError::Unavailable),
                TestVaultState::Valid(session) => Ok(Some(session)),
            }
        }

        fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
            *self
                .state
                .lock()
                .map_err(|_| CredentialError::Unavailable)? =
                TestVaultState::Valid(session.clone());
            Ok(())
        }

        fn delete(&self) -> Result<(), CredentialError> {
            self.delete_count.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.delete_gate {
                gate.block_first();
            }
            if self.fail_next_delete.swap(false, Ordering::SeqCst) {
                return Err(CredentialError::Unavailable);
            }
            *self
                .state
                .lock()
                .map_err(|_| CredentialError::Unavailable)? = TestVaultState::Missing;
            Ok(())
        }
    }

    struct PendingTransport;

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

    async fn database() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        database
    }

    fn session() -> StoredSession {
        StoredSession::new("private-listener", ProtectedString::new(SESSION_KEY)).unwrap()
    }

    fn pending(binding: LastFmAccountBinding) -> PendingLastFmScrobble {
        PendingLastFmScrobble::try_new(
            Uuid::new_v4(),
            binding,
            "private-artist".to_owned(),
            "private-title".to_owned(),
            Some("private-album".to_owned()),
            None,
            Some(1),
            60,
            1_700_000_000,
        )
        .unwrap()
    }

    async fn queue_one(database: &DatabaseConnection, binding: LastFmAccountBinding) {
        assert!(matches!(
            storage::enqueue(database, &pending(binding)).await.unwrap(),
            LastFmEnqueueOutcome::Inserted { .. }
        ));
    }

    async fn observe(receiver: &async_channel::Receiver<()>) {
        tokio::time::timeout(TEST_DEADLINE, receiver.recv())
            .await
            .expect("blocking vault operation must become observable")
            .expect("blocking vault observation channel must remain open");
    }

    #[tokio::test]
    async fn missing_vault_purges_without_attempting_credential_deletion() {
        let database = database().await;
        let retained = session();
        queue_one(&database, retained.account_binding()).await;
        let store = Arc::new(TestCredentialStore::new(TestVaultState::Missing));

        let outcome = recover_quarantined_lastfm_queue(database.clone(), store.clone())
            .await
            .unwrap();

        assert_eq!(
            outcome,
            LastFmQuarantinedQueueRecovery::MissingVault {
                purged_scrobbles: 1
            }
        );
        assert_eq!(outcome.purged_scrobbles(), 1);
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        assert_eq!(store.delete_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn corrupt_vault_is_deleted_only_after_its_queue_is_purged() {
        let database = database().await;
        let retained = session();
        queue_one(&database, retained.account_binding()).await;
        let store = Arc::new(TestCredentialStore::new(TestVaultState::Corrupt));

        let outcome = recover_quarantined_lastfm_queue(database.clone(), store.clone())
            .await
            .unwrap();

        assert_eq!(
            outcome,
            LastFmQuarantinedQueueRecovery::CorruptVault {
                purged_scrobbles: 1
            }
        );
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        assert!(matches!(store.state(), TestVaultState::Missing));
        assert_eq!(store.delete_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unavailable_or_valid_vault_refuses_recovery_without_mutation() {
        for (state, expected) in [
            (
                TestVaultState::Unavailable,
                LastFmQuarantinedQueueRecoveryError::CredentialStoreUnavailable,
            ),
            (
                TestVaultState::Valid(session()),
                LastFmQuarantinedQueueRecoveryError::ValidSessionPresent,
            ),
        ] {
            let database = database().await;
            let retained = session();
            queue_one(&database, retained.account_binding()).await;
            let store = Arc::new(TestCredentialStore::new(state));

            assert_eq!(
                recover_quarantined_lastfm_queue(database.clone(), store.clone()).await,
                Err(expected)
            );
            assert_eq!(storage::queue_len(&database).await.unwrap(), 1);
            assert_eq!(store.delete_count.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn failed_corrupt_record_cleanup_is_retryable_after_the_queue_is_empty() {
        let database = database().await;
        let retained = session();
        queue_one(&database, retained.account_binding()).await;
        let store = Arc::new(TestCredentialStore::new(TestVaultState::Corrupt));
        store.fail_next_delete.store(true, Ordering::SeqCst);

        assert_eq!(
            recover_quarantined_lastfm_queue(database.clone(), store.clone()).await,
            Err(LastFmQuarantinedQueueRecoveryError::CredentialCleanupRequired)
        );
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        assert!(matches!(store.state(), TestVaultState::Corrupt));

        assert_eq!(
            recover_quarantined_lastfm_queue(database.clone(), store.clone())
                .await
                .unwrap(),
            LastFmQuarantinedQueueRecovery::CorruptVault {
                purged_scrobbles: 0
            }
        );
        assert!(matches!(store.state(), TestVaultState::Missing));
        assert_eq!(store.delete_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancelled_caller_cannot_release_a_blocking_inspection_or_skip_purge() {
        let database = database().await;
        let retained = session();
        queue_one(&database, retained.account_binding()).await;
        let (gate, entered) = BlockingGate::new();
        let store = Arc::new(TestCredentialStore::with_load_gate(
            TestVaultState::Missing,
            gate.clone(),
        ));

        let recovery_database = database.clone();
        let recovery_store = store.clone();
        let recovery = tokio::spawn(async move {
            recover_quarantined_lastfm_queue(recovery_database, recovery_store).await
        });
        observe(&entered).await;

        let successor_database = database.clone();
        let successor_store = store.clone();
        let mut successor = tokio::spawn(async move {
            spawn_lastfm_runtime(
                LastFmRuntimeActivation::issue_after_consent_and_enablement(),
                successor_database,
                successor_store,
                Arc::new(PendingTransport),
                Arc::new(FixedClock),
            )
            .await
        });
        let successor_waited = tokio::time::timeout(Duration::from_millis(30), &mut successor)
            .await
            .is_err();
        let load_count_while_blocked = store.load_count.load(Ordering::SeqCst);

        recovery.abort();
        assert!(recovery.await.unwrap_err().is_cancelled());
        gate.release();
        let successor_result = tokio::time::timeout(TEST_DEADLINE, successor)
            .await
            .expect("detached recovery must eventually release the successor")
            .expect("successor task must not panic");
        assert!(
            successor_waited,
            "successor runtime must wait behind recovery's blocking vault inspection"
        );
        assert_eq!(load_count_while_blocked, 1);
        assert!(matches!(
            successor_result,
            Err(LastFmRuntimeStartError::CredentialMismatch)
        ));
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);
        assert_eq!(store.load_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn corrupt_record_deletion_retains_the_lease_through_blocking_cleanup() {
        let database = database().await;
        let retained = session();
        queue_one(&database, retained.account_binding()).await;
        let (gate, entered) = BlockingGate::new();
        let store = Arc::new(TestCredentialStore::with_delete_gate(
            TestVaultState::Corrupt,
            gate.clone(),
        ));

        let recovery_database = database.clone();
        let recovery_store = store.clone();
        let recovery = tokio::spawn(async move {
            recover_quarantined_lastfm_queue(recovery_database, recovery_store).await
        });
        observe(&entered).await;
        assert_eq!(storage::queue_len(&database).await.unwrap(), 0);

        let successor_database = database.clone();
        let successor_store = store.clone();
        let mut successor = tokio::spawn(async move {
            spawn_lastfm_runtime(
                LastFmRuntimeActivation::issue_after_consent_and_enablement(),
                successor_database,
                successor_store,
                Arc::new(PendingTransport),
                Arc::new(FixedClock),
            )
            .await
        });
        let successor_waited = tokio::time::timeout(Duration::from_millis(30), &mut successor)
            .await
            .is_err();

        gate.release();
        assert_eq!(
            tokio::time::timeout(TEST_DEADLINE, recovery)
                .await
                .expect("recovery must complete")
                .expect("recovery task must not panic")
                .unwrap(),
            LastFmQuarantinedQueueRecovery::CorruptVault {
                purged_scrobbles: 1
            }
        );
        assert!(
            successor_waited,
            "successor runtime must wait through corrupt-record deletion"
        );
        let successor_result = tokio::time::timeout(TEST_DEADLINE, successor)
            .await
            .expect("successor must run after cleanup")
            .expect("successor task must not panic");
        assert!(matches!(
            successor_result,
            Err(LastFmRuntimeStartError::CredentialMismatch)
        ));
    }

    #[test]
    fn recovery_diagnostics_are_content_free() {
        let private = "private-title-never-print";
        let diagnostics = format!(
            "{:?} {} {:?} {}",
            LastFmQuarantinedQueueRecovery::CorruptVault {
                purged_scrobbles: 7
            },
            LastFmQuarantinedQueueRecovery::MissingVault {
                purged_scrobbles: 7
            },
            LastFmQuarantinedQueueRecoveryError::CredentialCleanupRequired,
            LastFmQuarantinedQueueRecoveryError::Queue,
        );
        assert!(!diagnostics.contains(private));
    }
}
