//! Main application window — assembles all UI components and bridges
//! the background library engine, the GStreamer player, and the OS
//! media controls to the GTK main thread.

use std::cell::{Cell, RefCell};
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
use super::objects::TrackObject;
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

    // ── Sidebar sources (static for now) ─────────────────────────────
    let sources = super::dummy_data::build_sources();

    // ── Header Bar with all interactive widgets ──────────────────────
    let hb = header_bar::build_header_bar();

    let scan_spinner = gtk::Spinner::builder()
        .spinning(true)
        .tooltip_text("Scanning library…")
        .build();
    hb.header.pack_end(&scan_spinner);

    // ── Sidebar ──────────────────────────────────────────────────────
    let sidebar_widget = sidebar::build_sidebar(&sources);

    // ── Tracklist (starts empty — populated by FullSync) ──────────────
    let empty_tracks: Vec<TrackObject> = Vec::new();
    let (tracklist_widget, track_store, status_label, column_view, sort_model) =
        tracklist::build_tracklist(&empty_tracks);

    // ── Shared playback state ────────────────────────────────────────
    let master_tracks: Rc<RefCell<Vec<TrackObject>>> = Rc::new(RefCell::new(Vec::new()));
    let current_pos: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    let seeking = Rc::new(Cell::new(false));

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
                &browser_widget,
                browser_state,
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
                    &sort_model,
                    &player.borrow(),
                    &title_label,
                    &artist_label,
                    &media_ctrl,
                    &current_pos,
                );
            }
        });
    }

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
                &sm,
                &player.borrow(),
                &title_label,
                &artist_label,
                &media_ctrl,
                &current_pos,
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
                &sm,
                &player.borrow(),
                &title_label,
                &artist_label,
                &media_ctrl,
                &current_pos,
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
                    &sm,
                    &player.borrow(),
                    &title_label,
                    &artist_label,
                    &media_ctrl,
                    &current_pos,
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

        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = player_rx.recv().await {
                match event {
                    PlayerEvent::StateChanged(state) => {
                        let icon = match state {
                            PlayerState::Playing => "media-playback-pause-symbolic",
                            _ => "media-playback-start-symbolic",
                        };
                        play_btn.set_icon_name(icon);

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
                                    &sm,
                                    &player.borrow(),
                                    &title_label,
                                    &artist_label,
                                    &media_ctrl,
                                    &current_pos,
                                );
                                continue;
                            }
                        }

                        // Auto-advance (shuffle-aware).
                        let advanced = advance_track(
                            &sm,
                            &player.borrow(),
                            &title_label,
                            &artist_label,
                            &media_ctrl,
                            &current_pos,
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
        &browser_widget,
        browser_state,
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
    Some(hwnd.0 as *mut std::ffi::c_void)
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
fn play_track_at(
    position: u32,
    model: &gtk::SortListModel,
    player: &crate::audio::Player,
    title_label: &gtk::Label,
    artist_label: &gtk::Label,
    media_ctrl: &RefCell<Option<crate::desktop_integration::MediaController>>,
    current_pos: &Cell<Option<u32>>,
) -> bool {
    let Some(item) = model.item(position) else {
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

    player.load_uri(&uri);
    title_label.set_label(&track.title());
    artist_label.set_label(&format!("{} \u{2014} {}", track.artist(), track.album()));
    current_pos.set(Some(position));

    if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
        ctrl.update_metadata(&track.title(), &track.artist(), &track.album());
    }

    true
}

/// Advance to the next track, respecting shuffle and repeat-all.
///
/// Returns `true` if a new track was loaded, `false` if we've reached
/// the end (caller should reset to idle).
fn advance_track(
    model: &gtk::SortListModel,
    player: &crate::audio::Player,
    title_label: &gtk::Label,
    artist_label: &gtk::Label,
    media_ctrl: &RefCell<Option<crate::desktop_integration::MediaController>>,
    current_pos: &Cell<Option<u32>>,
    repeat_mode: RepeatMode,
    shuffle: bool,
) -> bool {
    let n = model.n_items();
    if n == 0 {
        return false;
    }

    if shuffle {
        // Pick a random track, avoiding the current one if possible.
        let pos = if n > 1 {
            let cur = current_pos.get().unwrap_or(u32::MAX);
            loop {
                let r = fastrand::u32(..n);
                if r != cur {
                    break r;
                }
            }
        } else {
            0
        };
        return play_track_at(
            pos,
            model,
            player,
            title_label,
            artist_label,
            media_ctrl,
            current_pos,
        );
    }

    // Sequential advance.
    let Some(pos) = current_pos.get() else {
        return play_track_at(
            0,
            model,
            player,
            title_label,
            artist_label,
            media_ctrl,
            current_pos,
        );
    };

    let next = pos + 1;
    if next < n {
        play_track_at(
            next,
            model,
            player,
            title_label,
            artist_label,
            media_ctrl,
            current_pos,
        )
    } else if repeat_mode == RepeatMode::All && n > 0 {
        play_track_at(
            0,
            model,
            player,
            title_label,
            artist_label,
            media_ctrl,
            current_pos,
        )
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
fn setup_library_events(
    engine_rx: async_channel::Receiver<LibraryEvent>,
    track_store: gtk::gio::ListStore,
    status_label: gtk::Label,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: &gtk::Box,
    browser_state: browser::BrowserState,
    scan_spinner: gtk::Spinner,
) {
    let browser_widget = browser_widget.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = engine_rx.recv().await {
            match event {
                LibraryEvent::FullSync(tracks) => {
                    info!(count = tracks.len(), "Received full library sync");

                    let objects: Vec<TrackObject> =
                        tracks.iter().map(arch_track_to_object).collect();

                    track_store.remove_all();
                    for obj in &objects {
                        track_store.append(obj);
                    }

                    tracklist::update_status(&status_label, &objects);
                    browser::rebuild_browser_data(&browser_widget, &browser_state, &objects);

                    *master_tracks.borrow_mut() = objects;
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
