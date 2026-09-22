//! Last.fm settings surface: consent disclosure, account actions, and the
//! consent-gated browser handoff.
//!
//! Everything here sits on top of the `crate::lastfm::account` composition
//! layer. The UI never sees native credential-store, HTTP, XML/JSON, or
//! queue details: the account layer's content-free errors are the only
//! failure vocabulary, and diagnostics keep the sanitized classification.
//!
//! Composition rules honored by this module:
//! - Consent precedes the browser flow. The disclosure must be accepted in
//!   the live policy generation before any authorization request starts.
//! - The browser URL is used exactly once for the system-browser launch,
//!   with a copy fallback; it never reaches logs or diagnostics.
//! - Dismissing the disclosure stores nothing and leaves the feature off.
//! - The connected username comes from the credential vault, never from
//!   plaintext configuration.

use std::sync::{Arc, Mutex};

use adw::prelude::*;
use gtk::glib;

use crate::lastfm::account::{
    begin_consent_gated_authorization, disconnect_and_purge, install_fresh_account,
    install_same_account_reauthorization, load_vault_account, stage_account_install_decision,
    LastFmAccountAuthorizationError, LastFmAccountDisconnectError, LastFmAccountInstallDecision,
    LastFmAccountInstallError,
};
use crate::lastfm::authorization::{LastFmAuthorizationChallenge, LastFmAuthorizationHandle};
use crate::lastfm::credentials::SessionCredentialStore;
use crate::lastfm::policy::{
    commit_policy_update, lock_policy_slot, LastFmConsentRecord, LastFmPolicyGeneration,
    LastFmPolicyStoreError, LastFmPolicyUpdate,
};

/// The revision of the shipped disclosure text this build presents. Bump it
/// whenever the disclosure's meaning changes so re-acceptance is required.
pub const DISCLOSURE_REVISION: u32 = 1;

/// Shared, late-binding composition context for the settings surface.
///
/// The database only exists after the engine task's asynchronous
/// initialization, so the handles arrive through this slot instead of being
/// threaded through widget constructors. `None` means the surface renders
/// its unavailable state and keeps every mutating action disabled.
#[derive(Clone, Default)]
pub struct LastFmSettingsContext {
    pub state: Arc<Mutex<Option<LastFmSettingsState>>>,
}

/// The live composition handles the account actions need.
#[derive(Clone)]
pub struct LastFmSettingsState {
    pub db: sea_orm::DatabaseConnection,
    pub credentials: Arc<dyn SessionCredentialStore>,
    pub authorization: LastFmAuthorizationHandle,
}

impl LastFmSettingsContext {
    /// Install the composition handles once the database has attached.
    pub fn set_state(&self, state: LastFmSettingsState) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
    }

    fn snapshot(&self) -> Option<LastFmSettingsState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Content-free connect-flow failures for the settings status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectFailure {
    /// The composition context is not attached yet.
    Unavailable,
    /// The live policy generation lacks recorded consent or enablement.
    ConsentRequired,
    /// The authorization owner refused or lost the flow.
    AuthorizationUnavailable,
    /// A different account is connected; replacement must be explicit.
    ReplacementRequired,
}

impl From<LastFmAccountAuthorizationError> for ConnectFailure {
    fn from(error: LastFmAccountAuthorizationError) -> Self {
        match error {
            LastFmAccountAuthorizationError::ConsentRequired => Self::ConsentRequired,
            LastFmAccountAuthorizationError::AuthorizationUnavailable => {
                Self::AuthorizationUnavailable
            }
        }
    }
}

/// Record the disclosure acceptance and enable the integration.
///
/// Runs the validated policy commit against the attached database and, on
/// success, publishes the successor generation into the shared UI slot. The
/// remote-source set starts empty: every remote catalogue source begins
/// excluded, and per-source opt-in is a separate, later choice.
pub async fn accept_disclosure(
    state: &LastFmSettingsState,
    policy_slot: &Arc<Mutex<LastFmPolicyGeneration>>,
    locale: &str,
) -> Result<LastFmPolicyGeneration, LastFmPolicyStoreError> {
    let observed = lock_policy_slot(policy_slot).generation();
    let consent = LastFmConsentRecord::try_new(locale, DISCLOSURE_REVISION)
        .map_err(|_| LastFmPolicyStoreError::InvalidUpdate)?;
    let update = LastFmPolicyUpdate {
        consent: Some(consent),
        enabled: true,
        enabled_remote_sources: std::collections::HashSet::new(),
    };
    let successor = commit_policy_update(&state.db, observed, update).await?;
    *lock_policy_slot(policy_slot) = successor.clone();
    Ok(successor)
}

