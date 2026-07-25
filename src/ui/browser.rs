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
use gtk::glib;
use gtk::prelude::*;

use super::album_pane_art::{AlbumArtCache, AlbumArtController};
use super::objects::{AlbumArtCandidate, BrowserItem, TrackObject};
use tracing::debug;

/// Callback invoked when the browser selection changes.
/// Receives (selected_genre, selected_artist, selected_album, search_text) — `None` = "All".
pub type FilterCallback = Box<dyn Fn(Option<String>, Option<String>, Option<String>, String)>;

/// Opaque handle to the browser's internal track snapshot.
/// Passed back to [`rebuild_browser_data`] when the library changes.
#[derive(Clone)]
pub struct BrowserState {
    tracks: Rc<RefCell<Vec<TrackSnapshot>>>,
    /// Current search text for the realtime filter.
    search_text: Rc<RefCell<String>>,
    /// When true, the Artist pane groups by album artist (with fallback
    /// to track artist for tracks that don't carry an album-artist tag).
    use_album_artist: Rc<Cell<bool>>,
    /// Whether the album pane should render artwork thumbnails alongside
    /// its text labels. Toggled by the preferences dialog and read by
    /// the album pane's bind factory.
    album_pane_artwork: Rc<Cell<bool>>,
    /// Side length (in device pixels) of each album-pane thumbnail.
    /// Persisted across restarts and forwarded to the cache probe.
    album_pane_artwork_size: Rc<Cell<i32>>,
    /// Coordinator for the album pane artwork path. Owned by the state
    /// so the bind factory's closures stay valid for the life of the
    /// browser even if the controller's internal references move.
    album_art_controller: Rc<AlbumArtController>,
    /// In-memory texture cache keyed by `(track_id, pixel_size)`. Shared
    /// with the album pane bind factory and exposed so callers can clear
    /// it on layout/preference changes.
    album_art_cache: Rc<AlbumArtCache>,
}

