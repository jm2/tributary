//! Last.fm settings group: consent disclosure, connect, reconnect, and
//! disconnect-and-purge.
//!
//! Every account action goes through the process [`LastFmApplicationHandle`]:
//! Connect installs the authorized account and activates it, Reconnect hands a
//! same-account grant to the running runtime, and Disconnect runs the
//! runtime's purge. Consent is committed through the shared
//! [`LastFmLivePolicy`], so queue capture, dispatch, and the runtime observe
//! the same generation. The surface renders only fixed localized categories;
//! no provider, vault, or queue detail reaches it.
//!
//! Every future in this module owns GTK widgets and runs on the GTK main
//! context through `spawn_local`; database and vault work is spawned onto the
//! tokio runtime and only its result is awaited here.
#![allow(clippy::future_not_send)]

use std::sync::{Arc, OnceLock};

use adw::prelude::*;
use gtk::glib;
use sea_orm::DatabaseConnection;

use crate::lastfm::authorization::LastFmAuthorizationGrant;
use crate::lastfm::credentials::{
    CredentialError, OsSessionCredentialStore, SessionCredentialStore,
};
use crate::lastfm::policy::{LastFmConsentRecord, LastFmLivePolicy, LastFmPolicyUpdate};
use crate::lastfm::production::{
    LastFmApplicationActivation, LastFmApplicationCommandError, LastFmApplicationHandle,
    LastFmApplicationPhase, LastFmApplicationStatus,
};
use crate::lastfm::runtime::{LastFmRuntimeCommandError, LastFmRuntimePhase};

mod dialogs;
#[cfg(test)]
mod tests;

use dialogs::{choose, disclosure_dialog, disconnect_dialog, finish_dialog, launch_browser};

/// Revision of the shipped disclosure text. Bump it whenever the disclosure's
/// meaning changes so the recorded consent names the text that was accepted.
pub const DISCLOSURE_REVISION: u32 = 1;

/// Handles the settings group needs. The database arrives after the window
/// is built, so it is attached late; until then the owner reports
/// `AwaitingDatabase` and every action stays disabled.
#[derive(Clone)]
pub struct LastFmSettingsContext {
    application: LastFmApplicationHandle,
    policy: LastFmLivePolicy,
    database: Arc<OnceLock<DatabaseConnection>>,
    runtime: tokio::runtime::Handle,
}

impl LastFmSettingsContext {
    pub(crate) fn new(
        application: LastFmApplicationHandle,
        policy: LastFmLivePolicy,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            application,
            policy,
            database: Arc::new(OnceLock::new()),
            runtime,
        }
    }

    pub fn attach_database(&self, database: DatabaseConnection) {
        let _ = self.database.set(database);
    }

    /// Whether this build carries Last.fm application credentials.
    /// Preferences shows the Last.fm group only when it does.
    pub fn available_in_build(&self) -> bool {
        self.application.build_available()
    }
}

/// The vault account as the surface may display it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StoredAccount {
    /// No usable record (missing or corrupt); Connect may install one.
    None,
    Named(String),
    /// The protected store could not be read (for example, locked).
    Unreadable,
}

/// The one account action the surface offers next to Disconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Authorize in the browser and install a new account.
    Connect,
    /// Start the stored account without a new authorization.
    Resume,
    /// Authorize again for the running account after Last.fm revoked it.
    Reconnect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Surface {
    title: String,
    action: Option<Action>,
    disconnect: bool,
}

impl Surface {
    fn message(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            action: None,
            disconnect: false,
        }
    }
}

fn t(key: &str) -> String {
    rust_i18n::t!(key).to_string()
}

fn named(key: &str, username: &str) -> String {
    rust_i18n::t!(key, username = username).to_string()
}

