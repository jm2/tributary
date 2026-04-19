//! Main application window — assembles all UI components and bridges
//! the background library engine, the GStreamer player, and the OS
//! media controls to the GTK main thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tracing::{info, warn};

use sea_orm::{EntityTrait, QueryFilter};

use crate::audio::airplay_output::AirPlayOutput;
use crate::audio::chromecast_output::ChromecastOutput;
use crate::audio::local_output::LocalOutput;
use crate::audio::mpd_output::MpdOutput;
use crate::audio::output::AudioOutput;
use crate::audio::{PlayerEvent, PlayerState};
use crate::desktop_integration::MediaAction;
use crate::local::engine::{LibraryEngine, LibraryEvent};
use crate::ui::header_bar::RepeatMode;

use super::browser;
use super::header_bar;
use super::objects::{SourceObject, TrackObject};
use super::output_dialogs::{load_saved_outputs, show_add_output_dialog};
use super::persistence::{
    extract_hwnd, load_css, load_repeat_mode, load_shuffle, restore_sort_state, save_repeat_mode,
    save_shuffle, save_sort_state,
};
use super::playback::{advance_track, format_ms, play_track_at, PlaybackContext};
use super::preferences;
use super::radio::{
    apply_radio_columns, handle_radio_nearme, is_radio_backend, radio_station_to_track_object,
};
use super::server_dialogs::{
    load_saved_servers, remove_saved_server, show_add_server_dialog, show_auth_dialog,
};
use super::sidebar;
use super::tracklist;

/// Default window dimensions.
const DEFAULT_WIDTH: i32 = 1400;
const DEFAULT_HEIGHT: i32 = 850;

/// Sidebar paned default position (px from left).
const SIDEBAR_POS: i32 = 200;

/// Browser paned default position (px from top of right content area).
const BROWSER_POS: i32 = 220;

/// If the user presses Previous when more than this many ms into a track,
/// restart the current track instead of going back.
const PREV_RESTART_THRESHOLD_MS: u64 = 3000;

