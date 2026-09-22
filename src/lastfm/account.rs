//! Process-wide Last.fm account composition.
//!
//! This is the product layer above the authorization core. It owns the one
//! process-lifetime authorization owner built from the release build's
//! application credentials, the consent-gated browser handoff for the exact
//! current challenge, atomic staged-session vault installation, and the
//! exact same-account versus different-account transition policy.
//!
//! The layer is intentionally GTK-free. The settings surface drives it
//! through its typed asynchronous operations and renders only content-free
//! outcomes and explicitly returned display values (never diagnostics with
//! provider or vault context).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::authorization::{
    LastFmAuthorizationClock, LastFmAuthorizationGrant, LastFmAuthorizationHandle,
    LastFmAuthorizationShutdown, LastFmAuthorizationTransport, SystemLastFmAuthorizationClock,
};
use super::client::{AppCredentials, LastFmClient, LastFmClientError};
use super::credentials::{
    CredentialError, LastFmAccountBinding, SessionCredentialStore, StoredSession,
};
use super::lifecycle::{
    acquire_vault_lifecycle, recover_quarantined_lastfm_queue, LastFmQuarantinedQueueRecoveryError,
    LastFmVaultLifecycleLease,
};
use super::storage::purge_account;

/// One process-lifetime authorization owner; a second construction is a bug.
static AUTHORIZATION_OWNER_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Content-free refusal from account-integration composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmAccountIntegrationError {
    #[error("Last.fm authorization owner is already claimed")]
    OwnerClaimed,
    #[error("Last.fm authorization is unavailable in this build")]
    BuildUnavailable,
}

/// Construct the one process-wide authorization owner from build credentials.
///
/// Missing or malformed build credentials classify the whole integration as
/// [`LastFmAccountIntegrationError::BuildUnavailable`] before any process
/// claim is taken, matching the application owner's fixed unavailable-build
/// classification. A successful claim is never released: exactly one owner
/// exists per process, and a second construction is refused.
pub fn spawn_lastfm_authorization_owner(
) -> Result<(LastFmAuthorizationHandle, LastFmAuthorizationShutdown), LastFmAccountIntegrationError>
{
    let client = AppCredentials::from_build().and_then(LastFmClient::new);
    let transport = match client {
        Ok(client) => Arc::new(client) as Arc<dyn LastFmAuthorizationTransport>,
        Err(
            LastFmClientError::AppCredentialsUnavailable | LastFmClientError::ClientConstruction,
        ) => return Err(LastFmAccountIntegrationError::BuildUnavailable),
        Err(_) => return Err(LastFmAccountIntegrationError::BuildUnavailable),
    };
    spawn_lastfm_authorization_owner_with(
        transport,
        Arc::new(SystemLastFmAuthorizationClock::default()),
    )
}

/// Spawn the claimed authorization owner with injected deterministic seams.
pub(in crate::lastfm) fn spawn_lastfm_authorization_owner_with(
    transport: Arc<dyn LastFmAuthorizationTransport>,
    clock: Arc<dyn LastFmAuthorizationClock>,
) -> Result<(LastFmAuthorizationHandle, LastFmAuthorizationShutdown), LastFmAccountIntegrationError>
{
    AUTHORIZATION_OWNER_CLAIMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| LastFmAccountIntegrationError::OwnerClaimed)?;
    Ok(super::authorization::spawn_lastfm_authorization(
        transport, clock,
    ))
}

/// Presence classification of the current valid vault record.
///
/// The username is an explicitly returned display value for the settings
/// surface; no key material or diagnostics context is exposed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LastFmVaultAccountSnapshot {
    username: Option<String>,
}

impl LastFmVaultAccountSnapshot {
    /// Snapshot of a vault without a valid account record.
    pub const MISSING: Self = Self { username: None };

    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }
}

/// Load the current valid account record for the settings surface.
///
/// This is a display read and does not hold the destructive vault lifecycle
/// lease; the single-record credential save keeps concurrent readers on
/// either the old or the new valid value.
pub async fn load_vault_account(
    credentials: Arc<dyn SessionCredentialStore>,
) -> Result<LastFmVaultAccountSnapshot, CredentialError> {
    tokio::task::spawn_blocking(move || credentials.load())
        .await
        .map_err(|_| CredentialError::Unavailable)?
        .map(|session| LastFmVaultAccountSnapshot {
            username: session.map(|session| session.username().to_owned()),
        })
}

