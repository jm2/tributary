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
//!
//! Split into child modules so every file and function stays under the
//! repository's size caps: `dialogs` holds the dialog builders and status
//! mapping, `flows` holds the surface flow futures.

use std::sync::{Arc, Mutex};

use adw::prelude::*;
use gtk::glib;

use crate::lastfm::account::{
    begin_consent_gated_authorization, install_fresh_account, install_same_account_reauthorization,
    stage_account_install_decision, LastFmAccountAuthorizationError, LastFmAccountInstallDecision,
};
use crate::lastfm::authorization::{LastFmAuthorizationChallenge, LastFmAuthorizationHandle};
use crate::lastfm::credentials::SessionCredentialStore;
use crate::lastfm::policy::{
    commit_policy_update, lock_policy_slot, LastFmConsentRecord, LastFmPolicyGeneration,
    LastFmPolicyStoreError, LastFmPolicyUpdate,
};

mod dialogs;
mod flows;

use dialogs::install_failure_is_account_rejection;

use flows::{
    connect_flow_future, disconnect_flow_future, refresh_surface_future, set_flow_busy,
    SurfaceWidgets,
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

// ── Preferences group ───────────────────────────────────────────────────

/// Wire the Connect action: consent gate first, then the browser handoff
/// and the one-shot finish. The row is refreshed from vault truth after
/// every path so a flow that ends without installing (dismissed disclosure,
/// cancelled approval) still shows a live state.
fn wire_connect_button(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
    policy_slot: &Arc<Mutex<LastFmPolicyGeneration>>,
    widgets: &SurfaceWidgets,
) {
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
            let persistent = connect_flow_future(&parent, &context, &policy_slot, &widgets).await;
            refresh_surface_future(&widgets, &context).await;
            if let Some(message) = persistent {
                widgets.row.set_title(message.as_str());
            }
        });
    });
}

/// Wire the Disconnect action: explicit confirmation; the consequence text
/// names the credential removal and the pending-scrobble discard up front.
fn wire_disconnect_button(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
    widgets: &SurfaceWidgets,
) {
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
            let persistent = disconnect_flow_future(&parent, &context).await;
            refresh_surface_future(&widgets, &context).await;
            if let Some(message) = persistent {
                widgets.row.set_title(message.as_str());
            }
        });
    });
}

/// Initial truthful paint (the detached/unavailable build renders its
/// disabled classification before any interaction).
fn spawn_initial_surface_refresh(context: &LastFmSettingsContext, widgets: &SurfaceWidgets) {
    let context = context.clone();
    let widgets = widgets.clone();
    glib::MainContext::default().spawn_local(async move {
        refresh_surface_future(&widgets, &context).await;
    });
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

    wire_connect_button(parent, context, policy_slot, &widgets);
    wire_disconnect_button(parent, context, &widgets);
    spawn_initial_surface_refresh(context, &widgets);

    group
}

// The sole caller is browser.rs's consolidated GTK test, which shares the
// crate's one GTK session only off-macOS; an ungated copy of these symbols
// is dead code there and fails clippy -D warnings. Mirror the caller's gate
// exactly, as preferences::widget_tests already does.
#[cfg(all(test, not(target_os = "macos")))]
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
            refresh_surface_future(&widgets, &context).await;
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