/// Begin the consent-gated connect flow and open the system browser.
///
/// Returns the exact challenge the user's browser approval will finish.
pub async fn begin_connect(
    state: &LastFmSettingsState,
    policy_slot: &Arc<Mutex<LastFmPolicyGeneration>>,
) -> Result<(LastFmAuthorizationChallenge, String), ConnectFailure> {
    let policy = lock_policy_slot(policy_slot).clone();
    begin_consent_gated_authorization(&state.authorization, &policy)
        .await
        .map_err(ConnectFailure::from)
}

/// Launch the system browser for the consent-gated handoff.
///
/// The URL is handed to the platform's URI handler exactly once through
/// `gtk::UriLauncher`; the copyable-field fallback only presents when no
/// handler consumed the URL. The URL never reaches logs or diagnostics.
pub async fn launch_browser(parent: &adw::ApplicationWindow, url: &str) {
    let launcher = gtk::UriLauncher::new(url);
    let (tx, rx) = async_channel::bounded::<Result<(), glib::Error>>(1);
    launcher.launch(Some(parent), None::<&gtk::gio::Cancellable>, move |result| {
        let _ = tx.send(result);
    });
    let opened = matches!(rx.recv().await, Ok(Ok(())));
    if !opened {
        let fallback = adw::AlertDialog::builder()
            .heading(rust_i18n::t!("lastfm.disclosure_title").as_ref())
            .body(rust_i18n::t!("lastfm.browser_fallback_body").as_ref())
            .close_response("close")
            .build();
        fallback.add_response("close", rust_i18n::t!("lastfm.close").as_ref());
        let entry = gtk::Entry::builder()
            .text(url)
            .editable(false)
            .width_chars(72)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        fallback.set_extra_child(Some(&entry));
        fallback.present(Some(parent));
    }
}

/// Finish a connect flow: exchange the browser approval and install the
/// account atomically according to the current vault state.
///
/// Returns the exact connected username on success. A different-account
/// vault refuses with [`ConnectFailure::ReplacementRequired`]; replacement
/// is the explicit disconnect-and-reconnect path, never an implicit
/// consequence of Connect.
pub async fn finish_connect(
    state: &LastFmSettingsState,
    challenge: &LastFmAuthorizationChallenge,
) -> Result<String, ConnectFailure> {
    let grant = state
        .authorization
        .try_finish(challenge)
        .map_err(|_| ConnectFailure::AuthorizationUnavailable)?
        .wait()
        .await
        .map_err(|_| ConnectFailure::AuthorizationUnavailable)?;
    let decision = stage_account_install_decision(Arc::clone(&state.credentials), &grant)
        .await
        .map_err(|_| ConnectFailure::AuthorizationUnavailable)?;
    match decision {
        LastFmAccountInstallDecision::FreshInstall => {
            let account = install_fresh_account(Arc::clone(&state.credentials), grant)
                .await
                .map_err(|error| {
                    if install_failure_is_account_rejection(&error).is_some() {
                        ConnectFailure::ReplacementRequired
                    } else {
                        ConnectFailure::AuthorizationUnavailable
                    }
                })?;
            Ok(account.username().to_owned())
        }
        LastFmAccountInstallDecision::SameAccountReauthorization => {
            let account =
                install_same_account_reauthorization(Arc::clone(&state.credentials), grant)
                    .await
                    .map_err(|error| {
                        if install_failure_is_account_rejection(&error).is_some() {
                            ConnectFailure::ReplacementRequired
                        } else {
                            ConnectFailure::AuthorizationUnavailable
                        }
                    })?;
            Ok(account.username().to_owned())
        }
        LastFmAccountInstallDecision::DifferentAccount {
            existing_username: _,
        } => Err(ConnectFailure::ReplacementRequired),
    }
}

