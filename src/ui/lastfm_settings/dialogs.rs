//! Dialogs for the Last.fm settings group: the consent disclosure, the
//! browser-approval continuation, the disconnect confirmation, and the
//! system-browser handoff with its copyable fallback.

use adw::prelude::*;

use super::t;

/// Present `dialog` and await the user's response.
pub(super) async fn choose(dialog: adw::AlertDialog, parent: &adw::ApplicationWindow) -> String {
    let (sender, receiver) = async_channel::bounded::<String>(1);
    dialog.choose(
        Some(parent),
        None::<&gtk::gio::Cancellable>,
        move |response| {
            // The callback is synchronous; `try_send` delivers immediately.
            let _ = sender.try_send(response.to_string());
        },
    );
    receiver.recv().await.unwrap_or_default()
}

/// A dialog whose first response is the safe default and the close action.
fn alert(heading: &str, body: &str, responses: &[(&str, String)]) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .close_response(responses[0].0)
        .default_response(responses[0].0)
        .build();
    for (id, label) in responses {
        dialog.add_response(id, label);
    }
    dialog
}

/// The localized disclosure: what is sent, how Last.fm may use it, local
/// offline storage, and the remote-source and external-scrobbler caveats.
pub(super) fn disclosure_dialog() -> adw::AlertDialog {
    let dialog = alert(
        &t("lastfm.disclosure_title"),
        &t("lastfm.disclosure_body"),
        &[
            ("decline", t("lastfm.decline")),
            ("accept", t("lastfm.accept")),
        ],
    );
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog
}

pub(super) fn finish_dialog() -> adw::AlertDialog {
    let dialog = alert(
        &t("lastfm.finish_title"),
        &t("lastfm.finish_body"),
        &[
            ("cancel", t("lastfm.cancel")),
            ("continue", t("lastfm.continue")),
        ],
    );
    dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
    dialog
}

/// The consequence text names the credential removal and the pending-scrobble
/// discard up front.
pub(super) fn disconnect_dialog() -> adw::AlertDialog {
    let dialog = alert(
        &t("lastfm.disconnect_confirm_title"),
        &t("lastfm.disconnect_confirm_body"),
        &[
            ("cancel", t("lastfm.cancel")),
            ("disconnect", t("lastfm.disconnect_confirm_accept")),
        ],
    );
    dialog.set_response_appearance("disconnect", adw::ResponseAppearance::Destructive);
    dialog
}

/// Hand the authorization URL to the system browser once. If no handler
/// takes it, show it in a copyable field instead. It never reaches logs.
pub(super) async fn launch_browser(parent: &adw::ApplicationWindow, url: &str) {
    let (sender, receiver) = async_channel::bounded(1);
    gtk::UriLauncher::new(url).launch(
        Some(parent),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let _ = sender.try_send(result.is_ok());
        },
    );
    if receiver.recv().await.unwrap_or(false) {
        return;
    }
    let fallback = alert(
        &t("lastfm.disclosure_title"),
        &t("lastfm.browser_fallback_body"),
        &[("close", t("lastfm.close"))],
    );
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
