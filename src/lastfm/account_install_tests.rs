//! Vault install regressions for the Last.fm account composition layer.
//!
//! Split verbatim from the former single-file `account_tests.rs` so each
//! module stays under the repository's per-file size cap; the shared
//! fixtures live in `account_test_support`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::lastfm::account::{
    install_fresh_account, install_same_account_reauthorization, load_vault_account,
    stage_account_install_decision, LastFmAccountInstallDecision, LastFmAccountInstallError,
};
use crate::lastfm::credentials::{CredentialError, SessionCredentialStore};
use crate::lastfm::lifecycle::acquire_vault_lifecycle;

use super::test_support::{
    staged_session, MemoryVault, VaultState, RENEWED_SESSION_KEY, SESSION_KEY,
};

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
