//! Focused regressions for the Last.fm authorization-owner composition:
//! the one process-lifetime owner claim, the consent gate, and the browser
//! handoff URL publication.
//!
//! Split from the former single-file `account_tests.rs` so each module and
//! function stays under the repository's size caps. The owner-claiming
//! scenario stays one sequential test on purpose: the owner claim is a
//! process-lifetime `AtomicBool` (a successful claim is never released), so
//! parallel focused tests could not each claim the owner without racing the
//! test harness. Its scaffolding is extracted into helpers instead, and the
//! one assertion group that needs no owner (the consent policy commit) is a
//! separate focused test.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::lastfm::account::{
    begin_consent_gated_authorization, spawn_lastfm_authorization_owner_with,
    LastFmAccountAuthorizationError, LastFmAccountIntegrationError,
};
use crate::lastfm::authorization::{
    LastFmAuthorizationChallenge, LastFmAuthorizationClock, LastFmAuthorizationPhase,
    LastFmAuthorizationTransport,
};
use crate::lastfm::client::{
    DesktopAuthToken, DesktopAuthorizationUrl, DesktopAuthorizedSession, LastFmClientError,
};
use crate::lastfm::policy::{
    commit_policy_update, LastFmConsentRecord, LastFmPolicyGeneration, LastFmPolicyUpdate,
};

use super::test_support::{fixture_auth_url, SESSION_KEY};

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
        DesktopAuthorizationUrl::for_test(&fixture_auth_url())
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

/// A live policy generation with an accepting consent record committed.
async fn consented_generation() -> LastFmPolicyGeneration {
    let db = super::test_support::account_database().await;
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
    consented
}

/// Poll until the owner publishes the challenge URL; the publication races
/// the owner's own request-token resolution on its spawned task.
async fn published_challenge_url(challenge: &LastFmAuthorizationChallenge) -> Option<String> {
    for _ in 0..200 {
        match challenge.authorization_url() {
            Ok(published) => return Some(published),
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
    None
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

    let consented = consented_generation().await;
    let (challenge, url) = begin_consent_gated_authorization(&handle, &consented)
        .await
        .expect("consented authorization begins");
    assert!(url.starts_with("https://www.last.fm/api/auth/"));
    assert!(url.contains("api_key="));
    assert!(url.contains("token="));

    // The owner publishes the URL once the request token resolves.
    let url_published = published_challenge_url(&challenge).await;
    assert_eq!(
        url_published.as_deref(),
        Some(url.as_str()),
        "the handoff URL must be the exact current challenge's URL"
    );
    assert_eq!(
        handle.subscribe_status().borrow().phase,
        LastFmAuthorizationPhase::AwaitingApproval
    );

    drop(challenge);
    drop(shutdown);
}

#[tokio::test]
async fn consented_policy_commits_and_enables_the_integration() {
    let consented = consented_generation().await;
    assert!(consented.consented_and_enabled());
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
