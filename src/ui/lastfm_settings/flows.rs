//! Surface flow futures for the Last.fm settings surface: connect,
//! browser handoff, finish, and disconnect.
//!
//! Split verbatim from the former single-file `lastfm_settings.rs` so each
//! module stays under the repository's per-file and per-function size caps;
//! the dialog builders these flows present live in the sibling `dialogs`
//! module, and the composition entry points (`accept_disclosure`,
//! `begin_connect`, `finish_connect`) stay on the parent module.

use std::future::Future;
use std::sync::{Arc, Mutex};

use adw::prelude::*;
use gtk::glib;

use crate::lastfm::account::{
    disconnect_and_purge, load_vault_account, LastFmAccountDisconnectError,
};
use crate::lastfm::authorization::LastFmAuthorizationChallenge;
use crate::lastfm::policy::LastFmPolicyGeneration;

use super::dialogs::{
    connect_failure_message, present_disclosure_dialog, present_disconnect_confirmation,
    present_finish_dialog,
};
use super::{
    accept_disclosure, begin_connect, finish_connect, ConnectFailure, LastFmSettingsContext,
    LastFmSettingsState,
};

/// The interactive pieces of one Last.fm preferences row, shared by the
/// flow handlers so every path can refresh the truthful state.
#[derive(Clone)]
pub(super) struct SurfaceWidgets {
    pub(super) row: adw::ActionRow,
    pub(super) connect_btn: gtk::Button,
    pub(super) disconnect_btn: gtk::Button,
}

/// Paint the row from the current vault truth.
///
/// A detached context (unavailable build) disables both actions; a missing
/// vault record offers Connect; a connected record offers Disconnect; an
/// unreadable vault disables everything. No plaintext ever leaves the vault
/// into configuration — the username is display-only.
///
/// Plain `fn` returning the future: the future owns `!Send` GTK widgets and
/// is only ever awaited on the main context (`future_not_send` convention).
// The returned future owns !Send GTK widgets by construction; every await
// site in this module runs on the main context (spawn_local / test
// block_on), never a multithreaded executor (see album_art.rs's note).
#[allow(clippy::future_not_send)]
pub(super) fn refresh_surface_future(
    widgets: &SurfaceWidgets,
    context: &LastFmSettingsContext,
) -> impl Future<Output = ()> + 'static {
    let widgets = widgets.clone();
    let context = context.clone();
    async move {
        let Some(state) = context.snapshot() else {
            widgets
                .row
                .set_title(rust_i18n::t!("lastfm.status_unavailable").as_ref());
            widgets.connect_btn.set_sensitive(false);
            widgets.disconnect_btn.set_visible(false);
            return;
        };
        match load_vault_account(Arc::clone(&state.credentials)).await {
            Ok(snapshot) => match snapshot.username() {
                Some(username) => {
                    widgets.row.set_title(
                        rust_i18n::t!("lastfm.connected_as", username = username).as_ref(),
                    );
                    widgets.connect_btn.set_sensitive(false);
                    widgets.disconnect_btn.set_visible(true);
                    widgets.disconnect_btn.set_sensitive(true);
                }
                None => {
                    widgets
                        .row
                        .set_title(rust_i18n::t!("lastfm.status_not_connected").as_ref());
                    widgets.connect_btn.set_sensitive(true);
                    widgets.disconnect_btn.set_visible(false);
                }
            },
            Err(_) => {
                widgets
                    .row
                    .set_title(rust_i18n::t!("lastfm.status_vault_unavailable").as_ref());
                widgets.connect_btn.set_sensitive(false);
                widgets.disconnect_btn.set_visible(false);
            }
        }
    }
}

pub(super) fn set_flow_busy(widgets: &SurfaceWidgets, busy: bool) {
    widgets.connect_btn.set_sensitive(!busy);
    widgets.disconnect_btn.set_sensitive(!busy);
}

