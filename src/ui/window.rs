//! Main application window — assembles all UI components and bridges
//! the background library engine to the GTK main thread.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tracing::info;

use crate::local::engine::{LibraryEngine, LibraryEvent};

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

/// Build and present the main Tributary window.
pub fn build_window(
    app: &adw::Application,
    rt_handle: tokio::runtime::Handle,
    engine_tx: async_channel::Sender<LibraryEvent>,
    engine_rx: async_channel::Receiver<LibraryEvent>,
) {
    info!("Building main window (Phase 3 — live local backend)");

    // ── Load custom CSS ──────────────────────────────────────────────
    load_css();

    // ── Sidebar sources (static for now) ─────────────────────────────
    let sources = super::dummy_data::build_sources();

    // ── Header Bar with scan spinner ─────────────────────────────────
    let header = header_bar::build_header_bar();

    let scan_spinner = gtk::Spinner::builder()
        .spinning(true)
        .tooltip_text("Scanning library…")
        .build();
    header.pack_end(&scan_spinner);

    // ── Sidebar ──────────────────────────────────────────────────────
    let sidebar_widget = sidebar::build_sidebar(&sources);

    // ── Tracklist (starts empty — populated by FullSync) ──────────────
    let empty_tracks: Vec<TrackObject> = Vec::new();
    let (tracklist_widget, track_store, status_label) =
        tracklist::build_tracklist(&empty_tracks);

    // ── Master track list (shared, mutable) ──────────────────────────
    let master_tracks: Rc<RefCell<Vec<TrackObject>>> =
        Rc::new(RefCell::new(Vec::new()));

    // ── Browser (starts empty, updated by FullSync) ──────────────────
    let track_store_for_filter = track_store.clone();
    let status_label_for_filter = status_label.clone();
    let master_for_filter = master_tracks.clone();

    let on_filter = Box::new(move |genre: Option<String>, artist: Option<String>, album: Option<String>| {
        let master = master_for_filter.borrow();
        let matching: Vec<&TrackObject> = master
            .iter()
            .filter(|t| {
                if let Some(ref g) = genre {
                    if &t.genre() != g { return false; }
                }
                if let Some(ref a) = artist {
                    if &t.artist() != a { return false; }
                }
                if let Some(ref al) = album {
                    if &t.album() != al { return false; }
                }
                true
            })
            .collect();

        track_store_for_filter.remove_all();
        let mut snapshot = Vec::new();
        for t in &matching {
            let new_t = TrackObject::new(
                t.track_number(), &t.title(), t.duration_secs(),
                &t.artist(), &t.album(), &t.genre(), t.year(),
                &t.date_modified(), t.bitrate_kbps(), t.sample_rate_hz(),
                t.play_count(), &t.format(),
            );
            track_store_for_filter.append(&new_t);
            snapshot.push(new_t);
        }
        tracklist::update_status(&status_label_for_filter, &snapshot);
    });

    let browser_widget = browser::build_browser(&empty_tracks, on_filter);

    // ── Right content ────────────────────────────────────────────────
    let right_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .position(BROWSER_POS)
        .wide_handle(true)
        .vexpand(true).hexpand(true)
        .start_child(&browser_widget)
        .end_child(&tracklist_widget)
        .shrink_start_child(false).shrink_end_child(false)
        .build();

    let main_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .position(SIDEBAR_POS)
        .wide_handle(true)
        .vexpand(true).hexpand(true)
        .start_child(&sidebar_widget)
        .end_child(&right_paned)
        .shrink_start_child(false).shrink_end_child(false)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
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

    // ── Receive LibraryEvents on GTK main thread ─────────────────────
    let track_store_for_events = track_store.clone();
    let status_label_for_events = status_label.clone();
    let master_tracks_for_events = master_tracks.clone();
    let spinner = scan_spinner.clone();

    // Keep references to browser stores so we can rebuild them on FullSync
    let browser_ref = browser_widget.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = engine_rx.recv().await {
            match event {
                LibraryEvent::FullSync(tracks) => {
                    info!(count = tracks.len(), "Received full library sync");

                    // Convert to TrackObjects
                    let objects: Vec<TrackObject> = tracks.iter().map(arch_track_to_object).collect();

                    // Update tracklist store
                    track_store_for_events.remove_all();
                    for obj in &objects {
                        track_store_for_events.append(obj);
                    }

                    // Update status bar
                    tracklist::update_status(&status_label_for_events, &objects);

                    // Rebuild browser panes with real data
                    browser::rebuild_browser_data(&browser_ref, &objects);

                    // Update master tracks
                    *master_tracks_for_events.borrow_mut() = objects;
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
                    spinner.set_spinning(false);
                    spinner.set_visible(false);
                }

                LibraryEvent::Error(msg) => {
                    tracing::error!(error = %msg, "Library engine error");
                    spinner.set_spinning(false);
                    spinner.set_visible(false);
                }
            }
        }
    });

    window.present();
    info!("Main window presented");
}

/// Convert an architecture `Track` to a UI `TrackObject`.
fn arch_track_to_object(t: &crate::architecture::models::Track) -> TrackObject {
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