/// Present the localized consent disclosure.
///
/// The dialog carries the required disclosure points: what data is sent,
/// how Last.fm may use it, local offline storage until delivery or purge,
/// the independent-scrobbling risk from remote sources, and the external
/// (MPD-style) scrobbler caveat. Accepting records consent and enables the
/// integration; dismissing stores nothing and leaves the feature off.
pub fn present_disclosure_dialog(parent: &adw::ApplicationWindow) -> adw::AlertDialog {
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
pub fn present_finish_dialog(parent: &adw::ApplicationWindow) -> adw::AlertDialog {
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

/// Map a connect failure onto its localized status message.
pub fn connect_failure_message(failure: ConnectFailure) -> String {
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
pub fn install_failure_is_account_rejection(
    error: &LastFmAccountInstallError,
) -> Option<&'static str> {
    match error {
        LastFmAccountInstallError::VaultAlreadyBound => Some("vault-already-bound"),
        _ => None,
    }
}

// ── Preferences group ───────────────────────────────────────────────────

/// The interactive pieces of one Last.fm preferences row, shared by the
/// flow handlers so every path can refresh the truthful state.
#[derive(Clone)]
struct SurfaceWidgets {
    row: adw::ActionRow,
    connect_btn: gtk::Button,
    disconnect_btn: gtk::Button,
}

/// Paint the row from the current vault truth.
///
/// A detached context (unavailable build) disables both actions; a missing
/// vault record offers Connect; a connected record offers Disconnect; an
/// unreadable vault disables everything. No plaintext ever leaves the vault
/// into configuration — the username is display-only.
async fn refresh_surface(widgets: &SurfaceWidgets, context: &LastFmSettingsContext) {
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
                widgets
                    .row
                    .set_title(rust_i18n::t!("lastfm.connected_as", username = username).as_ref());
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

fn set_flow_busy(widgets: &SurfaceWidgets, busy: bool) {
    widgets.connect_btn.set_sensitive(!busy);
    widgets.disconnect_btn.set_sensitive(!busy);
}

/// Await an `adw::AlertDialog` response from async context. The dialog
/// presents itself through its builder helpers; this only bridges the
/// user's choice back into the awaiting flow.
async fn await_dialog_response(
    dialog: &adw::AlertDialog,
    parent: &adw::ApplicationWindow,
) -> String {
    let (tx, rx) = async_channel::bounded::<String>(1);
    let dialog = dialog.clone();
    dialog.choose(Some(parent), None::<&gtk::gio::Cancellable>, move |response| {
        let _ = tx.send(response.to_string());
    });
    rx.recv().await.unwrap_or_default()
}

/// Run one consent-gated connect attempt to its end.
///
/// Returns a status message that must outlive the post-flow refresh (fixed
/// classification failures); `None` means the refreshed truth is the right
/// final display. Consent precedes the browser flow: when the live policy
/// generation lacks acceptance, the disclosure is presented first, and
/// dismissing it stores nothing and re-enables the row without a flow.
async fn run_connect_flow(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
    policy_slot: &Arc<Mutex<LastFmPolicyGeneration>>,
    widgets: &SurfaceWidgets,
) -> Option<String> {
    widgets.row.set_title(rust_i18n::t!("lastfm.connecting").as_ref());
    let Some(state) = context.snapshot() else {
        return Some(connect_failure_message(ConnectFailure::Unavailable));
    };
    match begin_connect(&state, policy_slot).await {
        Ok((challenge, url)) => finish_via_browser(parent, &state, &challenge, &url).await,
        Err(ConnectFailure::ConsentRequired) => {
            let response =
                await_dialog_response(&present_disclosure_dialog(parent), parent).await;
            if response != "accept" {
                // Dismissing stores nothing and leaves the feature off.
                return Some(rust_i18n::t!("lastfm.status_consent_required").to_string());
            }
            if accept_disclosure(&state, policy_slot, &rust_i18n::locale().to_string())
                .await
                .is_err()
            {
                return Some(rust_i18n::t!("lastfm.status_store_error").to_string());
            }
            match begin_connect(&state, policy_slot).await {
                Ok((challenge, url)) => {
                    finish_via_browser(parent, &state, &challenge, &url).await
                }
                Err(failure) => Some(connect_failure_message(failure)),
            }
        }
        Err(failure) => Some(connect_failure_message(failure)),
    }
}

/// Hand off to the browser and finish on the user's confirmation.
async fn finish_via_browser(
    parent: &adw::ApplicationWindow,
    state: &LastFmSettingsState,
    challenge: &LastFmAuthorizationChallenge,
    url: &str,
) -> Option<String> {
    launch_browser(parent, url).await;
    let response = await_dialog_response(&present_finish_dialog(parent), parent).await;
    if response != "continue" {
        // Cancelled: the owner revokes the superseded challenge; the
        // refreshed vault state is the truthful display.
        return None;
    }
    match finish_connect(state, challenge).await {
        Ok(_) => None,
        Err(failure) => Some(connect_failure_message(failure)),
    }
}

/// Run one explicit disconnect-and-purge after its confirmation dialog.
async fn run_disconnect_flow(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> Option<String> {
    let Some(state) = context.snapshot() else {
        return Some(connect_failure_message(ConnectFailure::Unavailable));
    };
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
    if await_dialog_response(&dialog, parent).await != "disconnect" {
        return None;
    }
    match disconnect_and_purge(Arc::clone(&state.credentials), state.db.clone()).await {
        // A missing vault record is already the truthful refreshed state.
        Ok(_) | Err(LastFmAccountDisconnectError::VaultMissing) => None,
        Err(LastFmAccountDisconnectError::QueuePurgeRefused) => {
            Some(rust_i18n::t!("lastfm.status_purge_refused").to_string())
        }
        Err(_) => {
            Some(rust_i18n::t!("lastfm.status_vault_unavailable").to_string())
        }
    }
}

/// Build the Last.fm scrobbling group for the preferences page.
///
/// The row carries the content-free status line plus the Connect and
/// Disconnect actions. Nothing on this surface starts an authorization
/// request, a durable queue insertion, or a scrobble until the localized
/// disclosure has been accepted in the live policy generation.
pub fn build_lastfm_group(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
    policy_slot: &Arc<Mutex<LastFmPolicyGeneration>>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("lastfm.group_title").as_ref())
        .description(rust_i18n::t!("lastfm.group_description").as_ref())
        .build();

    let row = adw::ActionRow::builder()
        .title(rust_i18n::t!("lastfm.status_not_connected").as_ref())
        .build();
    let connect_btn = gtk::Button::builder()
        .label(rust_i18n::t!("lastfm.connect").as_ref())
        .css_classes(["suggested-action"])
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();
    let disconnect_btn = gtk::Button::builder()
        .label(rust_i18n::t!("lastfm.disconnect").as_ref())
        .css_classes(["destructive-action"])
        .valign(gtk::Align::Center)
        .visible(false)
        .sensitive(false)
        .build();
    row.add_suffix(&connect_btn);
    row.add_suffix(&disconnect_btn);
    group.add(&row);

    let widgets = SurfaceWidgets {
        row,
        connect_btn,
        disconnect_btn,
    };

    {
        // Connect: consent gate first, then the browser handoff and the
        // one-shot finish. The row is refreshed from vault truth after
        // every path so a flow that ends without installing (dismissed
        // disclosure, cancelled approval) still shows a live state.
        let context = context.clone();
        let policy_slot = Arc::clone(policy_slot);
        let parent = parent.clone();
        let widgets = widgets.clone();
        let connect_signal = widgets.connect_btn.clone();
        connect_signal.connect_clicked(move |_| {
            let context = context.clone();
            let policy_slot = Arc::clone(&policy_slot);
            let parent = parent.clone();
            let widgets = widgets.clone();
            glib::MainContext::default().spawn_local(async move {
                set_flow_busy(&widgets, true);
                let persistent =
                    run_connect_flow(&parent, &context, &policy_slot, &widgets).await;
                refresh_surface(&widgets, &context).await;
                if let Some(message) = persistent {
                    widgets.row.set_title(message.as_str());
                }
            });
        });
    }
    {
        // Disconnect: explicit confirmation; the consequence text names the
        // credential removal and the pending-scrobble discard up front.
        let context = context.clone();
        let parent = parent.clone();
        let widgets = widgets.clone();
        let disconnect_signal = widgets.disconnect_btn.clone();
        disconnect_signal.connect_clicked(move |_| {
            let context = context.clone();
            let parent = parent.clone();
            let widgets = widgets.clone();
            glib::MainContext::default().spawn_local(async move {
                set_flow_busy(&widgets, true);
                let persistent = run_disconnect_flow(&parent, &context).await;
                refresh_surface(&widgets, &context).await;
                if let Some(message) = persistent {
                    widgets.row.set_title(message.as_str());
                }
            });
        });
    }
    {
        // Initial truthful paint (the detached/unavailable build renders
        // its disabled classification before any interaction).
        let context = context.clone();
        let widgets = widgets.clone();
        glib::MainContext::default().spawn_local(async move {
            refresh_surface(&widgets, &context).await;
        });
    }

    group
}

#[cfg(test)]
pub mod widget_tests {
    use super::*;

    /// A detached context (no packaged application credentials) renders the
    /// disabled unavailable classification: neither account action can
    /// start a flow and the row carries the content-free status text.
    pub fn detached_context_renders_the_unavailable_surface() {
        let widgets = SurfaceWidgets {
            row: adw::ActionRow::new(),
            connect_btn: gtk::Button::new(),
            disconnect_btn: gtk::Button::new(),
        };
        widgets.connect_btn.set_sensitive(true);
        widgets.disconnect_btn.set_visible(true);
        widgets.disconnect_btn.set_sensitive(true);
        let context = LastFmSettingsContext::default();
        // The detached path awaits nothing; block on the thread-default
        // main context the widget test session established.
        glib::MainContext::ref_thread_default().block_on(async {
            refresh_surface(&widgets, &context).await;
        });
        assert_eq!(
            widgets.row.title().as_str(),
            rust_i18n::t!("lastfm.status_unavailable").as_ref(),
            "a detached context must show the unavailable classification"
        );
        assert!(
            !widgets.connect_btn.is_sensitive(),
            "Connect must stay disabled when the authorization owner is unavailable"
        );
        assert!(
            !widgets.disconnect_btn.is_visible(),
            "Disconnect must stay hidden when the authorization owner is unavailable"
        );
    }
}
