//! Main application window — assembles all UI components.
//!
//! Layout:
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      HeaderBar (full)                          │
//! ├──────────┬──────────────────────────────────────────────────────┤
//! │          │  ╔══════════╦══════════╦══════════╗                  │
//! │ Sidebar  │  ║  Genre   ║  Artist  ║  Album   ║  ← Browser     │
//! │          │  ╚══════════╩══════════╩══════════╝                  │
//! │ (sources)│  ┌────────────────────────────────────────────────┐  │
//! │          │  │  #  Title  Time  Artist  Album  Genre  Year…  │  │
//! │          │  │  1  ...    3:42  ...     ...    ...    2019    │  │
//! │          │  └────────────────────────────────────────────────┘  │
//! │          │  40 songs, 3.2 hours                                 │
//! └──────────┴──────────────────────────────────────────────────────┘
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use tracing::info;

use super::browser;
use super::dummy_data;
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
pub fn build_window(app: &adw::Application) {
    info!("Building main window (Phase 2 — full UI shell)");

    // ── Load custom CSS ──────────────────────────────────────────────
    load_css();

    // ── Generate dummy data ──────────────────────────────────────────
    let sources = dummy_data::build_sources();
    let all_tracks = dummy_data::build_tracks();

    // ── Header Bar ───────────────────────────────────────────────────
    let header = header_bar::build_header_bar();

    // ── Sidebar ──────────────────────────────────────────────────────
    let sidebar_widget = sidebar::build_sidebar(&sources);

    // ── Tracklist (we need the store and label handles for filtering) ─
    let (tracklist_widget, track_store, status_label) =
        tracklist::build_tracklist(&all_tracks);

    // Keep a copy of all tracks for filtering
    let all_tracks = Rc::new(all_tracks);

    // ── Browser (with filtering callback) ────────────────────────────
    let track_store_for_filter = track_store.clone();
    let status_label_for_filter = status_label.clone();
    let all_tracks_for_filter = all_tracks.clone();

    // Track the currently filtered set for status updates
    let _filtered_tracks: Rc<RefCell<Vec<TrackObject>>> =
        Rc::new(RefCell::new(Vec::new()));
    let ft = _filtered_tracks.clone();

    let on_filter = Box::new(move |genre: Option<String>, artist: Option<String>, album: Option<String>| {
        info!(?genre, ?artist, ?album, "Filter changed — updating tracklist");

        // Filter the master list
        let matching: Vec<&TrackObject> = all_tracks_for_filter
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

        // Rebuild the store
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
            );
            track_store_for_filter.append(&new_t);
            snapshot.push(new_t);
        }

        // Update status
        tracklist::update_status(&status_label_for_filter, &snapshot);
        *ft.borrow_mut() = snapshot;
    });

    let browser_widget = browser::build_browser(&all_tracks, on_filter);

    // ── Right content: Browser (top) + Tracklist (bottom) ────────────
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

    // ── Main: Sidebar (left) + Right content ─────────────────────────
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

    // ── Outer layout: Header + main body ─────────────────────────────
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&main_paned);

    // ── Window ───────────────────────────────────────────────────────
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Tributary")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&content)
        .build();

    window.present();
    info!("Main window presented");
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