/// Build the 3-pane browser.
///
/// Returns `(gtk::Box, BrowserState)`.  The caller must keep the
/// `BrowserState` and pass it to [`rebuild_browser_data`] on FullSync.
pub fn build_browser(
    all_tracks: &[TrackObject],
    use_album_artist: bool,
    initial_album_pane_artwork: bool,
    initial_album_pane_artwork_size: i32,
    on_filter_changed: FilterCallback,
) -> (gtk::Box, BrowserState) {
    let use_album_artist: Rc<Cell<bool>> = Rc::new(Cell::new(use_album_artist));
    let album_pane_artwork: Rc<Cell<bool>> = Rc::new(Cell::new(initial_album_pane_artwork));
    let album_pane_artwork_size: Rc<Cell<i32>> =
        Rc::new(Cell::new(initial_album_pane_artwork_size));
    // Shared filter state
    let selected_genre: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_artist: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_album: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let search_text: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    // Re-entrancy guard: when one handler repopulates a sibling store,
    // the sibling's selection_changed fires.  The guard prevents that
    // from cascading into further repopulation.
    let updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Album-art coordinator: virtualized, accessible, bounded cache for
    // the album pane's per-row thumbnails. The source registry is wired
    // in later by the window so the controller can resolve credential-
    // isolated remote artwork without exposing endpoints here.
    let album_art_controller = Rc::new(AlbumArtController::new("audio-x-generic-symbolic"));

    // Stores for each pane
    let genre_store = gio::ListStore::new::<BrowserItem>();
    let artist_store = gio::ListStore::new::<BrowserItem>();
    let album_store = gio::ListStore::new::<BrowserItem>();

    // Shared mutable track snapshot — updated by rebuild_browser_data.
    let tracks: Rc<RefCell<Vec<TrackSnapshot>>> = Rc::new(RefCell::new(
        all_tracks.iter().map(TrackSnapshot::from_object).collect(),
    ));

    // Initial population
    let use_aa = use_album_artist.get();
    populate_genres(&genre_store, &tracks.borrow(), &None, &None, use_aa);
    populate_artists(&artist_store, &tracks.borrow(), &None, &None, use_aa);
    populate_albums(&album_store, &tracks.borrow(), &None, &None, use_aa);

    // Wrap callback in Rc for sharing across closures
    let on_filter_changed = Rc::new(on_filter_changed);

    // ── Search entry ─────────────────────────────────────────────────
    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text(rust_i18n::t!("browser.search_placeholder").as_ref())
        .hexpand(true)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    // ── Build the 3 panes ────────────────────────────────────────────
    let genre_pane = build_pane("Genre", &genre_store);
    let artist_pane = build_pane("Artist", &artist_store);
    let album_pane = build_album_pane(
        &album_store,
        album_art_controller.clone(),
        album_pane_artwork.clone(),
        album_pane_artwork_size.clone(),
    );

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
        let search_text = search_text.clone();
        let use_aa = use_album_artist.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if updating.get() {
                return;
            }
            let genre = get_selected_label(sel);
            debug!("Browser: genre changed");
            *sg.borrow_mut() = genre.clone();
            *sa.borrow_mut() = None;
            *sl.borrow_mut() = None;

            updating.set(true);
            let borrowed = tracks.borrow();
            let flag = use_aa.get();
            populate_artists(&artist_store, &borrowed, &genre, &None, flag);
            populate_albums(&album_store, &borrowed, &genre, &None, flag);
            updating.set(false);

            cb(genre, None, None, search_text.borrow().clone());
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
        let artist_store = artist_store.clone();
        let artist_pane = artist_pane.clone();
        let album_store = album_store.clone();
        let tracks = tracks.clone();
        let cb = on_filter_changed.clone();
        let updating = updating.clone();
        let search_text = search_text.clone();
        let use_aa = use_album_artist.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if updating.get() {
                return;
            }
            let artist = get_selected_label(sel);
            debug!("Browser: artist changed");
            *sa.borrow_mut() = artist.clone();
            *sl.borrow_mut() = None;
            let genre = sg.borrow().clone();

            updating.set(true);
            let borrowed = tracks.borrow();
            let flag = use_aa.get();
            populate_genres(&genre_store, &borrowed, &artist, &None, flag);
            restore_selection(&genre_pane, &genre);
            // When the user clears the artist filter ("All"), a prior album
            // selection may have narrowed the Artist pane down to a single
            // artist. Restore the full genre-filtered artist list so the
            // user can pick a different artist again (issue #30). Only do
            // this for the artist→All case — a normal artist pick must keep
            // cross-filtering genres/albums without wiping the artist list.
            if artist.is_none() {
                populate_artists(&artist_store, &borrowed, &genre, &None, flag);
                restore_selection(&artist_pane, &artist);
            }
            populate_albums(&album_store, &borrowed, &genre, &artist, flag);
            updating.set(false);

            cb(genre, artist, None, search_text.borrow().clone());
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
        let cb = on_filter_changed.clone();
        let updating = updating.clone();
        let search_text = search_text.clone();
        let use_aa = use_album_artist.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if updating.get() {
                return;
            }
            let album = get_selected_label(sel);
            debug!("Browser: album changed");
            *sl.borrow_mut() = album.clone();
            let genre = sg.borrow().clone();
            let artist = sa.borrow().clone();

            updating.set(true);
            let borrowed = tracks.borrow();
            let flag = use_aa.get();
            populate_genres(&genre_store, &borrowed, &artist, &album, flag);
            restore_selection(&genre_pane, &genre);
            populate_artists(&artist_store, &borrowed, &genre, &album, flag);
            restore_selection(&artist_pane, &artist);
            updating.set(false);

            cb(genre, artist, album, search_text.borrow().clone());
        });
    }

    // ── Search entry handler (debounced 100ms) ───────────────────────
    {
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let search_text = search_text.clone();
        let cb = on_filter_changed;
        let debounce_gen: Rc<Cell<u32>> = Rc::new(Cell::new(0));

        search_entry.connect_search_changed(move |entry| {
            let text = entry.text().to_string();
            debug!("Browser: search changed");
            *search_text.borrow_mut() = text.clone();

            // Debounce: invalidate any pending timer and schedule a new one.
            // 100ms is short enough to feel responsive but prevents the
            // expensive filter callback from firing on every keystroke
            // during fast typing.
            let gen = debounce_gen.get().wrapping_add(1);
            debounce_gen.set(gen);

            let sg = sg.clone();
            let sa = sa.clone();
            let cb = cb.clone();
            let gen_rc = debounce_gen.clone();

            glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                if gen_rc.get() != gen {
                    return; // Superseded by a newer keystroke.
                }
                let genre = sg.borrow().clone();
                let artist = sa.borrow().clone();
                cb(genre, artist, None, text);
            });
        });
    }

    // ── Layout ───────────────────────────────────────────────────────
    let panes_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .homogeneous(true)
        .spacing(1)
        .vexpand(true)
        .build();
    panes_box.append(&genre_pane);
    panes_box.append(&artist_pane);
    panes_box.append(&album_pane);

    let browser_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .vexpand(true)
        .build();
    browser_box.append(&search_entry);
    browser_box.append(&panes_box);

    let state = BrowserState {
        tracks,
        search_text,
        use_album_artist,
        album_pane_artwork,
        album_pane_artwork_size,
        album_art_cache: Rc::new(album_art_controller.cache().clone()),
        album_art_controller,
    };
    (browser_box, state)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Switch the album pane between the artwork thumbnail bind factory and
