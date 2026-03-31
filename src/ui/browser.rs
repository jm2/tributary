//! Browser — 3-pane genre / artist / album browser with filtering.
//!
//! Selecting an item in any pane filters the items in the downstream
//! panes and updates the tracklist via a callback.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gio;
use gtk::prelude::*;

use super::objects::{BrowserItem, TrackObject};
use tracing::info;

/// Callback invoked when the browser selection changes.
/// Receives (selected_genre, selected_artist, selected_album) — `None` = "All".
pub type FilterCallback = Box<dyn Fn(Option<String>, Option<String>, Option<String>)>;

/// Build the 3-pane browser.
///
/// `all_tracks` is the full unfiltered track list (used to recompute browser
/// item counts when filters change).
///
/// Returns `(gtk::Box containing the browser, filter_callback_handle)`.
pub fn build_browser(
    all_tracks: &[TrackObject],
    on_filter_changed: FilterCallback,
) -> gtk::Box {
    // Shared filter state
    let selected_genre: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_artist: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_album: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Stores for each pane
    let genre_store = gio::ListStore::new::<BrowserItem>();
    let artist_store = gio::ListStore::new::<BrowserItem>();
    let album_store = gio::ListStore::new::<BrowserItem>();

    // Clone tracks into a shared Vec for re-computation
    let tracks: Rc<Vec<TrackSnapshot>> = Rc::new(
        all_tracks
            .iter()
            .map(|t| TrackSnapshot {
                genre: t.genre(),
                artist: t.artist(),
                album: t.album(),
            })
            .collect(),
    );

    // Initial population
    populate_genres(&genre_store, &tracks, &None);
    populate_artists(&artist_store, &tracks, &None);
    populate_albums(&album_store, &tracks, &None, &None);

    // Wrap callback in Rc for sharing across closures
    let on_filter_changed = Rc::new(on_filter_changed);

    // ── Build the 3 panes ────────────────────────────────────────────
    let genre_pane = build_pane("Genre", &genre_store);
    let artist_pane = build_pane("Artist", &artist_store);
    let album_pane = build_pane("Album", &album_store);

    // ── Genre selection ──────────────────────────────────────────────
    {
        let sel = get_selection(&genre_pane);
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let sl = selected_album.clone();
        let artist_store = artist_store.clone();
        let album_store = album_store.clone();
        let tracks = tracks.clone();
        let cb = on_filter_changed.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            let genre = get_selected_label(sel);
            info!(?genre, "Browser: genre changed");
            *sg.borrow_mut() = genre.clone();
            // Reset downstream
            *sa.borrow_mut() = None;
            *sl.borrow_mut() = None;
            populate_artists(&artist_store, &tracks, &genre);
            populate_albums(&album_store, &tracks, &genre, &None);
            cb(genre, None, None);
        });
    }

    // ── Artist selection ─────────────────────────────────────────────
    {
        let sel = get_selection(&artist_pane);
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let sl = selected_album.clone();
        let album_store = album_store.clone();
        let tracks = tracks.clone();
        let cb = on_filter_changed.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            let artist = get_selected_label(sel);
            info!(?artist, "Browser: artist changed");
            *sa.borrow_mut() = artist.clone();
            *sl.borrow_mut() = None;
            let genre = sg.borrow().clone();
            populate_albums(&album_store, &tracks, &genre, &artist);
            cb(genre, artist, None);
        });
    }

    // ── Album selection ──────────────────────────────────────────────
    {
        let sel = get_selection(&album_pane);
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let sl = selected_album;
        let cb = on_filter_changed;

        sel.connect_selection_changed(move |sel, _, _| {
            let album = get_selected_label(sel);
            info!(?album, "Browser: album changed");
            *sl.borrow_mut() = album.clone();
            let genre = sg.borrow().clone();
            let artist = sa.borrow().clone();
            cb(genre, artist, album);
        });
    }

    // ── Layout ───────────────────────────────────────────────────────
    let browser_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .homogeneous(true)
        .spacing(1)
        .vexpand(true)
        .build();
    browser_box.append(&genre_pane);
    browser_box.append(&artist_pane);
    browser_box.append(&album_pane);

    browser_box
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Lightweight snapshot of track fields for filtering (avoids borrowing GObjects).
#[derive(Clone)]
struct TrackSnapshot {
    genre: String,
    artist: String,
    album: String,
}

