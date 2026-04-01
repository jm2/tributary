//! Browser — 3-pane genre / artist / album browser with filtering.
//!
//! Selecting an item in any pane filters the items in the sibling
//! panes and updates the tracklist via a callback.
//!
//! Cross-filtering is bidirectional: selecting an artist narrows the
//! genre and album lists; selecting an album narrows genre and artist.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::gio;
use gtk::prelude::*;

use super::objects::{BrowserItem, TrackObject};
use tracing::info;

/// Callback invoked when the browser selection changes.
/// Receives (selected_genre, selected_artist, selected_album) — `None` = "All".
pub type FilterCallback = Box<dyn Fn(Option<String>, Option<String>, Option<String>)>;

/// Opaque handle to the browser's internal track snapshot.
/// Passed back to [`rebuild_browser_data`] when the library changes.
#[derive(Clone)]
pub struct BrowserState {
    tracks: Rc<RefCell<Vec<TrackSnapshot>>>,
}

/// Build the 3-pane browser.
///
/// Returns `(gtk::Box, BrowserState)`.  The caller must keep the
/// `BrowserState` and pass it to [`rebuild_browser_data`] on FullSync.
pub fn build_browser(
    all_tracks: &[TrackObject],
    on_filter_changed: FilterCallback,
) -> (gtk::Box, BrowserState) {
    // Shared filter state
    let selected_genre: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_artist: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_album: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Re-entrancy guard: when one handler repopulates a sibling store,
    // the sibling's selection_changed fires.  The guard prevents that
    // from cascading into further repopulation.
    let updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Stores for each pane
    let genre_store = gio::ListStore::new::<BrowserItem>();
    let artist_store = gio::ListStore::new::<BrowserItem>();
    let album_store = gio::ListStore::new::<BrowserItem>();

    // Shared mutable track snapshot — updated by rebuild_browser_data.
    let tracks: Rc<RefCell<Vec<TrackSnapshot>>> = Rc::new(RefCell::new(
        all_tracks.iter().map(TrackSnapshot::from_object).collect(),
    ));

    // Initial population
    populate_genres(&genre_store, &tracks.borrow(), &None, &None);
    populate_artists(&artist_store, &tracks.borrow(), &None, &None);
    populate_albums(&album_store, &tracks.borrow(), &None, &None);

    // Wrap callback in Rc for sharing across closures
    let on_filter_changed = Rc::new(on_filter_changed);

    // ── Build the 3 panes ────────────────────────────────────────────
    let genre_pane = build_pane("Genre", &genre_store);
    let artist_pane = build_pane("Artist", &artist_store);
    let album_pane = build_pane("Album", &album_store);

    // ── Genre selection ──────────────────────────────────────────────
    // User picks a genre → repopulate artist + album (downstream).
    // Do NOT repopulate the genre store itself.
    {
        let sel = get_selection(&genre_pane);
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let sl = selected_album.clone();
        let artist_store = artist_store.clone();
        let album_store = album_store.clone();
        let tracks = tracks.clone();
        let cb = on_filter_changed.clone();
        let updating = updating.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if updating.get() {
                return;
            }
            let genre = get_selected_label(sel);
            info!(?genre, "Browser: genre changed");
            *sg.borrow_mut() = genre.clone();
            *sa.borrow_mut() = None;
            *sl.borrow_mut() = None;

            updating.set(true);
            let borrowed = tracks.borrow();
            populate_artists(&artist_store, &borrowed, &genre, &None);
            populate_albums(&album_store, &borrowed, &genre, &None);
            updating.set(false);

            cb(genre, None, None);
        });
    }

    // ── Artist selection ─────────────────────────────────────────────
    // User picks an artist → cross-filter genres, repopulate albums.
    {
        let sel = get_selection(&artist_pane);
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let sl = selected_album.clone();
        let genre_store = genre_store.clone();
        let genre_pane = genre_pane.clone();
        let album_store = album_store.clone();
        let tracks = tracks.clone();
        let cb = on_filter_changed.clone();
        let updating = updating.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if updating.get() {
                return;
            }
            let artist = get_selected_label(sel);
            info!(?artist, "Browser: artist changed");
            *sa.borrow_mut() = artist.clone();
            *sl.borrow_mut() = None;
            let genre = sg.borrow().clone();

            updating.set(true);
            let borrowed = tracks.borrow();
            populate_genres(&genre_store, &borrowed, &artist, &None);
            restore_selection(&genre_pane, &genre);
            populate_albums(&album_store, &borrowed, &genre, &artist);
            updating.set(false);

            cb(genre, artist, None);
        });
    }

    // ── Album selection ──────────────────────────────────────────────
    // User picks an album → cross-filter genres and artists.
    {
        let sel = get_selection(&album_pane);
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let sl = selected_album;
        let genre_store = genre_store.clone();
        let genre_pane = genre_pane.clone();
        let artist_store = artist_store.clone();
        let artist_pane = artist_pane.clone();
        let tracks = tracks.clone();
        let cb = on_filter_changed;
        let updating = updating.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if updating.get() {
                return;
            }
            let album = get_selected_label(sel);
            info!(?album, "Browser: album changed");
            *sl.borrow_mut() = album.clone();
            let genre = sg.borrow().clone();
            let artist = sa.borrow().clone();

            updating.set(true);
            let borrowed = tracks.borrow();
            populate_genres(&genre_store, &borrowed, &artist, &album);
            restore_selection(&genre_pane, &genre);
            populate_artists(&artist_store, &borrowed, &genre, &album);
            restore_selection(&artist_pane, &artist);
            updating.set(false);

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

    let state = BrowserState { tracks };
    (browser_box, state)
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

impl TrackSnapshot {
    fn from_object(t: &TrackObject) -> Self {
        Self {
            genre: t.genre(),
            artist: t.artist(),
            album: t.album(),
        }
    }
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
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
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
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
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

/// After repopulating a sibling pane's store, restore the previous
/// selection so the highlight doesn't jump to "All".
fn restore_selection(pane: &gtk::Box, label: &Option<String>) {
    let sel = get_selection(pane);
    if let Some(target) = label {
        let model = sel.model().unwrap();
        for i in 0..model.n_items() {
            if let Some(item) = model.item(i) {
                if let Some(bi) = item.downcast_ref::<BrowserItem>() {
                    if bi.label() == *target {
                        sel.set_selected(i);
                        return;
                    }
                }
            }
        }
    }
    // Label not found (or None) → select "All"
    sel.set_selected(0);
}

// ---------------------------------------------------------------------------
// Populate functions
// ---------------------------------------------------------------------------

fn populate_genres(
    store: &gio::ListStore,
    tracks: &[TrackSnapshot],
    artist_filter: &Option<String>,
    album_filter: &Option<String>,
) {
    store.remove_all();
    let mut map = std::collections::BTreeMap::<String, u32>::new();
    for t in tracks {
        if let Some(a) = artist_filter {
            if &t.artist != a {
                continue;
            }
        }
        if let Some(al) = album_filter {
            if &t.album != al {
                continue;
            }
        }
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
    album_filter: &Option<String>,
) {
    store.remove_all();
    let mut map = std::collections::BTreeMap::<String, u32>::new();
    for t in tracks {
        if let Some(g) = genre_filter {
            if &t.genre != g {
                continue;
            }
        }
        if let Some(al) = album_filter {
            if &t.album != al {
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

// ---------------------------------------------------------------------------
// Public API for rebuilding browser from FullSync
// ---------------------------------------------------------------------------

/// Rebuild all three browser pane stores from a new set of tracks.
///
/// Updates the shared `BrowserState` snapshot so that subsequent
/// selection changes use fresh data, then repopulates all three stores
/// with filters reset to "All".
pub fn rebuild_browser_data(browser_box: &gtk::Box, state: &BrowserState, tracks: &[TrackObject]) {
    // Update the shared snapshot that selection handlers reference.
    let snapshots: Vec<TrackSnapshot> = tracks.iter().map(TrackSnapshot::from_object).collect();
    *state.tracks.borrow_mut() = snapshots;

    let borrowed = state.tracks.borrow();

    // The browser_box has 3 children (genre_pane, artist_pane, album_pane)
    let mut child = browser_box.first_child();
    let mut panes = Vec::new();
    while let Some(widget) = child {
        if let Some(pane) = widget.downcast_ref::<gtk::Box>() {
            panes.push(pane.clone());
        }
        child = widget.next_sibling();
    }

    if panes.len() >= 3 {
        if let Some(genre_store) = get_store_from_pane(&panes[0]) {
            populate_genres(&genre_store, &borrowed, &None, &None);
        }
        if let Some(artist_store) = get_store_from_pane(&panes[1]) {
            populate_artists(&artist_store, &borrowed, &None, &None);
        }
        if let Some(album_store) = get_store_from_pane(&panes[2]) {
            populate_albums(&album_store, &borrowed, &None, &None);
        }
    }
}

/// Extract the `gio::ListStore` from a browser pane's widget tree.
fn get_store_from_pane(pane: &gtk::Box) -> Option<gio::ListStore> {
    let scrolled = pane.last_child()?.downcast::<gtk::ScrolledWindow>().ok()?;
    let list_view = scrolled.child()?.downcast::<gtk::ListView>().ok()?;
    let selection = list_view.model()?.downcast::<gtk::SingleSelection>().ok()?;
    selection
        .model()
        .and_then(|m| m.downcast::<gio::ListStore>().ok())
}