/// the plain label bind factory used by genre and artist.
///
/// Building a fresh pane is cheaper than mutating the factory in place:
/// GTK's `SignalListItemFactory` does not expose a clean replace API,
/// and rebuilding the list view also forces GTK to drop every cached
/// `gdk::Texture` reference on the obsolete row widgets.
pub fn set_album_pane_artwork(browser_box: &gtk::Box, state: &BrowserState, enabled: bool) {
    if state.album_pane_artwork.get() == enabled {
        return;
    }
    state.album_pane_artwork.set(enabled);
    rebuild_album_pane(browser_box, state);
}

/// Update the album-pane thumbnail size. The bind factory reads the
/// size knob on every bind, so the change applies to the next set of
/// rows that scroll into view. We rebuild the album pane here so the
/// cached textures (keyed by `(album_key, pixel_size)`) are dropped
/// alongside the old bind factory — a stale entry from the previous
/// size would never be queried again, and leaving it in the cache
/// would still consume the bounded-memory budget.
pub fn set_album_pane_artwork_size(browser_box: &gtk::Box, state: &BrowserState, pixel_size: i32) {
    if state.album_pane_artwork_size.get() == pixel_size {
        return;
    }
    state.album_pane_artwork_size.set(pixel_size);
    rebuild_album_pane(browser_box, state);
}

/// Replace the album pane in place with a freshly-built one wired to
/// the current `(album_pane_artwork, album_pane_artwork_size)` knobs.
/// The existing `gio::ListStore` is preserved across the swap so the
/// album rows survive; the album store is then repopulated from the
/// shared track snapshot so the artwork candidates match the latest
/// library state. The cache is cleared because every entry was decoded
/// at the previous size and would never match a new `(album_key,
/// pixel_size)` lookup under the recompiled bind factory.
fn rebuild_album_pane(browser_box: &gtk::Box, state: &BrowserState) {
    let panes_box = browser_box
        .last_child()
        .and_then(|w| w.downcast::<gtk::Box>().ok());
    let Some(panes_box) = panes_box else {
        return;
    };

    let mut child = panes_box.first_child();
    let mut panes = Vec::new();
    while let Some(widget) = child {
        if let Some(pane) = widget.downcast_ref::<gtk::Box>() {
            panes.push(pane.clone());
        }
        child = widget.next_sibling();
    }

    if panes.len() < 3 {
        return;
    }

    // Album pane is the 3rd child (index 2). Replace it.
    let old_pane = panes[2].clone();
    let album_store =
        album_store_from_pane(&old_pane).unwrap_or_else(gio::ListStore::new::<BrowserItem>);

    // Clear the cache so the new bind factory doesn't serve stale
    // textures from before the layout change — they're decoded at the
    // old size, and a stale hit would bypass the new bind path entirely.
    state.album_art_cache.inner.borrow_mut().entries.clear();
    state.album_art_cache.inner.borrow_mut().order.clear();

    let new_pane = build_album_pane(
        &album_store,
        state.album_art_controller.clone(),
        state.album_pane_artwork.clone(),
        state.album_pane_artwork_size.clone(),
    );
    panes_box.remove(&old_pane);
    panes_box.append(&new_pane);

    // The new album pane keeps the same `gio::ListStore` as the old one,
    // so the album rows survive the swap. Repopulate the store from the
    // current snapshot so the BrowserItem's artwork candidates are
    // refreshed against the latest library state — but do NOT call
    // `rebuild_browser_data` here: that helper would replace
    // `state.tracks` with whatever slice it is handed, and the call site
    // would have to pass the live master track list to avoid blanking
    // the genre and artist panes. Toggling the artwork checkbox or
    // changing the size is a layout event, not a library sync, so the
    // snapshot must stay put.
    let borrowed = state.tracks.borrow();
    let use_aa = state.use_album_artist.get();
    populate_albums(&album_store, &borrowed, &None, &None, use_aa);
}

