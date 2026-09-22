//! Focused regressions for the Last.fm account composition layer.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sea_orm::{
    ActiveValue::{NotSet, Set},
    Database, DatabaseConnection, EntityTrait, PaginatorTrait,
};
use sea_orm_migration::MigratorTrait;
use uuid::Uuid;

use crate::db::entities::lastfm_scrobble;
use crate::db::migration::Migrator;
use crate::lastfm::account::{
    begin_consent_gated_authorization, discard_quarantined_queue, install_fresh_account,
    install_replacement_account, install_same_account_reauthorization, load_vault_account,
    spawn_lastfm_authorization_owner_with, stage_account_install_decision,
    LastFmAccountAuthorizationError, LastFmAccountInstallDecision, LastFmAccountInstallError,
    LastFmAccountIntegrationError, LastFmAccountReplacementError,
};
use crate::lastfm::authorization::{
    LastFmAuthorizationClock, LastFmAuthorizationGrant, LastFmAuthorizationPhase,
    LastFmAuthorizationTransport,
};
use crate::lastfm::client::{
    DesktopAuthToken, DesktopAuthorizationUrl, DesktopAuthorizedSession, LastFmClientError,
};
use crate::lastfm::credentials::{
    CredentialError, LastFmAccountBinding, ProtectedString, SessionCredentialStore, StoredSession,
};
use crate::lastfm::lifecycle::acquire_vault_lifecycle;
use crate::lastfm::policy::{
    commit_policy_update, LastFmConsentRecord, LastFmPolicyGeneration, LastFmPolicyUpdate,
};

const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";
const RENEWED_SESSION_KEY: &str = "abcdef0123456789abcdef0123456789";
const FIXTURE_URL: &str = concat!(
    "https://www.last.fm/api/auth/?api_key=",
    "0123456789abcdef0123456789abcdef",
    "&token=0123456789abcdef0123456789abcdef"
);

/// Deterministic transport which approves every staged step.
struct ApprovingTransport;

#[async_trait::async_trait]
impl LastFmAuthorizationTransport for ApprovingTransport {
    async fn request_auth_token(&self) -> Result<DesktopAuthToken, LastFmClientError> {
        Ok(DesktopAuthToken::for_test(SESSION_KEY)?)
    }

    fn authorization_url(
        &self,
        _token: &DesktopAuthToken,
    ) -> Result<DesktopAuthorizationUrl, LastFmClientError> {
        DesktopAuthorizationUrl::for_test(FIXTURE_URL)
    }

    async fn exchange_auth_token(
        &self,
        _token: DesktopAuthToken,
    ) -> Result<DesktopAuthorizedSession, LastFmClientError> {
        Ok(DesktopAuthorizedSession::for_test(
            "private-listener",
            SESSION_KEY,
        )?)
    }
}

struct FrozenClock;

#[async_trait::async_trait]
impl LastFmAuthorizationClock for FrozenClock {
    fn now(&self) -> Duration {
        Duration::ZERO
    }

    async fn wait_until(&self, _deadline: Duration) {
        std::future::pending::<()>().await;
    }
}

