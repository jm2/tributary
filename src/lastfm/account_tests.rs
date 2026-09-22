//! Focused regressions for the Last.fm account composition layer.

use std::sync::Arc;
use std::time::Duration;

use crate::lastfm::account::{
    spawn_lastfm_authorization_owner_with, LastFmAccountIntegrationError,
};
use crate::lastfm::authorization::{
    LastFmAuthorizationClock, LastFmAuthorizationPhase, LastFmAuthorizationTransport,
};
use crate::lastfm::client::{
    DesktopAuthToken, DesktopAuthorizationUrl, DesktopAuthorizedSession, LastFmClientError,
};

const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";
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