/// Exact transition policy between a staged grant and the current record.
///
/// `FreshInstall` mints a brand-new opaque account identity. Reauthorization
/// preserves the exact stored identity only for byte-identical usernames;
/// anything else is a different-account replacement which the settings
/// surface must confirm with an explicit purge-and-install flow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LastFmAccountInstallDecision {
    FreshInstall,
    SameAccountReauthorization,
    DifferentAccount { existing_username: String },
}

/// Classify how a staged authorization would land in the current vault.
pub async fn stage_account_install_decision(
    credentials: Arc<dyn SessionCredentialStore>,
    staged: &LastFmAuthorizationGrant,
) -> Result<LastFmAccountInstallDecision, CredentialError> {
    let snapshot = load_vault_account(credentials).await?;
    match snapshot.username {
        None => Ok(LastFmAccountInstallDecision::FreshInstall),
        Some(existing) if existing == staged.username() => {
            Ok(LastFmAccountInstallDecision::SameAccountReauthorization)
        }
        Some(existing) => Ok(LastFmAccountInstallDecision::DifferentAccount {
            existing_username: existing,
        }),
    }
}

/// Content-free vault installation failures.
///
/// No variant carries the staged username, the stored username, or native
/// credential-store context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmAccountInstallError {
    #[error("protected credential store is unavailable")]
    CredentialStoreUnavailable,
    #[error("staged Last.fm authorization is invalid for vault installation")]
    InvalidStagedSession,
    #[error("a valid Last.fm account appeared before installation")]
    VaultAlreadyBound,
    #[error("the Last.fm account vanished before reauthorization")]
    VaultMissing,
    #[error("the new Last.fm authorization belongs to a different account")]
    ExactAccountRefused,
}

/// A durably installed account, safe to surface in settings.
#[derive(Clone, Debug)]
pub struct InstalledAccount {
    username: String,
    account_binding: LastFmAccountBinding,
}

impl InstalledAccount {
    pub fn username(&self) -> &str {
        &self.username
    }

    pub const fn account_binding(&self) -> LastFmAccountBinding {
        self.account_binding
    }
}

/// Load the vault record while provably holding the lifecycle lease.
///
/// The lease moves through each blocking task, so cancelling this future
/// cannot release the lease between inspection and a subsequent destructive
/// step.
async fn load_under_lease(
    credentials: Arc<dyn SessionCredentialStore>,
    lease: LastFmVaultLifecycleLease,
) -> Result<(LastFmVaultLifecycleLease, Option<StoredSession>), LastFmAccountInstallError> {
    let (lease, loaded) = tokio::task::spawn_blocking(move || {
        let loaded = credentials.load();
        (lease, loaded)
    })
    .await
    .map_err(|_| LastFmAccountInstallError::CredentialStoreUnavailable)?;
    let loaded = loaded.map_err(|_| LastFmAccountInstallError::CredentialStoreUnavailable)?;
    Ok((lease, loaded))
}

/// Save a vault record while provably holding the lifecycle lease.
async fn save_under_lease(
    credentials: Arc<dyn SessionCredentialStore>,
    lease: LastFmVaultLifecycleLease,
    session: StoredSession,
) -> Result<(), LastFmAccountInstallError> {
    let (_lease, saved) = tokio::task::spawn_blocking(move || {
        let saved = credentials.save(&session);
        (lease, saved)
    })
    .await
    .map_err(|_| LastFmAccountInstallError::CredentialStoreUnavailable)?;
    saved.map_err(|_| LastFmAccountInstallError::CredentialStoreUnavailable)
}

/// Install a staged grant as a brand-new account identity.
///
/// The lifecycle lease is held across the race re-verification and the
/// durable save, so a runtime successor cannot adopt the vault in between.
/// A valid record observed under the lease refuses the install untouched.
pub async fn install_fresh_account(
    credentials: Arc<dyn SessionCredentialStore>,
    staged: LastFmAuthorizationGrant,
) -> Result<InstalledAccount, LastFmAccountInstallError> {
    let lease = acquire_vault_lifecycle().await;
    let (staged_username, key) = staged.into_authorized_session().into_parts();
    let session = StoredSession::new(staged_username.as_str(), key)
        .map_err(|_| LastFmAccountInstallError::InvalidStagedSession)?;
    let (lease, current) = load_under_lease(Arc::clone(&credentials), lease).await?;
    if current.is_some() {
        return Err(LastFmAccountInstallError::VaultAlreadyBound);
    }
    let account = InstalledAccount {
        username: session.username().to_owned(),
        account_binding: session.account_binding(),
    };
    save_under_lease(credentials, lease, session).await?;
    Ok(account)
}

