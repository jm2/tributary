//! Internet-radio GTK helpers.
//!
//! Radio-Browser network and locator authority belongs to `SourceRegistry`.
//! This module contains only view-key mapping, column presentation, and the
//! explicit consent/navigation step required before requesting Near Me.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use tracing::info;

use crate::architecture::ViewOrigin;

use super::browser;
use super::objects::TrackObject;
use super::preferences;
use super::source_navigation::{SourceNavigation, SourceRequest};

/// Stable GTK/navigation keys for the three Radio-Browser view lanes.
pub const TOP_CLICK_SOURCE_KEY: &str = "radio-topclick";
pub const TOP_VOTE_SOURCE_KEY: &str = "radio-topvote";
pub const NEARME_SOURCE_KEY: &str = "radio-nearme";

const TOP_CLICK_VIEW_KEY: &str = "top-clicked";
const TOP_VOTE_VIEW_KEY: &str = "top-voted";
const NEARME_VIEW_KEY: &str = "near-me";

/// IDs of the columns to show when viewing radio stations.
const RADIO_VISIBLE_COLUMNS: &[&str] = &["Title", "Artist", "Album", "Genre", "Bitrate", "Format"];

/// Check if a backend type is an exact built-in radio view.
pub fn is_radio_backend(backend_type: &str) -> bool {
    matches!(
        backend_type,
        TOP_CLICK_SOURCE_KEY | TOP_VOTE_SOURCE_KEY | NEARME_SOURCE_KEY
    )
}

/// Convert one exact navigation key into its typed lifecycle view origin.
pub fn radio_view_origin(source_key: &str) -> Option<ViewOrigin> {
    let view_key = match source_key {
        TOP_CLICK_SOURCE_KEY => TOP_CLICK_VIEW_KEY,
        TOP_VOTE_SOURCE_KEY => TOP_VOTE_VIEW_KEY,
        NEARME_SOURCE_KEY => NEARME_VIEW_KEY,
        _ => return None,
    };
    Some(ViewOrigin::radio(view_key).expect("static radio view keys are valid"))
}

/// Map an accepted typed Radio-Browser lane back to its GTK cache key.
pub fn radio_source_key(view: &ViewOrigin) -> Option<&'static str> {
    match view {
        ViewOrigin::Radio(key) if key == TOP_CLICK_VIEW_KEY => Some(TOP_CLICK_SOURCE_KEY),
        ViewOrigin::Radio(key) if key == TOP_VOTE_VIEW_KEY => Some(TOP_VOTE_SOURCE_KEY),
        ViewOrigin::Radio(key) if key == NEARME_VIEW_KEY => Some(NEARME_SOURCE_KEY),
        _ => None,
    }
}

/// Switch column visibility for radio mode or restore music mode.
///
/// When `radio = true`: show only radio-relevant columns, with the Artist and
/// Album columns retitled for a station's country and state/province. When
/// false, restore every column and its music title; the caller then reapplies
/// the user's visibility preferences. Column IDs never change, so persisted
/// visibility, order, and sort stay keyed to the music columns.
pub fn apply_radio_columns(column_view: &gtk::ColumnView, radio: bool) {
    let locale = rust_i18n::locale();
    let columns = column_view.columns();
    for i in 0..columns.n_items() {
        let Some(col) = columns.item(i).and_downcast::<gtk::ColumnViewColumn>() else {
            continue;
        };
        let Some(id) = col.id() else {
            continue; // sentinel column
        };
        col.set_visible(!radio || RADIO_VISIBLE_COLUMNS.contains(&id.as_str()));
        col.set_title(Some(&column_title(&id, radio, &locale)));
    }
}

/// Display title of column `id` in radio or music mode.
fn column_title(id: &str, radio: bool, locale: &str) -> String {
    match (radio, id) {
        (true, "Artist") => rust_i18n::t!("columns.country", locale = locale).into_owned(),
        (true, "Album") => rust_i18n::t!("columns.state_province", locale = locale).into_owned(),
        _ => preferences::column_title(id, locale),
    }
}

#[allow(clippy::too_many_arguments)]
fn fall_back_to_local(
    app_config: &Rc<RefCell<preferences::AppConfig>>,
    track_store: &gtk::gio::ListStore,
    master_tracks: &Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: &gtk::Box,
    browser_state: &browser::BrowserState,
    status_label: &gtk::Label,
    column_view: &gtk::ColumnView,
    active_source_key: &Rc<RefCell<String>>,
    source_navigation: &Rc<RefCell<SourceNavigation>>,
    sidebar_store: &gtk::gio::ListStore,
    sidebar_selection: &gtk::SingleSelection,
    source_tracks: &Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
) {
    source_navigation.borrow_mut().select("local");
    *active_source_key.borrow_mut() = "local".to_string();
    super::window::select_sidebar_source_key(sidebar_store, sidebar_selection, "local");

    apply_radio_columns(column_view, false);
    let config = app_config.borrow();
    preferences::apply_column_visibility(column_view, &config.visible_columns);
    preferences::update_browser_visibility(browser_widget, &config.browser_views);
    drop(config);

    let local_tracks = source_tracks
        .borrow()
        .get("local")
        .cloned()
        .unwrap_or_default();
    super::window::display_local_tracks(
        &local_tracks,
        track_store,
        master_tracks,
        browser_widget,
        browser_state,
        status_label,
        column_view,
        app_config,
    );
}

