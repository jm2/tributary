//! Queue-transfer and disconnect regressions for the Last.fm account
//! composition layer.
//!
//! Split verbatim from the former single-file `account_tests.rs` so each
//! module stays under the repository's per-file size cap; the shared
//! fixtures live in `account_test_support`.

use std::sync::Arc;

use crate::lastfm::account::{
    discard_quarantined_queue, install_replacement_account, LastFmAccountReplacementError,
};
use crate::lastfm::credentials::{ProtectedString, SessionCredentialStore, StoredSession};

use super::test_support::{
    account_database, queue_len, seed_queue_rows, staged_session, MemoryVault, RENEWED_SESSION_KEY,
    SESSION_KEY,
};

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

#[tokio::test]
async fn disconnect_purges_queue_then_deletes_the_exact_record() {
    let vault = Arc::new(MemoryVault::default());
    vault.seed("leaving-user", SESSION_KEY);
    let binding = vault.stored_binding();
    let db = account_database().await;
    seed_queue_rows(&db, binding, 4).await;

    let credentials: Arc<dyn SessionCredentialStore> = vault.clone();
    let purged = crate::lastfm::account::disconnect_and_purge(Arc::clone(&credentials), db.clone())
        .await
        .expect("disconnect lands");
    assert_eq!(purged, 4);
    assert_eq!(queue_len(&db).await, 0);
    assert_eq!(vault.stored_username(), None);
    assert_eq!(
        crate::lastfm::account::disconnect_and_purge(credentials, db.clone())
            .await
            .unwrap_err(),
        crate::lastfm::account::LastFmAccountDisconnectError::VaultMissing
    );

    vault.seed("leaving-user", SESSION_KEY);
    seed_queue_rows(&db, binding, 1).await;
    let interloper = StoredSession::new("someone-else", ProtectedString::new(SESSION_KEY))
        .expect("fixture satisfies validation")
        .account_binding();
    seed_queue_rows(&db, interloper, 2).await;
    assert_eq!(
        crate::lastfm::account::disconnect_and_purge(vault.clone(), db)
            .await
            .unwrap_err(),
        crate::lastfm::account::LastFmAccountDisconnectError::QueuePurgeRefused,
        "an interloper binding must freeze the disconnect"
    );
    assert_eq!(vault.stored_username().as_deref(), Some("leaving-user"));
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