/// Pull the underlying `gio::ListStore<BrowserItem>` out of an album
/// pane Box so the swapped-in pane can keep the same data.
fn album_store_from_pane(pane: &gtk::Box) -> Option<gio::ListStore> {
    let scrolled = pane.last_child()?.downcast::<gtk::ScrolledWindow>().ok()?;
    let list_view = scrolled.child()?.downcast::<gtk::ListView>().ok()?;
    let selection = list_view.model()?.downcast::<gtk::SingleSelection>().ok()?;
    selection
        .model()
        .and_then(|m| m.downcast::<gio::ListStore>().ok())
}

/// Attach the live source registry to the album-art coordinator. Must
/// be called once after `build_browser` and once per registry
/// replacement (the controller will see the new handle on the next
/// bind). The pointer is intentional: only the resolver path needs
/// it, and lazy attachment keeps the coordinator construction cheap.
pub fn attach_source_registry(
    state: &BrowserState,
    source_registry: crate::source_registry::SourceRegistry,
) {
    state
        .album_art_controller
        .attach_source_registry(source_registry);
}

/// Lightweight snapshot of track fields for filtering (avoids borrowing GObjects).
#[derive(Clone)]
struct TrackSnapshot {
    #[allow(dead_code)] // Used by window.rs search filter via TrackObject, not directly here
    title: String,
    genre: String,
    artist: String,
    /// Album artist (used for browser grouping when the preference is on).
    album_artist: String,
    album: String,
    /// Stable track identifier. The album-pane artwork resolver reads
    /// this to call `SourceRegistry::resolve_artwork`.
    track_id: String,
    /// Playable locator or `file://` URI. Local album rows go through
    /// the embedded-art extractor when the resolver finds no remote art.
    uri: String,
    /// Track-provided cover URL string. Used as a third-tier fallback
    /// when no remote artwork can be resolved.
    cover_art_url: String,
    /// Source identity for the credential-isolated remote resolver.
    /// `None` for local / non-networked tracks.
    source_id: Option<crate::architecture::SourceId>,
    /// Source session epoch paired with `source_id`; the resolver
    /// rejects resolutions that cross an active replacement.
    source_session_epoch: Option<u64>,
}

impl TrackSnapshot {
    fn from_object(t: &TrackObject) -> Self {
        Self {
            title: t.title(),
            genre: t.genre(),
            artist: t.artist(),
            album_artist: t.album_artist(),
            album: t.album(),
            track_id: t.track_id(),
            uri: t.uri(),
            cover_art_url: t.cover_art_url(),
            source_id: t.source_id(),
            source_session_epoch: t.source_session_epoch(),
        }
    }