/// Map the owner status and the stored account onto the surface.
fn surface(status: &LastFmApplicationStatus, account: &StoredAccount) -> Surface {
    use LastFmApplicationPhase as Phase;

    match status.phase {
        Phase::UnavailableBuild => Surface::message(t("lastfm.status_unavailable")),
        Phase::ShuttingDown | Phase::Stopped | Phase::Failed => {
            Surface::message(t("lastfm.status_stopped"))
        }
        Phase::AwaitingDatabase | Phase::Starting => Surface::message(t("lastfm.connecting")),
        Phase::AwaitingConsent => dormant_surface(status, account),
        Phase::Active => {
            let runtime = status.runtime.map(|runtime| runtime.phase);
            let (title, action) = match (runtime, account) {
                (
                    Some(
                        LastFmRuntimePhase::CredentialCleanup | LastFmRuntimePhase::DisconnectRetry,
                    ),
                    _,
                ) => (t("lastfm.status_disconnect_incomplete"), None),
                (_, StoredAccount::None | StoredAccount::Unreadable) => {
                    (t("lastfm.status_vault_unavailable"), None)
                }
                (
                    Some(LastFmRuntimePhase::ReauthenticationRequired),
                    StoredAccount::Named(name),
                ) => (
                    named("lastfm.status_reauthenticate", name),
                    Some(Action::Reconnect),
                ),
                (_, StoredAccount::Named(name)) => (named("lastfm.connected_as", name), None),
            };
            Surface {
                title,
                action,
                disconnect: true,
            }
        }
    }
}

fn dormant_surface(status: &LastFmApplicationStatus, account: &StoredAccount) -> Surface {
    match account {
        StoredAccount::Named(username) => Surface {
            title: named("lastfm.status_not_running", username),
            action: Some(Action::Resume),
            disconnect: false,
        },
        // Rows left by an account that can no longer be loaded block a new
        // account until the user explicitly discards them.
        StoredAccount::None
            if status.failure == Some(LastFmApplicationCommandError::QuarantinedQueue) =>
        {
            Surface {
                title: t("lastfm.status_quarantined"),
                action: None,
                disconnect: true,
            }
        }
        StoredAccount::None => Surface {
            title: t("lastfm.status_not_connected"),
            action: Some(Action::Connect),
            disconnect: false,
        },
        StoredAccount::Unreadable => Surface::message(t("lastfm.status_vault_unavailable")),
    }
}

/// The interactive pieces of the group's one row.
#[derive(Clone)]
struct Widgets {
    row: adw::ActionRow,
    action: gtk::Button,
    disconnect: gtk::Button,
    current: std::rc::Rc<std::cell::Cell<Option<Action>>>,
}

fn render(widgets: &Widgets, surface: &Surface) {
    widgets.row.set_title(&surface.title);
    widgets.current.set(surface.action);
    widgets.action.set_visible(surface.action.is_some());
    widgets.action.set_sensitive(surface.action.is_some());
    if let Some(action) = surface.action {
        let key = if action == Action::Reconnect {
            "lastfm.reconnect"
        } else {
            "lastfm.connect"
        };
        widgets.action.set_label(&t(key));
    }
    widgets.disconnect.set_visible(surface.disconnect);
    widgets.disconnect.set_sensitive(surface.disconnect);
}

/// Read the stored account for display. Unavailable builds and a
/// database-less owner never touch the vault.
async fn stored_account(
    context: &LastFmSettingsContext,
    phase: LastFmApplicationPhase,
) -> StoredAccount {
    if !matches!(
        phase,
        LastFmApplicationPhase::AwaitingConsent | LastFmApplicationPhase::Active
    ) {
        return StoredAccount::None;
    }
    let loaded = context
        .runtime
        .spawn_blocking(|| OsSessionCredentialStore.load())
        .await;
    match loaded {
        Ok(Ok(Some(session))) => StoredAccount::Named(session.username().to_owned()),
        Ok(Ok(None) | Err(CredentialError::InvalidData)) => StoredAccount::None,
        Ok(Err(_)) | Err(_) => StoredAccount::Unreadable,
    }
}

/// Repaint from the owner's current status; `message` replaces the title
/// when a flow ended with a fixed failure.
async fn refresh(context: &LastFmSettingsContext, widgets: &Widgets, message: Option<String>) {
    let phase = context.application.subscribe_status().borrow().phase;
    let account = stored_account(context, phase).await;
    let status = *context.application.subscribe_status().borrow();
    let mut surface = surface(&status, &account);
    if let Some(message) = message {
        surface.title = message;
    }
    render(widgets, &surface);
}