#[tokio::test]
async fn process_authorization_owner_gates_second_claim_and_consent() {
    let (handle, shutdown) =
        spawn_lastfm_authorization_owner_with(Arc::new(ApprovingTransport), Arc::new(FrozenClock))
            .expect("the first process claim must succeed");

    assert_eq!(
        spawn_lastfm_authorization_owner_with(Arc::new(ApprovingTransport), Arc::new(FrozenClock),)
            .unwrap_err(),
        LastFmAccountIntegrationError::OwnerClaimed
    );

    // The closed default generation must refuse the handoff before any flow
    // starts: no request token is fetched, and the phase stays Idle.
    assert_eq!(
        begin_consent_gated_authorization(&handle, &LastFmPolicyGeneration::default())
            .await
            .unwrap_err(),
        LastFmAccountAuthorizationError::ConsentRequired
    );
    assert_eq!(
        handle.subscribe_status().borrow().phase,
        LastFmAuthorizationPhase::Idle
    );

    let db = account_database().await;
    let consented = commit_policy_update(
        &db,
        0,
        LastFmPolicyUpdate {
            consent: Some(LastFmConsentRecord::try_new("en", 1).expect("valid consent record")),
            enabled: true,
            enabled_remote_sources: HashSet::new(),
        },
    )
    .await
    .expect("consented policy commits");
    assert!(consented.consented_and_enabled());

    let (challenge, url) = begin_consent_gated_authorization(&handle, &consented)
        .await
        .expect("consented authorization begins");
    assert!(url.starts_with("https://www.last.fm/api/auth/"));
    assert!(url.contains("api_key="));
    assert!(url.contains("token="));

    // The owner publishes the URL once the request token resolves.
    let mut url_published = None;
    for _ in 0..200 {
        match challenge.authorization_url() {
            Ok(published) => {
                url_published = Some(published);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
    assert_eq!(
        url_published.as_deref(),
        Some(url.as_str()),
        "the handoff URL must be the exact current challenge's URL"
    );
    assert_eq!(
        handle.subscribe_status().borrow().phase,
        LastFmAuthorizationPhase::AwaitingApproval
    );

    drop(shutdown);
}

#[test]
fn integration_errors_are_content_free() {
    let diagnostics = format!(
        "{:?} {} {:?} {}",
        LastFmAccountIntegrationError::OwnerClaimed,
        LastFmAccountIntegrationError::BuildUnavailable,
        LastFmAccountIntegrationError::OwnerClaimed,
        LastFmAccountIntegrationError::BuildUnavailable,
    );
    assert!(!diagnostics.contains(SESSION_KEY));
    assert!(!diagnostics.contains("api_key"));
}

/// In-memory credential store with deliberately injectable failure modes.
struct MemoryVault {
    state: Mutex<VaultState>,
}

enum VaultState {
    Missing,
    Valid(StoredSession),
    Unavailable,
}

impl MemoryVault {
    fn lock(&self) -> Result<MutexGuard<'_, VaultState>, CredentialError> {
        self.state.lock().map_err(|_| CredentialError::Unavailable)
    }

    fn seed(&self, username: &str, key: &str) {
        *self.lock().expect("vault unlocked") = VaultState::Valid(
            StoredSession::new(username, ProtectedString::new(key))
                .expect("fixture usernames and keys satisfy the validation invariants"),
        );
    }

    fn stored_username(&self) -> Option<String> {
        match &*self.lock().expect("vault unlocked") {
            VaultState::Valid(session) => Some(session.username().to_owned()),
            _ => None,
        }
    }

    fn stored_binding(&self) -> LastFmAccountBinding {
        match &*self.lock().expect("vault unlocked") {
            VaultState::Valid(session) => session.account_binding(),
            _ => panic!("test precondition: a valid session is stored"),
        }
    }
}

impl Default for MemoryVault {
    fn default() -> Self {
        Self {
            state: Mutex::new(VaultState::Missing),
        }
    }
}

impl SessionCredentialStore for MemoryVault {
    fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
        match &*self.lock()? {
            VaultState::Missing => Ok(None),
            VaultState::Valid(session) => Ok(Some(session.clone())),
            VaultState::Unavailable => Err(CredentialError::Unavailable),
        }
    }

    fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
        *self.lock()? = VaultState::Valid(session.clone());
        Ok(())
    }

    fn delete(&self) -> Result<(), CredentialError> {
        *self.lock()? = VaultState::Missing;
        Ok(())
    }
}

fn staged_session(username: &str, key: &str) -> LastFmAuthorizationGrant {
    LastFmAuthorizationGrant::from_authorized_session(
        DesktopAuthorizedSession::for_test(username, key)
            .expect("fixture usernames and keys satisfy the validation invariants"),
    )
}

#[tokio::test]
async fn install_decision_classifies_all_three_vault_states() {
    let vault = Arc::new(MemoryVault::default());
    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();
    assert_eq!(
        stage_account_install_decision(
            Arc::clone(&credentials),
            &staged_session("fresh-user", SESSION_KEY)
        )
        .await
        .expect("decision on a missing vault"),
        LastFmAccountInstallDecision::FreshInstall
    );

    vault.seed("private-listener", SESSION_KEY);
    assert_eq!(
        stage_account_install_decision(
            Arc::clone(&credentials),
            &staged_session("private-listener", RENEWED_SESSION_KEY),
        )
        .await
        .expect("decision on an exact account"),
        LastFmAccountInstallDecision::SameAccountReauthorization
    );
    assert_eq!(
        stage_account_install_decision(
            Arc::clone(&credentials),
            &staged_session("someone-else", SESSION_KEY),
        )
        .await
        .expect("decision on a different account"),
        LastFmAccountInstallDecision::DifferentAccount {
            existing_username: "private-listener".to_owned(),
        }
    );
}

#[tokio::test]
async fn fresh_install_mints_a_new_identity_and_refuses_a_second() {
    let vault = Arc::new(MemoryVault::default());
    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();
    let account = install_fresh_account(
        Arc::clone(&credentials),
        staged_session("fresh-user", SESSION_KEY),
    )
    .await
    .expect("the first fresh install lands");
    assert_eq!(account.username(), "fresh-user");
    assert_eq!(vault.stored_username().as_deref(), Some("fresh-user"));

    assert_eq!(
        install_fresh_account(
            Arc::clone(&credentials),
            staged_session("someone-else", SESSION_KEY),
        )
        .await
        .unwrap_err(),
        LastFmAccountInstallError::VaultAlreadyBound
    );
    assert_eq!(
        vault.stored_username().as_deref(),
        Some("fresh-user"),
        "a refused install must leave the stored record untouched"
    );
}

