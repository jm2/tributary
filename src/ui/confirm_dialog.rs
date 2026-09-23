//! Confirmation dialogs for irreversible sidebar actions.
//!
//! Deleting a playlist and removing a saved server ask first. Cancel is both
//! the default and the close response, so Enter, Escape, and dismissing the
//! dialog never run the action; only the destructive response does.

use std::cell::Cell;

use adw::prelude::*;

const CANCEL_RESPONSE: &str = "cancel";
const CONFIRM_RESPONSE: &str = "confirm";

/// Whether a dialog response authorizes the destructive action.
fn authorizes(response: &str) -> bool {
    response == CONFIRM_RESPONSE
}

/// Build, without presenting, a confirmation whose destructive response runs
/// `on_confirm` at most once. Every other response does nothing.
fn destructive_confirmation(
    heading: &str,
    body: &str,
    confirm_label: &str,
    on_confirm: impl FnOnce() + 'static,
) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .close_response(CANCEL_RESPONSE)
        .default_response(CANCEL_RESPONSE)
        .build();
    dialog.add_response(CANCEL_RESPONSE, rust_i18n::t!("dialogs.cancel").as_ref());
    dialog.add_response(CONFIRM_RESPONSE, confirm_label);
    dialog.set_response_appearance(CONFIRM_RESPONSE, adw::ResponseAppearance::Destructive);

    let on_confirm = Cell::new(Some(on_confirm));
    dialog.connect_response(None, move |_, response| {
        if authorizes(response) {
            if let Some(on_confirm) = on_confirm.take() {
                on_confirm();
            }
        }
    });
    dialog
}

/// Confirmation before permanently deleting the playlist called `name`.
pub(super) fn delete_playlist(name: &str, on_confirm: impl FnOnce() + 'static) -> adw::AlertDialog {
    destructive_confirmation(
        &rust_i18n::t!("dialogs.delete_playlist_heading"),
        &rust_i18n::t!("dialogs.delete_playlist_body", name = name),
        &rust_i18n::t!("sidebar.delete"),
        on_confirm,
    )
}

/// Confirmation before removing the saved server called `name`.
pub(super) fn remove_server(name: &str, on_confirm: impl FnOnce() + 'static) -> adw::AlertDialog {
    destructive_confirmation(
        &rust_i18n::t!("dialogs.remove_server_heading"),
        &rust_i18n::t!("dialogs.remove_server_body", name = name),
        &rust_i18n::t!("dialogs.remove"),
        on_confirm,
    )
}

#[cfg(test)]
mod tests {
    use super::{authorizes, CANCEL_RESPONSE, CONFIRM_RESPONSE};

    #[test]
    fn only_the_destructive_response_authorizes() {
        assert!(authorizes(CONFIRM_RESPONSE));
        for response in [CANCEL_RESPONSE, "close", ""] {
            assert!(!authorizes(response), "{response:?} must not authorize");
        }
    }

    #[test]
    fn confirmation_copy_is_translated_in_every_catalog() {
        let name = "Road Trip";
        for locale in rust_i18n::available_locales!() {
            let locale: &str = locale.as_ref();
            for key in [
                "dialogs.delete_playlist_heading",
                "dialogs.delete_playlist_body",
                "dialogs.remove_server_heading",
                "dialogs.remove_server_body",
                "dialogs.remove",
            ] {
                let text = rust_i18n::t!(key, locale = locale, name = name);
                let english = rust_i18n::t!(key, locale = "en", name = name);
                assert!(!text.contains("%{"), "{locale}.{key} left a placeholder");
                if key.ends_with("_body") {
                    assert!(text.contains(name), "{locale}.{key} must name the item");
                }
                if locale != "en" {
                    assert_ne!(text, english, "{locale}.{key} fell back to English");
                }
            }
        }
    }
}

/// GTK-touching contracts folded into the crate's single consolidated
/// GTK-initializing test (browser.rs `gtk_widget_contracts_hold_on_one_session`);
/// see `ui::widget_test_session`. Mirrors the caller's macOS gate so these
/// helpers are never dead code there.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use adw::prelude::*;

    use super::{CANCEL_RESPONSE, CONFIRM_RESPONSE};

    fn assert_runs_only_on_the_destructive_response(
        build: impl FnOnce(Box<dyn FnOnce()>) -> adw::AlertDialog,
    ) {
        let runs = Rc::new(Cell::new(0_u32));
        let counter = Rc::clone(&runs);
        let dialog = build(Box::new(move || counter.set(counter.get() + 1)));

        assert_eq!(dialog.default_response().as_deref(), Some(CANCEL_RESPONSE));
        assert_eq!(dialog.close_response(), CANCEL_RESPONSE);
        assert_eq!(
            dialog.response_appearance(CONFIRM_RESPONSE),
            adw::ResponseAppearance::Destructive
        );

        dialog.emit_by_name::<()>("response", &[&CANCEL_RESPONSE]);
        assert_eq!(runs.get(), 0, "cancel must not run the action");
        dialog.emit_by_name::<()>("response", &[&CONFIRM_RESPONSE]);
        assert_eq!(runs.get(), 1, "the destructive response runs the action");
        dialog.emit_by_name::<()>("response", &[&CONFIRM_RESPONSE]);
        assert_eq!(runs.get(), 1, "the action runs at most once");
    }

    /// Deleting a playlist and removing a server do nothing until the user
    /// chooses the destructive response.
    pub fn nothing_is_removed_until_the_destructive_response() {
        assert_runs_only_on_the_destructive_response(|action| {
            super::delete_playlist("Road Trip", action)
        });
        assert_runs_only_on_the_destructive_response(|action| {
            super::remove_server("Living Room", action)
        });
    }
}