// ── Flows ───────────────────────────────────────────────────────────────

/// Record consent before anything else can happen. Declining stores nothing.
async fn ensure_consent(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> Result<(), String> {
    if context.policy.snapshot().consented_and_enabled() {
        return Ok(());
    }
    if choose(disclosure_dialog(), parent).await != "accept" {
        return Err(t("lastfm.status_consent_required"));
    }
    let Some(database) = context.database.get().cloned() else {
        return Err(t("lastfm.status_store_error"));
    };
    let Ok(consent) = LastFmConsentRecord::try_new(&rust_i18n::locale(), DISCLOSURE_REVISION)
    else {
        return Err(t("lastfm.status_store_error"));
    };
    let policy = context.policy.clone();
    let committed = context
        .runtime
        .spawn(async move {
            let update = LastFmPolicyUpdate {
                consent: Some(consent),
                enabled: true,
                enabled_remote_sources: policy.snapshot().enabled_remote_sources().clone(),
            };
            policy.commit(&database, update).await
        })
        .await;
    match committed {
        Ok(Ok(_)) => Ok(()),
        _ => Err(t("lastfm.status_store_error")),
    }
}

/// Run one browser authorization. `Err(None)` is a user cancel.
async fn authorize(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> Result<LastFmAuthorizationGrant, Option<String>> {
    let unavailable = || Some(t("lastfm.status_authorization_unavailable"));
    let authorization = context
        .application
        .authorization()
        .ok_or_else(unavailable)?;
    let start = authorization.try_begin().map_err(|_| unavailable())?;
    let challenge = start.wait().await.map_err(|_| unavailable())?;
    let url = challenge.authorization_url().map_err(|_| unavailable())?;
    launch_browser(parent, &url).await;
    if choose(finish_dialog(), parent).await != "continue" {
        if let Ok(cancel) = authorization.try_cancel(&challenge.flow()) {
            let _ = cancel.wait().await;
        }
        return Err(None);
    }
    let finish = authorization
        .try_finish(&challenge)
        .map_err(|_| unavailable())?;
    finish.wait().await.map_err(|_| unavailable())
}

fn command_failure(error: LastFmApplicationCommandError) -> Option<String> {
    use LastFmApplicationCommandError as Error;
    match error {
        // The refreshed surface shows the stored account or the quarantine.
        Error::AccountPresent | Error::QuarantinedQueue => None,
        Error::ConsentRequired => Some(t("lastfm.status_consent_required")),
        Error::CredentialStore => Some(t("lastfm.status_vault_unavailable")),
        _ => Some(t("lastfm.status_start_failed")),
    }
}

async fn connect(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> Option<String> {
    if let Err(message) = ensure_consent(parent, context).await {
        return Some(message);
    }
    let grant = match authorize(parent, context).await {
        Ok(grant) => grant,
        Err(message) => return message,
    };
    match context.application.try_connect(grant) {
        Ok(operation) => operation.wait().await.err().and_then(command_failure),
        Err(_) => Some(t("lastfm.status_start_failed")),
    }
}

async fn resume(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> Option<String> {
    if let Err(message) = ensure_consent(parent, context).await {
        return Some(message);
    }
    let Ok(activation) =
        LastFmApplicationActivation::issue_from_policy_generation(&context.policy.snapshot())
    else {
        return Some(t("lastfm.status_consent_required"));
    };
    match context.application.try_activate(activation) {
        Ok(operation) => operation.wait().await.err().and_then(command_failure),
        Err(_) => Some(t("lastfm.status_start_failed")),
    }
}

async fn reconnect(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> Option<String> {
    let grant = match authorize(parent, context).await {
        Ok(grant) => grant,
        Err(message) => return message,
    };
    let failed = Some(t("lastfm.status_authorization_unavailable"));
    let Ok(forward) = context.application.try_reauthorize_same_account(grant) else {
        return failed;
    };
    let Ok(Ok(operation)) = forward.wait().await else {
        return failed;
    };
    match operation.wait().await {
        Ok(()) => None,
        Err(LastFmRuntimeCommandError::AccountReplacementRequired) => {
            Some(t("lastfm.status_replacement_required"))
        }
        Err(_) => failed,
    }
}

async fn disconnect(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> Option<String> {
    if choose(disconnect_dialog(), parent).await != "disconnect" {
        return None;
    }
    let incomplete = || t("lastfm.status_disconnect_incomplete");
    match context.application.try_disconnect_and_purge() {
        Ok(operation) => operation.wait().await.err().map(|_| incomplete()),
        Err(_) => Some(incomplete()),
    }
}

// ── Group ───────────────────────────────────────────────────────────────

/// Run one button's flow with both buttons disabled, then repaint.
fn spawn_flow(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
    widgets: &Widgets,
    disconnecting: bool,
) {
    // Disable before spawning so a second click cannot start a second flow.
    widgets.action.set_sensitive(false);
    widgets.disconnect.set_sensitive(false);
    let (parent, context, widgets) = (parent.clone(), context.clone(), widgets.clone());
    glib::MainContext::default().spawn_local(async move {
        let message = match (disconnecting, widgets.current.get()) {
            (true, _) => disconnect(&parent, &context).await,
            (false, Some(Action::Connect)) => connect(&parent, &context).await,
            (false, Some(Action::Resume)) => resume(&parent, &context).await,
            (false, Some(Action::Reconnect)) => reconnect(&parent, &context).await,
            (false, None) => None,
        };
        refresh(&context, &widgets, message).await;
    });
}

/// Build the Last.fm scrobbling group for the preferences page.
///
/// Nothing on this surface starts an authorization request, a queue
/// insertion, or a scrobble until the disclosure has been accepted.
pub fn build_lastfm_group(
    parent: &adw::ApplicationWindow,
    context: &LastFmSettingsContext,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(t("lastfm.group_title"))
        .description(t("lastfm.group_description"))
        .build();
    let widgets = new_widgets();
    group.add(&widgets.row);

    for disconnecting in [false, true] {
        let (parent, context, flow_widgets) = (parent.clone(), context.clone(), widgets.clone());
        let button = if disconnecting {
            &widgets.disconnect
        } else {
            &widgets.action
        };
        button.connect_clicked(move |_| {
            spawn_flow(&parent, &context, &flow_widgets, disconnecting);
        });
    }

    render(&widgets, &Surface::message(t("lastfm.connecting")));
    let context = context.clone();
    glib::MainContext::default().spawn_local(async move {
        refresh(&context, &widgets, None).await;
    });
    group
}

fn new_widgets() -> Widgets {
    let row = adw::ActionRow::new();
    let action = gtk::Button::builder()
        .css_classes(["suggested-action"])
        .valign(gtk::Align::Center)
        .build();
    let disconnect = gtk::Button::builder()
        .label(t("lastfm.disconnect"))
        .css_classes(["destructive-action"])
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&action);
    row.add_suffix(&disconnect);
    Widgets {
        row,
        action,
        disconnect,
        current: std::rc::Rc::default(),
    }
}

// The sole caller is browser.rs's consolidated GTK test, which shares the
// crate's one GTK session only off macOS; mirror its gate exactly.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use super::*;

    pub fn render_shows_only_the_offered_actions() {
        let widgets = new_widgets();
        render(
            &widgets,
            &Surface {
                title: "reconnect".to_owned(),
                action: Some(Action::Reconnect),
                disconnect: true,
            },
        );
        assert_eq!(widgets.row.title().as_str(), "reconnect");
        assert!(widgets.action.is_visible() && widgets.action.is_sensitive());
        assert_eq!(
            widgets.action.label().as_deref(),
            Some(t("lastfm.reconnect").as_str())
        );
        assert!(widgets.disconnect.is_visible() && widgets.disconnect.is_sensitive());
        assert_eq!(widgets.current.get(), Some(Action::Reconnect));

        render(&widgets, &Surface::message("unavailable"));
        assert!(!widgets.action.is_visible() && !widgets.action.is_sensitive());
        assert!(!widgets.disconnect.is_visible() && !widgets.disconnect.is_sensitive());
        assert_eq!(widgets.current.get(), None);
    }
}