#[tokio::test]
async fn reauthorization_preserves_the_exact_stored_identity() {
    let vault = Arc::new(MemoryVault::default());
    vault.seed("private-listener", SESSION_KEY);
    let binding_before = vault.stored_binding();
    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();

    let account = install_same_account_reauthorization(
        Arc::clone(&credentials),
        staged_session("private-listener", RENEWED_SESSION_KEY),
    )
    .await
    .expect("exact reauthorization lands");
    assert_eq!(account.username(), "private-listener");
    assert_eq!(
        account.account_binding().as_bytes(),
        binding_before.as_bytes(),
        "reauthorization must preserve the opaque account identity"
    );

    assert_eq!(
        install_same_account_reauthorization(
            Arc::clone(&credentials),
            staged_session("someone-else", SESSION_KEY),
        )
        .await
        .unwrap_err(),
        LastFmAccountInstallError::ExactAccountRefused
    );
    assert_eq!(
        vault.stored_binding().as_bytes(),
        binding_before.as_bytes(),
        "a refused different-account grant must not overwrite the vault"
    );
}

#[tokio::test]
async fn reauthorization_refuses_a_vanished_record() {
    let credentials: Arc<dyn SessionCredentialStore> = Arc::new(MemoryVault::default());
    assert_eq!(
        install_same_account_reauthorization(
            Arc::clone(&credentials),
            staged_session("private-listener", SESSION_KEY),
        )
        .await
        .unwrap_err(),
        LastFmAccountInstallError::VaultMissing
    );
}

#[tokio::test]
async fn unavailable_store_fails_install_paths_closed() {
    let credentials: Arc<dyn SessionCredentialStore> = Arc::new(MemoryVault {
        state: Mutex::new(VaultState::Unavailable),
    });
    assert_eq!(
        load_vault_account(Arc::clone(&credentials)).await,
        Err(CredentialError::Unavailable)
    );
    assert_eq!(
        install_fresh_account(
            Arc::clone(&credentials),
            staged_session("fresh-user", SESSION_KEY)
        )
        .await
        .unwrap_err(),
        LastFmAccountInstallError::CredentialStoreUnavailable
    );
}

#[tokio::test]
async fn account_installs_serialize_behind_the_vault_lifecycle_lease() {
    let held = acquire_vault_lifecycle().await;
    let vault = Arc::new(MemoryVault::default());
    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();

    let fresh = install_fresh_account(
        Arc::clone(&credentials),
        staged_session("fresh-user", SESSION_KEY),
    );
    let reauth = install_same_account_reauthorization(
        Arc::clone(&credentials),
        staged_session("private-listener", SESSION_KEY),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), fresh)
            .await
            .is_err(),
        "fresh install must wait for the lifecycle lease"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), reauth)
            .await
            .is_err(),
        "reauthorization must wait for the lifecycle lease"
    );
    assert!(vault.stored_username().is_none());
    drop(held);
}

#[test]
fn install_errors_and_accounts_are_content_free() {
    let diagnostics = format!(
        "{:?} {} {:?} {:?} {:?}",
        LastFmAccountInstallError::CredentialStoreUnavailable,
        LastFmAccountInstallError::ExactAccountRefused,
        LastFmAccountInstallError::VaultAlreadyBound,
        LastFmAccountInstallError::VaultMissing,
        LastFmAccountInstallError::InvalidStagedSession,
    );
    assert!(!diagnostics.contains(SESSION_KEY));
    assert!(!diagnostics.contains("fresh-user"));
}

async fn account_database() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("open in-memory Last.fm queue database");
    Migrator::up(&db, None)
        .await
        .expect("run Last.fm queue migrations");
    db
}

async fn seed_queue_rows(db: &DatabaseConnection, binding: LastFmAccountBinding, count: usize) {
    let rows = (0..count).map(|_| lastfm_scrobble::ActiveModel {
        id: NotSet,
        occurrence_id: Set(Uuid::new_v4().as_bytes().to_vec().into()),
        account_binding: Set(binding.as_bytes().to_vec().into()),
        artist: Set("Seed Artist".to_owned().into()),
        track_title: Set("Seed Track".to_owned().into()),
        album: Set(Some("Seed Album".to_owned().into())),
        album_artist: Set(None),
        track_number: Set(Some(1.into())),
        duration_secs: Set(60.into()),
        started_at_unix_secs: Set(1_700_000_000_i64.into()),
        attempt_count: Set(0),
        next_attempt_at_ms: Set(0),
    });
    lastfm_scrobble::Entity::insert_many(rows)
        .exec(db)
        .await
        .expect("insert canonical Last.fm seed rows");
}

