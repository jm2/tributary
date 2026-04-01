//! Main application window — assembles all UI components and bridges
//! the background library engine, the GStreamer player, and the OS
//! media controls to the GTK main thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tracing::{info, warn};

use crate::audio::{PlayerEvent, PlayerState};
use crate::desktop_integration::MediaAction;
use crate::local::engine::{LibraryEngine, LibraryEvent};
use crate::ui::header_bar::RepeatMode;

use super::browser;
use super::header_bar;
use super::objects::{SourceObject, TrackObject};
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

    // ── Load custom CSS ──────────────────────────────────────────────
    load_css();

    // ── Sidebar sources ────────────────────────────────────────────────
    let sources = super::dummy_data::build_sources();

    // If env vars are set, add a pre-configured Subsonic entry.
    let mut sources = sources;
    if let (Ok(url), Ok(_user), Ok(_pass)) = (
        std::env::var("SUBSONIC_URL"),
        std::env::var("SUBSONIC_USER"),
        std::env::var("SUBSONIC_PASS"),
    ) {
        // Mark as connected (env vars provide credentials).
        let src = SourceObject::source("Subsonic (env)", "subsonic", "network-server-symbolic");
        sources.push(src);
        info!(url = %url, "Subsonic server configured via env vars");
    }

    // ── Header Bar with all interactive widgets ──────────────────────
    let hb = header_bar::build_header_bar();

    let scan_spinner = gtk::Spinner::builder()
        .spinning(true)
        .tooltip_text("Scanning library…")
        .build();
    hb.header.pack_end(&scan_spinner);

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
    let (sidebar_widget, sidebar_store, sidebar_selection) = sidebar::build_sidebar(&sources);

    // ── Tracklist (starts empty — populated by FullSync) ──────────────
    let empty_tracks: Vec<TrackObject> = Vec::new();
    let (tracklist_widget, track_store, status_label, column_view, sort_model) =
        tracklist::build_tracklist(&empty_tracks);

    // ── Shared playback state ────────────────────────────────────────
    let master_tracks: Rc<RefCell<Vec<TrackObject>>> = Rc::new(RefCell::new(Vec::new()));
    let current_pos: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    let seeking = Rc::new(Cell::new(false));

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
        move |genre: Option<String>, artist: Option<String>, album: Option<String>| {
            let master = master_for_filter.borrow();
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
    let music_dir = dirs::home_dir()
        .expect("Could not determine home directory")
        .join("Music");

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

    // ── mDNS zero-config discovery ─────────────────────────────────
    {
        let discovery_rx = crate::discovery::start_discovery();
        let store = sidebar_store.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(server) = discovery_rx.recv().await {
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
                    "Adding discovered server to sidebar"
                );
                let src = SourceObject::discovered(&server.name, "subsonic", &server.url);
                store.append(&src);
            }
        });
    }

    // ── Sidebar selection: source switching + auth dialog ───────────
    let sidebar_store_for_events = sidebar_store.clone();
    let sidebar_sel_for_events = sidebar_selection.clone();
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

            // Determine the source key.
            let url = src.server_url();
            let key = if url.is_empty() {
                "local".to_string()
            } else {
                url.clone()
            };

            // ── Connected source: switch view ───────────────────────
            if src.connected() {
                *active_source_key.borrow_mut() = key.clone();
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

            // ── Discovered (unauthenticated): show auth dialog ──────
            let server_name = src.name();
            let server_url = src.server_url();
            let engine_tx = engine_tx.clone();
            let rt_handle = rt_handle.clone();
            let win = win.clone();
            let sidebar_store = sidebar_store.clone();
            let selected_pos = sel.selected();

            let name_for_closure = server_name.clone();
            let url_for_closure = server_url.clone();

            show_auth_dialog(&win, &server_name, &server_url, move |user, pass| {
                let engine_tx = engine_tx.clone();
                let server_url = url_for_closure.clone();
                let server_name = name_for_closure.clone();

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
                // One-shot to signal failure back to the main thread so we
                // can clear the spinner (GObjects are not Send).
                let (fail_tx, fail_rx) = async_channel::bounded::<()>(1);
                let sidebar_store_for_fail = sidebar_store.clone();
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
                    }
                });

                rt_handle.spawn(async move {
                    info!(server = %server_url, "Authenticating with Subsonic...");
                    match crate::subsonic::SubsonicBackend::connect(
                        &server_name,
                        &server_url,
                        &user,
                        &pass,
                    )
                    .await
                    {
                        Ok(backend) => {
                            let tracks: Vec<crate::architecture::models::Track> =
                                backend.all_tracks().await;
                            info!(count = tracks.len(), "Subsonic library fetched");
                            let _ = engine_tx
                                .send(LibraryEvent::RemoteSync {
                                    source_key: server_url,
                                    tracks,
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Subsonic auth failed");
                            let _ = engine_tx
                                .send(LibraryEvent::Error(format!("Subsonic auth failed: {e}")))
                                .await;
                            let _ = fail_tx.send(()).await;
                        }
                    }
                });
            });
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
            );
            return;
        }
    };
    let player = Rc::new(RefCell::new(player));

    // Sync the volume slider to the player's persisted volume.
    hb.volume_adj.set_value(player.borrow().volume());

    // ── Extract native window handle (HWND on Windows) ──────────────
    let hwnd = extract_hwnd(&window);

    // ── Create OS media controls ────────────────────────────────────
    let media_ctrl: Rc<RefCell<Option<crate::desktop_integration::MediaController>>> =
        match crate::desktop_integration::MediaController::new(hwnd) {
            Ok((ctrl, media_rx)) => {
                let player = player.clone();
                glib::MainContext::default().spawn_local(async move {
                    while let Ok(action) = media_rx.recv().await {
                        info!(?action, "OS media key");
                        match action {
                            MediaAction::Play => player.borrow().play(),
                            MediaAction::Pause => player.borrow().pause(),
                            MediaAction::Toggle => player.borrow().toggle_play_pause(),
                            MediaAction::Stop => player.borrow().stop(),
                            MediaAction::Next | MediaAction::Previous => {
                                // TODO: forward to next/prev logic once playlist
                                // queue is decoupled from the tracklist store.
                            }
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
        let player = player.clone();
        let media_ctrl = media_ctrl.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sort_model = sort_model.clone();
        let current_pos = current_pos.clone();
        let shuffle = hb.shuffle_button.clone();

        hb.play_button.connect_clicked(move |_| {
            if current_pos.get().is_some() {
                // Already have a track loaded — just toggle.
                player.borrow().toggle_play_pause();
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
                        player: player.clone(),
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

    // ── Wire volume scale ───────────────────────────────────────────
    {
        let player = player.clone();
        hb.volume_adj.connect_value_changed(move |adj| {
            player.borrow_mut().set_volume(adj.value());
        });
    }

    // ── Wire progress scrubber (seek on user interaction) ───────────
    {
        let player = player.clone();
        let seeking = seeking.clone();
        hb.progress_adj.connect_value_changed(move |adj| {
            if !seeking.get() {
                player.borrow().seek_to(adj.value() as u64);
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
        let player = player.clone();
        let media_ctrl = media_ctrl.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let current_pos = current_pos.clone();

        column_view.connect_activate(move |_view, position| {
            play_track_at(
                position,
                &PlaybackContext {
                    model: sm.clone(),
                    player: player.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    current_pos: current_pos.clone(),
                },
            );
        });
    }

    // ── Wire Next button ────────────────────────────────────────────
    {
        let player = player.clone();
        let media_ctrl = media_ctrl.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let current_pos = current_pos.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();

        hb.next_button.connect_clicked(move |_| {
            advance_track(
                &PlaybackContext {
                    model: sm.clone(),
                    player: player.clone(),
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
        let player = player.clone();
        let media_ctrl = media_ctrl.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let current_pos = current_pos.clone();

        hb.prev_button.connect_clicked(move |_| {
            let Some(pos) = current_pos.get() else { return };

            // If more than 3 s into the track, restart it.
            let position_ms = player.borrow().position_ms().unwrap_or(0);
            if position_ms > PREV_RESTART_THRESHOLD_MS {
                player.borrow().seek_to(0);
                return;
            }

            // Otherwise go to the previous track (or restart track 0).
            if pos > 0 {
                play_track_at(
                    pos - 1,
                    &PlaybackContext {
                        model: sm.clone(),
                        player: player.clone(),
                        title_label: title_label.clone(),
                        artist_label: artist_label.clone(),
                        media_ctrl: media_ctrl.clone(),
                        current_pos: current_pos.clone(),
                    },
                );
            } else {
                player.borrow().seek_to(0);
            }
        });
    }

    // ── Receive PlayerEvents on GTK main thread ─────────────────────
    {
        let play_btn = hb.play_button.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let progress_adj = hb.progress_adj.clone();
        let position_label = hb.position_label.clone();
        let duration_label = hb.duration_label.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();
        let seeking = seeking.clone();
        let media_ctrl = media_ctrl.clone();
        let player = player.clone();
        let sm = sort_model.clone();
        let current_pos = current_pos.clone();

        // Pre-build a spinner widget for the buffering state.
        let buffering_spinner = gtk::Spinner::builder()
            .spinning(true)
            .width_request(16)
            .height_request(16)
            .build();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = player_rx.recv().await {
                match event {
                    PlayerEvent::StateChanged(state) => {
                        match state {
                            PlayerState::Buffering => {
                                // Replace the button icon with a spinner.
                                play_btn.set_child(Some(&buffering_spinner));
                                play_btn.set_icon_name("");
                            }
                            PlayerState::Playing => {
                                // Restore icon: show pause.
                                play_btn.set_child(Option::<&gtk::Widget>::None);
                                play_btn.set_icon_name("media-playback-pause-symbolic");
                            }
                            _ => {
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
                        seeking.set(true);
                        progress_adj.set_upper(duration_ms as f64);
                        progress_adj.set_value(position_ms as f64);
                        seeking.set(false);

                        position_label.set_label(&format_ms(position_ms));
                        duration_label.set_label(&format_ms(duration_ms));
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
                                        player: player.clone(),
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
                                player: player.clone(),
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
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

/// Extract the native window handle for `souvlaki`.
#[cfg(target_os = "windows")]
fn extract_hwnd(window: &adw::ApplicationWindow) -> Option<*mut std::ffi::c_void> {
    use gtk::prelude::NativeExt;

    let surface = window.surface()?;
    let win32_surface = surface.downcast_ref::<gdk4_win32::Win32Surface>()?;
    let hwnd = win32_surface.handle();
    Some(hwnd.0)
}

#[cfg(not(target_os = "windows"))]
fn extract_hwnd(_window: &adw::ApplicationWindow) -> Option<*mut std::ffi::c_void> {
    None
}

/// Try to play the track at `position` in the given model.
///
/// Uses the `SortListModel` so positions match the visible sorted order.
/// Updates the now-playing labels, the OS media overlay metadata, and
/// the `current_pos` tracker.  Returns `true` on success.
/// Replace the visible tracklist, browser, and master track list with a
/// new set of tracks (e.g., when switching sidebar sources).
fn display_tracks(
    objects: &[TrackObject],
    track_store: &gtk::gio::ListStore,
    master_tracks: &RefCell<Vec<TrackObject>>,
    browser_widget: &gtk::Box,
    browser_state: &browser::BrowserState,
    status_label: &gtk::Label,
    column_view: &gtk::ColumnView,
) {
    track_store.remove_all();
    for obj in objects {
        track_store.append(obj);
    }
    tracklist::update_status(status_label, objects);
    browser::rebuild_browser_data(browser_widget, browser_state, objects);
    *master_tracks.borrow_mut() = objects.to_vec();
    column_view.scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
}

struct PlaybackContext {
    model: gtk::SortListModel,
    player: Rc<RefCell<crate::audio::Player>>,
    title_label: gtk::Label,
    artist_label: gtk::Label,
    media_ctrl: Rc<RefCell<Option<crate::desktop_integration::MediaController>>>,
    current_pos: Rc<Cell<Option<u32>>>,
}

fn play_track_at(position: u32, ctx: &PlaybackContext) -> bool {
    let Some(item) = ctx.model.item(position) else {
        return false;
    };
    let Some(track) = item.downcast_ref::<TrackObject>() else {
        return false;
    };
    let uri = track.uri();
    if uri.is_empty() {
        warn!("Track has no playable URI");
        return false;
    }

    info!(
        title = %track.title(),
        artist = %track.artist(),
        "Playing track"
    );

    ctx.player.borrow().load_uri(&uri);
    ctx.title_label.set_label(&track.title());
    ctx.artist_label
        .set_label(&format!("{} \u{2014} {}", track.artist(), track.album()));
    ctx.current_pos.set(Some(position));

    if let Some(ref mut ctrl) = *ctx.media_ctrl.borrow_mut() {
        ctrl.update_metadata(&track.title(), &track.artist(), &track.album());
    }

    true
}

/// Advance to the next track, respecting shuffle and repeat-all.
///
/// Returns `true` if a new track was loaded, `false` if we've reached
/// the end (caller should reset to idle).
fn advance_track(ctx: &PlaybackContext, repeat_mode: RepeatMode, shuffle: bool) -> bool {
    let n = ctx.model.n_items();
    if n == 0 {
        return false;
    }

    if shuffle {
        // Pick a random track, avoiding the current one if possible.
        let pos = if n > 1 {
            let cur = ctx.current_pos.get().unwrap_or(u32::MAX);
            loop {
                let r = fastrand::u32(..n);
                if r != cur {
                    break r;
                }
            }
        } else {
            0
        };
        return play_track_at(pos, ctx);
    }

    // Sequential advance.
    let Some(pos) = ctx.current_pos.get() else {
        return play_track_at(0, ctx);
    };

    let next = pos + 1;
    if next < n {
        play_track_at(next, ctx)
    } else if repeat_mode == RepeatMode::All && n > 0 {
        play_track_at(0, ctx)
    } else {
        false
    }
}

/// Format milliseconds as `m:ss` (or `h:mm:ss` for ≥ 1 hour).
fn format_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
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
) {
    let browser_widget = browser_widget.clone();
    let column_view = column_view.clone();

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
                    info!(
                        title = %track.title,
                        artist = %track.artist_name,
                        "Track upserted"
                    );
                }

                LibraryEvent::TrackRemoved(path) => {
                    info!(path = %path, "Track removed");
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

    TrackObject::new(
        t.track_number.unwrap_or(0),
        &t.title,
        t.duration_secs.unwrap_or(0),
        &t.artist_name,
        &t.album_title,
        t.genre.as_deref().unwrap_or(""),
        t.year.unwrap_or(0),
        &t.date_modified
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        t.bitrate_kbps.unwrap_or(0),
        t.sample_rate_hz.unwrap_or(0),
        t.play_count.unwrap_or(0),
        t.format.as_deref().unwrap_or(""),
        &uri,
    )
}

/// Load the custom CSS from the embedded stylesheet.
fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));

    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("Could not get default display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

// ── Playback mode persistence ───────────────────────────────────────

fn settings_path(name: &str) -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join(name))
}

fn load_repeat_mode() -> RepeatMode {
    settings_path("repeat")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| match s.trim() {
            "all" => RepeatMode::All,
            "one" => RepeatMode::One,
            _ => RepeatMode::Off,
        })
        .unwrap_or(RepeatMode::Off)
}

fn save_repeat_mode(mode: RepeatMode) {
    if let Some(path) = settings_path("repeat") {
        let s = match mode {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        };
        let _ = std::fs::write(path, s);
    }
}

fn load_shuffle() -> bool {
    settings_path("shuffle")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

fn save_shuffle(active: bool) {
    if let Some(path) = settings_path("shuffle") {
        let _ = std::fs::write(path, if active { "true" } else { "false" });
    }
}

fn save_sort_state(column_view: &gtk::ColumnView) {
    let Some(sorter) = column_view.sorter() else {
        return;
    };
    let Some(cv_sorter) = sorter.downcast_ref::<gtk::ColumnViewSorter>() else {
        return;
    };

    match cv_sorter.primary_sort_column() {
        Some(column) => {
            let title = column.title().map(|t| t.to_string()).unwrap_or_default();
            let dir = match cv_sorter.primary_sort_order() {
                gtk::SortType::Descending => "desc",
                _ => "asc",
            };
            if let Some(path) = settings_path("sort") {
                let _ = std::fs::write(path, format!("{title}\n{dir}"));
            }
        }
        None => {
            // No active sort — remove saved state.
            if let Some(path) = settings_path("sort") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn restore_sort_state(column_view: &gtk::ColumnView) {
    let Some(text) = settings_path("sort").and_then(|p| std::fs::read_to_string(p).ok()) else {
        return;
    };
    let mut lines = text.lines();
    let Some(title) = lines.next() else { return };
    let order = match lines.next() {
        Some("desc") => gtk::SortType::Descending,
        _ => gtk::SortType::Ascending,
    };

    let columns = column_view.columns();
    for i in 0..columns.n_items() {
        if let Some(col) = columns.item(i) {
            let col = col.downcast_ref::<gtk::ColumnViewColumn>().unwrap();
            if col.title().is_some_and(|t| t == title) {
                column_view.sort_by_column(Some(col), order);
                return;
            }
        }
    }
}

// ── Auth dialog for discovered servers ──────────────────────────────

/// Present an `adw::AlertDialog` asking for Subsonic credentials.
///
/// `on_connect` is called with `(username, password)` if the user
/// clicks Connect.  Cancel / Escape simply dismisses the dialog.
fn show_auth_dialog(
    window: &adw::ApplicationWindow,
    server_name: &str,
    server_url: &str,
    on_connect: impl Fn(String, String) + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Connect to {server_name}"))
        .body(server_url)
        .close_response("cancel")
        .default_response("connect")
        .build();

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("connect", "Connect");
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);

    // ── Credential entry fields ─────────────────────────────────────
    let user_entry = gtk::Entry::builder()
        .placeholder_text("Username")
        .activates_default(true)
        .build();

    let pass_entry = gtk::PasswordEntry::builder()
        .placeholder_text("Password")
        .show_peek_icon(true)
        .activates_default(true)
        .build();

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .build();
    vbox.append(&user_entry);
    vbox.append(&pass_entry);

    dialog.set_extra_child(Some(&vbox));

    let user_entry_clone = user_entry.clone();
    let pass_entry_clone = pass_entry.clone();

    dialog.connect_response(None, move |_dialog, response| {
        if response == "connect" {
            let user = user_entry_clone.text().to_string();
            let pass = pass_entry_clone.text().to_string();
            if !user.is_empty() && !pass.is_empty() {
                on_connect(user, pass);
            }
        }
    });

    dialog.present(Some(window));
}