    /// Return the artist name to use for browser grouping.
    ///
    /// When `use_album_artist` is true and the track has a non-empty
    /// album artist tag, return it; otherwise fall back to the track artist.
    fn browser_artist(&self, use_album_artist: bool) -> &str {
        if use_album_artist && !self.album_artist.is_empty() {
            &self.album_artist
        } else {
            &self.artist
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

/// Build the album pane with an optional artwork column.
///
/// When `album_pane_artwork_visible` is on, the bind factory uses the
/// `AlbumArtController` to fetch a thumbnail for each row. When off,
/// the factory falls back to the plain label used by genre and artist.
fn build_album_pane(
    store: &gio::ListStore,
    album_art_controller: Rc<AlbumArtController>,
    album_pane_artwork_visible: Rc<Cell<bool>>,
    album_pane_artwork_size: Rc<Cell<i32>>,
) -> gtk::Box {
    let header = gtk::Label::builder()
        .label(rust_i18n::t!("browser.album").as_ref())
        .css_classes(["heading"])
        .halign(gtk::Align::Start)
        .margin_start(8)
        .margin_top(4)
        .margin_bottom(2)
        .build();

    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(true);

    let factory = gtk::SignalListItemFactory::new();

    if album_pane_artwork_visible.get() {
        let (setup, bind, unbind, _binder) =
            album_art_controller.build_binder_with_size(album_pane_artwork_size.clone());
        factory.connect_setup(setup);
        factory.connect_bind(bind);
        factory.connect_unbind(unbind);
    } else {
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
    }

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
    use_album_artist: bool,
) {
    store.remove_all();
    let mut map = std::collections::BTreeMap::<String, u32>::new();
    for t in tracks {
        if let Some(a) = artist_filter {
            if t.browser_artist(use_album_artist) != a {
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
    use_album_artist: bool,
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
        *map.entry(t.browser_artist(use_album_artist).to_string())
            .or_insert(0) += 1;
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
    use_album_artist: bool,
) {
    store.remove_all();
    // Track the first representative per album so the browser pane can
    // resolve artwork lazily — only the chosen representative's source
    // identity and URI need to be retained in the BrowserItem.
    let mut candidates: std::collections::BTreeMap<String, AlbumArtCandidate> =
        std::collections::BTreeMap::new();
    let mut map = std::collections::BTreeMap::<String, u32>::new();
    for t in tracks {
        if let Some(g) = genre_filter {
            if &t.genre != g {
                continue;
            }
        }
        if let Some(a) = artist_filter {
            if t.browser_artist(use_album_artist) != a {
                continue;
            }
        }
        *map.entry(t.album.clone()).or_insert(0) += 1;
        candidates
            .entry(t.album.clone())
            .or_insert_with(|| AlbumArtCandidate {
                track_id: t.track_id.clone(),
                uri: t.uri.clone(),
                cover_art_url: t.cover_art_url.clone(),
                source_id: t.source_id,
                source_session_epoch: t.source_session_epoch,
            });
    }
    let total: u32 = map.values().sum();
    store.append(&BrowserItem::new("All", total));
    for (album, count) in &map {
        if let Some(candidate) = candidates.get(album) {
            store.append(&BrowserItem::new_with_artwork(
                album,
                *count,
                candidate.clone(),
            ));
        } else {
            store.append(&BrowserItem::new(album, *count));
        }
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

    // Clear search text on data rebuild (new source / full sync).
    *state.search_text.borrow_mut() = String::new();

    // Clear the search entry widget if present (first child of browser_box).
    if let Some(first) = browser_box.first_child() {
        if let Some(entry) = first.downcast_ref::<gtk::SearchEntry>() {
            entry.set_text("");
        }
    }

    let borrowed = state.tracks.borrow();
    let use_aa = state.use_album_artist.get();

    // The browser_box layout is: SearchEntry, panes_box (horizontal Box).
    // The panes_box contains 3 children (genre_pane, artist_pane, album_pane).
    let panes_box = browser_box
        .last_child()
        .and_then(|w| w.downcast::<gtk::Box>().ok());

    if let Some(ref panes_box) = panes_box {
        let mut child = panes_box.first_child();
        let mut panes = Vec::new();
        while let Some(widget) = child {
            if let Some(pane) = widget.downcast_ref::<gtk::Box>() {
                panes.push(pane.clone());
            }
            child = widget.next_sibling();
        }

        if panes.len() >= 3 {
            if let Some(genre_store) = get_store_from_pane(&panes[0]) {
                populate_genres(&genre_store, &borrowed, &None, &None, use_aa);
            }
            if let Some(artist_store) = get_store_from_pane(&panes[1]) {
                populate_artists(&artist_store, &borrowed, &None, &None, use_aa);
            }
            if let Some(album_store) = get_store_from_pane(&panes[2]) {
                populate_albums(&album_store, &borrowed, &None, &None, use_aa);
            }
        }
    }
}

/// Toggle album-artist grouping and rebuild the browser panes.
///
/// Updates the shared flag, then refreshes all three panes from the
/// current snapshot.  Selections reset to "All" because the artist
/// pane's contents are about to change.
pub fn set_album_artist_grouping(browser_box: &gtk::Box, state: &BrowserState, enabled: bool) {
    state.use_album_artist.set(enabled);

    let borrowed = state.tracks.borrow();
    let panes_box = browser_box
        .last_child()
        .and_then(|w| w.downcast::<gtk::Box>().ok());

    if let Some(ref panes_box) = panes_box {
        let mut child = panes_box.first_child();
        let mut panes = Vec::new();
        while let Some(widget) = child {
            if let Some(pane) = widget.downcast_ref::<gtk::Box>() {
                panes.push(pane.clone());
            }
            child = widget.next_sibling();
        }

        if panes.len() >= 3 {
            if let Some(genre_store) = get_store_from_pane(&panes[0]) {
                populate_genres(&genre_store, &borrowed, &None, &None, enabled);
            }
            if let Some(artist_store) = get_store_from_pane(&panes[1]) {
                populate_artists(&artist_store, &borrowed, &None, &None, enabled);
            }
            if let Some(album_store) = get_store_from_pane(&panes[2]) {
                populate_albums(&album_store, &borrowed, &None, &None, enabled);
            }
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