async fn queue_len(db: &DatabaseConnection) -> u64 {
    lastfm_scrobble::Entity::find()
        .count(db)
        .await
        .expect("count Last.fm queue rows")
}

#[tokio::test]
async fn replacement_purges_the_old_queue_and_installs_a_new_identity() {
    let vault = Arc::new(MemoryVault::default());
    vault.seed("old-user", SESSION_KEY);
    let old_binding = vault.stored_binding();
    let db = account_database().await;
    seed_queue_rows(&db, old_binding, 3).await;
    assert_eq!(queue_len(&db).await, 3);

    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();
    let account = install_replacement_account(
        Arc::clone(&credentials),
        db.clone(),
        staged_session("new-user", RENEWED_SESSION_KEY),
    )
    .await
    .expect("replacement lands");

    assert_eq!(account.username(), "new-user");
    assert_ne!(
        account.account_binding().as_bytes(),
        old_binding.as_bytes(),
        "a replaced account must mint a brand-new opaque identity"
    );
    assert_eq!(vault.stored_username().as_deref(), Some("new-user"));
    assert_eq!(
        queue_len(&db).await,
        0,
        "pending scrobbles must never transfer across accounts"
    );
}

#[tokio::test]
async fn replacement_refuses_when_another_account_still_owns_queue_rows() {
    let vault = Arc::new(MemoryVault::default());
    vault.seed("old-user", SESSION_KEY);
    let old_binding = vault.stored_binding();
    let interloper = StoredSession::new("someone-else", ProtectedString::new(SESSION_KEY))
        .expect("fixture satisfies validation")
        .account_binding();
    let db = account_database().await;
    seed_queue_rows(&db, old_binding, 1).await;
    seed_queue_rows(&db, interloper, 2).await;

    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();
    assert_eq!(
        install_replacement_account(
            Arc::clone(&credentials),
            db,
            staged_session("new-user", SESSION_KEY),
        )
        .await
        .unwrap_err(),
        LastFmAccountReplacementError::QueuePurgeRefused
    );
    assert_eq!(
        vault.stored_username().as_deref(),
        Some("old-user"),
        "a refused replacement must keep the valid prior record"
    );
}

#[tokio::test]
async fn replacement_refuses_a_vanished_record() {
    let vault = Arc::new(MemoryVault::default());
    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();
    let db = account_database().await;
    assert_eq!(
        install_replacement_account(
            Arc::clone(&credentials),
            db,
            staged_session("new-user", SESSION_KEY),
        )
        .await
        .unwrap_err(),
        LastFmAccountReplacementError::VaultMissing
    );
}

#[tokio::test]
async fn quarantine_discard_purges_orphaned_rows_and_refuses_a_valid_account() {
    let vault = Arc::new(MemoryVault::default());
    let binding = StoredSession::new("quarantined-user", ProtectedString::new(SESSION_KEY))
        .expect("fixture satisfies validation")
        .account_binding();
    let db = account_database().await;
    seed_queue_rows(&db, binding, 2).await;

    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();
    let purged = discard_quarantined_queue(Arc::clone(&credentials), db.clone())
        .await
        .expect("quarantined rows are discardable without a valid account");
    assert_eq!(purged, 2);
    assert_eq!(queue_len(&db).await, 0);

    vault.seed("private-listener", SESSION_KEY);
    seed_queue_rows(&db, binding, 1).await;
    assert_eq!(
        discard_quarantined_queue(Arc::clone(&credentials), db.clone())
            .await
            .unwrap_err(),
        crate::lastfm::lifecycle::LastFmQuarantinedQueueRecoveryError::ValidSessionPresent
    );
    assert_eq!(
        queue_len(&db).await,
        1,
        "a valid account's rows must never be discardable as quarantined"
    );
}

#[test]
fn replacement_errors_are_content_free() {
    let diagnostics = format!(
        "{:?} {} {:?} {:?} {:?}",
        LastFmAccountReplacementError::CredentialStoreUnavailable,
        LastFmAccountReplacementError::InvalidStagedSession,
        LastFmAccountReplacementError::VaultMissing,
        LastFmAccountReplacementError::QueuePurgeRefused,
        LastFmAccountReplacementError::CredentialCleanupRequired,
    );
    assert!(!diagnostics.contains(SESSION_KEY));
    assert!(!diagnostics.contains("old-user"));
}
