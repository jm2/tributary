//! Main application window — assembles all UI components and bridges
//! the background library engine, the GStreamer player, and the OS
//! media controls to the GTK main thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tracing::{info, warn};

use crate::audio::local_output::LocalOutput;
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
    extract_hwnd, load_css, load_repeat_mode, load_shuffle, load_window_geometry,
    restore_sort_state, save_repeat_mode, save_shuffle, save_sort_state, save_window_geometry,
};
use super::playback::{
    advance_track, format_ms, play_or_start, play_track_at, previous_track,
    refresh_projected_library_uris, replay_current, stop_playback, toggle_or_start,
    BufferingTracker, PlaybackContext, PlaybackSession, QueueTrackRefresh, PLAYLIST_SOURCE_PREFIX,
};
use super::preferences;
use super::root_trust;
use super::server_dialogs::{load_saved_servers, remove_saved_server, show_add_server_dialog};
use super::sidebar;
use super::source_navigation::{PendingConnection, SourceNavigation};
use super::tracklist;
use super::window_state::WindowState;

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

/// User trust decisions are serialized by the engine; this bounded queue
/// prevents a stalled engine from accumulating unbounded confirmations.
const LIBRARY_COMMAND_CAPACITY: usize = 16;

type SharedAudioOutput = Rc<RefCell<Box<dyn AudioOutput>>>;
type PlaybackUiReset = Rc<dyn Fn()>;
type SourcePlaybackInvalidator = Rc<dyn Fn(&str)>;

fn configured_server_url(variable: &'static str) -> Option<String> {
    let raw = std::env::var(variable).ok()?;
    match crate::http_security::parse_base_url(&raw) {
        Ok(url) => Some(url.to_string()),
        Err(error) => {
            // The rejected value may itself contain a password/token. Log only
            // the fixed validation category and the non-secret variable name.
            warn!(variable, error, "Ignoring invalid configured server URL");
            None
        }
    }
}

