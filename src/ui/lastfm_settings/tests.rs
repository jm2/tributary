//! Surface mapping for every owner phase and stored-account state.

use super::*;
use crate::lastfm::runtime::LastFmRuntimeStatus;

fn status(
    phase: LastFmApplicationPhase,
    failure: Option<LastFmApplicationCommandError>,
    runtime: Option<LastFmRuntimePhase>,
) -> LastFmApplicationStatus {
    LastFmApplicationStatus {
        revision: 1,
        phase,
        failure,
        runtime: runtime.map(|phase| LastFmRuntimeStatus {
            revision: 1,
            phase,
            pending_scrobbles: 0,
            accepted_scrobbles: 0,
            ignored_scrobbles: 0,
            rejected_scrobbles: 0,
            failure: None,
        }),
    }
}

fn named_account() -> StoredAccount {
    StoredAccount::Named("listener".to_owned())
}

fn offered(surface: &Surface) -> (Option<Action>, bool) {
    (surface.action, surface.disconnect)
}

#[test]
fn unavailable_starting_and_stopped_owners_offer_no_action() {
    for phase in [
        LastFmApplicationPhase::UnavailableBuild,
        LastFmApplicationPhase::AwaitingDatabase,
        LastFmApplicationPhase::Starting,
        LastFmApplicationPhase::Failed,
        LastFmApplicationPhase::Stopped,
    ] {
        let surface = surface(&status(phase, None, None), &named_account());
        assert_eq!(offered(&surface), (None, false), "{phase:?}");
    }
}

#[test]
fn dormant_owner_connects_resumes_or_offers_the_explicit_discard() {
    let dormant = |failure| status(LastFmApplicationPhase::AwaitingConsent, failure, None);
    let empty = surface(&dormant(None), &StoredAccount::None);
    assert_eq!(offered(&empty), (Some(Action::Connect), false));
    let stored = surface(&dormant(None), &named_account());
    assert_eq!(offered(&stored), (Some(Action::Resume), false));
    let quarantined = surface(
        &dormant(Some(LastFmApplicationCommandError::QuarantinedQueue)),
        &StoredAccount::None,
    );
    assert_eq!(offered(&quarantined), (None, true));
    let locked = surface(&dormant(None), &StoredAccount::Unreadable);
    assert_eq!(offered(&locked), (None, false));
}

#[test]
fn active_owner_offers_disconnect_and_reconnect_only_when_revoked() {
    let active = |runtime| status(LastFmApplicationPhase::Active, None, Some(runtime));
    let running = surface(&active(LastFmRuntimePhase::Active), &named_account());
    assert_eq!(offered(&running), (None, true));
    assert!(running.title.contains("listener"));
    let revoked = surface(
        &active(LastFmRuntimePhase::ReauthenticationRequired),
        &named_account(),
    );
    assert_eq!(offered(&revoked), (Some(Action::Reconnect), true));
    let cleanup = surface(
        &active(LastFmRuntimePhase::CredentialCleanup),
        &named_account(),
    );
    assert_eq!(offered(&cleanup), (None, true));
    let locked = surface(
        &active(LastFmRuntimePhase::Active),
        &StoredAccount::Unreadable,
    );
    assert_eq!(offered(&locked), (None, true));
}
