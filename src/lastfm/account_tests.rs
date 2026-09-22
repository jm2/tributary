//! Focused regressions for the Last.fm account composition layer.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::lastfm::account::{
    install_fresh_account, install_same_account_reauthorization, load_vault_account,
    spawn_lastfm_authorization_owner_with, stage_account_install_decision,
    LastFmAccountInstallDecision, LastFmAccountInstallError, LastFmAccountIntegrationError,
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
async fn process_authorization_owner_claims_exactly_once() {
    let (handle, shutdown) =
        spawn_lastfm_authorization_owner_with(Arc::new(ApprovingTransport), Arc::new(FrozenClock))
            .expect("the first process claim must succeed");

    assert_eq!(
        handle.subscribe_status().borrow().phase,
        LastFmAuthorizationPhase::Idle
    );
    assert_eq!(
        spawn_lastfm_authorization_owner_with(Arc::new(ApprovingTransport), Arc::new(FrozenClock),)
            .unwrap_err(),
        LastFmAccountIntegrationError::OwnerClaimed
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
