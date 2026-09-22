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
    LastFmAuthorizationClock, LastFmAuthorizationHandle, LastFmAuthorizationShutdown,
    LastFmAuthorizationTransport, SystemLastFmAuthorizationClock,
};
use super::client::{AppCredentials, LastFmClient, LastFmClientError};

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

#[cfg(test)]
#[path = "account_tests.rs"]
mod tests;
