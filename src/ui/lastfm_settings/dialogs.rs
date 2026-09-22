//! Dialog builders and status mapping for the Last.fm settings surface.
//!
//! Split verbatim from the former single-file `lastfm_settings.rs` so each
//! module stays under the repository's per-file and per-function size caps;
//! the flow futures that drive these dialogs live in the sibling `flows`
//! module.

use adw::prelude::*;

use crate::lastfm::account::LastFmAccountInstallError;

use super::ConnectFailure;

/// Present the localized consent disclosure.
///
/// The dialog carries the required disclosure points: what data is sent,
/// how Last.fm may use it, local offline storage until delivery or purge,
/// the independent-scrobbling risk from remote sources, and the external
/// (MPD-style) scrobbler caveat. Accepting records consent and enables the
/// integration; dismissing stores nothing and leaves the feature off.
pub(super) fn present_disclosure_dialog(parent: &adw::ApplicationWindow) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("lastfm.disclosure_title").as_ref())
        .body(rust_i18n::t!("lastfm.disclosure_body").as_ref())
        .close_response("decline")
        .default_response("decline")
        .build();
    dialog.add_response("decline", rust_i18n::t!("lastfm.decline").as_ref());
    dialog.add_response("accept", rust_i18n::t!("lastfm.accept").as_ref());
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.present(Some(parent));
    dialog
}

/// Present the localized "I have approved in the browser" continuation.
pub(super) fn present_finish_dialog(parent: &adw::ApplicationWindow) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("lastfm.finish_title").as_ref())
        .body(rust_i18n::t!("lastfm.finish_body").as_ref())
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_response("cancel", rust_i18n::t!("lastfm.cancel").as_ref());
    dialog.add_response("continue", rust_i18n::t!("lastfm.continue").as_ref());
    dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
    dialog.present(Some(parent));
    dialog
}

/// Present the localized disconnect confirmation. The consequence text
/// names the credential removal and the pending-scrobble discard up front.
pub(super) fn present_disconnect_confirmation(parent: &adw::ApplicationWindow) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("lastfm.disconnect_confirm_title").as_ref())
        .body(rust_i18n::t!("lastfm.disconnect_confirm_body").as_ref())
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_response("cancel", rust_i18n::t!("lastfm.cancel").as_ref());
    dialog.add_response(
        "disconnect",
        rust_i18n::t!("lastfm.disconnect_confirm_accept").as_ref(),
    );
    dialog.set_response_appearance("disconnect", adw::ResponseAppearance::Destructive);
    dialog.present(Some(parent));
    dialog
}

/// Map a connect failure onto its localized status message.
pub(super) fn connect_failure_message(failure: ConnectFailure) -> String {
    match failure {
        ConnectFailure::Unavailable => rust_i18n::t!("lastfm.status_unavailable").to_string(),
        ConnectFailure::ConsentRequired => {
            rust_i18n::t!("lastfm.status_consent_required").to_string()
        }
        ConnectFailure::AuthorizationUnavailable => {
            rust_i18n::t!("lastfm.status_authorization_unavailable").to_string()
        }
        ConnectFailure::ReplacementRequired => {
            rust_i18n::t!("lastfm.status_replacement_required").to_string()
        }
    }
}

/// Translate an install-stage failure for diagnostics-free status display.
pub(super) fn install_failure_is_account_rejection(
    error: &LastFmAccountInstallError,
) -> Option<&'static str> {
    match error {
        LastFmAccountInstallError::VaultAlreadyBound => Some("vault-already-bound"),
        _ => None,
    }
}