/// Enforce the Near Me consent prerequisite, then request the typed lifecycle
/// view through `refresh`. No network result or station locator crosses GTK.
#[allow(clippy::too_many_arguments)]
pub fn handle_radio_nearme(
    window: &adw::ApplicationWindow,
    app_config: Rc<RefCell<preferences::AppConfig>>,
    track_store: gtk::gio::ListStore,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: gtk::Box,
    browser_state: browser::BrowserState,
    status_label: gtk::Label,
    column_view: gtk::ColumnView,
    active_source_key: Rc<RefCell<String>>,
    source_navigation: Rc<RefCell<SourceNavigation>>,
    request: SourceRequest,
    near_me_consent_request: Rc<RefCell<Option<SourceRequest>>>,
    sidebar_store: gtk::gio::ListStore,
    sidebar_selection: gtk::SingleSelection,
    source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    refresh: Rc<dyn Fn()>,
) {
    let location_enabled = app_config.borrow().location_enabled;
    match location_enabled {
        Some(true) => {
            clear_exact_consent_prerequisite(&near_me_consent_request, &request);
            refresh();
        }
        Some(false) => {
            clear_exact_consent_prerequisite(&near_me_consent_request, &request);
            info!("Location declined previously, switching to local");
            fall_back_to_local(
                &app_config,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
                &active_source_key,
                &source_navigation,
                &sidebar_store,
                &sidebar_selection,
                &source_tracks,
            );
        }
        None => {
            *near_me_consent_request.borrow_mut() = Some(request.clone());
            let dialog = adw::AlertDialog::builder()
                .heading(rust_i18n::t!("dialogs.enable_location").as_ref())
                .body(rust_i18n::t!("dialogs.enable_location_body").as_ref())
                .close_response(CONSENT_DISMISSED)
                .default_response(CONSENT_ENABLE)
                .build();
            dialog.add_response(CONSENT_DECLINE, rust_i18n::t!("dialogs.no_thanks").as_ref());
            dialog.add_response(
                CONSENT_ENABLE,
                rust_i18n::t!("dialogs.enable_location_btn").as_ref(),
            );
            dialog.set_response_appearance(CONSENT_ENABLE, adw::ResponseAppearance::Suggested);

            dialog.connect_response(None, move |_dialog, response| {
                let decision = consent_decision(response);
                clear_exact_consent_prerequisite(&near_me_consent_request, &request);
                if let Some(enabled) = decision {
                    let mut config = app_config.borrow_mut();
                    config.location_enabled = Some(enabled);
                    preferences::save_config(&config);
                }

                if !source_navigation.borrow().is_current(&request) {
                    tracing::debug!(
                        generation = request.generation(),
                        ?decision,
                        "Handled stale Near Me consent response without changing navigation"
                    );
                    return;
                }

                if decision == Some(true) {
                    info!("Location enabled by user");
                    refresh();
                } else {
                    info!(
                        remembered = decision.is_some(),
                        "Location not enabled, switching to local"
                    );
                    fall_back_to_local(
                        &app_config,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                        &active_source_key,
                        &source_navigation,
                        &sidebar_store,
                        &sidebar_selection,
                        &source_tracks,
                    );
                }
            });
            dialog.present(Some(window));
        }
    }
}

const CONSENT_ENABLE: &str = "enable";
const CONSENT_DECLINE: &str = "decline";
/// Reported when the prompt is closed with Escape or a system action.
const CONSENT_DISMISSED: &str = "dismiss";

/// The location consent a prompt response records: only an explicit button
/// is remembered, so dismissing the prompt asks again next time.
fn consent_decision(response: &str) -> Option<bool> {
    match response {
        CONSENT_ENABLE => Some(true),
        CONSENT_DECLINE => Some(false),
        _ => None,
    }
}

fn clear_exact_consent_prerequisite(
    pending: &RefCell<Option<SourceRequest>>,
    request: &SourceRequest,
) {
    let mut pending = pending.borrow_mut();
    if pending.as_ref() == Some(request) {
        *pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        column_title, consent_decision, is_radio_backend, radio_source_key, radio_view_origin,
        CONSENT_DECLINE, CONSENT_DISMISSED, CONSENT_ENABLE, NEARME_SOURCE_KEY,
        TOP_CLICK_SOURCE_KEY, TOP_VOTE_SOURCE_KEY,
    };
    use crate::architecture::ViewOrigin;

    #[test]
    fn only_exact_builtin_radio_keys_are_admitted() {
        for (key, view_key) in [
            (TOP_CLICK_SOURCE_KEY, "top-clicked"),
            (TOP_VOTE_SOURCE_KEY, "top-voted"),
            (NEARME_SOURCE_KEY, "near-me"),
        ] {
            assert!(is_radio_backend(key));
            assert_eq!(
                radio_view_origin(key),
                Some(ViewOrigin::Radio(view_key.to_string()))
            );
            assert_eq!(
                radio_view_origin(key).as_ref().and_then(radio_source_key),
                Some(key)
            );
        }
        assert!(!is_radio_backend("radio-attacker-defined"));
        assert_eq!(radio_view_origin("radio-attacker-defined"), None);
    }

    #[test]
    fn only_an_explicit_answer_records_location_consent() {
        assert_eq!(consent_decision(CONSENT_ENABLE), Some(true));
        assert_eq!(consent_decision(CONSENT_DECLINE), Some(false));
        // Escape or closing the prompt must not persist a permanent decline.
        assert_eq!(consent_decision(CONSENT_DISMISSED), None);
        assert_eq!(consent_decision("close"), None);
    }

    #[test]
    fn radio_mode_retitles_only_artist_and_album_in_the_given_locale() {
        assert_eq!(column_title("Artist", true, "de"), "Land");
        assert_eq!(column_title("Album", true, "de"), "Bundesland");
        assert_eq!(column_title("Rating", true, "de"), "Bewertung");
        assert_eq!(column_title("Artist", false, "de"), "Künstler");
        assert_eq!(column_title("Album", false, "fr"), "Album");
    }
}