/// Await an `adw::AlertDialog` response from a main-thread flow. The dialog
/// presents itself through its builder helpers; this only bridges the
/// user's choice back into the awaiting flow.
///
/// Plain `fn` returning the future (`future_not_send` convention: the
/// future owns `!Send` GTK widgets; `try_send` because the GTK callback is
/// sync and an unawaited `send` future would deliver nothing).
// The returned future owns !Send GTK widgets by construction; every await
// site in this module runs on the main context (spawn_local / test
// block_on), never a multithreaded executor (see album_art.rs's note).
#[allow(clippy::future_not_send)]
pub(super) fn dialog_response_future(
    dialog: &adw::AlertDialog,
    parent: &adw::ApplicationWindow,
) -> impl Future<Output = String> + 'static {
    let dialog = dialog.clone();
    let parent = parent.clone();
    async move {
        let (tx, rx) = async_channel::bounded::<String>(1);
        dialog.choose(
            Some(&parent),
            None::<&gtk::gio::Cancellable>,
            move |response| {
                let _ = tx.try_send(response.to_string());
            },
        );
        rx.recv().await.unwrap_or_default()
    }
}

/// Launch the system browser for the consent-gated handoff.
///
/// The URL is handed to the platform's URI handler exactly once through
/// `gtk::UriLauncher`; the copyable-field fallback only presents when no
/// handler consumed the URL. The URL never reaches logs or diagnostics.
///
/// Plain `fn` returning the future (not an `async fn`) per the crate's
/// `future_not_send` convention: the future owns `!Send` GTK widgets and is
/// only ever awaited inside a `spawn_local` block on the main context.
// The returned future owns !Send GTK widgets by construction; every await
// site in this module runs on the main context (spawn_local / test
// block_on), never a multithreaded executor (see album_art.rs's note).
#[allow(clippy::future_not_send)]
fn launch_browser_future(
    parent: &adw::ApplicationWindow,
    url: &str,
) -> impl Future<Output = ()> + 'static {
    let parent = parent.clone();
    let url = url.to_owned();
    async move {
        let launcher = gtk::UriLauncher::new(&url);
        let (tx, rx) = async_channel::bounded::<Result<(), glib::Error>>(1);
        launcher.launch(
            Some(&parent),
            None::<&gtk::gio::Cancellable>,
            move |result| {
                // try_send, not send: the callback is sync and `send` here is
                // an async fn — an unawaited future would deliver nothing and
                // leave the flow awaiting forever.
                let _ = tx.try_send(result);
            },
        );
        let opened = matches!(rx.recv().await, Ok(Ok(())));
        if !opened {
            let fallback = adw::AlertDialog::builder()
                .heading(rust_i18n::t!("lastfm.disclosure_title").as_ref())
                .body(rust_i18n::t!("lastfm.browser_fallback_body").as_ref())
                .close_response("close")
                .build();
            fallback.add_response("close", rust_i18n::t!("lastfm.close").as_ref());
            let entry = gtk::Entry::builder()
                .text(&url)
                .editable(false)
                .width_chars(72)
                .margin_top(8)
                .margin_bottom(8)
                .build();
            fallback.set_extra_child(Some(&entry));
            fallback.present(Some(&parent));
        }
    }
}