/// Reauthorize the exact stored account with a fresh session key.
///
/// The staged grant must carry the byte-identical stored username; the
/// exact-account guard inside [`StoredSession::reauthorized`] is the
/// authoritative check and preserves the stored opaque identity, so the
/// scrobble queue and all durable bindings stay valid.
pub async fn install_same_account_reauthorization(
    credentials: Arc<dyn SessionCredentialStore>,
    staged: LastFmAuthorizationGrant,
) -> Result<InstalledAccount, LastFmAccountInstallError> {
    let lease = acquire_vault_lifecycle().await;
    let (staged_username, key) = staged.into_authorized_session().into_parts();
    let (lease, existing) = load_under_lease(Arc::clone(&credentials), lease).await?;
    let existing = existing.ok_or(LastFmAccountInstallError::VaultMissing)?;
    let session = existing
        .reauthorized(staged_username.as_str(), key)
        .map_err(|_| LastFmAccountInstallError::ExactAccountRefused)?;
    let account = InstalledAccount {
        username: session.username().to_owned(),
        account_binding: session.account_binding(),
    };
    save_under_lease(credentials, lease, session).await?;
    Ok(account)
}

/// Content-free different-account replacement failures.
///
/// No variant carries either username, the queue length, or native
/// credential-store context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmAccountReplacementError {
    #[error("Last.fm protected credential store is unavailable")]
    CredentialStoreUnavailable,
    #[error("staged Last.fm authorization is invalid for vault installation")]
    InvalidStagedSession,
    #[error("the replaced Last.fm account vanished before the replacement")]
    VaultMissing,
    #[error("the old account's pending scrobbles could not be discarded")]
    QueuePurgeRefused,
    #[error("the old protected account record could not be deleted")]
    CredentialCleanupRequired,
}

/// Replace the stored account with a different account after explicit
/// consent in the settings surface.
///
/// The lifecycle lease is held across the whole ordered transition:
/// transactionally purge the old account's pending scrobbles (they can
/// never transfer across accounts), delete the exact old protected record,
/// and only then install the brand-new identity. A failure leaves the
/// already-valid old record in place; the purge is idempotent, so a retry
/// converges.
pub async fn install_replacement_account(
    credentials: Arc<dyn SessionCredentialStore>,
    db: sea_orm::DatabaseConnection,
    staged: LastFmAuthorizationGrant,
) -> Result<InstalledAccount, LastFmAccountReplacementError> {
    use LastFmAccountReplacementError as Error;

    let lease = acquire_vault_lifecycle().await;
    let (staged_username, key) = staged.into_authorized_session().into_parts();
    let session = StoredSession::new(staged_username.as_str(), key)
        .map_err(|_| Error::InvalidStagedSession)?;
    let (lease, existing) = load_under_lease(Arc::clone(&credentials), lease)
        .await
        .map_err(|_| Error::CredentialStoreUnavailable)?;
    let existing = existing.ok_or(Error::VaultMissing)?;

    purge_account(&db, existing.account_binding())
        .await
        .map_err(|_| Error::QueuePurgeRefused)?;

    let deleter = Arc::clone(&credentials);
    let (lease, deleted) = tokio::task::spawn_blocking(move || {
        let deleted = deleter.delete();
        (lease, deleted)
    })
    .await
    .map_err(|_| Error::CredentialCleanupRequired)?;
    deleted.map_err(|_| Error::CredentialCleanupRequired)?;

    let account = InstalledAccount {
        username: session.username().to_owned(),
        account_binding: session.account_binding(),
    };
    save_under_lease(credentials, lease, session)
        .await
        .map_err(|_| Error::CredentialStoreUnavailable)?;
    Ok(account)
}

/// Explicitly discard a quarantined queue which cannot be associated with a
/// loadable account record.
///
/// Thin product wrapper over the lifecycle recovery: it refuses while a
/// valid account exists and otherwise purges the orphaned private rows
/// under the same lifecycle lease as every destructive vault operation.
pub async fn discard_quarantined_queue(
    credentials: Arc<dyn SessionCredentialStore>,
    db: sea_orm::DatabaseConnection,
) -> Result<u64, LastFmQuarantinedQueueRecoveryError> {
    match recover_quarantined_lastfm_queue(db, credentials).await {
        Ok(recovery) => Ok(recovery.purged_scrobbles()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[path = "account_tests.rs"]
mod tests;
