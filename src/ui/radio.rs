//! Internet radio helpers — column switching, station conversion, geo-fetch.
//!
//! Extracted from `window.rs` to keep radio-specific logic isolated.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tracing::info;

use super::browser;
use super::objects::TrackObject;
use super::preferences;

/// Columns to show when viewing radio stations.
const RADIO_VISIBLE_COLUMNS: &[&str] = &["Title", "Artist", "Album", "Genre", "Bitrate", "Format"];

/// Check if a backend type is a radio source.
pub fn is_radio_backend(backend_type: &str) -> bool {
    backend_type.starts_with("radio-")
}

/// Switch column visibility for radio mode or restore music mode.
///
/// When `radio = true`: show only radio-relevant columns.
/// When `radio = false`: restore all columns from user preferences
/// (caller should call `preferences::apply_column_visibility` after).
pub fn apply_radio_columns(column_view: &gtk::ColumnView, radio: bool) {
    let columns = column_view.columns();
    for i in 0..columns.n_items() {
        if let Some(col) = columns.item(i).and_downcast_ref::<gtk::ColumnViewColumn>() {
            if let Some(title) = col.title() {
                if title.is_empty() {
                    continue; // sentinel column
                }
                let title_str = title.to_string();
                // Match on both original and renamed titles since the column
                // may already be renamed from a previous radio selection.
                if radio {
                    let is_artist_col = title_str == "Artist" || title_str == "Country";
                    let is_album_col = title_str == "Album" || title_str == "State/Province";
                    if is_artist_col {
                        col.set_visible(true);
                        col.set_title(Some("Country"));
                    } else if is_album_col {
                        col.set_visible(true);
                        col.set_title(Some("State/Province"));
                    } else {
                        col.set_visible(RADIO_VISIBLE_COLUMNS.contains(&title_str.as_str()));
                    }
                } else {
                    // Restore all — the caller will apply user prefs after.
                    col.set_visible(true);
                    // Rename radio columns back to music names.
                    if title_str == "Country" {
                        col.set_title(Some("Artist"));
                    } else if title_str == "State/Province" {
                        col.set_title(Some("Album"));
                    }
                }
            }
        }
    }
}

/// Convert a `RadioStation` to a `TrackObject` for display in the tracklist.
///
/// Mapping: name→title, country→artist, tags→genre, codec→format,
/// bitrate→bitrate, url_resolved→uri.
pub fn radio_station_to_track_object(station: &crate::radio::RadioStation) -> TrackObject {
    TrackObject::new(
        0,                // track_number (unused for radio)
        &station.name,    // title = station name
        0,                // duration_secs (live stream)
        &station.country, // artist = country
        &station.state,   // album = state/province
        &station.tags,    // genre = tags
        0,                // year (unused)
        "",               // date_modified (unused)
        station.bitrate,  // bitrate
        0,                // sample_rate (unused)
        0,                // play_count (unused)
        &station.codec,   // format = codec
        &station.url_resolved,
    )
}