/// Run one consent-gated connect attempt to its end.
///
/// Returns a status message that must outlive the post-flow refresh (fixed
/// classification failures); `None` means the refreshed truth is the right
/// final display. Consent precedes the browser flow: when the live policy
/// generation lacks acceptance, the disclosure is presented first, and
/// dismissing it stores nothing and re-enables the row without a flow.
///
/// Plain `fn` returning the future (`future_not_send` convention: the flow
/// owns `!Send` GTK widgets across dialog awaits and runs only on the main
/// context's `spawn_local`).
// The returned future owns !Send GTK widgets by construction; every await
// site in this module runs on the main context (spawn_local / test
// block_on), never a multithreaded executor (see album_art.rs's note).
#[allow(clippy::future_not_send)]
pub(super) fn connect_flow_future(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
    policy_slot: &Arc<Mutex<LastFmPolicyGeneration>>,
    widgets: &SurfaceWidgets,
) -> impl Future<Output = Option<String>> + 'static {
    let parent = parent.clone();
    let context = context.clone();
    let policy_slot = Arc::clone(policy_slot);
    let widgets = widgets.clone();
    async move {
        widgets
            .row
            .set_title(rust_i18n::t!("lastfm.connecting").as_ref());
        let Some(state) = context.snapshot() else {
            return Some(connect_failure_message(ConnectFailure::Unavailable));
        };
        match begin_connect(&state, &policy_slot).await {
            Ok((challenge, url)) => {
                finish_via_browser_future(&parent, &state, &challenge, &url).await
            }
            Err(ConnectFailure::ConsentRequired) => {
                let response =
                    dialog_response_future(&present_disclosure_dialog(&parent), &parent).await;
                if response != "accept" {
                    // Dismissing stores nothing and leaves the feature off.
                    return Some(rust_i18n::t!("lastfm.status_consent_required").to_string());
                }
                if accept_disclosure(&state, &policy_slot, &rust_i18n::locale())
                    .await
                    .is_err()
                {
                    return Some(rust_i18n::t!("lastfm.status_store_error").to_string());
                }
                match begin_connect(&state, &policy_slot).await {
                    Ok((challenge, url)) => {
                        finish_via_browser_future(&parent, &state, &challenge, &url).await
                    }
                    Err(failure) => Some(connect_failure_message(failure)),
                }
            }
            Err(failure) => Some(connect_failure_message(failure)),
        }
    }
}

/// Hand off to the browser and finish on the user's confirmation.
///
/// Plain `fn` returning the future (`future_not_send` convention).
// The returned future owns !Send GTK widgets by construction; every await
// site in this module runs on the main context (spawn_local / test
// block_on), never a multithreaded executor (see album_art.rs's note).
#[allow(clippy::future_not_send)]
pub(super) fn finish_via_browser_future(
    parent: &adw::ApplicationWindow,
    state: &LastFmSettingsState,
    challenge: &LastFmAuthorizationChallenge,
    url: &str,
) -> impl Future<Output = Option<String>> + 'static {
    let parent = parent.clone();
    let state = state.clone();
    let challenge = challenge.clone();
    let url = url.to_owned();
    async move {
        launch_browser_future(&parent, &url).await;
        let response = dialog_response_future(&present_finish_dialog(&parent), &parent).await;
        if response != "continue" {
            // Cancelled: the owner revokes the superseded challenge; the
            // refreshed vault state is the truthful display.
            return None;
        }
        match finish_connect(&state, &challenge).await {
            Ok(_) => None,
            Err(failure) => Some(connect_failure_message(failure)),
        }
    }
}

/// Run one explicit disconnect-and-purge after its confirmation dialog.
///
/// Plain `fn` returning the future (`future_not_send` convention).
// The returned future owns !Send GTK widgets by construction; every await
// site in this module runs on the main context (spawn_local / test
// block_on), never a multithreaded executor (see album_art.rs's note).
#[allow(clippy::future_not_send)]
pub(super) fn disconnect_flow_future(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> impl Future<Output = Option<String>> + 'static {
    let parent = parent.clone();
    let context = context.clone();
    async move {
        let Some(state) = context.snapshot() else {
            return Some(connect_failure_message(ConnectFailure::Unavailable));
        };
        let dialog = present_disconnect_confirmation(&parent);
        if dialog_response_future(&dialog, &parent).await != "disconnect" {
            return None;
        }
        match disconnect_and_purge(Arc::clone(&state.credentials), state.db.clone()).await {
            // A missing vault record is already the truthful refreshed state.
            Ok(_) | Err(LastFmAccountDisconnectError::VaultMissing) => None,
            Err(LastFmAccountDisconnectError::QueuePurgeRefused) => {
                Some(rust_i18n::t!("lastfm.status_purge_refused").to_string())
            }
            Err(_) => Some(rust_i18n::t!("lastfm.status_vault_unavailable").to_string()),
        }
    }
}