fn build_pane(title: &str, store: &gio::ListStore) -> gtk::Box {
    let header = gtk::Label::builder()
        .label(title)
        .css_classes(["heading"])
        .halign(gtk::Align::Start)
        .margin_start(8)
        .margin_top(4)
        .margin_bottom(2)
        .build();

    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(true);

    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem");
        let label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .margin_start(8)
            .margin_end(8)
            .margin_top(2)
            .margin_bottom(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        list_item.set_child(Some(&label));
    });

    factory.connect_bind(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem");
        let item = list_item
            .item()
            .and_downcast::<BrowserItem>()
            .expect("BrowserItem");
        let label = list_item
            .child()
            .and_downcast::<gtk::Label>()
            .expect("Label");
        label.set_text(&item.display());
    });

    let list_view = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        .build();

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&list_view)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();

    let pane = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    pane.append(&header);
    pane.append(&scrolled);

    pane
}

/// Extract the `SingleSelection` from a browser pane box.
fn get_selection(pane: &gtk::Box) -> gtk::SingleSelection {
    // pane → label, scrolled_window → list_view → model (SingleSelection)
    let scrolled = pane
        .last_child()
        .and_downcast::<gtk::ScrolledWindow>()
        .expect("ScrolledWindow");
    let list_view = scrolled
        .child()
        .and_downcast::<gtk::ListView>()
        .expect("ListView");
    list_view
        .model()
        .and_downcast::<gtk::SingleSelection>()
        .expect("SingleSelection")
}

/// Get the selected BrowserItem label, or None if index 0 ("All") is selected.
fn get_selected_label(sel: &gtk::SingleSelection) -> Option<String> {
    let pos = sel.selected();
    if pos == 0 || pos == gtk::INVALID_LIST_POSITION {
        return None; // "All" selected
    }
    sel.selected_item()
        .and_downcast::<BrowserItem>()
        .map(|item| item.label())
}

fn populate_genres(
    store: &gio::ListStore,
    tracks: &[TrackSnapshot],
    _filter: &Option<String>,
) {
    store.remove_all();
    let mut map = std::collections::BTreeMap::<String, u32>::new();
    for t in tracks {
        *map.entry(t.genre.clone()).or_insert(0) += 1;
    }
    let total: u32 = map.values().sum();
    store.append(&BrowserItem::new("All", total));
    for (genre, count) in &map {
        store.append(&BrowserItem::new(genre, *count));
    }
}

fn populate_artists(
    store: &gio::ListStore,
    tracks: &[TrackSnapshot],
    genre_filter: &Option<String>,
) {
    store.remove_all();
    let mut map = std::collections::BTreeMap::<String, u32>::new();
    for t in tracks {
        if let Some(g) = genre_filter {
            if &t.genre != g {
                continue;
            }
        }
        *map.entry(t.artist.clone()).or_insert(0) += 1;
    }
    let total: u32 = map.values().sum();
    store.append(&BrowserItem::new("All", total));
    for (artist, count) in &map {
        store.append(&BrowserItem::new(artist, *count));
    }
}

fn populate_albums(
    store: &gio::ListStore,
    tracks: &[TrackSnapshot],
    genre_filter: &Option<String>,
    artist_filter: &Option<String>,
) {
    store.remove_all();
    let mut map = std::collections::BTreeMap::<String, u32>::new();
    for t in tracks {
        if let Some(g) = genre_filter {
            if &t.genre != g {
                continue;
            }
        }
        if let Some(a) = artist_filter {
            if &t.artist != a {
                continue;
            }
        }
        *map.entry(t.album.clone()).or_insert(0) += 1;
    }
    let total: u32 = map.values().sum();
    store.append(&BrowserItem::new("All", total));
    for (album, count) in &map {
        store.append(&BrowserItem::new(album, *count));
    }
}