/// Handle the "Stations Near Me" radio source with geolocation consent.
#[allow(clippy::too_many_arguments)]
pub fn handle_radio_nearme(
    window: &adw::ApplicationWindow,
    app_config: Rc<RefCell<preferences::AppConfig>>,
    rt_handle: tokio::runtime::Handle,
    track_store: gtk::gio::ListStore,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: gtk::Box,
    browser_state: browser::BrowserState,
    status_label: gtk::Label,
    column_view: gtk::ColumnView,
    active_source_key: Rc<RefCell<String>>,
    sidebar_selection: gtk::SingleSelection,
    current_pos: Rc<Cell<Option<u32>>>,
    source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
) {
    let location_enabled = app_config.borrow().location_enabled;

    match location_enabled {
        Some(true) => {
            // Already consented — fetch directly.
            fetch_and_display_nearme(
                rt_handle,
                track_store,
                master_tracks,
                browser_widget,
                browser_state,
                status_label,
                column_view,
                current_pos,
            );
        }
        Some(false) => {
            // Previously declined — switch back to local.
            info!("Location declined previously, switching to local");
            *active_source_key.borrow_mut() = "local".to_string();
            sidebar_selection.set_selected(1);

            // Restore music columns.
            apply_radio_columns(&column_view, false);
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            let st = source_tracks.borrow();
            let local_tracks = st.get("local").cloned().unwrap_or_default();
            super::window::display_tracks(
                &local_tracks,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
        }
        None => {
            // Not yet asked — show consent dialog.
            let dialog = adw::AlertDialog::builder()
                .heading("Enable Location?")
                .body(
                    "Stations Near Me uses your approximate location (via IP address) \
                     to find nearby radio stations.\n\n\
                     Your IP address will be sent to a geolocation service to determine \
                     your approximate coordinates. No personal data is stored.",
                )
                .close_response("decline")
                .default_response("enable")
                .build();

            dialog.add_response("decline", "No Thanks");
            dialog.add_response("enable", "Enable Location");
            dialog.set_response_appearance("enable", adw::ResponseAppearance::Suggested);

            let app_config = app_config.clone();
            let rt_handle = rt_handle.clone();
            let track_store = track_store.clone();
            let master_tracks = master_tracks.clone();
            let browser_widget = browser_widget.clone();
            let browser_state = browser_state.clone();
            let status_label = status_label.clone();
            let column_view = column_view.clone();
            let active_source_key = active_source_key.clone();
            let sidebar_selection = sidebar_selection.clone();
            let current_pos = current_pos.clone();
            let source_tracks = source_tracks.clone();

            dialog.connect_response(None, move |_dialog, response| {
                if response == "enable" {
                    // Save consent.
                    {
                        let mut cfg = app_config.borrow_mut();
                        cfg.location_enabled = Some(true);
                        preferences::save_config(&cfg);
                    }
                    info!("Location enabled by user");

                    fetch_and_display_nearme(
                        rt_handle.clone(),
                        track_store.clone(),
                        master_tracks.clone(),
                        browser_widget.clone(),
                        browser_state.clone(),
                        status_label.clone(),
                        column_view.clone(),
                        current_pos.clone(),
                    );
                } else {
                    // Save decline.
                    {
                        let mut cfg = app_config.borrow_mut();
                        cfg.location_enabled = Some(false);
                        preferences::save_config(&cfg);
                    }
                    info!("Location declined by user, switching to local");

                    *active_source_key.borrow_mut() = "local".to_string();
                    sidebar_selection.set_selected(1);

                    // Restore music columns.
                    apply_radio_columns(&column_view, false);
                    let cfg = app_config.borrow();
                    preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
                    preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
                    drop(cfg);

                    let st = source_tracks.borrow();
                    let local_tracks = st.get("local").cloned().unwrap_or_default();
                    super::window::display_tracks(
                        &local_tracks,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                }
            });

            dialog.present(Some(window));
        }
    }
}

/// Fetch geolocation + nearby stations and display them.
///
/// Uses a tiered search strategy:
/// 1. Geo-distance sorted (stations with lat/lon in user's country)
/// 2. State/province match (stations with state but no geo coords, e.g. WBAA)
/// 3. Country-only fallback (remaining stations, sorted by votes)
///
/// Results are merged and deduplicated by `stationuuid`, preserving tier order.
#[allow(clippy::too_many_arguments)]
fn fetch_and_display_nearme(
    rt_handle: tokio::runtime::Handle,
    track_store: gtk::gio::ListStore,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: gtk::Box,
    browser_state: browser::BrowserState,
    status_label: gtk::Label,
    column_view: gtk::ColumnView,
    current_pos: Rc<Cell<Option<u32>>>,
) {
    let (stations_tx, stations_rx) = async_channel::bounded::<String>(1);

    rt_handle.spawn(async move {
        // First get geolocation (multi-provider cascade).
        if let Some(geo) = crate::radio::client::fetch_geolocation().await {
            let client = crate::radio::RadioBrowserClient::new();
            let cc = &geo.country_code;

            // Tier 1: Geo-distance sorted (stations with real coordinates).
            let tier1 = if !cc.is_empty() {
                client
                    .fetch_near_me_with_country(geo.latitude, geo.longitude, cc, None)
                    .await
            } else {
                client
                    .fetch_near_me(geo.latitude, geo.longitude, None)
                    .await
            };
            info!(tier1 = tier1.len(), "Near Me tier 1 (geo-distance)");

            // Tier 2: State/province match (catches stations like WBAA).
            let tier2 = if !cc.is_empty() && !geo.region.is_empty() {
                client.fetch_near_me_with_state(cc, &geo.region, None).await
            } else {
                Vec::new()
            };
            info!(tier2 = tier2.len(), state = %geo.region, "Near Me tier 2 (state)");

            // Tier 3: Country-only fallback.
            let tier3 = if !cc.is_empty() {
                client.fetch_near_me_country_only(cc, Some(50)).await
            } else {
                Vec::new()
            };
            info!(tier3 = tier3.len(), "Near Me tier 3 (country-only)");

            // Merge and dedup by stationuuid, preserving tier order.
            let mut seen = std::collections::HashSet::new();
            let mut merged = Vec::new();
            for station in tier1.into_iter().chain(tier2).chain(tier3) {
                if seen.insert(station.stationuuid.clone()) {
                    merged.push(station);
                }
            }
            info!(total = merged.len(), "Near Me merged results");

            let json = serde_json::to_string(&merged).unwrap_or_default();
            let _ = stations_tx.send(json).await;
        } else {
            tracing::warn!("Geolocation failed — showing empty station list");
            let _ = stations_tx.send("[]".to_string()).await;
        }
    });

    glib::MainContext::default().spawn_local(async move {
        if let Ok(json) = stations_rx.recv().await {
            let stations: Vec<crate::radio::RadioStation> =
                serde_json::from_str(&json).unwrap_or_default();
            let objects: Vec<TrackObject> =
                stations.iter().map(radio_station_to_track_object).collect();
            super::window::display_tracks(
                &objects,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
            current_pos.set(None);
        }
    });
}