/// Build and present the main Tributary window.
pub fn build_window(
    app: &adw::Application,
    rt_handle: tokio::runtime::Handle,
    engine_tx: async_channel::Sender<LibraryEvent>,
    engine_rx: async_channel::Receiver<LibraryEvent>,
) {
    info!("Building main window (Phase 4 — audio + desktop integration)");

    // ── Load and apply persisted preferences ─────────────────────────
    let app_config: Rc<RefCell<preferences::AppConfig>> =
        Rc::new(RefCell::new(preferences::load_config()));

    // ── Load custom CSS ──────────────────────────────────────────────
    load_css();

    // ── Detect USB devices and add to sidebar ────────────────────────
    let usb_devices = crate::device::usb::detect_usb_devices();

    // ── Sidebar sources ────────────────────────────────────────────────
    let sources = super::dummy_data::build_sources();
    let mut sources = sources;

    // Add detected USB devices under a "Devices" category header.
    if !usb_devices.is_empty() {
        sources.push(SourceObject::header("Devices"));
        for dev in &usb_devices {
            let src =
                SourceObject::source(&dev.name, "usb-device", "drive-removable-media-symbolic");
            // Store the mount point path as the server_url for retrieval
            // when the user clicks the device in the sidebar.
            let obj = SourceObject::discovered(
                &dev.name,
                "usb-device",
                &dev.mount_point.to_string_lossy(),
            );
            obj.set_connected(true);
            obj.set_requires_password(false);
            obj.set_icon_name("drive-removable-media-symbolic");
            sources.push(obj);
            let _ = src; // consumed above via discovered()
        }
    }

    // Load manually-added servers from servers.json.
    let saved_servers = load_saved_servers();
    for entry in &saved_servers {
        ensure_category_header_vec(&mut sources, &entry.server_type);
        let src = SourceObject::manual(&entry.name, &entry.server_type, &entry.url);
        sources.push(src);
        info!(
            name = %entry.name,
            url = %entry.url,
            backend = %entry.server_type,
            "Loaded saved server from servers.json"
        );
    }

    // If env vars are set, add pre-configured remote server entries
    // under their respective category headers.
    if let (Ok(url), Ok(_user), Ok(_pass)) = (
        std::env::var("SUBSONIC_URL"),
        std::env::var("SUBSONIC_USER"),
        std::env::var("SUBSONIC_PASS"),
    ) {
        ensure_category_header_vec(&mut sources, "subsonic");
        let src = SourceObject::source("Subsonic (env)", "subsonic", "network-server-symbolic");
        sources.push(src);
        info!(url = %url, "Subsonic server configured via env vars");
    }

    if let (Ok(url), Ok(_key), Ok(_uid)) = (
        std::env::var("JELLYFIN_URL"),
        std::env::var("JELLYFIN_API_KEY"),
        std::env::var("JELLYFIN_USER_ID"),
    ) {
        ensure_category_header_vec(&mut sources, "jellyfin");
        let src = SourceObject::source("Jellyfin (env)", "jellyfin", "network-server-symbolic");
        sources.push(src);
        info!(url = %url, "Jellyfin server configured via env vars");
    }

    if let (Ok(url), Ok(_token)) = (std::env::var("PLEX_URL"), std::env::var("PLEX_TOKEN")) {
        ensure_category_header_vec(&mut sources, "plex");
        let src = SourceObject::source("Plex (env)", "plex", "network-server-symbolic");
        sources.push(src);
        info!(url = %url, "Plex server configured via env vars");
    }

    if let Ok(url) = std::env::var("DAAP_URL") {
        ensure_category_header_vec(&mut sources, "daap");
        let src = SourceObject::source("DAAP (env)", "daap", "network-server-symbolic");
        sources.push(src);
        info!(url = %url, "DAAP server configured via env vars");
    }

    // ── Header Bar with all interactive widgets ──────────────────────
    let hb = header_bar::build_header_bar();

    let scan_spinner = gtk::Spinner::builder()
        .spinning(true)
        .tooltip_text("Scanning library…")
        .build();
    hb.header.pack_end(&scan_spinner);

    // ── Load saved outputs into the output selector popover ──────────
    {
        let saved_outputs = load_saved_outputs();
        for output in &saved_outputs {
            let icon = match output.output_type.as_str() {
                "mpd" => "network-server-symbolic",
                _ => "audio-speakers-symbolic",
            };
            let row = header_bar::build_output_row(&output.name, icon, false);
            hb.output_list.append(&row);
        }
        if !saved_outputs.is_empty() {
            info!(
                count = saved_outputs.len(),
                "Loaded saved outputs from outputs.json"
            );
        }
    }

    // ── Restore persisted playback modes ─────────────────────────────
    {
        let saved_repeat = load_repeat_mode();
        hb.repeat_mode.set(saved_repeat);
        let (icon, tooltip, active) = match saved_repeat {
            RepeatMode::Off => ("media-playlist-repeat-symbolic", "Repeat: Off", false),
            RepeatMode::All => ("media-playlist-repeat-symbolic", "Repeat: All", true),
            RepeatMode::One => ("media-playlist-repeat-song-symbolic", "Repeat: One", true),
        };
        hb.repeat_button.set_icon_name(icon);
        hb.repeat_button.set_tooltip_text(Some(tooltip));
        hb.repeat_button.set_active(active);

        hb.shuffle_button.set_active(load_shuffle());
    }

    // ── Sidebar ──────────────────────────────────────────────────────
    let (
        sidebar_widget,
        sidebar_store,
        sidebar_selection,
        disconnect_rx,
        delete_rx,
        add_button,
        playlist_action_rx,
    ) = sidebar::build_sidebar(&sources);

    // ── Tracklist (starts empty — populated by FullSync) ──────────────
    let empty_tracks: Vec<TrackObject> = Vec::new();
    let (tracklist_widget, track_store, status_label, column_view, sort_model) =
        tracklist::build_tracklist(&empty_tracks);

    // ── Shared playback state ────────────────────────────────────────
    let master_tracks: Rc<RefCell<Vec<TrackObject>>> = Rc::new(RefCell::new(Vec::new()));
    let current_pos: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    let seeking = Rc::new(Cell::new(false));

    // ── Connection guard ─────────────────────────────────────────────
    // Tracks which server URL is currently being connected to, and the
    // sidebar position that was active before the connection attempt.
    // Used to (a) only auto-select on RemoteSync if the source matches
    // the pending connection, and (b) revert the sidebar on failure.
    let pending_connection: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let pre_connect_selection: Rc<Cell<u32>> = Rc::new(Cell::new(1)); // default: local (index 1)

    // ── Per-source track storage ────────────────────────────────────
    // Key: "local" for local filesystem, or server URL for remote.
    let source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let active_source_key: Rc<RefCell<String>> = Rc::new(RefCell::new("local".to_string()));

    // ── Browser (starts empty, updated by FullSync) ──────────────────
    let track_store_for_filter = track_store.clone();
    let status_label_for_filter = status_label.clone();
    let master_for_filter = master_tracks.clone();
    let current_pos_for_filter = current_pos.clone();

    let on_filter = Box::new(
        move |genre: Option<String>,
              artist: Option<String>,
              album: Option<String>,
              search_text: String| {
            let master = master_for_filter.borrow();
            let search_lower = search_text.to_lowercase();
            let matching: Vec<&TrackObject> = master
                .iter()
                .filter(|t| {
                    if let Some(ref g) = genre {
                        if &t.genre() != g {
                            return false;
                        }
                    }
                    if let Some(ref a) = artist {
                        if &t.artist() != a {
                            return false;
                        }
                    }
                    if let Some(ref al) = album {
                        if &t.album() != al {
                            return false;
                        }
                    }
                    // Text search filter — match across title, artist, album, genre.
                    if !search_lower.is_empty() {
                        let matches = t.title().to_lowercase().contains(&search_lower)
                            || t.artist().to_lowercase().contains(&search_lower)
                            || t.album().to_lowercase().contains(&search_lower)
                            || t.genre().to_lowercase().contains(&search_lower);
                        if !matches {
                            return false;
                        }
                    }
                    true
                })
                .collect();

            track_store_for_filter.remove_all();
            let mut snapshot = Vec::new();
            for t in &matching {
                let new_t = TrackObject::new(
                    t.track_number(),
                    &t.title(),
                    t.duration_secs(),
                    &t.artist(),
                    &t.album(),
                    &t.genre(),
                    t.year(),
                    &t.date_modified(),
                    t.bitrate_kbps(),
                    t.sample_rate_hz(),
                    t.play_count(),
                    &t.format(),
                    &t.uri(),
                );
                track_store_for_filter.append(&new_t);
                snapshot.push(new_t);
            }
            tracklist::update_status(&status_label_for_filter, &snapshot);

            // Invalidate playback position — the store indices changed.
            current_pos_for_filter.set(None);
        },
    );

    let (browser_widget, browser_state) = browser::build_browser(&empty_tracks, on_filter);

    // ── Right content ────────────────────────────────────────────────
    let right_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .position(BROWSER_POS)
        .wide_handle(true)
        .vexpand(true)
        .hexpand(true)
        .start_child(&browser_widget)
        .end_child(&tracklist_widget)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .build();

    let main_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .position(SIDEBAR_POS)
        .wide_handle(true)
        .vexpand(true)
        .hexpand(true)
        .start_child(&sidebar_widget)
        .end_child(&right_paned)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&hb.header);
    content.append(&main_paned);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Tributary")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&content)
        .build();

    // ── Start the library engine on tokio ────────────────────────────
    // Use the configured library path from preferences, which defaults
    // to the XDG / platform music directory (e.g. ~/Musique on French
    // systems) via dirs::audio_dir() with a ~/Music fallback.
    let music_dir = std::path::PathBuf::from(&app_config.borrow().library_path);

    let engine_tx_clone = engine_tx.clone();
    rt_handle.spawn(async move {
        match crate::db::connection::init_db().await {
            Ok(db) => {
                let engine = LibraryEngine::new(db, music_dir, engine_tx_clone);
                engine.run().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to initialise database");
                let _ = engine_tx_clone
                    .send(LibraryEvent::Error(format!("Database error: {e}")))
                    .await;
            }
        }
    });

    // ── Start Subsonic backend if configured via env vars ──────────
    if let (Ok(url), Ok(user), Ok(pass)) = (
        std::env::var("SUBSONIC_URL"),
        std::env::var("SUBSONIC_USER"),
        std::env::var("SUBSONIC_PASS"),
    ) {
        let tx = engine_tx.clone();
        rt_handle.spawn(async move {
            info!(server = %url, "Connecting to Subsonic server...");
            match crate::subsonic::SubsonicBackend::connect("Subsonic", &url, &user, &pass).await {
                Ok(backend) => {
                    let tracks: Vec<crate::architecture::models::Track> =
                        backend.all_tracks().await;
                    info!(count = tracks.len(), "Subsonic library fetched");
                    let _ = tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: url.clone(),
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Subsonic connection failed");
                    let _ = tx.send(LibraryEvent::Error(format!("Subsonic: {e}"))).await;
                }
            }
        });
    }

    // ── Start Jellyfin backend if configured via env vars ──────────
    if let (Ok(url), Ok(api_key), Ok(user_id)) = (
        std::env::var("JELLYFIN_URL"),
        std::env::var("JELLYFIN_API_KEY"),
        std::env::var("JELLYFIN_USER_ID"),
    ) {
        let tx = engine_tx.clone();
        rt_handle.spawn(async move {
            info!(server = %url, "Connecting to Jellyfin server...");
            match crate::jellyfin::JellyfinBackend::connect("Jellyfin", &url, &api_key, &user_id)
                .await
            {
                Ok(backend) => {
                    let tracks: Vec<crate::architecture::models::Track> =
                        backend.all_tracks().await;
                    info!(count = tracks.len(), "Jellyfin library fetched");
                    let _ = tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: url.clone(),
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Jellyfin connection failed");
                    let _ = tx.send(LibraryEvent::Error(format!("Jellyfin: {e}"))).await;
                }
            }
        });
    }

    // ── Start Plex backend if configured via env vars ──────────────
    if let (Ok(url), Ok(token)) = (std::env::var("PLEX_URL"), std::env::var("PLEX_TOKEN")) {
        let tx = engine_tx.clone();
        rt_handle.spawn(async move {
            info!(server = %url, "Connecting to Plex server...");
            match crate::plex::PlexBackend::connect("Plex", &url, &token).await {
                Ok(backend) => {
                    let tracks: Vec<crate::architecture::models::Track> =
                        backend.all_tracks().await;
                    info!(count = tracks.len(), "Plex library fetched");
                    let _ = tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: url.clone(),
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Plex connection failed");
                    let _ = tx.send(LibraryEvent::Error(format!("Plex: {e}"))).await;
                }
            }
        });
    }

    // ── Start DAAP backend if configured via env vars ──────────────
    if let Ok(url) = std::env::var("DAAP_URL") {
        let password = std::env::var("DAAP_PASSWORD").ok();
        let tx = engine_tx.clone();
        rt_handle.spawn(async move {
            info!(server = %url, "Connecting to DAAP server...");
            match crate::daap::DaapBackend::connect("DAAP", &url, password.as_deref()).await {
                Ok(backend) => {
                    let tracks: Vec<crate::architecture::models::Track> =
                        backend.all_tracks().await;
                    info!(count = tracks.len(), "DAAP library fetched");
                    let _ = tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: url.clone(),
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "DAAP connection failed");
                    let _ = tx.send(LibraryEvent::Error(format!("DAAP: {e}"))).await;
                }
            }
        });
    }

    // ── mDNS zero-config discovery ─────────────────────────────────
    {
        let discovery_rx = crate::discovery::start_discovery();
        let store = sidebar_store.clone();
        let rt_handle_for_discovery = rt_handle.clone();
        let source_tracks_for_discovery = source_tracks.clone();
        let active_source_key_for_discovery = active_source_key.clone();
        let sidebar_selection_for_discovery = sidebar_selection.clone();
        let track_store_for_discovery = track_store.clone();
        let master_tracks_for_discovery = master_tracks.clone();
        let browser_widget_for_discovery = browser_widget.clone();
        let browser_state_for_discovery = browser_state.clone();
        let status_label_for_discovery = status_label.clone();
        let column_view_for_discovery = column_view.clone();
        let hb_output_list_for_discovery = hb.output_list.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = discovery_rx.recv().await {
                match event {
                    crate::discovery::DiscoveryEvent::Found(server) => {
                        // ── AirPlay devices go to the output selector, not sidebar ──
                        if server.service_type == "airplay" {
                            // Parse host:port from the URL for the output selector.
                            // The URL looks like "http://host:port".
                            let airplay_url = &server.url;
                            let airplay_name = server.name.clone();

                            // Dedup: check if this AirPlay device is already in outputs.
                            let already_in_outputs = {
                                let mut child = hb_output_list_for_discovery.first_child();
                                let mut found = false;
                                while let Some(c) = child {
                                    if let Some(row_box) = c
                                        .first_child()
                                        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                                    {
                                        // Check the label (second child of the row box).
                                        if let Some(label) = row_box
                                            .first_child()
                                            .and_then(|icon| icon.next_sibling())
                                            .and_then(|l| l.downcast::<gtk::Label>().ok())
                                        {
                                            if label.text() == airplay_name {
                                                found = true;
                                                break;
                                            }
                                        }
                                    }
                                    child = c.next_sibling();
                                }
                                found
                            };

                            if !already_in_outputs {
                                info!(
                                    name = %airplay_name,
                                    url = %airplay_url,
                                    "AirPlay receiver discovered — adding to output selector"
                                );
                                let row = header_bar::build_output_row(
                                    &airplay_name,
                                    "network-wireless-symbolic",
                                    false,
                                );
                                // Store the host:port on the ListBoxRow's
                                // widget name so the output selector can
                                // extract it when the user clicks the row.
                                if let Ok(parsed) = url::Url::parse(airplay_url) {
                                    let host = parsed.host_str().unwrap_or("").to_string();
                                    let port = parsed.port().unwrap_or(7000);
                                    // The row is a gtk::Box; when appended to
                                    // the ListBox it gets wrapped in a
                                    // gtk::ListBoxRow — set the name on the
                                    // Box and propagate it to the ListBoxRow
                                    // after appending.
                                    row.set_widget_name(&format!("{host}:{port}"));
                                }
                                hb_output_list_for_discovery.append(&row);
                                // Propagate widget name to the wrapping ListBoxRow.
                                if let Some(last_row) = hb_output_list_for_discovery.last_child() {
                                    if let Some(list_row) =
                                        last_row.downcast_ref::<gtk::ListBoxRow>()
                                    {
                                        if let Some(inner) = list_row.first_child() {
                                            let name = inner.widget_name().to_string();
                                            if !name.is_empty() && name != "GtkBox" {
                                                list_row.set_widget_name(&name);
                                            }
                                        }
                                    }
                                }
                            }
                            continue;
                        }

                        // ── Chromecast devices go to the output selector, not sidebar ──
                        if server.service_type == "chromecast" {
                            // Parse host:port from the cast:// URL.
                            let cast_url = &server.url;
                            let cast_name = server.name.clone();

                            // Dedup: check if this Chromecast is already in outputs.
                            let already_in_outputs = {
                                let mut child = hb_output_list_for_discovery.first_child();
                                let mut found = false;
                                while let Some(c) = child {
                                    if let Some(row_box) = c
                                        .first_child()
                                        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                                    {
                                        if let Some(label) = row_box
                                            .first_child()
                                            .and_then(|icon| icon.next_sibling())
                                            .and_then(|l| l.downcast::<gtk::Label>().ok())
                                        {
                                            if label.text() == cast_name {
                                                found = true;
                                                break;
                                            }
                                        }
                                    }
                                    child = c.next_sibling();
                                }
                                found
                            };

                            if !already_in_outputs {
                                info!(
                                    name = %cast_name,
                                    url = %cast_url,
                                    "Chromecast device discovered — adding to output selector"
                                );
                                let row = header_bar::build_output_row(
                                    &cast_name,
                                    "video-display-symbolic",
                                    false,
                                );
                                // Extract host:port from cast://host:port URL.
                                let host_port =
                                    cast_url.strip_prefix("cast://").unwrap_or(cast_url);
                                row.set_widget_name(host_port);
                                hb_output_list_for_discovery.append(&row);
                                // Propagate widget name to the wrapping ListBoxRow.
                                if let Some(last_row) = hb_output_list_for_discovery.last_child() {
                                    if let Some(list_row) =
                                        last_row.downcast_ref::<gtk::ListBoxRow>()
                                    {
                                        if let Some(inner) = list_row.first_child() {
                                            let name = inner.widget_name().to_string();
                                            if !name.is_empty() && name != "GtkBox" {
                                                list_row.set_widget_name(&name);
                                            }
                                        }
                                    }
                                }
                            }
                            continue;
                        }

                        // Dedup: check if this URL is already in the sidebar.
                        let already_exists = (0..store.n_items()).any(|i| {
                            store
                                .item(i)
                                .and_downcast_ref::<SourceObject>()
                                .is_some_and(|s| s.server_url() == server.url)
                        });
                        if already_exists {
                            continue;
                        }

                        info!(
                            name = %server.name,
                            url = %server.url,
                            backend = %server.service_type,
                            "Adding discovered server to sidebar"
                        );

                        // Insert under the correct category header.
                        let insert_pos = ensure_category_header_store(&store, &server.service_type);
                        let src = SourceObject::discovered(
                            &server.name,
                            &server.service_type,
                            &server.url,
                        );

                        // Apply requires_password if already known from discovery.
                        if let Some(rp) = server.requires_password {
                            src.set_requires_password(rp);
                        }

                        store.insert(insert_pos, &src);

                        // For DAAP servers, probe whether a password is required
                        // in the background and update the sidebar item.
                        if server.service_type == "daap" && server.requires_password.is_none() {
                            let probe_url = server.url.clone();
                            let store_for_probe = store.clone();
                            let (probe_tx, probe_rx) = async_channel::bounded::<Option<bool>>(1);

                            rt_handle_for_discovery.spawn(async move {
                                let result =
                                    crate::daap::client::DaapClient::probe_requires_password(
                                        &probe_url,
                                    )
                                    .await;
                                let _ = probe_tx.send(result).await;
                            });

                            let probe_server_url = server.url.clone();
                            glib::MainContext::default().spawn_local(async move {
                                if let Ok(Some(requires_pw)) = probe_rx.recv().await {
                                    // Find the source in the store and update it.
                                    for i in 0..store_for_probe.n_items() {
                                        if let Some(src) = store_for_probe
                                            .item(i)
                                            .and_downcast_ref::<SourceObject>()
                                        {
                                            if src.server_url() == probe_server_url
                                                && !src.connected()
                                            {
                                                src.set_requires_password(requires_pw);
                                                // Force rebind by remove + re-insert.
                                                let src = src.clone();
                                                store_for_probe.remove(i);
                                                store_for_probe.insert(i, &src);
                                                break;
                                            }
                                        }
                                    }
                                }
                            });
                        }
                    }

                    crate::discovery::DiscoveryEvent::Lost { url, service_type } => {
                        // ── Chromecast devices: remove from output selector ──
                        if service_type == "chromecast" {
                            info!(
                                url = %url,
                                "Chromecast device lost — removing from output selector"
                            );
                            let mut child = hb_output_list_for_discovery.first_child();
                            let mut row_idx = 0i32;
                            while let Some(c) = child {
                                let next = c.next_sibling();
                                if row_idx > 0 {
                                    if let Some(row_box) = c
                                        .first_child()
                                        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                                    {
                                        if let Some(icon) = row_box
                                            .first_child()
                                            .and_then(|i| i.downcast::<gtk::Image>().ok())
                                        {
                                            if icon
                                                .icon_name()
                                                .is_some_and(|n| n == "video-display-symbolic")
                                            {
                                                if let Some(list_row) =
                                                    c.downcast_ref::<gtk::ListBoxRow>()
                                                {
                                                    // Match by widget name (host:port).
                                                    let row_hp = list_row.widget_name().to_string();
                                                    let lost_hp =
                                                        url.strip_prefix("cast://").unwrap_or(&url);
                                                    if row_hp == lost_hp {
                                                        hb_output_list_for_discovery
                                                            .remove(list_row);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                row_idx += 1;
                                child = next;
                            }
                            continue;
                        }

                        // ── AirPlay devices: remove from output selector ──
                        if service_type == "airplay" {
                            info!(
                                url = %url,
                                "AirPlay receiver lost — removing from output selector"
                            );
                            // Walk the ListBox children and remove the row whose
                            // label matches the lost device.  AirPlay URLs are
                            // formatted as "http://host:port" by discovery; we
                            // can't match by URL on the row (not stored), so we
                            // remove by checking all non-"My Computer" rows.
                            // This is best-effort — if the name changed between
                            // discovery and loss events the row won't be found.
                            let mut child = hb_output_list_for_discovery.first_child();
                            let mut row_idx = 0i32;
                            while let Some(c) = child {
                                let next = c.next_sibling();
                                // Skip index 0 ("My Computer") — never remove it.
                                if row_idx > 0 {
                                    if let Some(row_box) = c
                                        .first_child()
                                        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                                    {
                                        // Check the icon — AirPlay rows use
                                        // "network-wireless-symbolic".
                                        if let Some(icon) = row_box
                                            .first_child()
                                            .and_then(|i| i.downcast::<gtk::Image>().ok())
                                        {
                                            if icon
                                                .icon_name()
                                                .is_some_and(|n| n == "network-wireless-symbolic")
                                            {
                                                // This is an AirPlay row — remove it.
                                                // (If multiple AirPlay devices are present,
                                                // removing by icon is imprecise; a future
                                                // enhancement could store host:port on the
                                                // row widget for precise matching.)
                                                if let Some(list_row) =
                                                    c.downcast_ref::<gtk::ListBoxRow>()
                                                {
                                                    hb_output_list_for_discovery.remove(list_row);
                                                }
                                            }
                                        }
                                    }
                                }
                                row_idx += 1;
                                child = next;
                            }
                            continue;
                        }

                        info!(
                            url = %url,
                            backend = %service_type,
                            "Removing lost server from sidebar"
                        );

                        // Find the sidebar entry for this URL.
                        for i in 0..store.n_items() {
                            if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
                                if src.server_url() == url {
                                    // Never auto-remove manually-added servers.
                                    if src.manually_added() {
                                        break;
                                    }
                                    // If connected and still the active source,
                                    // switch to local before removing.
                                    let was_active =
                                        *active_source_key_for_discovery.borrow() == url;

                                    if src.connected() {
                                        // Remove from source_tracks map.
                                        source_tracks_for_discovery.borrow_mut().remove(&url);
                                    }

                                    // Remove the sidebar entry.
                                    store.remove(i);

                                    // Clean up empty category header.
                                    let category = category_for_backend(&service_type);
                                    remove_empty_category_header(&store, category);

                                    // If this was the active source, switch to local.
                                    if was_active {
                                        *active_source_key_for_discovery.borrow_mut() =
                                            "local".to_string();
                                        sidebar_selection_for_discovery.set_selected(1);

                                        let st = source_tracks_for_discovery.borrow();
                                        let local_tracks =
                                            st.get("local").cloned().unwrap_or_default();
                                        display_tracks(
                                            &local_tracks,
                                            &track_store_for_discovery,
                                            &master_tracks_for_discovery,
                                            &browser_widget_for_discovery,
                                            &browser_state_for_discovery,
                                            &status_label_for_discovery,
                                            &column_view_for_discovery,
                                        );
                                    }

                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // ── Wire "+" add-server button ──────────────────────────────────
    {
        let win = window.clone();
        let store = sidebar_store.clone();
        let engine_tx = engine_tx.clone();
        let rt_handle = rt_handle.clone();
        add_button.connect_clicked(move |_| {
            show_add_server_dialog(&win, &store, &engine_tx, &rt_handle);
        });
    }

    // ── Wire output selector "+" button (now that window exists) ─────
    {
        let win = window.clone();
        let output_list = hb.output_list.clone();
        if let Some(popover) = hb.output_button.popover() {
            if let Some(popover_box) = popover.child().and_then(|c| c.downcast::<gtk::Box>().ok()) {
                if let Some(add_btn) = popover_box
                    .last_child()
                    .and_then(|c| c.downcast::<gtk::Button>().ok())
                {
                    add_btn.connect_clicked(move |_| {
                        show_add_output_dialog(&win, &output_list);
                    });
                }
            }
        }
    }

    // ── Manual server delete (trash) handler ────────────────────────
    {
        let sidebar_store = sidebar_store.clone();
        let sidebar_selection = sidebar_selection.clone();
        let source_tracks = source_tracks.clone();
        let active_source_key = active_source_key.clone();
        let track_store = track_store.clone();
        let master_tracks = master_tracks.clone();
        let browser_widget = browser_widget.clone();
        let browser_state = browser_state.clone();
        let status_label = status_label.clone();
        let column_view = column_view.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(source_key) = delete_rx.recv().await {
                info!(source = %source_key, "Manual server delete requested");

                // Remove from servers.json.
                remove_saved_server(&source_key);

                // Remove from source_tracks map.
                source_tracks.borrow_mut().remove(&source_key);

                // Remove from sidebar.
                for i in 0..sidebar_store.n_items() {
                    if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
                        if src.server_url() == source_key {
                            let backend = src.backend_type();
                            sidebar_store.remove(i);
                            let category = category_for_backend(&backend);
                            remove_empty_category_header(&sidebar_store, category);
                            break;
                        }
                    }
                }

                // If this was the active source, switch to "local".
                if *active_source_key.borrow() == source_key {
                    *active_source_key.borrow_mut() = "local".to_string();
                    sidebar_selection.set_selected(1);

                    let st = source_tracks.borrow();
                    let local_tracks = st.get("local").cloned().unwrap_or_default();
                    display_tracks(
                        &local_tracks,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                }
            }
        });
    }

    // ── DAAP disconnect (eject) handler ─────────────────────────────
    {
        let sidebar_store = sidebar_store.clone();
        let sidebar_selection = sidebar_selection.clone();
        let source_tracks = source_tracks.clone();
        let active_source_key = active_source_key.clone();
        let track_store = track_store.clone();
        let master_tracks = master_tracks.clone();
        let browser_widget = browser_widget.clone();
        let browser_state = browser_state.clone();
        let status_label = status_label.clone();
        let column_view = column_view.clone();
        let rt_handle = rt_handle.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(source_key) = disconnect_rx.recv().await {
                info!(source = %source_key, "DAAP disconnect requested");

                // Best-effort logout: find the logout URL on the SourceObject.
                for i in 0..sidebar_store.n_items() {
                    if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
                        if src.server_url() == source_key {
                            let logout_url = src.logout_url();
                            if !logout_url.is_empty() {
                                let rt = rt_handle.clone();
                                rt.spawn(async move {
                                    let client = reqwest::Client::builder()
                                        .timeout(std::time::Duration::from_secs(5))
                                        .build()
                                        .unwrap_or_default();
                                    let _ = client.get(&logout_url).send().await;
                                });
                            }
                            break;
                        }
                    }
                }

                // 1. Remove from source_tracks map.
                source_tracks.borrow_mut().remove(&source_key);

                // 2. Reset the sidebar item back to discovered (unconnected)
                //    state instead of removing it entirely.
                for i in 0..sidebar_store.n_items() {
                    if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
                        if src.server_url() == source_key {
                            src.set_connected(false);
                            src.set_connecting(false);
                            src.set_logout_url("");
                            src.set_icon_name("network-server-symbolic");
                            // Force rebind by remove + re-insert.
                            let src = src.clone();
                            sidebar_store.remove(i);
                            sidebar_store.insert(i, &src);
                            break;
                        }
                    }
                }

                // 3. If this was the active source, switch to "local".
                if *active_source_key.borrow() == source_key {
                    *active_source_key.borrow_mut() = "local".to_string();

                    // Select the local source in the sidebar (index 1, after header).
                    sidebar_selection.set_selected(1);

                    // Display local tracks.
                    let st = source_tracks.borrow();
                    let local_tracks = st.get("local").cloned().unwrap_or_default();
                    display_tracks(
                        &local_tracks,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                }
            }
        });
    }

    // ── Sidebar selection: source switching + auth dialog ───────────
    let sidebar_store_for_events = sidebar_store.clone();
    let sidebar_sel_for_events = sidebar_selection.clone();
    let pending_connection_for_events = pending_connection.clone();
    {
        let sel = sidebar_selection.clone();
        let engine_tx = engine_tx.clone();
        let rt_handle = rt_handle.clone();
        let win = window.clone();
        let track_store = track_store.clone();
        let master_tracks = master_tracks.clone();
        let source_tracks = source_tracks.clone();
        let active_source_key = active_source_key.clone();
        let browser_widget = browser_widget.clone();
        let browser_state = browser_state.clone();
        let status_label = status_label.clone();
        let column_view = column_view.clone();
        let current_pos = current_pos.clone();
        let app_config = app_config.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            let Some(item) = sel.selected_item() else {
                return;
            };
            let Some(src) = item.downcast_ref::<SourceObject>() else {
                return;
            };
            if src.is_header() {
                return;
            }

            // ── Connection guard: ignore clicks while a connection is pending ──
            // This prevents duplicate auth dialogs and duplicate connection
            // tasks when the user clicks a server that is still connecting
            // (applies to all network sources: Subsonic, Jellyfin, Plex, DAAP).
            if pending_connection.borrow().is_some() {
                let pending_url = pending_connection.borrow().clone();
                if let Some(ref pu) = pending_url {
                    // If clicking the same server that's already connecting, just ignore.
                    if src.server_url() == *pu {
                        return;
                    }
                    // If clicking a different server while one is connecting,
                    // also ignore — let the first connection finish first.
                    if src.connecting() || (!src.connected() && !src.server_url().is_empty()) {
                        return;
                    }
                }
            }

            let backend_type = src.backend_type();

            // Determine the source key.
            // Remote sources use their server URL; local sources with a
            // specific backend_type (radio, playlist) use that type as
            // the key so they don't all collapse into "local".
            let url = src.server_url();
            let key = if !url.is_empty() {
                url.clone()
            } else if backend_type == "local" || backend_type.is_empty() {
                "local".to_string()
            } else {
                backend_type.clone()
            };

            // ── Local source: switch to local view ───────────────────
            if key == "local" {
                *active_source_key.borrow_mut() = "local".to_string();

                // Restore music column layout if coming from radio.
                apply_radio_columns(&column_view, false);
                let cfg = app_config.borrow();
                preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
                preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
                drop(cfg);

                let st = source_tracks.borrow();
                let local_tracks = st.get("local").cloned().unwrap_or_default();
                display_tracks(
                    &local_tracks,
                    &track_store,
                    &master_tracks,
                    &browser_widget,
                    &browser_state,
                    &status_label,
                    &column_view,
                );
                current_pos.set(None);
                return;
            }

            // ── Playlist source: fetch playlist tracks ───────────────
            if backend_type == "playlist" || backend_type == "smart-playlist" {
                let playlist_id = src.playlist_id();
                if playlist_id.is_empty() {
                    return;
                }

                *active_source_key.borrow_mut() = format!("playlist:{playlist_id}");

                // Restore music column layout (not radio).
                apply_radio_columns(&column_view, false);
                // Restore browser visibility from config.
                let cfg = app_config.borrow();
                preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
                drop(cfg);

                let is_smart = backend_type == "smart-playlist";
                let rt_handle = rt_handle.clone();
                let track_store = track_store.clone();
                let master_tracks = master_tracks.clone();
                let browser_widget = browser_widget.clone();
                let browser_state = browser_state.clone();
                let status_label = status_label.clone();
                let column_view = column_view.clone();
                let current_pos = current_pos.clone();
                let pid = playlist_id.clone();

                let (tracks_tx, tracks_rx) = async_channel::bounded::<String>(1);

                rt_handle.spawn(async move {
                    match crate::db::connection::init_db().await {
                        Ok(db) => {
                            let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                            let tracks = if is_smart {
                                mgr.evaluate_smart_playlist(&pid).await
                            } else {
                                mgr.get_playlist_tracks(&pid).await
                            };
                            match tracks {
                                Ok(models) => {
                                    let arch_tracks: Vec<crate::architecture::models::Track> =
                                        models
                                            .iter()
                                            .map(crate::local::engine::db_model_to_track)
                                            .collect();
                                    let json =
                                        serde_json::to_string(&arch_tracks).unwrap_or_default();
                                    let _ = tracks_tx.send(json).await;
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "Failed to load playlist tracks");
                                    let _ = tracks_tx.send("[]".to_string()).await;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to open DB for playlist");
                            let _ = tracks_tx.send("[]".to_string()).await;
                        }
                    }
                });

                glib::MainContext::default().spawn_local(async move {
                    if let Ok(json) = tracks_rx.recv().await {
                        let tracks: Vec<crate::architecture::models::Track> =
                            serde_json::from_str(&json).unwrap_or_default();
                        let objects: Vec<TrackObject> =
                            tracks.iter().map(arch_track_to_object).collect();
                        display_tracks(
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
                return;
            }

            // ── USB device source: scan and display music files ──────
            if backend_type == "usb-device" {
                let mount_point = src.server_url();
                if mount_point.is_empty() {
                    return;
                }

                *active_source_key.borrow_mut() = key.clone();

                // Restore music column layout if coming from radio.
                apply_radio_columns(&column_view, false);
                let cfg = app_config.borrow();
                preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
                preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
                drop(cfg);

                // Check if we already scanned this device.
                let already_scanned = source_tracks.borrow().contains_key(&key);
                if already_scanned {
                    let st = source_tracks.borrow();
                    let tracks = st.get(&key).cloned().unwrap_or_default();
                    display_tracks(
                        &tracks,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                    current_pos.set(None);
                } else {
                    // Scan on a background thread to avoid blocking UI.
                    let mount = mount_point.clone();
                    let source_key = key.clone();
                    let track_store = track_store.clone();
                    let master_tracks = master_tracks.clone();
                    let source_tracks = source_tracks.clone();
                    let browser_widget = browser_widget.clone();
                    let browser_state = browser_state.clone();
                    let status_label = status_label.clone();
                    let column_view = column_view.clone();
                    let active_source_key = active_source_key.clone();
                    let current_pos = current_pos.clone();

                    // Serialisable track data for cross-thread transfer.
                    // TrackObject is a GObject (not Send), so we send
                    // tuples of raw data from the background thread and
                    // construct TrackObjects on the GTK main thread.
                    type ScanRow = (
                        u32,
                        String,
                        u64,
                        String,
                        String,
                        String,
                        i32,
                        String,
                        u32,
                        u32,
                        String,
                        String,
                    );
                    let (scan_tx, scan_rx) = async_channel::unbounded::<ScanRow>();

                    // Background thread: walk the device filesystem.
                    std::thread::spawn(move || {
                        let mount_path = std::path::Path::new(&mount);
                        for entry in walkdir::WalkDir::new(mount_path)
                            .follow_links(true)
                            .into_iter()
                            .filter_map(|e| e.ok())
                        {
                            let path = entry.path();
                            if !path.is_file() {
                                continue;
                            }
                            if !crate::local::tag_parser::is_audio_file(path) {
                                continue;
                            }
                            if let Ok(parsed) = crate::local::tag_parser::parse_audio_file(path) {
                                let uri = url::Url::from_file_path(path)
                                    .map(|u| u.to_string())
                                    .unwrap_or_default();
                                let row: ScanRow = (
                                    parsed.track_number.unwrap_or(0),
                                    parsed.title,
                                    parsed.duration_secs.unwrap_or(0),
                                    parsed.artist_name,
                                    parsed.album_title,
                                    parsed.genre.unwrap_or_else(|| "Unknown".to_string()),
                                    parsed.year.unwrap_or(0),
                                    parsed.date_modified.format("%Y-%m-%d").to_string(),
                                    parsed.bitrate_kbps.unwrap_or(0),
                                    parsed.sample_rate_hz.unwrap_or(0),
                                    parsed.format,
                                    uri,
                                );
                                let _ = scan_tx.try_send(row);
                            }
                        }
                        // Close the sender to signal completion.
                        drop(scan_tx);
                    });

                    // Collect results on the GTK main thread.
                    glib::MainContext::default().spawn_local(async move {
                        let mut objects = Vec::new();
                        while let Ok(row) = scan_rx.recv().await {
                            let obj = TrackObject::new(
                                row.0, &row.1, row.2, &row.3, &row.4, &row.5, row.6, &row.7, row.8,
                                row.9, 0, &row.10, &row.11,
                            );
                            objects.push(obj);
                        }

                        // Store for future source switches.
                        source_tracks
                            .borrow_mut()
                            .insert(source_key.clone(), objects.clone());

                        // Display if still the active source.
                        if *active_source_key.borrow() == source_key {
                            display_tracks(
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
                return;
            }

            // ── Radio source: fetch stations ────────────────────────
            if is_radio_backend(&backend_type) {
                *active_source_key.borrow_mut() = backend_type.clone();

                // Switch to radio column layout.
                apply_radio_columns(&column_view, true);
                // Hide browser for radio.
                browser_widget.set_visible(false);

                // Clear the tracklist immediately so local songs don't
                // show while radio stations are loading asynchronously.
                track_store.remove_all();
                tracklist::update_status(&status_label, &[]);
                *master_tracks.borrow_mut() = Vec::new();
                current_pos.set(None);

                // Handle "Stations Near Me" with geo consent.
                if backend_type == "radio-nearme" {
                    let app_config = app_config.clone();
                    let rt_handle = rt_handle.clone();
                    let track_store = track_store.clone();
                    let master_tracks = master_tracks.clone();
                    let browser_widget = browser_widget.clone();
                    let browser_state = browser_state.clone();
                    let status_label = status_label.clone();
                    let column_view = column_view.clone();
                    let active_source_key = active_source_key.clone();
                    let sidebar_selection = sel.clone();
                    let current_pos = current_pos.clone();
                    let source_tracks = source_tracks.clone();
                    let win = win.clone();

                    handle_radio_nearme(
                        &win,
                        app_config,
                        rt_handle,
                        track_store,
                        master_tracks,
                        browser_widget,
                        browser_state,
                        status_label,
                        column_view,
                        active_source_key,
                        sidebar_selection,
                        current_pos,
                        source_tracks,
                    );
                } else {
                    // Top Clicked or Top Voted — fetch directly.
                    let bt = backend_type.clone();
                    let rt_handle = rt_handle.clone();
                    let track_store = track_store.clone();
                    let master_tracks = master_tracks.clone();
                    let browser_widget = browser_widget.clone();
                    let browser_state = browser_state.clone();
                    let status_label = status_label.clone();
                    let column_view = column_view.clone();
                    let current_pos = current_pos.clone();

                    let (stations_tx, stations_rx) = async_channel::bounded::<String>(1);

                    rt_handle.spawn(async move {
                        let client = crate::radio::RadioBrowserClient::new();
                        let stations = if bt == "radio-topclick" {
                            client.fetch_top_click(None).await
                        } else {
                            client.fetch_top_vote(None).await
                        };
                        let json = serde_json::to_string(&stations).unwrap_or_default();
                        let _ = stations_tx.send(json).await;
                    });

                    glib::MainContext::default().spawn_local(async move {
                        if let Ok(json) = stations_rx.recv().await {
                            let stations: Vec<crate::radio::RadioStation> =
                                serde_json::from_str(&json).unwrap_or_default();
                            let objects: Vec<TrackObject> =
                                stations.iter().map(radio_station_to_track_object).collect();
                            display_tracks(
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
                return;
            }

            // ── Connected source: switch view ───────────────────────
            if src.connected() {
                *active_source_key.borrow_mut() = key.clone();

                // Restore music column layout if coming from radio.
                apply_radio_columns(&column_view, false);
                // Restore browser visibility from config.
                let cfg = app_config.borrow();
                preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
                drop(cfg);

                let st = source_tracks.borrow();
                let tracks = st.get(&key).cloned().unwrap_or_default();
                display_tracks(
                    &tracks,
                    &track_store,
                    &master_tracks,
                    &browser_widget,
                    &browser_state,
                    &status_label,
                    &column_view,
                );
                current_pos.set(None);
                return;
            }

            // ── Discovered (unauthenticated) ────────────────────────
            let server_name = src.name();
            let server_url = src.server_url();
            let engine_tx = engine_tx.clone();
            let rt_handle = rt_handle.clone();
            let win = win.clone();
            let sidebar_store = sidebar_store.clone();
            let selected_pos = sel.selected();

            let backend_type = src.backend_type();
            let name_for_closure = server_name.clone();
            let url_for_closure = server_url.clone();
            let requires_password = src.requires_password();

            // For passwordless DAAP servers, bypass the dialog entirely
            // and connect directly.
            if backend_type == "daap" && !requires_password {
                // Save the current selection so we can revert on failure.
                pre_connect_selection.set(selected_pos);
                *pending_connection.borrow_mut() = Some(url_for_closure.clone());

                // Mark as connecting → spinner in sidebar.
                if let Some(src) = sidebar_store
                    .item(selected_pos)
                    .and_downcast_ref::<SourceObject>()
                {
                    src.set_connecting(true);
                    let src = src.clone();
                    sidebar_store.remove(selected_pos);
                    sidebar_store.insert(selected_pos, &src);
                }

                let (fail_tx, fail_rx) = async_channel::bounded::<()>(1);
                let sidebar_store_for_fail = sidebar_store.clone();
                let sel_for_fail = sel.clone();
                let pre_sel = pre_connect_selection.get();
                let pending_for_fail = pending_connection.clone();
                glib::MainContext::default().spawn_local(async move {
                    if fail_rx.recv().await.is_ok() {
                        if let Some(src) = sidebar_store_for_fail
                            .item(selected_pos)
                            .and_downcast_ref::<SourceObject>()
                        {
                            src.set_connecting(false);
                            let src = src.clone();
                            sidebar_store_for_fail.remove(selected_pos);
                            sidebar_store_for_fail.insert(selected_pos, &src);
                        }
                        // Revert sidebar to the previous selection.
                        sel_for_fail.set_selected(pre_sel);
                        // Clear the pending connection guard.
                        *pending_for_fail.borrow_mut() = None;
                    }
                });

                let engine_tx = engine_tx.clone();
                let server_url = url_for_closure.clone();
                let server_name = name_for_closure.clone();
                rt_handle.spawn(async move {
                    info!(server = %server_url, "Connecting to passwordless DAAP server...");
                    match crate::daap::DaapBackend::connect(&server_name, &server_url, None).await {
                        Ok(backend) => {
                            let tracks = backend.all_tracks().await;
                            info!(count = tracks.len(), "DAAP library fetched (no password)");
                            let _ = engine_tx
                                .send(LibraryEvent::RemoteSync {
                                    source_key: server_url,
                                    tracks,
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "DAAP connection failed");
                            let _ = engine_tx
                                .send(LibraryEvent::Error(format!("DAAP auth failed: {e}")))
                                .await;
                            let _ = fail_tx.send(()).await;
                        }
                    }
                });
                return;
            }

            let password_only = backend_type == "daap";

            // Clone Rc's before moving into the auth dialog closure so
            // the outer `Fn` closure can be called multiple times.
            let pre_connect_for_auth = pre_connect_selection.clone();
            let pending_for_auth = pending_connection.clone();
            let sel_for_auth = sel.clone();

            show_auth_dialog(
                &win,
                &server_name,
                &server_url,
                password_only,
                move |user, pass| {
                    let engine_tx = engine_tx.clone();
                    let server_url = url_for_closure.clone();
                    let server_name = name_for_closure.clone();
                    let backend_type = backend_type.clone();

                    // Mark as connecting → spinner in sidebar.
                    if let Some(src) = sidebar_store
                        .item(selected_pos)
                        .and_downcast_ref::<SourceObject>()
                    {
                        src.set_connecting(true);
                        let src = src.clone();
                        sidebar_store.remove(selected_pos);
                        sidebar_store.insert(selected_pos, &src);
                    }
                    // Save the current selection so we can revert on failure.
                    pre_connect_for_auth.set(selected_pos);
                    *pending_for_auth.borrow_mut() = Some(server_url.clone());

                    // One-shot to signal failure back to the main thread so we
                    // can clear the spinner (GObjects are not Send).
                    let (fail_tx, fail_rx) = async_channel::bounded::<()>(1);
                    let sidebar_store_for_fail = sidebar_store.clone();
                    let sel_for_fail = sel_for_auth.clone();
                    let pre_sel = pre_connect_for_auth.get();
                    let pending_for_fail = pending_for_auth.clone();
                    glib::MainContext::default().spawn_local(async move {
                        if fail_rx.recv().await.is_ok() {
                            if let Some(src) = sidebar_store_for_fail
                                .item(selected_pos)
                                .and_downcast_ref::<SourceObject>()
                            {
                                src.set_connecting(false);
                                let src = src.clone();
                                sidebar_store_for_fail.remove(selected_pos);
                                sidebar_store_for_fail.insert(selected_pos, &src);
                            }
                            // Revert sidebar to the previous selection.
                            sel_for_fail.set_selected(pre_sel);
                            // Clear the pending connection guard.
                            *pending_for_fail.borrow_mut() = None;
                        }
                    });

                    rt_handle.spawn(async move {
                        let result: Result<
                            Vec<crate::architecture::models::Track>,
                            crate::architecture::error::BackendError,
                        > = match backend_type.as_str() {
                            "jellyfin" => {
                                info!(server = %server_url, "Authenticating with Jellyfin...");
                                match crate::jellyfin::client::JellyfinClient::authenticate(
                                    &server_url,
                                    &user,
                                    &pass,
                                )
                                .await
                                {
                                    Ok(client) => {
                                        match crate::jellyfin::JellyfinBackend::from_client(
                                            &server_name,
                                            client,
                                        )
                                        .await
                                        {
                                            Ok(backend) => Ok(backend.all_tracks().await),
                                            Err(e) => Err(e),
                                        }
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                            "plex" => {
                                info!(server = %server_url, "Authenticating with Plex...");
                                match crate::plex::client::PlexClient::authenticate(
                                    &server_url,
                                    &user,
                                    &pass,
                                )
                                .await
                                {
                                    Ok(client) => {
                                        match crate::plex::PlexBackend::from_client(
                                            &server_name,
                                            client,
                                        )
                                        .await
                                        {
                                            Ok(backend) => Ok(backend.all_tracks().await),
                                            Err(e) => Err(e),
                                        }
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                            "daap" => {
                                info!(server = %server_url, "Connecting to DAAP server...");
                                let password = if pass.is_empty() {
                                    None
                                } else {
                                    Some(pass.as_str())
                                };
                                match crate::daap::DaapBackend::connect(
                                    &server_name,
                                    &server_url,
                                    password,
                                )
                                .await
                                {
                                    Ok(backend) => Ok(backend.all_tracks().await),
                                    Err(e) => Err(e),
                                }
                            }
                            _ => {
                                // Default: Subsonic
                                info!(server = %server_url, "Authenticating with Subsonic...");
                                match crate::subsonic::SubsonicBackend::connect(
                                    &server_name,
                                    &server_url,
                                    &user,
                                    &pass,
                                )
                                .await
                                {
                                    Ok(backend) => Ok(backend.all_tracks().await),
                                    Err(e) => Err(e),
                                }
                            }
                        };

                        match result {
                            Ok(tracks) => {
                                info!(
                                    backend = %backend_type,
                                    count = tracks.len(),
                                    "Remote library fetched"
                                );
                                let _ = engine_tx
                                    .send(LibraryEvent::RemoteSync {
                                        source_key: server_url,
                                        tracks,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                tracing::error!(
                                    backend = %backend_type,
                                    error = %e,
                                    "Authentication failed"
                                );
                                let _ = engine_tx
                                    .send(LibraryEvent::Error(format!(
                                        "{backend_type} auth failed: {e}"
                                    )))
                                    .await;
                                let _ = fail_tx.send(()).await;
                            }
                        }
                    });
                },
            );
        });
    }

    // ═══════════════════════════════════════════════════════════════════
    // Phase 4: Audio Player + Desktop Integration
    // ═══════════════════════════════════════════════════════════════════

    // Present the window EARLY so that the native OS surface is
    // allocated.  On Windows, souvlaki needs the HWND which only
    // exists after the window has been realized and mapped.
    window.present();
    info!("Main window presented");

    // ── Create GStreamer player ──────────────────────────────────────
    let (player, player_rx) = match crate::audio::Player::new() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create audio player — playback disabled");
            setup_library_events(
                engine_rx,
                track_store,
                status_label,
                master_tracks,
                source_tracks,
                active_source_key,
                &browser_widget,
                browser_state,
                &column_view,
                sidebar_store_for_events,
                sidebar_sel_for_events,
                scan_spinner,
                pending_connection_for_events.clone(),
            );
            return;
        }
    };
    // Grab the event sender before wrapping in LocalOutput — needed
    // to give MpdOutput (and future outputs) a sender into the same
    // player_rx event loop.
    let event_sender = player.event_sender();

    // Wrap the raw Player in LocalOutput → Box<dyn AudioOutput>.
    let local_output = LocalOutput::new(player);
    let active_output: Rc<RefCell<Box<dyn AudioOutput>>> =
        Rc::new(RefCell::new(Box::new(local_output)));

    // Parking slot for the local output when an MPD output is active.
    // When switching to MPD we move the LocalOutput out of active_output
    // into this slot; when switching back we move it back.
    let parked_local: Rc<RefCell<Option<Box<dyn AudioOutput>>>> = Rc::new(RefCell::new(None));

    // Sync the volume slider to the output's persisted volume.
    hb.volume_adj.set_value(active_output.borrow().volume());

    // ── Extract native window handle (HWND on Windows) ──────────────
    let hwnd = extract_hwnd(&window);

    // ── Create OS media controls ────────────────────────────────────
    let media_ctrl: Rc<RefCell<Option<crate::desktop_integration::MediaController>>> =
        match crate::desktop_integration::MediaController::new(hwnd) {
            Ok((ctrl, media_rx)) => {
                let active_output = active_output.clone();
                glib::MainContext::default().spawn_local(async move {
                    while let Ok(action) = media_rx.recv().await {
                        info!(?action, "OS media key");
                        match action {
                            MediaAction::Play => active_output.borrow().play(),
                            MediaAction::Pause => active_output.borrow().pause(),
                            MediaAction::Toggle => active_output.borrow().toggle_play_pause(),
                            MediaAction::Stop => active_output.borrow().stop(),
                            MediaAction::Next => {
                                // Media key next/previous cannot call
                                // advance_track directly because we don't
                                // have the PlaybackContext here.  The OS
                                // media controls are best-effort — the
                                // header bar buttons are the primary UI.
                            }
                            MediaAction::Previous => {}
                        }
                    }
                });
                Rc::new(RefCell::new(Some(ctrl)))
            }
            Err(e) => {
                warn!(error = %e, "Media controls unavailable — media keys disabled");
                Rc::new(RefCell::new(None))
            }
        };

    // ── Wire play/pause button ──────────────────────────────────────
    // If nothing is playing, start from track 0 (or random if shuffle).
    {
        let active_output = active_output.clone();
        let parked_local = parked_local.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sort_model = sort_model.clone();
        let current_pos = current_pos.clone();
        let shuffle = hb.shuffle_button.clone();

        hb.play_button.connect_clicked(move |_| {
            if current_pos.get().is_some() {
                // Already have a track loaded — just toggle.
                active_output.borrow().toggle_play_pause();
            } else if sort_model.n_items() > 0 {
                // Nothing playing — start from the list.
                let pos = if shuffle.is_active() {
                    fastrand::u32(..sort_model.n_items())
                } else {
                    0
                };
                play_track_at(
                    pos,
                    &PlaybackContext {
                        model: sort_model.clone(),
                        active_output: active_output.clone(),
                        parked_local: parked_local.clone(),
                        album_art: album_art.clone(),
                        title_label: title_label.clone(),
                        artist_label: artist_label.clone(),
                        media_ctrl: media_ctrl.clone(),
                        current_pos: current_pos.clone(),
                    },
                );
            }
        });
    }

    // ── Persist repeat/shuffle on change ────────────────────────────
    {
        let mode = hb.repeat_mode.clone();
        hb.repeat_button.connect_clicked(move |_| {
            save_repeat_mode(mode.get());
        });
    }
    hb.shuffle_button.connect_toggled(move |btn| {
        save_shuffle(btn.is_active());
    });

    // ── Wire output selector row-click handler ──────────────────────
    // Clicking a row in the output popover swaps the active output.
    // Index 0 = "My Computer" (LocalOutput), index 1+ = saved MPD outputs.
    {
        let active_output = active_output.clone();
        let parked_local = parked_local.clone();
        let event_sender = event_sender.clone();
        let volume_scale = hb.volume_scale.clone();
        let _output_list = hb.output_list.clone();
        let output_button = hb.output_button.clone();

        hb.output_list
            .connect_row_activated(move |list_box, activated_row| {
                let idx = activated_row.index();

                // ── Stop the current output before switching ──────────
                active_output.borrow().stop();

                if idx == 0 {
                    // ── Switch to "My Computer" (LocalOutput) ─────────
                    // If the local output is parked, move it back.
                    if let Some(local) = parked_local.borrow_mut().take() {
                        *active_output.borrow_mut() = local;
                        info!("Switched to local output (My Computer)");
                    }
                    // else: already local, no-op.

                    volume_scale.set_sensitive(true);
                } else {
                    // ── Determine if this is an MPD or AirPlay row ────
                    // Check the icon on the activated row to distinguish
                    // AirPlay (network-wireless-symbolic) from MPD
                    // (network-server-symbolic).
                    let row_icon_name = activated_row
                        .first_child()
                        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                        .and_then(|row_box| {
                            row_box
                                .first_child()
                                .and_then(|i| i.downcast::<gtk::Image>().ok())
                        })
                        .and_then(|icon| icon.icon_name())
                        .unwrap_or_default();

                    let is_airplay = row_icon_name == "network-wireless-symbolic";
                    let is_chromecast = row_icon_name == "video-display-symbolic";

                    if is_chromecast {
                        // ── Switch to Chromecast output ──────────────
                        let cast_name = activated_row
                            .first_child()
                            .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                            .and_then(|row_box| {
                                row_box
                                    .first_child()
                                    .and_then(|icon| icon.next_sibling())
                                    .and_then(|l| l.downcast::<gtk::Label>().ok())
                            })
                            .map(|l| l.text().to_string())
                            .unwrap_or_default();

                        let host_port = activated_row.widget_name().to_string();
                        let (host, port) = if let Some(colon) = host_port.rfind(':') {
                            let h = &host_port[..colon];
                            let p = host_port[colon + 1..].parse::<u16>().unwrap_or(8009);
                            (h.to_string(), p)
                        } else {
                            (host_port.clone(), 8009)
                        };

                        // Park the local output if needed.
                        if parked_local.borrow().is_none() {
                            let dummy: Box<dyn AudioOutput> = Box::new(MpdOutput::new(
                                "_dummy",
                                "127.0.0.1",
                                1,
                                event_sender.clone(),
                            ));
                            let local = std::mem::replace(&mut *active_output.borrow_mut(), dummy);
                            *parked_local.borrow_mut() = Some(local);
                        }

                        let chromecast =
                            ChromecastOutput::new(&cast_name, &host, port, event_sender.clone());
                        *active_output.borrow_mut() = Box::new(chromecast);
                        info!(
                            name = %cast_name,
                            host = %host,
                            port,
                            "Switched to Chromecast output"
                        );

                        // Chromecast supports volume — keep slider enabled.
                        volume_scale.set_sensitive(true);
                    } else if is_airplay {
                        // ── Switch to AirPlay output ─────────────────
                        // Extract the display name from the row label.
                        let airplay_name = activated_row
                            .first_child()
                            .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                            .and_then(|row_box| {
                                row_box
                                    .first_child()
                                    .and_then(|icon| icon.next_sibling())
                                    .and_then(|l| l.downcast::<gtk::Label>().ok())
                            })
                            .map(|l| l.text().to_string())
                            .unwrap_or_default();

                        // Extract host:port from the row's widget name
                        // (set during discovery, format "host:port").
                        let host_port = activated_row.widget_name().to_string();
                        let (host, port) = if let Some(colon) = host_port.rfind(':') {
                            let h = &host_port[..colon];
                            let p = host_port[colon + 1..].parse::<u16>().unwrap_or(7000);
                            (h.to_string(), p)
                        } else {
                            // Fallback: try to parse from the name or use defaults.
                            (host_port.clone(), 7000)
                        };

                        // Park the local output if needed.
                        if parked_local.borrow().is_none() {
                            let dummy: Box<dyn AudioOutput> = Box::new(MpdOutput::new(
                                "_dummy",
                                "127.0.0.1",
                                1,
                                event_sender.clone(),
                            ));
                            let local = std::mem::replace(&mut *active_output.borrow_mut(), dummy);
                            *parked_local.borrow_mut() = Some(local);
                        }

                        let airplay =
                            AirPlayOutput::new(&airplay_name, &host, port, event_sender.clone());
                        *active_output.borrow_mut() = Box::new(airplay);
                        info!(
                            name = %airplay_name,
                            host = %host,
                            port,
                            "Switched to AirPlay output"
                        );

                        volume_scale.set_sensitive(false);
                    } else {
                        // ── Switch to an MPD output ───────────────────
                        // Load saved outputs to find the one at this index.
                        let saved = load_saved_outputs();
                        // Count non-AirPlay rows before this one (excluding
                        // index 0 = "My Computer") to get the saved_idx.
                        let mut mpd_idx = 0usize;
                        let mut child = list_box.first_child();
                        let mut row_count = 0i32;
                        while let Some(c) = child {
                            if row_count > 0 && row_count < idx {
                                // Check if this row is NOT an AirPlay row.
                                let is_ap = c
                                    .first_child()
                                    .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                                    .and_then(|rb| {
                                        rb.first_child()
                                            .and_then(|i| i.downcast::<gtk::Image>().ok())
                                    })
                                    .and_then(|icon| icon.icon_name())
                                    .is_some_and(|n| n == "network-wireless-symbolic");
                                if !is_ap {
                                    mpd_idx += 1;
                                }
                            }
                            row_count += 1;
                            child = c.next_sibling();
                        }

                        if let Some(entry) = saved.get(mpd_idx) {
                            // Park the current output if it's local.
                            if parked_local.borrow().is_none() {
                                let dummy: Box<dyn AudioOutput> = Box::new(MpdOutput::new(
                                    "_dummy",
                                    "127.0.0.1",
                                    1,
                                    event_sender.clone(),
                                ));
                                let local =
                                    std::mem::replace(&mut *active_output.borrow_mut(), dummy);
                                *parked_local.borrow_mut() = Some(local);
                            }
                            let mpd = MpdOutput::new(
                                &entry.name,
                                &entry.host,
                                entry.port,
                                event_sender.clone(),
                            );
                            *active_output.borrow_mut() = Box::new(mpd);
                            info!(
                                name = %entry.name,
                                host = %entry.host,
                                port = entry.port,
                                "Switched to MPD output"
                            );

                            volume_scale.set_sensitive(false);
                        }
                    }
                }

                // ── Update checkmark visibility on all rows ───────────
                let mut row_idx = 0i32;
                let mut child = list_box.first_child();
                while let Some(c) = child {
                    // Each row is a gtk::ListBoxRow wrapping our gtk::Box.
                    if let Some(row_box) = c
                        .first_child()
                        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                    {
                        // The checkmark is the last child Image with widget name "output-check".
                        let mut box_child = row_box.first_child();
                        while let Some(bc) = box_child {
                            if let Some(img) = bc.downcast_ref::<gtk::Image>() {
                                if img.widget_name() == "output-check" {
                                    img.set_visible(row_idx == idx);
                                }
                            }
                            box_child = bc.next_sibling();
                        }
                    }
                    row_idx += 1;
                    child = c.next_sibling();
                }

                // Close the popover after selection.
                if let Some(popover) = output_button.popover() {
                    popover.popdown();
                }
            });
    }

    // ── Wire volume scale ───────────────────────────────────────────
    {
        let active_output = active_output.clone();
        hb.volume_adj.connect_value_changed(move |adj| {
            active_output.borrow_mut().set_volume(adj.value());
        });
    }

    // ── Wire progress scrubber (seek on user interaction) ───────────
    {
        let active_output = active_output.clone();
        let seeking = seeking.clone();
        hb.progress_adj.connect_value_changed(move |adj| {
            if !seeking.get() {
                active_output.borrow().seek_to(adj.value() as u64);
            }
        });
    }

    // ── Persist and restore column sort ────────────────────────────
    restore_sort_state(&column_view);
    if let Some(sorter) = column_view.sorter() {
        let cv = column_view.clone();
        sorter.connect_changed(move |_, _| {
            save_sort_state(&cv);
        });
    }

    // ── Wire tracklist double-click → load track ────────────────────
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let current_pos = current_pos.clone();

        let parked_local = parked_local.clone();
        column_view.connect_activate(move |_view, position| {
            play_track_at(
                position,
                &PlaybackContext {
                    model: sm.clone(),
                    active_output: active_output.clone(),
                    parked_local: parked_local.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    current_pos: current_pos.clone(),
                },
            );
        });
    }

    // ── Right-click context menu on tracklist ────────────────────────
    {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3); // right-click
        let sm = sort_model.clone();
        let sidebar_store_for_ctx = sidebar_store_for_events.clone();
        let active_source_key_for_ctx = active_source_key.clone();
        let rt_handle_for_ctx = rt_handle.clone();
        let track_store_for_ctx = track_store.clone();
        let source_tracks_for_ctx = source_tracks.clone();
        let master_tracks_for_ctx = master_tracks.clone();
        let status_label_for_ctx = status_label.clone();
        let browser_widget_for_ctx = browser_widget.clone();
        let browser_state_for_ctx = browser_state.clone();

        gesture.connect_pressed(move |gesture, _n_press, x, y| {
            let Some(widget) = gesture.widget() else { return };
            let Ok(cv) = widget.downcast::<gtk::ColumnView>() else { return };

            let active_key = active_source_key_for_ctx.borrow().clone();
            let is_playlist_view = active_key.starts_with("playlist:");

            // Collect selected track URIs from the MultiSelection model.
            let selection_model = cv.model();
            let Some(sel) = selection_model.and_then(|m| m.downcast::<gtk::MultiSelection>().ok()) else {
                return;
            };

            let selected = sel.selection();
            if selected.is_empty() {
                return;
            }

            let menu = gtk::gio::Menu::new();
            let action_group = gtk::gio::SimpleActionGroup::new();

            if is_playlist_view {
                // ── Remove from Playlist ─────────────────────────────
                let playlist_id = active_key.strip_prefix("playlist:").unwrap_or("").to_string();
                let rt = rt_handle_for_ctx.clone();
                let track_store = track_store_for_ctx.clone();
                let source_tracks = source_tracks_for_ctx.clone();
                let master_tracks = master_tracks_for_ctx.clone();
                let status_label = status_label_for_ctx.clone();
                let browser_widget = browser_widget_for_ctx.clone();
                let browser_state = browser_state_for_ctx.clone();
                let active_key = active_key.clone();

                // Collect URIs of selected tracks.
                let mut selected_uris = Vec::new();
                let mut pos = 0u32;
                while pos < sm.n_items() {
                    if selected.contains(pos) {
                        if let Some(item) = sm.item(pos) {
                            if let Some(track) = item.downcast_ref::<TrackObject>() {
                                selected_uris.push(track.uri());
                            }
                        }
                    }
                    pos += 1;
                }

                let remove_action = gtk::gio::SimpleAction::new("remove-from-playlist", None);
                let uris = selected_uris.clone();
                remove_action.connect_activate(move |_, _| {
                    let pid = playlist_id.clone();
                    let uris = uris.clone();
                    let track_store = track_store.clone();
                    let source_tracks = source_tracks.clone();
                    let master_tracks = master_tracks.clone();
                    let status_label = status_label.clone();
                    let browser_widget = browser_widget.clone();
                    let browser_state = browser_state.clone();
                    let active_key = active_key.clone();

                    // Remove from visible store immediately.
                    for uri in &uris {
                        for i in 0..track_store.n_items() {
                            if let Some(t) = track_store.item(i).and_downcast_ref::<TrackObject>() {
                                if t.uri() == *uri {
                                    track_store.remove(i);
                                    break;
                                }
                            }
                        }
                    }

                    // Update master + status.
                    {
                        let mut st = source_tracks.borrow_mut();
                        if let Some(tracks) = st.get_mut(&active_key) {
                            tracks.retain(|t| !uris.contains(&t.uri()));
                        }
                    }
                    let st = source_tracks.borrow();
                    let current = st.get(&active_key).cloned().unwrap_or_default();
                    *master_tracks.borrow_mut() = current.clone();
                    tracklist::update_status(&status_label, &current);
                    browser::rebuild_browser_data(&browser_widget, &browser_state, &current);

                    // Remove from DB in background.
                    rt.spawn(async move {
                        match crate::db::connection::init_db().await {
                            Ok(db) => {
                                let mgr = crate::local::playlist_manager::PlaylistManager::new(db.clone());
                                // Get all entries for this playlist, match by track file path.
                                if let Ok(entries) = crate::db::entities::playlist_entry::Entity::find()
                                    .filter(
                                        <crate::db::entities::playlist_entry::Column as sea_orm::ColumnTrait>::eq(
                                            &crate::db::entities::playlist_entry::Column::PlaylistId,
                                            &pid,
                                        ),
                                    )
                                    .all(&db)
                                    .await
                                {
                                    for entry in entries {
                                        if let Some(ref track_id) = entry.track_id {
                                            // Look up the track to get its file path / URI.
                                            if let Ok(Some(track)) = crate::db::entities::track::Entity::find_by_id(track_id.clone())
                                                .one(&db)
                                                .await
                                            {
                                                let track_uri = url::Url::from_file_path(&track.file_path)
                                                    .map(|u| u.to_string())
                                                    .unwrap_or_default();
                                                if uris.contains(&track_uri) {
                                                    let _ = mgr.remove_entry(&entry.id).await;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to open DB for playlist remove");
                            }
                        }
                    });
                });
                action_group.add_action(&remove_action);
                menu.append(Some("Remove from Playlist"), Some("tracklist-ctx.remove-from-playlist"));
            } else {
                // ── Add to Playlist (flat list with disabled header) ─
                // Use a flat menu structure instead of a submenu/section
                // to avoid GTK4's internal ScrolledWindow which adds
                // unwanted scrollbars for small menus.
                let mut has_playlists = false;

                // Find all regular playlists from the sidebar store.
                let n = sidebar_store_for_ctx.n_items();
                for i in 0..n {
                    if let Some(src) = sidebar_store_for_ctx.item(i).and_downcast_ref::<SourceObject>() {
                        if src.backend_type() == "playlist" {
                            // Add the "Add to Playlist" header on first playlist found.
                            if !has_playlists {
                                has_playlists = true;
                                // Disabled action renders as an unclickable label header.
                                let header_action = gtk::gio::SimpleAction::new("add-to-playlist-header", None);
                                header_action.set_enabled(false);
                                action_group.add_action(&header_action);
                                menu.append(Some("Add to Playlist"), Some("tracklist-ctx.add-to-playlist-header"));
                            }

                            let pl_name = src.name();
                            let pl_id = src.playlist_id();
                            let action_name = format!("add-to-{}", pl_id.replace('-', "_"));

                            // Collect selected URIs.
                            let mut selected_uris = Vec::new();
                            let mut pos = 0u32;
                            while pos < sm.n_items() {
                                if selected.contains(pos) {
                                    if let Some(item) = sm.item(pos) {
                                        if let Some(track) = item.downcast_ref::<TrackObject>() {
                                            selected_uris.push(track.uri());
                                        }
                                    }
                                }
                                pos += 1;
                            }

                            let rt = rt_handle_for_ctx.clone();
                            let add_action = gtk::gio::SimpleAction::new(&action_name, None);
                            let uris = selected_uris;
                            let pid = pl_id.clone();
                            add_action.connect_activate(move |_, _| {
                                let uris = uris.clone();
                                let pid = pid.clone();
                                rt.spawn(async move {
                                    match crate::db::connection::init_db().await {
                                        Ok(db) => {
                                            let mgr = crate::local::playlist_manager::PlaylistManager::new(db.clone());
                                            for uri in &uris {
                                                // Convert file:// URI back to path, find track in DB.
                                                if let Ok(url) = url::Url::parse(uri) {
                                                    if let Ok(path) = url.to_file_path() {
                                                        let path_str = path.to_string_lossy().to_string();
                                                        if let Ok(Some(track)) = <crate::db::entities::track::Entity as sea_orm::EntityTrait>::find()
                                                            .filter(<crate::db::entities::track::Column as sea_orm::ColumnTrait>::eq(
                                                                &crate::db::entities::track::Column::FilePath,
                                                                &path_str,
                                                            ))
                                                            .one(&db)
                                                            .await
                                                        {
                                                            let _ = mgr.add_track(&pid, &track).await;
                                                        }
                                                    }
                                                }
                                            }
                                            tracing::info!(playlist = %pid, count = uris.len(), "Tracks added to playlist");
                                        }
                                        Err(e) => {
                                            tracing::error!(error = %e, "Failed to open DB for playlist add");
                                        }
                                    }
                                });
                            });
                            action_group.add_action(&add_action);
                            menu.append(Some(&format!("  {pl_name}")), Some(&format!("tracklist-ctx.{action_name}")));
                        }
                    }
                }
            }

            // ── Properties… ──────────────────────────────────────────
            {
                let props_action = gtk::gio::SimpleAction::new("properties", None);
                let sm_for_props = sm.clone();
                let selected_for_props = selected.clone();
                let win_for_props = gesture.widget().and_then(|w| {
                    w.root()
                        .and_then(|r| r.downcast::<adw::ApplicationWindow>().ok())
                });

                props_action.connect_activate(move |_, _| {
                    let Some(ref win) = win_for_props else { return };

                    // Collect TrackInfo for selected tracks.
                    let mut track_infos = Vec::new();
                    let mut pos = 0u32;
                    while pos < sm_for_props.n_items() {
                        if selected_for_props.contains(pos) {
                            if let Some(item) = sm_for_props.item(pos) {
                                if let Some(track) = item.downcast_ref::<TrackObject>() {
                                    let uri = track.uri();
                                    // Only show properties for local file:// tracks.
                                    if uri.starts_with("file://") {
                                        track_infos.push(
                                            super::properties_dialog::TrackInfo {
                                                uri,
                                                title: track.title(),
                                                artist: track.artist(),
                                                album: track.album(),
                                                genre: track.genre(),
                                                year: track.year_display(),
                                                track_number: if track.track_number() > 0 {
                                                    track.track_number().to_string()
                                                } else {
                                                    String::new()
                                                },
                                                disc_number: String::new(),
                                                format: track.format(),
                                                bitrate: track.bitrate_display(),
                                                sample_rate: track.sample_rate_display(),
                                                duration: track.duration_display(),
                                            },
                                        );
                                    }
                                }
                            }
                        }
                        pos += 1;
                    }

                    if track_infos.is_empty() {
                        return;
                    }

                    super::properties_dialog::show_properties_dialog(
                        win,
                        &track_infos,
                    );
                });

                action_group.add_action(&props_action);
                menu.append(Some("Properties…"), Some("tracklist-ctx.properties"));
            }

            if menu.n_items() == 0 {
                return;
            }

            cv.insert_action_group("tracklist-ctx", Some(&action_group));

            let popover = gtk::PopoverMenu::from_model(Some(&menu));
            popover.set_parent(&cv);
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

            // Disable the internal ScrolledWindow that GTK4 PopoverMenu
            // creates — it adds unnecessary scrollbars for small menus
            // like "Add to Playlist" with only a few entries.
            disable_popover_scrollbars(&popover);

            popover.popup();
        });

        column_view.add_controller(gesture);
    }

    // ── Wire Next button ────────────────────────────────────────────
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let current_pos = current_pos.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();
        let parked_local = parked_local.clone();

        hb.next_button.connect_clicked(move |_| {
            advance_track(
                &PlaybackContext {
                    model: sm.clone(),
                    active_output: active_output.clone(),
                    parked_local: parked_local.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    current_pos: current_pos.clone(),
                },
                repeat_mode.get(),
                shuffle.is_active(),
            );
        });
    }

    // ── Wire Previous button ────────────────────────────────────────
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let current_pos = current_pos.clone();
        let parked_local = parked_local.clone();

        hb.prev_button.connect_clicked(move |_| {
            let Some(pos) = current_pos.get() else { return };

            // If more than 3 s into the track, restart it.
            let position_ms = active_output.borrow().position_ms().unwrap_or(0);
            if position_ms > PREV_RESTART_THRESHOLD_MS {
                active_output.borrow().seek_to(0);
                return;
            }

            // Otherwise go to the previous track (or restart track 0).
            if pos > 0 {
                play_track_at(
                    pos - 1,
                    &PlaybackContext {
                        model: sm.clone(),
                        active_output: active_output.clone(),
                        parked_local: parked_local.clone(),
                        album_art: album_art.clone(),
                        title_label: title_label.clone(),
                        artist_label: artist_label.clone(),
                        media_ctrl: media_ctrl.clone(),
                        current_pos: current_pos.clone(),
                    },
                );
            } else {
                active_output.borrow().seek_to(0);
            }
        });
    }

    // ── Receive PlayerEvents on GTK main thread ─────────────────────
    {
        let play_btn = hb.play_button.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let progress_adj = hb.progress_adj.clone();
        let position_label = hb.position_label.clone();
        let duration_label = hb.duration_label.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();
        let seeking = seeking.clone();
        let media_ctrl = media_ctrl.clone();
        let active_output = active_output.clone();
        let parked_local = parked_local.clone();
        let sm = sort_model.clone();
        let current_pos = current_pos.clone();

        // Pre-build a spinner widget for the buffering state.
        let buffering_spinner = gtk::Spinner::builder()
            .spinning(true)
            .width_request(16)
            .height_request(16)
            .build();

        // Debounce: only show the spinner if buffering persists for
        // longer than this threshold.  Increased from 100 ms to 300 ms
        // to prevent sub-100 ms blinking on fast-loading local files.
        const BUFFERING_DELAY_MS: u32 = 300;
        // Generation counter — incremented on every state change so
        // a stale timeout callback can detect it was superseded.
        let buffering_gen: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        // Track whether we are in a buffering state so that
        // PositionChanged can clear the spinner definitively.
        let is_buffering: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = player_rx.recv().await {
                match event {
                    PlayerEvent::StateChanged(state) => {
                        // Bump generation on every state change to
                        // invalidate any pending buffering timer.
                        let gen = buffering_gen.get().wrapping_add(1);
                        buffering_gen.set(gen);

                        match state {
                            PlayerState::Buffering => {
                                is_buffering.set(true);
                                // Schedule the spinner after a short
                                // delay — if Playing arrives first the
                                // generation will have changed and the
                                // callback becomes a no-op.
                                let btn = play_btn.clone();
                                let spinner = buffering_spinner.clone();
                                let gen_rc = buffering_gen.clone();
                                glib::timeout_add_local_once(
                                    Duration::from_millis(BUFFERING_DELAY_MS as u64),
                                    move || {
                                        if gen_rc.get() == gen {
                                            btn.set_child(Some(&spinner));
                                        }
                                    },
                                );
                            }
                            PlayerState::Playing => {
                                is_buffering.set(false);
                                // Restore icon: show pause.
                                play_btn.set_child(Option::<&gtk::Widget>::None);
                                play_btn.set_icon_name("media-playback-pause-symbolic");
                            }
                            _ => {
                                is_buffering.set(false);
                                // Stopped or Paused: show play.
                                play_btn.set_child(Option::<&gtk::Widget>::None);
                                play_btn.set_icon_name("media-playback-start-symbolic");
                            }
                        }

                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(state == PlayerState::Playing);
                        }
                    }

                    PlayerEvent::PositionChanged {
                        position_ms,
                        duration_ms,
                    } => {
                        // If we receive a position tick while still in
                        // the buffering state, audio is actually playing
                        // — clear the spinner definitively.  This is the
                        // sure-fire fix for remote streams where GStreamer
                        // never sends a clean Playing state change after
                        // buffering completes.
                        if is_buffering.get() {
                            is_buffering.set(false);
                            let gen = buffering_gen.get().wrapping_add(1);
                            buffering_gen.set(gen);
                            play_btn.set_child(Option::<&gtk::Widget>::None);
                            play_btn.set_icon_name("media-playback-pause-symbolic");

                            if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                                ctrl.update_playback(true);
                            }
                        }

                        // Always update the elapsed time label.
                        position_label.set_label(&format_ms(position_ms));

                        // Only update the progress slider and duration label
                        // when the stream has a known duration (> 0).
                        // Live streams (radio) have duration_ms == 0.
                        seeking.set(true);
                        if duration_ms > 0 {
                            progress_adj.set_upper(duration_ms as f64);
                            progress_adj.set_value(position_ms as f64);
                            seeking.set(false);
                            duration_label.set_label(&format_ms(duration_ms));
                        } else {
                            // Live stream: keep slider at 0, show "LIVE" or
                            // blank for the duration label.
                            progress_adj.set_upper(1.0);
                            progress_adj.set_value(0.0);
                            seeking.set(false);
                            duration_label.set_label("LIVE");
                        }
                    }

                    PlayerEvent::TrackEnded => {
                        let mode = repeat_mode.get();

                        // Repeat-one: replay the same track.
                        if mode == RepeatMode::One {
                            if let Some(pos) = current_pos.get() {
                                play_track_at(
                                    pos,
                                    &PlaybackContext {
                                        model: sm.clone(),
                                        active_output: active_output.clone(),
                                        parked_local: parked_local.clone(),
                                        album_art: album_art.clone(),
                                        title_label: title_label.clone(),
                                        artist_label: artist_label.clone(),
                                        media_ctrl: media_ctrl.clone(),
                                        current_pos: current_pos.clone(),
                                    },
                                );
                                continue;
                            }
                        }

                        // Auto-advance (shuffle-aware).
                        let advanced = advance_track(
                            &PlaybackContext {
                                model: sm.clone(),
                                active_output: active_output.clone(),
                                parked_local: parked_local.clone(),
                                album_art: album_art.clone(),
                                title_label: title_label.clone(),
                                artist_label: artist_label.clone(),
                                media_ctrl: media_ctrl.clone(),
                                current_pos: current_pos.clone(),
                            },
                            mode,
                            shuffle.is_active(),
                        );

                        if !advanced {
                            // End of playlist — reset to idle.
                            play_btn.set_icon_name("media-playback-start-symbolic");
                            title_label.set_label("Not Playing");
                            artist_label.set_label("");
                            album_art.set_icon_name(Some("audio-x-generic-symbolic"));
                            current_pos.set(None);

                            seeking.set(true);
                            progress_adj.set_value(0.0);
                            progress_adj.set_upper(1.0);
                            seeking.set(false);

                            position_label.set_label("0:00");
                            duration_label.set_label("0:00");

                            if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                                ctrl.set_stopped();
                            }
                        }
                    }

                    PlayerEvent::Error(msg) => {
                        tracing::error!(error = %msg, "Player error");
                        // On error, restore the play icon (stop the spinner
                        // if we were buffering).
                        play_btn.set_child(Option::<&gtk::Widget>::None);
                        play_btn.set_icon_name("media-playback-start-symbolic");
                    }
                }
            }
        });
    }

    // ── Apply persisted preferences (column visibility, order, browser) ─
    {
        let cfg = app_config.borrow();
        preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
        preferences::apply_column_order(&column_view, &cfg.column_order);
        preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
    }

    // ── Persist column order on drag-and-drop reorder ────────────────
    {
        let config = app_config.clone();
        let cv = column_view.clone();
        column_view
            .columns()
            .connect_items_changed(move |_list, _pos, _removed, _added| {
                let order = preferences::read_column_order(&cv);
                if !order.is_empty() {
                    let mut cfg = config.borrow_mut();
                    cfg.column_order = order;
                    preferences::save_config(&cfg);
                }
            });
    }

    // ── Wire preferences action to the window ────────────────────────
    {
        let win = window.clone();
        let cv = column_view.clone();
        let bw = browser_widget.clone();
        let cfg = app_config.clone();
        let prefs_action = gtk::gio::SimpleAction::new("show-preferences", None);
        prefs_action.connect_activate(move |_, _| {
            preferences::show_preferences(&win, &cv, &bw, &cfg);
        });
        window.add_action(&prefs_action);
    }

    // ── Ctrl+F: focus browser search entry ───────────────────────────
    {
        let bw = browser_widget.clone();
        let search_action = gtk::gio::SimpleAction::new("focus-search", None);
        search_action.connect_activate(move |_, _| {
            // The browser_widget is a vertical Box: SearchEntry on top,
            // panes_box below.  Find the SearchEntry (first child).
            if let Some(first) = bw.first_child() {
                if let Some(entry) = first.downcast_ref::<gtk::SearchEntry>() {
                    bw.set_visible(true);
                    entry.grab_focus();
                }
            }
        });
        window.add_action(&search_action);
    }
    app.set_accels_for_action("win.focus-search", &["<primary>f"]);

    // ── Handle playlist context menu actions ─────────────────────────
    {
        let sidebar_store = sidebar_store_for_events.clone();
        let rt_handle = rt_handle.clone();
        let win = window.clone();
        let _engine_tx = engine_tx.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(action) = playlist_action_rx.recv().await {
                match action {
                    sidebar::PlaylistAction::CreateRegular => {
                        info!("Creating new regular playlist");
                        let sidebar_store = sidebar_store.clone();
                        let rt_handle = rt_handle.clone();

                        // Show a simple name entry dialog.
                        let dialog = adw::AlertDialog::builder()
                            .heading("New Playlist")
                            .close_response("cancel")
                            .default_response("create")
                            .build();
                        dialog.add_response("cancel", "Cancel");
                        dialog.add_response("create", "Create");
                        dialog.set_response_appearance(
                            "create",
                            adw::ResponseAppearance::Suggested,
                        );

                        let name_entry = gtk::Entry::builder()
                            .placeholder_text("Playlist name")
                            .activates_default(true)
                            .build();
                        dialog.set_extra_child(Some(&name_entry));

                        dialog.connect_response(None, move |_dialog, response| {
                            if response != "create" {
                                return;
                            }
                            let name = name_entry.text().to_string();
                            if name.is_empty() {
                                return;
                            }

                            let sidebar_store = sidebar_store.clone();
                            let (result_tx, result_rx) =
                                async_channel::bounded::<(String, String, bool)>(1);

                            rt_handle.spawn(async move {
                                match crate::db::connection::init_db().await {
                                    Ok(db) => {
                                        let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                                        match mgr.create_playlist(&name, false).await {
                                            Ok(pl) => {
                                                let _ = result_tx
                                                    .send((pl.id, pl.name, pl.is_smart))
                                                    .await;
                                            }
                                            Err(e) => {
                                                tracing::error!(error = %e, "Failed to create playlist");
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(error = %e, "Failed to open DB");
                                    }
                                }
                            });

                            glib::MainContext::default().spawn_local(async move {
                                if let Ok((id, name, is_smart)) = result_rx.recv().await {
                                    // Insert into sidebar under Playlists header.
                                    let src = SourceObject::playlist(&name, &id, is_smart);
                                    let n = sidebar_store.n_items();
                                    for i in 0..n {
                                        if let Some(s) = sidebar_store
                                            .item(i)
                                            .and_downcast_ref::<SourceObject>()
                                        {
                                            if s.is_header() && s.name() == "Playlists" {
                                                // Find end of playlists section.
                                                let mut pos = i + 1;
                                                while pos < sidebar_store.n_items() {
                                                    if let Some(next) = sidebar_store
                                                        .item(pos)
                                                        .and_downcast_ref::<SourceObject>()
                                                    {
                                                        if next.is_header() {
                                                            break;
                                                        }
                                                        let bt = next.backend_type();
                                                        if bt == "playlist"
                                                            || bt == "smart-playlist"
                                                        {
                                                            pos += 1;
                                                        } else {
                                                            break;
                                                        }
                                                    } else {
                                                        break;
                                                    }
                                                }
                                                sidebar_store.insert(pos, &src);
                                                break;
                                            }
                                        }
                                    }
                                }
                            });
                        });

                        dialog.present(Some(&win));
                    }

                    sidebar::PlaylistAction::CreateSmart => {
                        info!("Creating new smart playlist");
                        let sidebar_store = sidebar_store.clone();
                        let rt_handle = rt_handle.clone();

                        super::playlist_editor::show_smart_playlist_editor(
                            &win,
                            "Untitled",
                            None,
                            move |rules| {
                                let sidebar_store = sidebar_store.clone();
                                let (result_tx, result_rx) =
                                    async_channel::bounded::<(String, String, bool)>(1);

                                rt_handle.spawn(async move {
                                    match crate::db::connection::init_db().await {
                                        Ok(db) => {
                                            let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                                            match mgr.create_playlist("Smart Playlist", true).await
                                            {
                                                Ok(pl) => {
                                                    let _ =
                                                        mgr.set_smart_rules(&pl.id, &rules).await;
                                                    let _ = result_tx
                                                        .send((pl.id, pl.name, pl.is_smart))
                                                        .await;
                                                }
                                                Err(e) => {
                                                    tracing::error!(error = %e, "Failed to create smart playlist");
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!(error = %e, "Failed to open DB");
                                        }
                                    }
                                });

                                glib::MainContext::default().spawn_local(async move {
                                    if let Ok((id, name, is_smart)) = result_rx.recv().await {
                                        let src = SourceObject::playlist(&name, &id, is_smart);
                                        let n = sidebar_store.n_items();
                                        for i in 0..n {
                                            if let Some(s) = sidebar_store
                                                .item(i)
                                                .and_downcast_ref::<SourceObject>()
                                            {
                                                if s.is_header() && s.name() == "Playlists" {
                                                    let mut pos = i + 1;
                                                    while pos < sidebar_store.n_items() {
                                                        if let Some(next) = sidebar_store
                                                            .item(pos)
                                                            .and_downcast_ref::<SourceObject>()
                                                        {
                                                            if next.is_header() {
                                                                break;
                                                            }
                                                            let bt = next.backend_type();
                                                            if bt == "playlist"
                                                                || bt == "smart-playlist"
                                                            {
                                                                pos += 1;
                                                            } else {
                                                                break;
                                                            }
                                                        } else {
                                                            break;
                                                        }
                                                    }
                                                    sidebar_store.insert(pos, &src);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                });
                            },
                        );
                    }

                    sidebar::PlaylistAction::Rename(playlist_id) => {
                        info!(id = %playlist_id, "Renaming playlist");
                        let sidebar_store = sidebar_store.clone();
                        let rt_handle = rt_handle.clone();
                        let pid = playlist_id.clone();

                        let dialog = adw::AlertDialog::builder()
                            .heading("Rename Playlist")
                            .close_response("cancel")
                            .default_response("rename")
                            .build();
                        dialog.add_response("cancel", "Cancel");
                        dialog.add_response("rename", "Rename");
                        dialog.set_response_appearance(
                            "rename",
                            adw::ResponseAppearance::Suggested,
                        );

                        let name_entry = gtk::Entry::builder()
                            .placeholder_text("New name")
                            .activates_default(true)
                            .build();
                        dialog.set_extra_child(Some(&name_entry));

                        dialog.connect_response(None, move |_dialog, response| {
                            if response != "rename" {
                                return;
                            }
                            let new_name = name_entry.text().to_string();
                            if new_name.is_empty() {
                                return;
                            }

                            let sidebar_store = sidebar_store.clone();
                            let pid_for_db = pid.clone();
                            let pid_for_ui = pid.clone();
                            let new_name_for_ui = new_name.clone();
                            let (done_tx, done_rx) = async_channel::bounded::<()>(1);

                            rt_handle.spawn(async move {
                                match crate::db::connection::init_db().await {
                                    Ok(db) => {
                                        let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                                        if let Err(e) =
                                            mgr.rename_playlist(&pid_for_db, &new_name).await
                                        {
                                            tracing::error!(error = %e, "Failed to rename playlist");
                                        }
                                        let _ = done_tx.send(()).await;
                                    }
                                    Err(e) => {
                                        tracing::error!(error = %e, "Failed to open DB");
                                    }
                                }
                            });

                            glib::MainContext::default().spawn_local(async move {
                                if done_rx.recv().await.is_ok() {
                                    // Update sidebar entry name.
                                    for i in 0..sidebar_store.n_items() {
                                        if let Some(src) = sidebar_store
                                            .item(i)
                                            .and_downcast_ref::<SourceObject>()
                                        {
                                            if src.playlist_id() == pid_for_ui {
                                                let is_smart =
                                                    src.backend_type() == "smart-playlist";
                                                let new_src = SourceObject::playlist(
                                                    &new_name_for_ui,
                                                    &pid_for_ui,
                                                    is_smart,
                                                );
                                                sidebar_store.remove(i);
                                                sidebar_store.insert(i, &new_src);
                                                break;
                                            }
                                        }
                                    }
                                }
                            });
                        });

                        dialog.present(Some(&win));
                    }

                    sidebar::PlaylistAction::Delete(playlist_id) => {
                        info!(id = %playlist_id, "Deleting playlist");
                        let sidebar_store = sidebar_store.clone();
                        let rt_handle = rt_handle.clone();
                        let pid = playlist_id.clone();

                        let (done_tx, done_rx) = async_channel::bounded::<()>(1);

                        rt_handle.spawn(async move {
                            match crate::db::connection::init_db().await {
                                Ok(db) => {
                                    let mgr =
                                        crate::local::playlist_manager::PlaylistManager::new(db);
                                    if let Err(e) = mgr.delete_playlist(&pid).await {
                                        tracing::error!(error = %e, "Failed to delete playlist");
                                    }
                                    let _ = done_tx.send(()).await;
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Failed to open DB");
                                }
                            }
                        });

                        let pid = playlist_id.clone();
                        glib::MainContext::default().spawn_local(async move {
                            if done_rx.recv().await.is_ok() {
                                for i in 0..sidebar_store.n_items() {
                                    if let Some(src) = sidebar_store
                                        .item(i)
                                        .and_downcast_ref::<SourceObject>()
                                    {
                                        if src.playlist_id() == pid {
                                            sidebar_store.remove(i);
                                            break;
                                        }
                                    }
                                }
                            }
                        });
                    }

                    sidebar::PlaylistAction::EditSmart(playlist_id) => {
                        info!(id = %playlist_id, "Editing smart playlist rules");
                        let sidebar_store = sidebar_store.clone();
                        let rt_handle = rt_handle.clone();
                        let pid = playlist_id.clone();
                        let win = win.clone();

                        // Fetch existing rules from DB.
                        let (rules_tx, rules_rx) = async_channel::bounded::<(
                            String,
                            Option<crate::local::smart_rules::SmartRules>,
                        )>(1);

                        let pid_fetch = pid.clone();
                        rt_handle.spawn(async move {
                            match crate::db::connection::init_db().await {
                                Ok(db) => {
                                    let mgr =
                                        crate::local::playlist_manager::PlaylistManager::new(db);
                                    match mgr.get_playlist(&pid_fetch).await {
                                        Ok(Some(pl)) => {
                                            let rules = pl
                                                .smart_rules_json
                                                .as_deref()
                                                .and_then(|j| serde_json::from_str(j).ok());
                                            let _ = rules_tx.send((pl.name, rules)).await;
                                        }
                                        _ => {
                                            let _ = rules_tx
                                                .send(("Smart Playlist".to_string(), None))
                                                .await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Failed to open DB");
                                }
                            }
                        });

                        glib::MainContext::default().spawn_local(async move {
                            if let Ok((name, existing_rules)) = rules_rx.recv().await {
                                let rt_handle = rt_handle.clone();
                                let _sidebar_store = sidebar_store.clone();
                                let pid = pid.clone();

                                super::playlist_editor::show_smart_playlist_editor(
                                    &win,
                                    &name,
                                    existing_rules.as_ref(),
                                    move |rules| {
                                        let pid = pid.clone();
                                        rt_handle.spawn(async move {
                                            match crate::db::connection::init_db().await {
                                                Ok(db) => {
                                                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                                                    if let Err(e) =
                                                        mgr.set_smart_rules(&pid, &rules).await
                                                    {
                                                        tracing::error!(error = %e, "Failed to save smart rules");
                                                    } else {
                                                        info!(id = %pid, "Smart playlist rules saved");
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::error!(error = %e, "Failed to open DB");
                                                }
                                            }
                                        });
                                    },
                                );
                            }
                        });
                    }
                }
            }
        });
    }

    // ── Receive LibraryEvents on GTK main thread ─────────────────────
    setup_library_events(
        engine_rx,
        track_store,
        status_label,
        master_tracks,
        source_tracks,
        active_source_key,
        &browser_widget,
        browser_state,
        &column_view,
        sidebar_store_for_events,
        sidebar_sel_for_events,
        scan_spinner,
        pending_connection_for_events,
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers (kept in window.rs — used by multiple extracted modules)
// ═══════════════════════════════════════════════════════════════════════

/// Replace the visible tracklist, browser, and master track list with a
/// new set of tracks (e.g., when switching sidebar sources).
pub fn display_tracks(
    objects: &[TrackObject],
    track_store: &gtk::gio::ListStore,
    master_tracks: &RefCell<Vec<TrackObject>>,
    browser_widget: &gtk::Box,
    browser_state: &browser::BrowserState,
    status_label: &gtk::Label,
    column_view: &gtk::ColumnView,
) {
    // Use splice() to replace all items in a single operation.
    // This emits one `items-changed` signal instead of N individual
    // signals, which is dramatically faster for large libraries
    // (thousands of tracks) and prevents multi-second UI freezes.
    track_store.splice(0, track_store.n_items(), objects);

    tracklist::update_status(status_label, objects);
    browser::rebuild_browser_data(browser_widget, browser_state, objects);
    *master_tracks.borrow_mut() = objects.to_vec();
    column_view.scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
}

/// Spawn the library event receiver loop on the GTK main thread.
#[allow(clippy::too_many_arguments)]
fn setup_library_events(
    engine_rx: async_channel::Receiver<LibraryEvent>,
    track_store: gtk::gio::ListStore,
    status_label: gtk::Label,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    active_source_key: Rc<RefCell<String>>,
    browser_widget: &gtk::Box,
    browser_state: browser::BrowserState,
    column_view: &gtk::ColumnView,
    sidebar_store: gtk::gio::ListStore,
    sidebar_selection: gtk::SingleSelection,
    scan_spinner: gtk::Spinner,
    pending_connection: Rc<RefCell<Option<String>>>,
) {
    let browser_widget = browser_widget.clone();
    let column_view = column_view.clone();

    // ── Debounce browser rebuilds for TrackUpserted / TrackRemoved ──
    // During initial scan, dozens of upsert events fire in quick
    // succession.  Instead of rebuilding the 3-pane browser on every
    // single event, we defer the rebuild by 500 ms.  If another event
    // arrives within that window the previous timer is invalidated.
    let browser_rebuild_gen: Rc<Cell<u32>> = Rc::new(Cell::new(0));

    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = engine_rx.recv().await {
            match event {
                LibraryEvent::FullSync(tracks) => {
                    info!(count = tracks.len(), "Received full library sync");

                    let objects: Vec<TrackObject> =
                        tracks.iter().map(arch_track_to_object).collect();

                    // Store per-source.
                    source_tracks
                        .borrow_mut()
                        .insert("local".to_string(), objects.clone());

                    // Display only if local is the active source.
                    if *active_source_key.borrow() == "local" {
                        display_tracks(
                            &objects,
                            &track_store,
                            &master_tracks,
                            &browser_widget,
                            &browser_state,
                            &status_label,
                            &column_view,
                        );
                    }
                }

                LibraryEvent::RemoteSync { source_key, tracks } => {
                    info!(
                        source = %source_key,
                        count = tracks.len(),
                        "Received remote library sync"
                    );

                    let objects: Vec<TrackObject> =
                        tracks.iter().map(arch_track_to_object).collect();

                    // Store per-source.
                    source_tracks
                        .borrow_mut()
                        .insert(source_key.clone(), objects.clone());

                    // Update the sidebar item: mark connected, force rebind.
                    for i in 0..sidebar_store.n_items() {
                        if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>()
                        {
                            if src.server_url() == source_key && !src.connected() {
                                src.set_connected(true);
                                src.set_connecting(false);
                                // Remove + re-insert to force ListView rebind.
                                let src = src.clone();
                                sidebar_store.remove(i);
                                sidebar_store.insert(i, &src);
                                // Auto-select this source.
                                sidebar_selection.set_selected(i);
                                // Clear the pending connection guard now that
                                // the connection succeeded.
                                *pending_connection.borrow_mut() = None;
                                break;
                            }
                        }
                    }

                    // Display if this source is now active (set by
                    // the selection_changed handler triggered above).
                    if *active_source_key.borrow() == source_key {
                        display_tracks(
                            &objects,
                            &track_store,
                            &master_tracks,
                            &browser_widget,
                            &browser_state,
                            &status_label,
                            &column_view,
                        );
                    }
                }

                LibraryEvent::TrackUpserted(track) => {
                    let obj = arch_track_to_object(&track);
                    let uri = obj.uri();

                    // Update source_tracks["local"].
                    {
                        let mut st = source_tracks.borrow_mut();
                        let local = st.entry("local".to_string()).or_default();
                        // Replace existing (by URI) or append.
                        if let Some(pos) = local.iter().position(|t| t.uri() == uri) {
                            local[pos] = obj.clone();
                        } else {
                            local.push(obj.clone());
                        }
                    }

                    // If local is the active source, update the visible tracklist.
                    if *active_source_key.borrow() == "local" {
                        // Check if already in the store (update) or new (append).
                        let mut found = false;
                        for i in 0..track_store.n_items() {
                            if let Some(existing) =
                                track_store.item(i).and_downcast_ref::<TrackObject>()
                            {
                                if existing.uri() == uri {
                                    track_store.remove(i);
                                    track_store.insert(i, &obj);
                                    found = true;
                                    break;
                                }
                            }
                        }
                        if !found {
                            track_store.append(&obj);
                        }

                        // Update master tracks immediately.
                        let st = source_tracks.borrow();
                        let local_tracks = st.get("local").cloned().unwrap_or_default();
                        *master_tracks.borrow_mut() = local_tracks.clone();

                        // Debounce browser rebuild + status update (500 ms).
                        // The tracklist store is already up-to-date above;
                        // only the 3-pane browser and status bar are deferred.
                        let gen = browser_rebuild_gen.get().wrapping_add(1);
                        browser_rebuild_gen.set(gen);

                        let gen_rc = browser_rebuild_gen.clone();
                        let source_tracks = source_tracks.clone();
                        let browser_widget = browser_widget.clone();
                        let browser_state = browser_state.clone();
                        let status_label = status_label.clone();

                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            if gen_rc.get() != gen {
                                return; // Superseded by a newer event.
                            }
                            let st = source_tracks.borrow();
                            let local_tracks = st.get("local").cloned().unwrap_or_default();
                            tracklist::update_status(&status_label, &local_tracks);
                            browser::rebuild_browser_data(
                                &browser_widget,
                                &browser_state,
                                &local_tracks,
                            );
                        });
                    }
                }

                LibraryEvent::TrackRemoved(path) => {
                    // Build the file:// URI for comparison.
                    let removed_uri = url::Url::from_file_path(&path)
                        .map(|u| u.to_string())
                        .unwrap_or_default();

                    // Remove from source_tracks["local"].
                    {
                        let mut st = source_tracks.borrow_mut();
                        if let Some(local) = st.get_mut("local") {
                            local.retain(|t| t.uri() != removed_uri);
                        }
                    }

                    // If local is the active source, remove from visible tracklist.
                    if *active_source_key.borrow() == "local" {
                        for i in 0..track_store.n_items() {
                            if let Some(existing) =
                                track_store.item(i).and_downcast_ref::<TrackObject>()
                            {
                                if existing.uri() == removed_uri {
                                    track_store.remove(i);
                                    break;
                                }
                            }
                        }

                        // Update master tracks immediately.
                        let st = source_tracks.borrow();
                        let local_tracks = st.get("local").cloned().unwrap_or_default();
                        *master_tracks.borrow_mut() = local_tracks.clone();

                        // Debounce browser rebuild + status update (500 ms).
                        let gen = browser_rebuild_gen.get().wrapping_add(1);
                        browser_rebuild_gen.set(gen);

                        let gen_rc = browser_rebuild_gen.clone();
                        let source_tracks = source_tracks.clone();
                        let browser_widget = browser_widget.clone();
                        let browser_state = browser_state.clone();
                        let status_label = status_label.clone();

                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            if gen_rc.get() != gen {
                                return; // Superseded by a newer event.
                            }
                            let st = source_tracks.borrow();
                            let local_tracks = st.get("local").cloned().unwrap_or_default();
                            tracklist::update_status(&status_label, &local_tracks);
                            browser::rebuild_browser_data(
                                &browser_widget,
                                &browser_state,
                                &local_tracks,
                            );
                        });
                    }
                }

                LibraryEvent::ScanProgress(done, total) => {
                    if done % 500 == 0 || done == total {
                        info!(done, total, "Scan progress");
                    }
                }

                LibraryEvent::ScanComplete => {
                    info!("Library scan complete");
                    scan_spinner.set_spinning(false);
                    scan_spinner.set_visible(false);
                }

                LibraryEvent::PlaylistsLoaded(playlists) => {
                    info!(count = playlists.len(), "Populating sidebar with playlists");

                    // Find the "Playlists" header position in sidebar.
                    let mut playlist_header_pos = None;
                    let n = sidebar_store.n_items();
                    for i in 0..n {
                        if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>()
                        {
                            if src.is_header() && src.name() == "Playlists" {
                                playlist_header_pos = Some(i);
                                break;
                            }
                        }
                    }

                    if let Some(header_pos) = playlist_header_pos {
                        // Remove old playlist entries (between Playlists header
                        // and the next header).
                        let insert_pos = header_pos + 1;
                        while insert_pos < sidebar_store.n_items() {
                            if let Some(src) = sidebar_store
                                .item(insert_pos)
                                .and_downcast_ref::<SourceObject>()
                            {
                                if src.is_header() {
                                    break; // Hit next section header.
                                }
                                let bt = src.backend_type();
                                if bt == "playlist" || bt == "smart-playlist" {
                                    sidebar_store.remove(insert_pos);
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }

                        // Insert new playlist entries.
                        for (idx, (id, name, is_smart)) in playlists.iter().enumerate() {
                            let src = SourceObject::playlist(name, id, *is_smart);
                            sidebar_store.insert(insert_pos + idx as u32, &src);
                        }
                    }
                }

                LibraryEvent::Error(msg) => {
                    tracing::error!(error = %msg, "Library engine error");
                    scan_spinner.set_spinning(false);
                    scan_spinner.set_visible(false);
                }
            }
        }
    });
}

/// Convert an architecture `Track` to a UI `TrackObject`.
fn arch_track_to_object(t: &crate::architecture::models::Track) -> TrackObject {
    // Build playable URI: prefer stream_url, fall back to file:// from file_path.
    let uri = t
        .stream_url
        .as_ref()
        .map(|u| u.to_string())
        .or_else(|| {
            t.file_path
                .as_ref()
                .and_then(|p| url::Url::from_file_path(p).ok().map(|u| u.to_string()))
        })
        .unwrap_or_default();

    let obj = TrackObject::new(
        t.track_number.unwrap_or(0),
        &t.title,
        t.duration_secs.unwrap_or(0),
        &t.artist_name,
        &t.album_title,
        t.genre.as_deref().unwrap_or("Unknown"),
        t.year.unwrap_or(0),
        &t.date_modified
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        t.bitrate_kbps.unwrap_or(0),
        t.sample_rate_hz.unwrap_or(0),
        t.play_count.unwrap_or(0),
        t.format.as_deref().unwrap_or(""),
        &uri,
    );

    // Propagate cover art URL for remote tracks.
    if let Some(ref art_url) = t.cover_art_url {
        obj.set_cover_art_url(art_url.as_str());
    }

    // Propagate album artist for browser grouping.
    if let Some(ref aa) = t.album_artist_name {
        obj.set_album_artist(aa);
    }

    obj
}

// ── Sidebar category management ─────────────────────────────────────

/// The fixed ordering of sidebar category headers.
const CATEGORY_ORDER: &[&str] = &[
    "Local",
    "DAAP",
    "Subsonic",
    "Jellyfin",
    "Plex",
    "Internet Radio",
];

/// Map a backend type string to its sidebar category header name.
fn category_for_backend(backend_type: &str) -> &'static str {
    match backend_type {
        "subsonic" => "Subsonic",
        "jellyfin" => "Jellyfin",
        "plex" => "Plex",
        "daap" => "DAAP",
        _ => "Subsonic", // fallback
    }
}

/// Ensure the category header for `backend_type` exists in a `Vec<SourceObject>`
/// (used during initial source list construction before the ListStore is built).
fn ensure_category_header_vec(sources: &mut Vec<SourceObject>, backend_type: &str) {
    let category = category_for_backend(backend_type);
    let already_exists = sources
        .iter()
        .any(|s| s.is_header() && s.name() == category);
    if !already_exists {
        sources.push(SourceObject::header(category));
    }
}

/// Ensure the category header for `backend_type` exists in the sidebar
/// `ListStore`. Returns the index at which a new source should be inserted
/// (right after the last item in that category, or right after the header
/// if the category is empty).
pub fn ensure_category_header_store(store: &gtk::gio::ListStore, backend_type: &str) -> u32 {
    let category = category_for_backend(backend_type);
    let cat_order = CATEGORY_ORDER
        .iter()
        .position(|&c| c == category)
        .unwrap_or(CATEGORY_ORDER.len());

    // Check if the header already exists.
    for i in 0..store.n_items() {
        if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
            if src.is_header() && src.name() == category {
                // Header exists — find the end of this category
                // (next header or end of list).
                let mut insert_pos = i + 1;
                while insert_pos < store.n_items() {
                    if let Some(next) = store.item(insert_pos).and_downcast_ref::<SourceObject>() {
                        if next.is_header() {
                            break;
                        }
                    }
                    insert_pos += 1;
                }
                return insert_pos;
            }
        }
    }

    // Header doesn't exist — find the correct insertion point based on
    // CATEGORY_ORDER. Insert before the first header that comes after
    // this category in the ordering.
    let mut insert_at = store.n_items(); // default: end of list
    for i in 0..store.n_items() {
        if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
            if src.is_header() {
                let other_order = CATEGORY_ORDER
                    .iter()
                    .position(|&c| c == src.name().as_str())
                    .unwrap_or(CATEGORY_ORDER.len());
                if other_order > cat_order {
                    insert_at = i;
                    break;
                }
            }
        }
    }

    // Insert the header.
    let header = SourceObject::header(category);
    store.insert(insert_at, &header);
    insert_at + 1 // return position right after the new header
}

/// Remove a category header from the store if it has no remaining
/// non-header children (i.e., the category is now empty).
fn remove_empty_category_header(store: &gtk::gio::ListStore, category: &str) {
    for i in 0..store.n_items() {
        if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
            if src.is_header() && src.name() == category {
                // Check if the next item is another header or end of list.
                let next_is_header_or_end = if i + 1 >= store.n_items() {
                    true
                } else {
                    store
                        .item(i + 1)
                        .and_downcast_ref::<SourceObject>()
                        .is_some_and(|s| s.is_header())
                };
                if next_is_header_or_end {
                    store.remove(i);
                }
                return;
            }
        }
    }
}

// ── Popover scrollbar fix ───────────────────────────────────────────

/// Traverse a `PopoverMenu`'s widget tree and disable scrollbars on any
/// internal `ScrolledWindow`.  GTK4's `PopoverMenu::from_model()` wraps
/// its content in a `ScrolledWindow` that adds unnecessary scrollbars
/// for small menus (e.g., "Add to Playlist" with only 2–3 entries).
fn disable_popover_scrollbars(popover: &gtk::PopoverMenu) {
    fn walk(widget: &gtk::Widget) {
        if let Some(sw) = widget.downcast_ref::<gtk::ScrolledWindow>() {
            sw.set_hscrollbar_policy(gtk::PolicyType::Never);
            sw.set_vscrollbar_policy(gtk::PolicyType::Never);
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            walk(&c);
            child = c.next_sibling();
        }
    }
    walk(popover.upcast_ref::<gtk::Widget>());
}