/// Build and present the main Tributary window.
pub fn build_window(
    app: &adw::Application,
    rt_handle: tokio::runtime::Handle,
    engine_tx: async_channel::Sender<LibraryEvent>,
    engine_rx: async_channel::Receiver<LibraryEvent>,
) {
    info!("Building main window (Phase 4 — audio + desktop integration)");

    // Parse external source identities once, before they can be logged,
    // published to GTK, or handed to a connection registry. Invalid values
    // may contain credentials, so `configured_server_url` never returns or
    // formats them in an error.
    let subsonic_env = match (
        configured_server_url("SUBSONIC_URL"),
        std::env::var("SUBSONIC_USER"),
        std::env::var("SUBSONIC_PASS"),
    ) {
        (Some(url), Ok(user), Ok(pass)) => Some((url, user, pass)),
        _ => None,
    };
    let jellyfin_env = match (
        configured_server_url("JELLYFIN_URL"),
        std::env::var("JELLYFIN_API_KEY"),
        std::env::var("JELLYFIN_USER_ID"),
    ) {
        (Some(url), Ok(api_key), Ok(user_id)) => Some((url, api_key, user_id)),
        _ => None,
    };
    let plex_env = match (
        configured_server_url("PLEX_URL"),
        std::env::var("PLEX_TOKEN"),
    ) {
        (Some(url), Ok(token)) => Some((url, token)),
        _ => None,
    };
    let daap_env =
        configured_server_url("DAAP_URL").map(|url| (url, std::env::var("DAAP_PASSWORD").ok()));

    // ── Load and apply persisted preferences ─────────────────────────
    let app_config: Rc<RefCell<preferences::AppConfig>> =
        Rc::new(RefCell::new(preferences::load_config()));

    // ── Load custom CSS ──────────────────────────────────────────────
    load_css();

    // ── Sidebar sources ────────────────────────────────────────────────
    let sources = super::dummy_data::build_sources();
    let mut sources = sources;

    // Load manually-added servers from servers.json.
    let saved_servers = load_saved_servers();
    for entry in &saved_servers {
        ensure_category_header_vec(&mut sources, &entry.server_type);
        let src = SourceObject::manual(&entry.name, &entry.server_type, &entry.url);
        sources.push(src);
        info!(
            name = %entry.name,
            backend = %entry.server_type,
            "Loaded saved server from servers.json"
        );
    }

    // If env vars are set, add pre-configured remote server entries
    // under their respective category headers.
    if let Some((url, _user, _pass)) = subsonic_env.as_ref() {
        ensure_category_header_vec(&mut sources, "subsonic");
        let src = SourceObject::discovered("Subsonic (env)", "subsonic", url);
        src.set_connecting(true);
        sources.push(src);
        info!("Subsonic server configured via env vars");
    }

    if let Some((url, _key, _uid)) = jellyfin_env.as_ref() {
        ensure_category_header_vec(&mut sources, "jellyfin");
        let src = SourceObject::discovered("Jellyfin (env)", "jellyfin", url);
        src.set_connecting(true);
        sources.push(src);
        info!("Jellyfin server configured via env vars");
    }

    if let Some((url, _token)) = plex_env.as_ref() {
        ensure_category_header_vec(&mut sources, "plex");
        let src = SourceObject::discovered("Plex (env)", "plex", url);
        src.set_connecting(true);
        sources.push(src);
        info!("Plex server configured via env vars");
    }

    if let Some((url, _password)) = daap_env.as_ref() {
        ensure_category_header_vec(&mut sources, "daap");
        // Keep the configured URL as the source identity so the retained
        // session, generation-scoped sync event, sidebar row, and disconnect action all
        // address the same owner.
        let src = SourceObject::discovered("DAAP (env)", "daap", url);
        src.set_connecting(true);
        sources.push(src);
        info!("DAAP server configured via env vars");
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
    let playback_session = Rc::new(RefCell::new(PlaybackSession::default()));
    let seeking = Rc::new(Cell::new(false));
    let buffering_tracker = Rc::new(BufferingTracker::default());

    // Source discovery and deletion are wired before the audio output exists.
    // Keep indirection slots so those handlers can still retire playback
    // deterministically once the output/UI are installed later in this build.
    let active_output_slot: Rc<RefCell<Option<SharedAudioOutput>>> = Rc::new(RefCell::new(None));
    let playback_ui_reset_slot: Rc<RefCell<Option<PlaybackUiReset>>> = Rc::new(RefCell::new(None));
    let invalidate_source_playback: SourcePlaybackInvalidator = {
        let playback_session = playback_session.clone();
        let active_output_slot = active_output_slot.clone();
        let playback_ui_reset_slot = playback_ui_reset_slot.clone();
        Rc::new(move |source_key| {
            if !playback_session.borrow_mut().clear_if_source(source_key) {
                return;
            }

            if let Some(active_output) = active_output_slot.borrow().as_ref().cloned() {
                active_output.borrow().stop();
            }
            if let Some(clear_ui) = playback_ui_reset_slot.borrow().as_ref().cloned() {
                clear_ui();
            }
            info!("Stopped playback owned by a retired source");
        })
    };

    // ── Connection guard ─────────────────────────────────────────────
    // Tracks which server URL is currently being connected to, and the
    // sidebar position that was active before the connection attempt.
    // Used to (a) only auto-select on a remote sync if the source matches
    // the pending connection, and (b) revert the sidebar on failure.
    let pending_connection = Rc::new(RefCell::new(None));
    let pre_connect_selection: Rc<Cell<u32>> = Rc::new(Cell::new(1)); // default: local (index 1)

    // ── Per-source track storage ────────────────────────────────────
    // Key: "local" for local filesystem, or server URL for remote.
    let source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let active_source_key: Rc<RefCell<String>> = Rc::new(RefCell::new("local".to_string()));
    let source_navigation = Rc::new(RefCell::new(SourceNavigation::new("local")));

    // ── Browser (starts empty, updated by FullSync) ──────────────────
    let track_store_for_filter = track_store.clone();
    let status_label_for_filter = status_label.clone();
    let master_for_filter = master_tracks.clone();
    let app_config_for_filter = app_config.clone();
    let on_filter = Box::new(
        move |genre: Option<String>,
              artist: Option<String>,
              album: Option<String>,
              search_text: String| {
            let master = master_for_filter.borrow();
            let search_lower = search_text.to_lowercase();
            let use_album_artist = app_config_for_filter.borrow().group_by_album_artist;
            let filtered: Vec<TrackObject> = master
                .iter()
                .filter(|t| {
                    if let Some(ref g) = genre {
                        if &t.genre() != g {
                            return false;
                        }
                    }
                    if let Some(ref a) = artist {
                        // When album-artist grouping is on, match against
                        // the album-artist tag (falling back to track artist
                        // for tracks that lack one), so selecting an album
                        // artist returns every track on that artist's albums
                        // even on compilation discs.
                        let track_aa = t.album_artist();
                        let key = if use_album_artist && !track_aa.is_empty() {
                            track_aa
                        } else {
                            t.artist()
                        };
                        if &key != a {
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
                // Clone bumps the GObject refcount, so the same instance may
                // live in both `master_tracks` and the store.
                .cloned()
                .collect();

            // Replace the whole store in a single splice. This emits one
            // `items-changed` signal instead of N appends and keeps the rows'
            // identity. Playback navigation uses its own immutable queue and
            // is deliberately unaffected by this view mutation.
            track_store_for_filter.splice(0, track_store_for_filter.n_items(), &filtered);
            tracklist::update_status(&status_label_for_filter, &filtered);
        },
    );

    let initial_use_album_artist = app_config.borrow().group_by_album_artist;
    let (browser_widget, browser_state) =
        browser::build_browser(&empty_tracks, initial_use_album_artist, on_filter);

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

    // The root-trust flow uses non-modal status feedback after a guarded
    // confirmation. Tributary previously had no window-level toast host.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&content));

    // Restore persisted window geometry (size + maximized state).
    let saved_geo = load_window_geometry();
    let win_width = saved_geo.as_ref().map(|g| g.width).unwrap_or(DEFAULT_WIDTH);
    let win_height = saved_geo
        .as_ref()
        .map(|g| g.height)
        .unwrap_or(DEFAULT_HEIGHT);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Tributary")
        .default_width(win_width)
        .default_height(win_height)
        .content(&toast_overlay)
        .build();

    if saved_geo.is_some_and(|g| g.is_maximized) {
        window.maximize();
    }

    // Save geometry, revoke standard remote leases, and explicitly close every
    // retained DAAP session. Gate new connections synchronously, then let Tokio
    // perform DAAP network I/O while GTK remains responsive. Once shutdown
    // completes, close() re-enters this handler and is allowed to proceed.
    let shutdown_handle = rt_handle.clone();
    let shutdown_started = Rc::new(Cell::new(false));
    let shutdown_complete = Rc::new(Cell::new(false));
    window.connect_close_request(move |w| {
        save_window_geometry(w);
        if shutdown_complete.get() {
            return glib::Propagation::Proceed;
        }

        if !shutdown_started.replace(true) {
            crate::source_registry::begin_shutdown();
            crate::daap::begin_shutdown();
            let (done_tx, done_rx) = async_channel::bounded::<()>(1);
            shutdown_handle.spawn(async move {
                crate::daap::shutdown_all().await;
                let _ = done_tx.send(()).await;
            });

            let window = w.clone();
            let shutdown_complete = shutdown_complete.clone();
            glib::MainContext::default().spawn_local(async move {
                // A closed channel also unblocks the window if the runtime
                // task unexpectedly terminates.
                let _ = done_rx.recv().await;
                shutdown_complete.set(true);
                window.close();
            });
        }

        glib::Propagation::Stop
    });

    // Root trust is the only UI-to-library-engine command path. The engine
    // validates every request against fresh filesystem evidence before it can
    // change persisted trust; the GTK side only queues an affirmative intent.
    let (library_command_tx, library_command_rx) = async_channel::bounded(LIBRARY_COMMAND_CAPACITY);
    let root_trust_prompts =
        root_trust::RootTrustPromptController::new(&window, &toast_overlay, library_command_tx);

    // ── Start the library engine on tokio ────────────────────────────
    // Use the configured library paths from preferences, which default
    // to the XDG / platform music directory (e.g. ~/Musique on French
    // systems) via dirs::audio_dir() with a ~/Music fallback.
    let music_dirs: Vec<std::path::PathBuf> = app_config
        .borrow()
        .library_paths
        .iter()
        .map(std::path::PathBuf::from)
        .collect();

    let engine_tx_clone = engine_tx.clone();
    rt_handle.spawn(async move {
        match crate::db::connection::init_db().await {
            Ok(db) => {
                let engine =
                    LibraryEngine::new(db, music_dirs, engine_tx_clone, library_command_rx);
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
    if let Some((url, user, pass)) = subsonic_env {
        let tx = engine_tx.clone();
        rt_handle.spawn(async move {
            info!("Connecting to Subsonic server...");
            let Some(attempt) = crate::source_registry::begin_connect(url.clone()) else {
                tracing::debug!("Skipping Subsonic connect during shutdown");
                return;
            };
            match crate::subsonic::SubsonicBackend::connect("Subsonic", &url, &user, &pass).await {
                Ok(backend) => {
                    let tracks: Vec<crate::architecture::models::Track> =
                        backend.all_tracks().await;
                    let Some(source) = attempt.retain(Arc::new(backend)) else {
                        tracing::debug!("Subsonic connect was superseded");
                        return;
                    };
                    if !source.is_current() {
                        tracing::debug!("Subsonic sync was superseded");
                        return;
                    }
                    info!(count = tracks.len(), "Subsonic library fetched");
                    let _ = tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: url.clone(),
                            generation: source.generation(),
                            lease_key: source.lease_key(),
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    if !attempt.is_latest() {
                        tracing::debug!("Ignoring superseded Subsonic connection failure");
                        return;
                    }
                    tracing::error!(error = %e, "Subsonic connection failed");
                    let _ = tx.send(LibraryEvent::Error(format!("Subsonic: {e}"))).await;
                }
            }
        });
    }

    // ── Start Jellyfin backend if configured via env vars ──────────
    if let Some((url, api_key, user_id)) = jellyfin_env {
        let tx = engine_tx.clone();
        rt_handle.spawn(async move {
            info!("Connecting to Jellyfin server...");
            let Some(attempt) = crate::source_registry::begin_connect(url.clone()) else {
                tracing::debug!("Skipping Jellyfin connect during shutdown");
                return;
            };
            match crate::jellyfin::JellyfinBackend::connect("Jellyfin", &url, &api_key, &user_id)
                .await
            {
                Ok(backend) => {
                    let tracks: Vec<crate::architecture::models::Track> =
                        backend.all_tracks().await;
                    let Some(source) = attempt.retain(Arc::new(backend)) else {
                        tracing::debug!("Jellyfin connect was superseded");
                        return;
                    };
                    if !source.is_current() {
                        tracing::debug!("Jellyfin sync was superseded");
                        return;
                    }
                    info!(count = tracks.len(), "Jellyfin library fetched");
                    let _ = tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: url.clone(),
                            generation: source.generation(),
                            lease_key: source.lease_key(),
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    if !attempt.is_latest() {
                        tracing::debug!("Ignoring superseded Jellyfin connection failure");
                        return;
                    }
                    tracing::error!(error = %e, "Jellyfin connection failed");
                    let _ = tx.send(LibraryEvent::Error(format!("Jellyfin: {e}"))).await;
                }
            }
        });
    }

    // ── Start Plex backend if configured via env vars ──────────────
    if let Some((url, token)) = plex_env {
        let tx = engine_tx.clone();
        rt_handle.spawn(async move {
            info!("Connecting to Plex server...");
            let Some(attempt) = crate::source_registry::begin_connect(url.clone()) else {
                tracing::debug!("Skipping Plex connect during shutdown");
                return;
            };
            match crate::plex::PlexBackend::connect("Plex", &url, &token).await {
                Ok(backend) => {
                    let tracks: Vec<crate::architecture::models::Track> =
                        backend.all_tracks().await;
                    let Some(source) = attempt.retain(Arc::new(backend)) else {
                        tracing::debug!("Plex connect was superseded");
                        return;
                    };
                    if !source.is_current() {
                        tracing::debug!("Plex sync was superseded");
                        return;
                    }
                    info!(count = tracks.len(), "Plex library fetched");
                    let _ = tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: url.clone(),
                            generation: source.generation(),
                            lease_key: source.lease_key(),
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    if !attempt.is_latest() {
                        tracing::debug!("Ignoring superseded Plex connection failure");
                        return;
                    }
                    tracing::error!(error = %e, "Plex connection failed");
                    let _ = tx.send(LibraryEvent::Error(format!("Plex: {e}"))).await;
                }
            }
        });
    }

    // ── Start DAAP backend if configured via env vars ──────────────
    if let Some((url, password)) = daap_env {
        let tx = engine_tx.clone();
        rt_handle.spawn(async move {
            info!("Connecting to DAAP server...");
            let Some(attempt) = crate::daap::begin_connect(url.clone()) else {
                tracing::debug!("Skipping DAAP connect during shutdown");
                return;
            };
            match crate::daap::DaapBackend::connect("DAAP", &url, password.as_deref()).await {
                Ok(backend) => {
                    let Some(session) = attempt.retain(backend).await else {
                        tracing::debug!("DAAP connect was superseded");
                        return;
                    };
                    let tracks: Vec<crate::architecture::models::Track> =
                        session.all_tracks().await;
                    if !session.is_current() {
                        tracing::debug!("DAAP sync was superseded");
                        return;
                    }
                    info!(count = tracks.len(), "DAAP library fetched");
                    let _ = tx
                        .send(LibraryEvent::DaapSync {
                            source_key: url.clone(),
                            generation: session.generation(),
                            session_key: session.session_key(),
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    if !attempt.is_latest() {
                        tracing::debug!("Ignoring superseded DAAP connection failure");
                        return;
                    }
                    tracing::error!(error = %e, "DAAP connection failed");
                    let _ = tx.send(LibraryEvent::Error(format!("DAAP: {e}"))).await;
                }
            }
        });
    }

    // ── mDNS zero-config discovery ─────────────────────────────────
    super::discovery_handler::setup_discovery(
        &WindowState {
            window: window.clone(),
            rt_handle: rt_handle.clone(),
            engine_tx: engine_tx.clone(),
            track_store: track_store.clone(),
            master_tracks: master_tracks.clone(),
            source_tracks: source_tracks.clone(),
            active_source_key: active_source_key.clone(),
            source_navigation: source_navigation.clone(),
            sidebar_store: sidebar_store.clone(),
            sidebar_selection: sidebar_selection.clone(),
            browser_widget: browser_widget.clone(),
            browser_state: browser_state.clone(),
            status_label: status_label.clone(),
            column_view: column_view.clone(),
            sort_model: sort_model.clone(),
            app_config: app_config.clone(),
            pending_connection: pending_connection.clone(),
            pre_connect_selection: pre_connect_selection.clone(),
        },
        &hb.output_list,
        invalidate_source_playback.clone(),
    );

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
        let source_navigation = source_navigation.clone();
        let track_store = track_store.clone();
        let master_tracks = master_tracks.clone();
        let browser_widget = browser_widget.clone();
        let browser_state = browser_state.clone();
        let status_label = status_label.clone();
        let column_view = column_view.clone();
        let rt_handle = rt_handle.clone();
        let invalidate_source_playback = invalidate_source_playback.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(source_key) = delete_rx.recv().await {
                info!("Manual server delete requested");
                invalidate_source_playback(&source_key);

                // A connected DAAP source normally uses the eject action,
                // but deletion must still transfer and close ownership if a
                // stale/rebound row emits delete instead.
                if let Some(backend) = crate::daap::release_source(&source_key) {
                    rt_handle.spawn(async move {
                        backend.disconnect().await;
                    });
                }
                crate::source_registry::release_source(&source_key);

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
                    source_navigation.borrow_mut().select("local");
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
        let source_navigation = source_navigation.clone();
        let track_store = track_store.clone();
        let master_tracks = master_tracks.clone();
        let browser_widget = browser_widget.clone();
        let browser_state = browser_state.clone();
        let status_label = status_label.clone();
        let column_view = column_view.clone();
        let rt_handle = rt_handle.clone();
        let invalidate_source_playback = invalidate_source_playback.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(source_key) = disconnect_rx.recv().await {
                info!("DAAP disconnect requested");
                invalidate_source_playback(&source_key);

                // Transfer ownership out of the live-session registry before
                // updating the UI. This makes a subsequent fast reconnect
                // independent of the old session's asynchronous logout.
                if let Some(backend) = crate::daap::release_source(&source_key) {
                    rt_handle.spawn(async move {
                        backend.disconnect().await;
                    });
                } else {
                    tracing::warn!("DAAP source had no retained session");
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
                    source_navigation.borrow_mut().select("local");
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
    let pre_connect_selection_for_events = pre_connect_selection.clone();
    let source_connection_state = WindowState {
        window: window.clone(),
        rt_handle: rt_handle.clone(),
        engine_tx: engine_tx.clone(),
        track_store: track_store.clone(),
        master_tracks: master_tracks.clone(),
        source_tracks: source_tracks.clone(),
        active_source_key: active_source_key.clone(),
        source_navigation: source_navigation.clone(),
        sidebar_store: sidebar_store.clone(),
        sidebar_selection: sidebar_selection.clone(),
        browser_widget: browser_widget.clone(),
        browser_state: browser_state.clone(),
        status_label: status_label.clone(),
        column_view: column_view.clone(),
        sort_model: sort_model.clone(),
        app_config: app_config.clone(),
        pending_connection: pending_connection.clone(),
        pre_connect_selection: pre_connect_selection.clone(),
    };
    super::source_connect::setup_source_connect(&source_connection_state);

    // ═══════════════════════════════════════════════════════════════════
    // Phase 4: Audio Player + Desktop Integration
    // ═══════════════════════════════════════════════════════════════════

    // Present the window EARLY so that the native OS surface is
    // allocated.  On Windows, souvlaki needs the HWND which only
    // exists after the window has been realized and mapped.
    window.present();
    info!("Main window presented");

    // GVolumeMonitor must stay on the GTK main thread. Its cached mount
    // metadata drives an idempotent sidebar reconciliation, while selecting a
    // device still performs filesystem walking/tag parsing on a bounded worker.
    super::removable_media::setup_removable_media(
        &source_connection_state,
        invalidate_source_playback.clone(),
    );

    // ── Create GStreamer player ──────────────────────────────────────
    let (player, player_rx) = match crate::audio::Player::new(rt_handle.clone()) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create audio player — playback disabled");
            setup_library_events(
                engine_rx,
                rt_handle.clone(),
                track_store,
                status_label,
                master_tracks,
                source_tracks,
                active_source_key,
                source_navigation.clone(),
                &browser_widget,
                browser_state,
                &column_view,
                sidebar_store_for_events,
                sidebar_sel_for_events,
                scan_spinner,
                pending_connection_for_events.clone(),
                playback_session.clone(),
                root_trust_prompts.clone(),
                invalidate_source_playback.clone(),
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
    let active_output: SharedAudioOutput = Rc::new(RefCell::new(Box::new(local_output)));
    *active_output_slot.borrow_mut() = Some(active_output.clone());
    let active_output_target = Rc::new(RefCell::new(super::output_switch::OutputTarget::Local));

    // Parking slot for the local output when an MPD output is active.
    // When switching to MPD we move the LocalOutput out of active_output
    // into this slot; when switching back we move it back.
    let parked_local: Rc<RefCell<Option<Box<dyn AudioOutput>>>> = Rc::new(RefCell::new(None));

    // Sync the volume slider to the output's persisted volume.
    hb.volume_adj.set_value(active_output.borrow().volume());

    // ── Extract native window handle (HWND on Windows) ──────────────
    let hwnd = extract_hwnd(&window);

    // ── Enable Windows 11 Snap Layout ───────────────────────────────
    // Install a WM_NCHITTEST / WM_GETMINMAXINFO subclass on the
    // top-level HWND.
    //
    // `window.present()` is supposed to allocate the native surface,
    // but in practice on Windows the surface isn't always ready by the
    // time we read it back here. If `extract_hwnd` returns None, defer
    // the install to the first `notify::is-active`, which fires once
    // the window is mapped.
    #[cfg(target_os = "windows")]
    {
        if let Some(hwnd_ptr) = hwnd {
            tracing::info!("Installing Snap Layout subclass (HWND ready at present)");
            super::win32_snap::enable_snap_layout(hwnd_ptr, (win_width - 92, 0, 46, 36));

            window.connect_default_width_notify(move |win| {
                let (w, _) = win.default_size();
                super::win32_snap::update_maximize_rect((w - 92, 0, 46, 36));
            });
        } else {
            tracing::warn!(
                "HWND not available immediately after window.present() — deferring Snap Layout install to first notify::is-active"
            );
            let installed = std::rc::Rc::new(std::cell::Cell::new(false));
            let installed_for_handler = installed.clone();
            window.connect_is_active_notify(move |w| {
                if installed_for_handler.get() {
                    return;
                }
                let Some(hwnd_ptr) = extract_hwnd(w) else {
                    return;
                };
                tracing::info!("Installing Snap Layout subclass (deferred, HWND now ready)");
                let (cw, _) = w.default_size();
                super::win32_snap::enable_snap_layout(hwnd_ptr, (cw - 92, 0, 46, 36));
                installed_for_handler.set(true);

                w.connect_default_width_notify(move |win| {
                    let (cw, _) = win.default_size();
                    super::win32_snap::update_maximize_rect((cw - 92, 0, 46, 36));
                });
            });
        }
    }

    // ── Create OS media controls ────────────────────────────────────
    //
    // The Next / Previous handlers need a `PlaybackContext`, which in
    // turn references the album-art widget, title/artist labels, and
    // the OS media controller itself. We capture the fields up-front
    // into the spawn_local closure (cloned each iteration so the
    // PlaybackContext can be built fresh per event without moving the
    // captured Rc's out of the closure).
    let media_ctrl: Rc<RefCell<Option<crate::desktop_integration::MediaController>>> =
        Rc::new(RefCell::new(None));

    // Every terminal/reset path uses this one operation. Besides resetting the
    // visible controls it invalidates delayed spinner callbacks and both local
    // and remote artwork workers before installing the idle placeholder.
    let clear_playback_ui: PlaybackUiReset = {
        let play_button = hb.play_button.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let album_art = hb.album_art.clone();
        let progress_adj = hb.progress_adj.clone();
        let position_label = hb.position_label.clone();
        let duration_label = hb.duration_label.clone();
        let seeking = seeking.clone();
        let media_ctrl = media_ctrl.clone();
        let buffering_tracker = buffering_tracker.clone();
        Rc::new(move || {
            buffering_tracker.invalidate();
            play_button.set_child(Option::<&gtk::Widget>::None);
            play_button.set_icon_name("media-playback-start-symbolic");
            title_label.set_label("Not Playing");
            title_label.set_tooltip_text(Option::<&str>::None);
            artist_label.set_label("");
            artist_label.set_tooltip_text(Option::<&str>::None);
            super::album_art::invalidate();
            album_art.set_icon_name(Some("audio-x-generic-symbolic"));
            seeking.set(true);
            progress_adj.set_value(0.0);
            progress_adj.set_upper(1.0);
            seeking.set(false);
            position_label.set_label("0:00");
            duration_label.set_label("0:00");
            if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                ctrl.set_stopped();
            }
        })
    };
    *playback_ui_reset_slot.borrow_mut() = Some(clear_playback_ui.clone());

    match crate::desktop_integration::MediaController::new(hwnd) {
        Ok((ctrl, media_rx)) => {
            *media_ctrl.borrow_mut() = Some(ctrl);

            let active_output = active_output.clone();
            let album_art = hb.album_art.clone();
            let title_label = hb.title_label.clone();
            let artist_label = hb.artist_label.clone();
            let sm = sort_model.clone();
            let active_source_key = active_source_key.clone();
            let playback_session = playback_session.clone();
            let repeat_mode = hb.repeat_mode.clone();
            let shuffle = hb.shuffle_button.clone();
            let ctrl_for_ctx = media_ctrl.clone();
            let column_view_for_keys = column_view.clone();
            let clear_playback_ui = clear_playback_ui.clone();

            glib::MainContext::default().spawn_local(async move {
                while let Ok(action) = media_rx.recv().await {
                    info!(?action, "OS media key");
                    let ctx = PlaybackContext {
                        model: sm.clone(),
                        active_source_key: active_source_key.clone(),
                        active_output: active_output.clone(),
                        album_art: album_art.clone(),
                        title_label: title_label.clone(),
                        artist_label: artist_label.clone(),
                        media_ctrl: ctrl_for_ctx.clone(),
                        session: playback_session.clone(),
                        column_view: column_view_for_keys.clone(),
                    };
                    match action {
                        MediaAction::Play => {
                            if play_or_start(&ctx, shuffle.is_active()) {
                                if let Some(ref mut ctrl) = *ctrl_for_ctx.borrow_mut() {
                                    ctrl.update_playback(true);
                                }
                            }
                        }
                        MediaAction::Pause => {
                            if playback_session
                                .borrow_mut()
                                .cancel_pending_resolution_for_retry()
                            {
                                // No media has reached the output yet. Stop is
                                // cleanup only; the cancelled resolver cannot
                                // claim the session, and the next Play resolves
                                // the protected reference again.
                                active_output.borrow().stop();
                                if let Some(ref mut ctrl) = *ctrl_for_ctx.borrow_mut() {
                                    ctrl.update_playback(false);
                                }
                            } else if playback_session.borrow().has_current() {
                                active_output.borrow().pause();
                                if let Some(ref mut ctrl) = *ctrl_for_ctx.borrow_mut() {
                                    ctrl.update_playback(false);
                                }
                            }
                        }
                        MediaAction::Toggle => {
                            toggle_or_start(&ctx, shuffle.is_active());
                        }
                        MediaAction::Stop => {
                            stop_playback(&ctx);
                            clear_playback_ui();
                        }
                        MediaAction::Next => {
                            advance_track(&ctx, repeat_mode.get(), shuffle.is_active());
                        }
                        MediaAction::Previous => {
                            // Mirror the header-bar heuristic: if we're past
                            // the restart threshold, restart the current track.
                            let position_ms = active_output.borrow().position_ms().unwrap_or(0);
                            if position_ms > PREV_RESTART_THRESHOLD_MS {
                                active_output.borrow().seek_to(0);
                            } else {
                                let stepped =
                                    previous_track(&ctx, repeat_mode.get(), shuffle.is_active());
                                if !stepped {
                                    active_output.borrow().seek_to(0);
                                }
                            }
                        }
                    }
                }
            });
        }
        Err(e) => {
            warn!(error = %e, "Media controls unavailable — media keys disabled");
        }
    }

    // ── Wire play/pause button ──────────────────────────────────────
    // If nothing is playing, start from track 0 (or random if shuffle).
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sort_model = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let shuffle = hb.shuffle_button.clone();
        let column_view_c = column_view.clone();

        hb.play_button.connect_clicked(move |_| {
            toggle_or_start(
                &PlaybackContext {
                    model: sort_model.clone(),
                    active_source_key: active_source_key.clone(),
                    active_output: active_output.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    session: playback_session.clone(),
                    column_view: column_view_c.clone(),
                },
                shuffle.is_active(),
            );
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
    {
        super::output_switch::setup_output_selector(
            &hb.output_list,
            &hb.output_button,
            &active_output,
            &parked_local,
            &active_output_target,
            &playback_session,
            clear_playback_ui.clone(),
            &event_sender,
            &hb.volume_scale,
            &rt_handle,
        );
    }

    // ── Wire volume scale ───────────────────────────────────────────
    // Throttled (trailing): a slider drag emits a burst of value-changed
    // signals, and for MPD/Chromecast outputs each set_volume spawns a
    // worker thread + connection. Collapse the burst to ~one command per
    // window; the final value always lands within the window.
    {
        let active_output = active_output.clone();
        let pending: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
        let scheduled = Rc::new(Cell::new(false));
        hb.volume_adj.connect_value_changed(move |adj| {
            pending.set(Some(adj.value()));
            if scheduled.replace(true) {
                return;
            }
            let active_output = active_output.clone();
            let pending = pending.clone();
            let scheduled = scheduled.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                scheduled.set(false);
                if let Some(v) = pending.take() {
                    active_output.borrow_mut().set_volume(v);
                }
            });
        });
    }

    // ── Wire progress scrubber (seek on user interaction) ───────────
    // Same trailing-throttle as the volume slider, and skip programmatic
    // position-poll updates (guarded by `seeking`) so they never seek.
    {
        let active_output = active_output.clone();
        let seeking = seeking.clone();
        let pending: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        let scheduled = Rc::new(Cell::new(false));
        hb.progress_adj.connect_value_changed(move |adj| {
            if seeking.get() {
                return;
            }
            pending.set(Some(adj.value() as u64));
            if scheduled.replace(true) {
                return;
            }
            let active_output = active_output.clone();
            let pending = pending.clone();
            let seeking = seeking.clone();
            let scheduled = scheduled.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
                scheduled.set(false);
                // Re-check the guard: don't fire a stale seek if a
                // programmatic update is in progress when the timer lands.
                if !seeking.get() {
                    if let Some(p) = pending.take() {
                        active_output.borrow().seek_to(p);
                    }
                }
            });
        });
    }

    // ── Persist and restore column sort ────────────────────────────
    restore_sort_state(&column_view);
    if let Some(sorter) = column_view.sorter() {
        let cv = column_view.clone();
        let active_source_key = active_source_key.clone();
        sorter.connect_changed(move |_, _| {
            // Don't persist sort state while viewing a radio station: in
            // radio mode the Artist/Album columns are renamed to
            // Country/State-Province, so the saved title could never be
            // re-matched against the music-mode columns on the next launch
            // (issue #38).
            if super::radio::is_radio_backend(&active_source_key.borrow()) {
                return;
            }
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
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let cv = column_view.clone();

        column_view.connect_activate(move |_view, position| {
            play_track_at(
                position,
                &PlaybackContext {
                    model: sm.clone(),
                    active_source_key: active_source_key.clone(),
                    active_output: active_output.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    session: playback_session.clone(),
                    column_view: cv.clone(),
                },
            );
        });
    }

    // ── Right-click context menu on tracklist ────────────────────────
    super::context_menu::setup_context_menu(&WindowState {
        window: window.clone(),
        rt_handle: rt_handle.clone(),
        engine_tx: engine_tx.clone(),
        track_store: track_store.clone(),
        master_tracks: master_tracks.clone(),
        source_tracks: source_tracks.clone(),
        active_source_key: active_source_key.clone(),
        source_navigation: source_navigation.clone(),
        sidebar_store: sidebar_store_for_events.clone(),
        sidebar_selection: sidebar_sel_for_events.clone(),
        browser_widget: browser_widget.clone(),
        browser_state: browser_state.clone(),
        status_label: status_label.clone(),
        column_view: column_view.clone(),
        sort_model: sort_model.clone(),
        app_config: app_config.clone(),
        pending_connection: pending_connection_for_events.clone(),
        pre_connect_selection: pre_connect_selection_for_events.clone(),
    });

    // ── Wire Next button ────────────────────────────────────────────
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();
        let cv = column_view.clone();

        hb.next_button.connect_clicked(move |_| {
            advance_track(
                &PlaybackContext {
                    model: sm.clone(),
                    active_source_key: active_source_key.clone(),
                    active_output: active_output.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    session: playback_session.clone(),
                    column_view: cv.clone(),
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
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();
        let cv = column_view.clone();

        hb.prev_button.connect_clicked(move |_| {
            // If more than 3 s into the track, restart it.
            let position_ms = active_output.borrow().position_ms().unwrap_or(0);
            if position_ms > PREV_RESTART_THRESHOLD_MS {
                active_output.borrow().seek_to(0);
                return;
            }

            let stepped = previous_track(
                &PlaybackContext {
                    model: sm.clone(),
                    active_source_key: active_source_key.clone(),
                    active_output: active_output.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    session: playback_session.clone(),
                    column_view: cv.clone(),
                },
                repeat_mode.get(),
                shuffle.is_active(),
            );

            // If we couldn't step back (track 0 with repeat off, or no
            // current track), restart whatever is playing instead.
            if !stepped {
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
        let sm = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let cv = column_view.clone();
        let buffering_tracker = buffering_tracker.clone();
        let clear_playback_ui = clear_playback_ui.clone();

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
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = player_rx.recv().await {
                let event_generation = event.generation();
                if !playback_session
                    .borrow()
                    .accepts_event_generation(event_generation)
                {
                    tracing::debug!(?event_generation, "Ignoring stale player event");
                    continue;
                }
                match event {
                    PlayerEvent::StateChanged { state, .. } => {
                        match state {
                            PlayerState::Buffering => {
                                let generation = buffering_tracker.begin();
                                // Schedule the spinner after a short
                                // delay — if Playing arrives first the
                                // generation will have changed and the
                                // callback becomes a no-op.
                                let btn = play_btn.clone();
                                let spinner = buffering_spinner.clone();
                                let tracker = buffering_tracker.clone();
                                let session = playback_session.clone();
                                glib::timeout_add_local_once(
                                    Duration::from_millis(BUFFERING_DELAY_MS as u64),
                                    move || {
                                        if tracker.is_current(generation)
                                            && session
                                                .borrow()
                                                .accepts_event_generation(event_generation)
                                        {
                                            btn.set_child(Some(&spinner));
                                        }
                                    },
                                );
                            }
                            PlayerState::Playing => {
                                buffering_tracker.invalidate();
                                // Restore icon: show pause.
                                play_btn.set_child(Option::<&gtk::Widget>::None);
                                play_btn.set_icon_name("media-playback-pause-symbolic");
                            }
                            _ => {
                                buffering_tracker.invalidate();
                                // Stopped or Paused: show play.
                                play_btn.set_child(Option::<&gtk::Widget>::None);
                                play_btn.set_icon_name("media-playback-start-symbolic");
                            }
                        }

                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            match state {
                                PlayerState::Playing => ctrl.update_playback(true),
                                PlayerState::Paused | PlayerState::Stopped => {
                                    ctrl.update_playback(false);
                                }
                                // OS media APIs do not expose Buffering. Keep
                                // the optimistic Playing state published when
                                // the session load was accepted.
                                PlayerState::Buffering => {}
                            }
                        }
                    }

                    PlayerEvent::PositionChanged {
                        position_ms,
                        duration_ms,
                        ..
                    } => {
                        // If we receive a position tick while still in
                        // the buffering state, audio is actually playing
                        // — clear the spinner definitively.  This is the
                        // sure-fire fix for remote streams where GStreamer
                        // never sends a clean Playing state change after
                        // buffering completes.
                        if buffering_tracker.is_buffering() {
                            buffering_tracker.invalidate();
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

                    PlayerEvent::TrackEnded { .. } => {
                        buffering_tracker.invalidate();
                        play_btn.set_child(Option::<&gtk::Widget>::None);
                        let mode = repeat_mode.get();

                        // Repeat-one: replay the same track.
                        if mode == RepeatMode::One
                            && replay_current(&PlaybackContext {
                                model: sm.clone(),
                                active_source_key: active_source_key.clone(),
                                active_output: active_output.clone(),
                                album_art: album_art.clone(),
                                title_label: title_label.clone(),
                                artist_label: artist_label.clone(),
                                media_ctrl: media_ctrl.clone(),
                                session: playback_session.clone(),
                                column_view: cv.clone(),
                            })
                        {
                            continue;
                        }

                        // Auto-advance (shuffle-aware).
                        let advanced = advance_track(
                            &PlaybackContext {
                                model: sm.clone(),
                                active_source_key: active_source_key.clone(),
                                active_output: active_output.clone(),
                                album_art: album_art.clone(),
                                title_label: title_label.clone(),
                                artist_label: artist_label.clone(),
                                media_ctrl: media_ctrl.clone(),
                                session: playback_session.clone(),
                                column_view: cv.clone(),
                            },
                            mode,
                            shuffle.is_active(),
                        );

                        if !advanced {
                            // End of playlist — invalidate the event generation
                            // before resetting every async/visible UI surface.
                            playback_session.borrow_mut().clear();
                            clear_playback_ui();
                        }
                    }

                    PlayerEvent::Error { message, .. } => {
                        tracing::error!(error = %message, "Player error");
                        // A protected resolver has already handed its request
                        // to the output at this point. If that load fails, keep
                        // the queue item but force the next Play through a new
                        // resolution instead of calling `play()` on an output
                        // that may never have accepted media.
                        if playback_session
                            .borrow_mut()
                            .mark_protected_load_failed(event_generation)
                        {
                            active_output.borrow().stop();
                        }
                        // On error, restore the play icon (stop the spinner
                        // if we were buffering).
                        buffering_tracker.invalidate();
                        play_btn.set_child(Option::<&gtk::Widget>::None);
                        play_btn.set_icon_name("media-playback-start-symbolic");
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
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
        let active_source_key = active_source_key.clone();
        column_view
            .columns()
            .connect_items_changed(move |_list, _pos, _removed, _added| {
                // Skip persistence while in radio mode — the renamed
                // Artist→Country / Album→State-Province columns would
                // corrupt the saved column order (issue #38).
                if super::radio::is_radio_backend(&active_source_key.borrow()) {
                    return;
                }
                let order = preferences::read_column_order(&cv);
                if !order.is_empty() {
                    let mut cfg = config.borrow_mut();
                    cfg.column_order = order;
                    preferences::save_config(&cfg);
                }
            });
    }

    // ── "Open With" pending-files action ────────────────────────────
    //
    // The OS file-open handler in main.rs queues paths in
    // `super::open_files`.  We expose a stateless application-level
    // GAction `app.play-pending-files` that drains the queue and plays
    // the file(s) on the active output.  The action is registered on
    // the GApplication (not the window) so the file-open handler can
    // look it up via `app.lookup_action`.
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let cv = column_view.clone();

        let play_pending = gtk::gio::SimpleAction::new("play-pending-files", None);
        play_pending.connect_activate(move |_, _| {
            let paths = super::open_files::drain();
            if paths.is_empty() {
                return;
            }
            // Play only the first file for now.  If multiple files were
            // delivered, the rest are dropped — multi-file Open With with
            // a temp playlist is a separate feature.
            let ctx = super::playback::PlaybackContext {
                model: sm.clone(),
                active_source_key: active_source_key.clone(),
                active_output: active_output.clone(),
                album_art: album_art.clone(),
                title_label: title_label.clone(),
                artist_label: artist_label.clone(),
                media_ctrl: media_ctrl.clone(),
                session: playback_session.clone(),
                column_view: cv.clone(),
            };
            for path in paths {
                if super::playback::play_local_file(&path, &ctx) {
                    break;
                }
            }
        });
        app.add_action(&play_pending);

        // Drain any paths that arrived before the window was built
        // (the typical case on first-launch Open With).
        play_pending.activate(None);
    }

    // ── Wire preferences action to the window ────────────────────────
    {
        let win = window.clone();
        let cv = column_view.clone();
        let bw = browser_widget.clone();
        let cfg = app_config.clone();
        let bs = browser_state.clone();
        let master_for_pref = master_tracks.clone();
        let prefs_action = gtk::gio::SimpleAction::new("show-preferences", None);
        prefs_action.connect_activate(move |_, _| {
            let bw_for_cb = bw.clone();
            let bs_for_cb = bs.clone();
            let master_for_cb = master_for_pref.clone();
            let on_aa_change: std::rc::Rc<dyn Fn(bool)> = std::rc::Rc::new(move |enabled: bool| {
                // Refresh the browser snapshot so the album-artist
                // grouping change takes effect against the latest
                // library state, not just whatever was loaded when
                // the browser was first built.
                let tracks = master_for_cb.borrow().clone();
                browser::rebuild_browser_data(&bw_for_cb, &bs_for_cb, &tracks);
                browser::set_album_artist_grouping(&bw_for_cb, &bs_for_cb, enabled);
            });
            preferences::show_preferences(&win, &cv, &bw, &cfg, on_aa_change);
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
    super::playlist_actions::setup_playlist_actions(
        &WindowState {
            window: window.clone(),
            rt_handle: rt_handle.clone(),
            engine_tx: engine_tx.clone(),
            track_store: track_store.clone(),
            master_tracks: master_tracks.clone(),
            source_tracks: source_tracks.clone(),
            active_source_key: active_source_key.clone(),
            source_navigation: source_navigation.clone(),
            sidebar_store: sidebar_store_for_events.clone(),
            sidebar_selection: sidebar_sel_for_events.clone(),
            browser_widget: browser_widget.clone(),
            browser_state: browser_state.clone(),
            status_label: status_label.clone(),
            column_view: column_view.clone(),
            sort_model: sort_model.clone(),
            app_config: app_config.clone(),
            pending_connection: pending_connection_for_events.clone(),
            pre_connect_selection: pre_connect_selection_for_events.clone(),
        },
        playlist_action_rx,
    );

    // ── Receive LibraryEvents on GTK main thread ─────────────────────
    setup_library_events(
        engine_rx,
        rt_handle.clone(),
        track_store,
        status_label,
        master_tracks,
        source_tracks,
        active_source_key,
        source_navigation,
        &browser_widget,
        browser_state,
        &column_view,
        sidebar_store_for_events,
        sidebar_sel_for_events,
        scan_spinner,
        pending_connection_for_events,
        playback_session,
        root_trust_prompts,
        invalidate_source_playback,
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

/// Re-resolve queued library items from committed library state.
///
/// The playback queue is an immutable snapshot of identities, so it survives
/// sorting, filtering, and navigation — but a filesystem rename changes where a
/// track lives without changing which track it is. Only the library can say
/// where it moved to, and only the queue knows it is still holding it.
fn refresh_playback_queue(session: &Rc<RefCell<PlaybackSession>>, objects: &[TrackObject]) {
    let updates = {
        let session = session.borrow();
        let queue_ids = session.library_track_ids();
        if queue_ids.is_empty() {
            return;
        }

        // FullSync can contain tens of thousands of tracks. Scan the snapshot,
        // but clone refresh metadata only for the usually small set the queue
        // owns.
        let mut updates = HashMap::with_capacity(queue_ids.len());
        for track in objects {
            let track_id = track.track_id();
            if queue_ids.contains(track_id.as_str()) {
                updates.insert(track_id, QueueTrackRefresh::from_track(track));
            }
        }
        updates
    };
    if updates.is_empty() {
        return;
    }

    let refreshed = session.borrow_mut().refresh_library_tracks(&updates);
    if refreshed > 0 {
        info!(
            refreshed,
            "Re-resolved playback queue items after library change"
        );
    }
}

/// Retarget the rows of an already-open playlist after committed local changes.
/// The visible store and `master_tracks` share these GObject instances, so an
/// in-place URI overlay is immediately used by the next click without changing
/// playlist order, duplicates, or selection identity.
fn refresh_active_playlist_uris(
    active_source_key: &Rc<RefCell<String>>,
    master_tracks: &Rc<RefCell<Vec<TrackObject>>>,
    committed_local_rows: &[TrackObject],
) {
    if !active_source_key
        .borrow()
        .starts_with(PLAYLIST_SOURCE_PREFIX)
    {
        return;
    }

    let rows = master_tracks.borrow();
    let refreshed = refresh_projected_library_uris(&rows, committed_local_rows);
    if refreshed > 0 {
        info!(refreshed, "Refreshed active playlist paths");
    }
}

/// Spawn the library event receiver loop on the GTK main thread.
#[allow(clippy::too_many_arguments)]
fn setup_library_events(
    engine_rx: async_channel::Receiver<LibraryEvent>,
    rt_handle: tokio::runtime::Handle,
    track_store: gtk::gio::ListStore,
    status_label: gtk::Label,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    active_source_key: Rc<RefCell<String>>,
    source_navigation: Rc<RefCell<SourceNavigation>>,
    browser_widget: &gtk::Box,
    browser_state: browser::BrowserState,
    column_view: &gtk::ColumnView,
    sidebar_store: gtk::gio::ListStore,
    sidebar_selection: gtk::SingleSelection,
    scan_spinner: gtk::Spinner,
    pending_connection: Rc<RefCell<Option<PendingConnection>>>,
    playback_session: Rc<RefCell<PlaybackSession>>,
    root_trust_prompts: root_trust::RootTrustPromptController,
    invalidate_source_playback: SourcePlaybackInvalidator,
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
            // A remote connection can be replaced while its track snapshot is
            // queued for GTK. Validate exact ownership at the publication
            // boundary so an older session cannot repopulate the source.
            match &event {
                LibraryEvent::RemoteSync {
                    source_key,
                    generation,
                    lease_key,
                    ..
                } if !crate::source_registry::is_current_source(
                    source_key,
                    *generation,
                    *lease_key,
                ) =>
                {
                    tracing::debug!(
                        generation,
                        %lease_key,
                        "Ignoring stale remote library sync"
                    );
                    continue;
                }
                LibraryEvent::DaapSync {
                    source_key,
                    generation,
                    session_key,
                    ..
                } if !crate::daap::is_current_session(source_key, *generation, *session_key) => {
                    tracing::debug!(
                        generation,
                        %session_key,
                        "Ignoring stale DAAP library sync"
                    );
                    continue;
                }
                _ => {}
            }

            match event {
                LibraryEvent::FullSync(tracks) => {
                    info!(count = tracks.len(), "Received full library sync");

                    let objects: Vec<TrackObject> =
                        tracks.iter().map(arch_track_to_object).collect();

                    refresh_active_playlist_uris(&active_source_key, &master_tracks, &objects);

                    // A bulk change — a renamed album, a reconciliation — can
                    // move the files behind tracks the queue is holding. The
                    // queue owns identities, not rows, so it re-resolves them
                    // from the snapshot rather than being rebuilt from the view.
                    refresh_playback_queue(&playback_session, &objects);

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

                LibraryEvent::RemoteSync {
                    source_key,
                    generation: _,
                    lease_key,
                    tracks,
                } => {
                    let replaces_current_queue = {
                        let session = playback_session.borrow();
                        session.current_identity().is_some_and(|identity| {
                            identity.source_id.as_str() == source_key.as_str()
                                && session.current().is_some_and(|item| {
                                    !crate::source_registry::stream_reference_uses_lease(
                                        item.uri(),
                                        lease_key,
                                    )
                                })
                        })
                    };
                    if replaces_current_queue {
                        // Retargeting old queue IDs could cross into a new
                        // login or library on the same URL. Stop it instead;
                        // the newly published rows carry the replacement lease.
                        invalidate_source_playback(&source_key);
                    }

                    info!(count = tracks.len(), "Received remote library sync");

                    let objects: Vec<TrackObject> = tracks
                        .iter()
                        .map(|track| arch_remote_track_to_object(track, lease_key))
                        .collect();
                    publish_remote_library(
                        source_key,
                        objects,
                        &source_tracks,
                        &sidebar_store,
                        &pending_connection,
                        &sidebar_selection,
                        &active_source_key,
                        &source_navigation,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                }

                LibraryEvent::TrackUpserted(track) => {
                    let obj = arch_track_to_object(&track);
                    let uri = obj.uri();

                    refresh_active_playlist_uris(
                        &active_source_key,
                        &master_tracks,
                        std::slice::from_ref(&obj),
                    );

                    // A single-file rename keeps the track's identity and moves
                    // its path, so a queue holding it must follow the track, not
                    // the path it was captured at.
                    refresh_playback_queue(&playback_session, std::slice::from_ref(&obj));

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
                        let active_source_key = active_source_key.clone();
                        let source_navigation = source_navigation.clone();
                        let navigation_request = source_navigation.borrow().latest_request("local");
                        let pending_connection = pending_connection.clone();

                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            let Some(navigation_request) = navigation_request else {
                                return;
                            };
                            let pending_request = pending_connection
                                .borrow()
                                .as_ref()
                                .map(|pending| pending.request().clone());
                            let may_refresh = source_navigation.borrow().may_refresh_visible(
                                "local",
                                &navigation_request,
                                pending_request.as_ref(),
                            );
                            if gen_rc.get() != gen
                                || *active_source_key.borrow() != "local"
                                || !may_refresh
                            {
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
                        let active_source_key = active_source_key.clone();
                        let source_navigation = source_navigation.clone();
                        let navigation_request = source_navigation.borrow().latest_request("local");
                        let pending_connection = pending_connection.clone();

                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            let Some(navigation_request) = navigation_request else {
                                return;
                            };
                            let pending_request = pending_connection
                                .borrow()
                                .as_ref()
                                .map(|pending| pending.request().clone());
                            let may_refresh = source_navigation.borrow().may_refresh_visible(
                                "local",
                                &navigation_request,
                                pending_request.as_ref(),
                            );
                            if gen_rc.get() != gen
                                || *active_source_key.borrow() != "local"
                                || !may_refresh
                            {
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

                LibraryEvent::PlaylistProjectionsInvalidated => {
                    let active_key = active_source_key.borrow().clone();

                    // Any local mutation can change a live smart playlist, and
                    // reconciliation can remint/relink regular-playlist track
                    // IDs. Retire pre-settlement requests before clearing the
                    // cache so a late query cannot put stale rows back.
                    source_navigation
                        .borrow_mut()
                        .invalidate_prefix(PLAYLIST_SOURCE_PREFIX);
                    source_tracks
                        .borrow_mut()
                        .retain(|key, _| !key.starts_with(PLAYLIST_SOURCE_PREFIX));

                    if let Some(playlist_id) = active_key
                        .strip_prefix(PLAYLIST_SOURCE_PREFIX)
                        .map(str::to_string)
                    {
                        // The old rows may hold orphaned/reminted IDs. Do not
                        // leave them actionable while the settled projection
                        // is loading.
                        display_tracks(
                            &[],
                            &track_store,
                            &master_tracks,
                            &browser_widget,
                            &browser_state,
                            &status_label,
                            &column_view,
                        );

                        // `active_source_key` names the visible rows, while
                        // SourceNavigation names the user's latest intent.
                        // During remote authentication those intentionally
                        // differ. Never let background playlist maintenance
                        // supersede that newer remote intent.
                        if source_navigation.borrow().is_key(&active_key) {
                            let request = source_navigation.borrow_mut().select(active_key.clone());
                            super::source_connect::load_playlist_source(
                                rt_handle.clone(),
                                playlist_id,
                                request,
                                source_navigation.clone(),
                                source_tracks.clone(),
                                active_source_key.clone(),
                                track_store.clone(),
                                master_tracks.clone(),
                                browser_widget.clone(),
                                browser_state.clone(),
                                status_label.clone(),
                                column_view.clone(),
                            );
                        }
                    }
                }

                LibraryEvent::PlaylistsLoaded(playlists) => {
                    info!(count = playlists.len(), "Populating sidebar with playlists");
                    let active_key = active_source_key.borrow().clone();
                    let active_playlist_id = source_navigation
                        .borrow()
                        .is_key(&active_key)
                        .then(|| {
                            active_key
                                .strip_prefix(PLAYLIST_SOURCE_PREFIX)
                                .map(str::to_string)
                        })
                        .flatten();

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
                        let mut active_position = None;
                        for (idx, (id, name, is_smart)) in playlists.iter().enumerate() {
                            let src = SourceObject::playlist(name, id, *is_smart);
                            let position = insert_pos + idx as u32;
                            sidebar_store.insert(position, &src);
                            if active_playlist_id.as_deref() == Some(id.as_str()) {
                                active_position = Some(position);
                            }
                        }

                        // Rebuilding the rows invalidates GtkSingleSelection's
                        // selected object. Restore the row that corresponds to
                        // the still-active playlist so sidebar and content do
                        // not diverge during watcher fallback scans.
                        if let Some(position) = active_position {
                            sidebar_selection.set_selected(position);
                        }
                    }
                }

                LibraryEvent::RootTrustRequired(requests) => {
                    root_trust_prompts.enqueue(requests);
                }

                LibraryEvent::RootTrustFinished {
                    request_id,
                    path,
                    reason,
                    outcome,
                } => {
                    root_trust_prompts.handle_finished(request_id, path, reason, outcome);
                }

                LibraryEvent::Error(msg) => {
                    tracing::error!(error = %msg, "Library engine error");
                    scan_spinner.set_spinning(false);
                    scan_spinner.set_visible(false);
                }

                LibraryEvent::DaapSync {
                    source_key, tracks, ..
                } => {
                    // DAAP publishes one snapshot per retained session. A new
                    // snapshot for the same source therefore replaces the
                    // session embedded in any captured `daap://` queue refs.
                    invalidate_source_playback(&source_key);
                    info!(count = tracks.len(), "Received DAAP library sync");
                    let objects: Vec<TrackObject> =
                        tracks.iter().map(arch_track_to_object).collect();
                    publish_remote_library(
                        source_key,
                        objects,
                        &source_tracks,
                        &sidebar_store,
                        &pending_connection,
                        &sidebar_selection,
                        &active_source_key,
                        &source_navigation,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn publish_remote_library(
    source_key: String,
    objects: Vec<TrackObject>,
    source_tracks: &Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    sidebar_store: &gtk::gio::ListStore,
    pending_connection: &Rc<RefCell<Option<PendingConnection>>>,
    sidebar_selection: &gtk::SingleSelection,
    active_source_key: &Rc<RefCell<String>>,
    source_navigation: &Rc<RefCell<SourceNavigation>>,
    track_store: &gtk::gio::ListStore,
    master_tracks: &Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: &gtk::Box,
    browser_state: &browser::BrowserState,
    status_label: &gtk::Label,
    column_view: &gtk::ColumnView,
) {
    source_tracks
        .borrow_mut()
        .insert(source_key.clone(), objects.clone());

    let pending_intent = pending_connection.borrow().clone();
    let should_auto_select = pending_intent
        .as_ref()
        .is_some_and(|pending| pending.may_auto_select(&source_key, &source_navigation.borrow()));
    let should_clear_pending = pending_intent
        .as_ref()
        .is_some_and(|pending| pending.source_key() == source_key);
    if should_clear_pending {
        // Clear before any programmatic selection: the selection handler
        // otherwise treats this completed connection as still pending.
        *pending_connection.borrow_mut() = None;
    }

    let mut auto_selected = false;
    for i in 0..sidebar_store.n_items() {
        if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
            if src.server_url() == source_key && !src.connected() {
                src.set_connected(true);
                src.set_connecting(false);
                let src = src.clone();
                sidebar_store.remove(i);
                sidebar_store.insert(i, &src);
                if should_auto_select {
                    sidebar_selection.set_selected(i);
                    auto_selected = true;
                }
                break;
            }
        }
    }

    if !auto_selected
        && *active_source_key.borrow() == source_key
        && source_navigation.borrow().is_key(&source_key)
    {
        display_tracks(
            &objects,
            track_store,
            master_tracks,
            browser_widget,
            browser_state,
            status_label,
            column_view,
        );
    }
}

/// Convert an architecture `Track` to a UI `TrackObject`.
pub fn arch_track_to_object(t: &crate::architecture::models::Track) -> TrackObject {
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

    track_to_object(t, &uri, t.cover_art_url.as_ref().map(url::Url::as_str))
}

/// Convert a standard remote track using only opaque registry references.
fn arch_remote_track_to_object(
    track: &crate::architecture::models::Track,
    lease_key: uuid::Uuid,
) -> TrackObject {
    let stream_reference = crate::source_registry::stream_reference(lease_key, track.id);
    let artwork_reference = crate::source_registry::artwork_reference(lease_key, track.id);
    track_to_object(track, &stream_reference, Some(&artwork_reference))
}

fn track_to_object(
    t: &crate::architecture::models::Track,
    uri: &str,
    artwork_reference: Option<&str>,
) -> TrackObject {
    let obj = TrackObject::new(
        t.track_number.unwrap_or(0),
        &t.title,
        t.duration_secs.unwrap_or(0),
        &t.artist_name,
        &t.album_title,
        t.genre.as_deref().unwrap_or("Unknown"),
        t.composer.as_deref().unwrap_or(""),
        t.year.unwrap_or(0),
        &t.date_modified
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        t.bitrate_kbps.unwrap_or(0),
        t.sample_rate_hz.unwrap_or(0),
        t.play_count.unwrap_or(0),
        t.format.as_deref().unwrap_or(""),
        uri,
    );

    obj.set_track_id(&t.id.to_string());

    if let Some(artwork_reference) = artwork_reference {
        obj.set_cover_art_url(artwork_reference);
    }

    // Propagate album artist for browser grouping.
    if let Some(ref aa) = t.album_artist_name {
        obj.set_album_artist(aa);
    }

    // Propagate disc number (shown in the Properties dialog).
    obj.set_disc_number(t.disc_number.unwrap_or(0));

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
pub fn category_for_backend(backend_type: &str) -> &'static str {
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
pub fn remove_empty_category_header(store: &gtk::gio::ListStore, category: &str) {
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
