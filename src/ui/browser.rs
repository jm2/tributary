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
use gtk::subclass::prelude::ObjectSubclassIsExt;

use super::objects::{BrowserItem, TrackObject};
use crate::ui::folder_browser::{FolderBrowser, RootBrowseError};
use tracing::debug;

/// Callback invoked when the browser selection changes.
/// Receives (selected_genre, selected_artist, selected_album, folder_prefix, search_text) —
/// `None` = "All" / no folder filter.
pub type FilterCallback =
    Box<dyn Fn(Option<String>, Option<String>, Option<String>, Option<String>, String)>;

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
    /// Lazy folder-browsing model for the local library, attached by the
    /// window when local library tracks are displayed. `None` while a
    /// pathless source is active or no library roots are configured — the
    /// folder pane then shows the explicit omission notice instead.
    folder_model: Rc<RefCell<Option<FolderBrowser>>>,
    /// Where the folder pane currently points.
    folder_location: Rc<RefCell<FolderLocation>>,
    /// The file-path prefix the folder pane currently filters by
    /// (`None` at the roots level or while no model is attached).
    folder_prefix: Rc<RefCell<Option<String>>>,
    /// The folder pane's store, so attach/clear/navigation can repopulate.
    folder_store: gio::ListStore,
    /// The folder pane's selection model, so navigation can reset selection
    /// without re-triggering the handler.
    folder_selection: gtk::SingleSelection,
    /// Re-entrancy guard shared with the pane handlers (attach/clear also
    /// repopulate the folder store programmatically).
    updating: Rc<Cell<bool>>,
}

/// Where the folder pane currently points.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
enum FolderLocation {
    /// The top level: rows are the configured library roots.
    #[default]
    Roots,
    /// Inside one root, at a root-relative directory (empty string = the
    /// root itself).
    Inside { root_id: String, dir: String },
}

/// Build the 3-pane browser.
///
/// Returns `(gtk::Box, BrowserState)`.  The caller must keep the
/// `BrowserState` and pass it to [`rebuild_browser_data`] on FullSync.
pub fn build_browser(
    all_tracks: &[TrackObject],
    use_album_artist: bool,
    on_filter_changed: FilterCallback,
) -> (gtk::Box, BrowserState) {
    let use_album_artist: Rc<Cell<bool>> = Rc::new(Cell::new(use_album_artist));
    // Shared filter state
    let selected_genre: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_artist: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_album: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let search_text: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    // Re-entrancy guard: when one handler repopulates a sibling store,
    // the sibling's selection_changed fires.  The guard prevents that
    // from cascading into further repopulation.
    let updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Stores for each pane
    let genre_store = gio::ListStore::new::<BrowserItem>();
    let artist_store = gio::ListStore::new::<BrowserItem>();
    let album_store = gio::ListStore::new::<BrowserItem>();
    let folder_store = gio::ListStore::new::<BrowserItem>();

    // Folder-browsing state. The model is attached later by the window
    // (once the local library is known); until then the pane shows the
    // explicit omission notice.
    let folder_model: Rc<RefCell<Option<FolderBrowser>>> = Rc::new(RefCell::new(None));
    let folder_location: Rc<RefCell<FolderLocation>> = Rc::new(RefCell::new(FolderLocation::Roots));
    let folder_prefix: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Shared mutable track snapshot — updated by rebuild_browser_data.
    let tracks: Rc<RefCell<Vec<TrackSnapshot>>> = Rc::new(RefCell::new(
        all_tracks.iter().map(TrackSnapshot::from_object).collect(),
    ));

    // Initial population
    let use_aa = use_album_artist.get();
    populate_genres(&genre_store, &tracks.borrow(), &None, &None, use_aa);
    populate_artists(&artist_store, &tracks.borrow(), &None, &None, use_aa);
    populate_albums(&album_store, &tracks.borrow(), &None, &None, use_aa);
    // Folder pane starts detached: the notice row explains the policy
    // until the window attaches the local-library model.
    populate_folder_pane(&folder_store, None, &FolderLocation::Roots);

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
    let album_pane = build_pane("Album", &album_store);
    let folder_pane = build_pane("Folder", &folder_store);

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
        let fp = folder_prefix.clone();

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

            cb(
                genre,
                None,
                None,
                fp.borrow().clone(),
                search_text.borrow().clone(),
            );
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
        let fp = folder_prefix.clone();

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

            cb(
                genre,
                artist,
                None,
                fp.borrow().clone(),
                search_text.borrow().clone(),
            );
        });
    }

    // ── Album selection ──────────────────────────────────────────────
    // User picks an album → cross-filter genres and artists.
    {
        let sel = get_selection(&album_pane);
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let sl = selected_album.clone();
        let genre_store = genre_store.clone();
        let genre_pane = genre_pane.clone();
        let artist_store = artist_store.clone();
        let artist_pane = artist_pane.clone();
        let tracks = tracks.clone();
        let cb = on_filter_changed.clone();
        let updating = updating.clone();
        let search_text = search_text.clone();
        let use_aa = use_album_artist.clone();
        let fp = folder_prefix.clone();

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

            cb(
                genre,
                artist,
                album,
                fp.borrow().clone(),
                search_text.borrow().clone(),
            );
        });
    }

    // ── Folder selection / navigation ────────────────────────────────
    // Selecting a row navigates (roots → folders) and applies a path
    // filter to the tracklist. The pane's tree is derived lazily from the
    // attached model: nothing is walked until a level is displayed.
    {
        let sel = get_selection(&folder_pane);
        let selection = sel.clone();
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let sl = selected_album.clone();
        let model = folder_model.clone();
        let location = folder_location.clone();
        let prefix = folder_prefix.clone();
        let store = folder_store.clone();
        let cb = on_filter_changed.clone();
        let updating = updating.clone();
        let search_text = search_text.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if updating.get() {
                return;
            }
            let Some(item) = sel.selected_item().and_downcast::<BrowserItem>() else {
                return;
            };
            let label = item.label();

            // Interpret the row by the current location.
            let current = location.borrow().clone();
            let next = match &current {
                FolderLocation::Roots => {
                    let model_ref = model.borrow();
                    let Some(browser) = model_ref.as_ref() else {
                        return;
                    };
                    let labels = browser.disambiguated_root_labels();
                    let Some(pos) = labels.iter().position(|l| l == &label) else {
                        return;
                    };
                    let root = &browser.roots()[pos];
                    if !root.browsable() {
                        // Listed for visibility, but navigation is refused.
                        return;
                    }
                    Some(FolderLocation::Inside {
                        root_id: root.root_id.clone(),
                        dir: String::new(),
                    })
                }
                FolderLocation::Inside { root_id, dir } => {
                    if label == FOLDER_UP_LABEL {
                        if dir.is_empty() {
                            Some(FolderLocation::Roots)
                        } else {
                            let parent = match dir.rsplit_once('/') {
                                Some((parent, _)) => parent.to_string(),
                                None => String::new(),
                            };
                            Some(FolderLocation::Inside {
                                root_id: root_id.clone(),
                                dir: parent,
                            })
                        }
                    } else {
                        let child_dir = if dir.is_empty() {
                            label.clone()
                        } else {
                            format!("{dir}/{label}")
                        };
                        Some(FolderLocation::Inside {
                            root_id: root_id.clone(),
                            dir: child_dir,
                        })
                    }
                }
            };
            let Some(next) = next else { return };

            *location.borrow_mut() = next.clone();
            updating.set(true);
            let new_prefix =
                populate_folder_pane(&store, model.borrow().as_ref(), &location.borrow());
            selection.set_selected(0);
            updating.set(false);
            *prefix.borrow_mut() = new_prefix.clone();

            let genre = sg.borrow().clone();
            let artist = sa.borrow().clone();
            let album = sl.borrow().clone();
            cb(
                genre,
                artist,
                album,
                new_prefix,
                search_text.borrow().clone(),
            );
        });
    }

    // ── Search entry handler (debounced 100ms) ───────────────────────
    {
        let sg = selected_genre.clone();
        let sa = selected_artist.clone();
        let search_text = search_text.clone();
        let fp = folder_prefix.clone();
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
            let fp = fp.clone();
            let cb = cb.clone();
            let gen_rc = debounce_gen.clone();

            glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                if gen_rc.get() != gen {
                    return; // Superseded by a newer keystroke.
                }
                let genre = sg.borrow().clone();
                let artist = sa.borrow().clone();
                cb(genre, artist, None, fp.borrow().clone(), text);
            });
        });
    }

    // ── Layout ───────────────────────────────────────────────────────
    let panes_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(1)
        .vexpand(true)
        .build();
    // A real 1px `.browser-separator` gutter between the panes (HIG: a
    // gutter, not a hard divider). Homogeneous is off because a
    // homogeneous Box counts the separators in its equal split and would
    // shrink every pane; each pane expands instead, and a Box distributes
    // the extra width equally across expanding children, which preserves
    // the equal-pane layout.
    for (index, pane) in [&genre_pane, &artist_pane, &album_pane, &folder_pane]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            let separator = gtk::Separator::builder()
                .orientation(gtk::Orientation::Vertical)
                .css_classes(["browser-separator"])
                .build();
            panes_box.append(&separator);
        }
        pane.set_hexpand(true);
        panes_box.append(pane);
    }

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
        folder_model,
        folder_location,
        folder_prefix,
        folder_store,
        folder_selection: get_selection(&folder_pane),
        updating,
    };
    (browser_box, state)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
}

impl TrackSnapshot {
    fn from_object(t: &TrackObject) -> Self {
        Self {
            title: t.title(),
            genre: t.genre(),
            artist: t.artist(),
            album_artist: t.album_artist(),
            album: t.album(),
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

/// One browser-pane row: the primary label plus the dimmed trailing
/// count, owned by a dedicated [`gtk::Box`] subclass so bind / unbind
/// can address the labels by name — without GObject data pointers
/// (`set_data` / `data`, which require `unsafe`) and without
/// insertion-order traversal (`first_child` / `last_child`).
mod imp {
    use super::*;
    use gtk::subclass::prelude::*;

    #[derive(Debug)]
    pub struct BrowserRow {
        pub label: gtk::Label,
        pub count: gtk::Label,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BrowserRow {
        const NAME: &'static str = "TributaryBrowserRow";
        type Type = super::BrowserRow;
        type ParentType = gtk::Box;

        fn new() -> Self {
            let label = gtk::Label::builder()
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Center)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .hexpand(true)
                .single_line_mode(true)
                .build();
            // Presentational: the row's combined accessible label is
            // set in bind, so the screen reader announces the row as
            // one utterance rather than two separate labels.
            label.set_accessible_role(gtk::AccessibleRole::Presentation);
            let count = gtk::Label::builder()
                .halign(gtk::Align::End)
                .valign(gtk::Align::Center)
                .css_classes(["dim-label", "caption", "numeric", "browser-count"])
                .build();
            count.set_accessible_role(gtk::AccessibleRole::Presentation);
            Self { label, count }
        }
    }

    impl ObjectImpl for BrowserRow {
        fn constructed(&self) {
            self.parent_constructed();
            let row = self.obj();
            row.set_orientation(gtk::Orientation::Horizontal);
            row.set_spacing(6);
            row.set_margin_start(8);
            row.set_margin_end(8);
            row.set_margin_top(2);
            row.set_margin_bottom(2);
            row.append(&self.label);
            row.append(&self.count);
        }
    }

    impl BoxImpl for BrowserRow {}
    impl WidgetImpl for BrowserRow {}
}

glib::wrapper! {
    pub struct BrowserRow(ObjectSubclass<imp::BrowserRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget,
                    gtk::Orientable;
}

impl BrowserRow {
    fn new() -> Self {
        glib::Object::builder().build()
    }

    fn label(&self) -> &gtk::Label {
        &self.imp().label
    }

    fn count(&self) -> &gtk::Label {
        &self.imp().count
    }
}

/// Bind one browser row to its item: populate the visible texts and
/// publish the combined accessible label on the [`gtk::ListItem`].
///
/// The label and parenthesized count are combined into a single
/// utterance ("Artist Name, (123)") and exposed on the GtkListItem
/// itself — the list-row boundary assistive technology actually
/// navigates — via the dedicated GtkListItem:accessible-label property
/// (GTK 4.12), which GTK uses as the row's accessible name. The child
/// labels are marked Presentation in setup so they are not announced
/// individually.
///
/// Split out from the factory closure so tests can drive the exact
/// production bind on a standalone `GtkListItem` (`ListItem:item` is
/// read-only and set by the ListView, so a factory-driven bind cannot
/// be exercised outside a realized list).
fn bind_browser_row(list_item: &gtk::ListItem, item: &BrowserItem) {
    let row = list_item
        .child()
        .and_downcast::<BrowserRow>()
        .expect("BrowserRow attached in setup");
    // Folder navigation and status rows carry no count; render only the
    // label rather than a meaningless "(0)" — numeric secondary text is
    // only announced when it carries information (HIG).
    let count_text = if item.count() > 0 {
        format!("({})", item.count())
    } else {
        String::new()
    };
    row.label().set_text(&item.label());
    row.count().set_text(&count_text);
    let accessible = if count_text.is_empty() {
        item.label()
    } else {
        format!("{}, {}", item.label(), count_text)
    };
    list_item.set_accessible_label(&accessible);
}

/// Unbind one browser row: reset the row's accessible name so a
/// recycled list item never announces a stale label while waiting for
/// its next bind, and clear the visible texts.
fn unbind_browser_row(list_item: &gtk::ListItem) {
    list_item.set_accessible_label("");
    let Some(row) = list_item.child().and_downcast::<BrowserRow>() else {
        return;
    };
    row.label().set_text("");
    row.count().set_text("");
}

/// Row factory shared by every browser pane: each [`gtk::ListItem`]
/// hosts a [`BrowserRow`], and bind / unbind keep the visible texts and
/// the combined accessible label in sync with the item. Exposed as a
/// function so tests can drive the setup / bind / unbind contract
/// directly on a standalone [`gtk::ListItem`].
fn browser_row_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        // The BrowserRow subclass owns the label / count widgets as
        // named children, so bind / unbind can reach them by downcasting
        // the ListItem's child — no GObject data storage needed.
        list_item.set_child(Some(&BrowserRow::new()));
    });

    factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let item = list_item
            .item()
            .and_downcast::<BrowserItem>()
            .expect("BrowserItem");
        bind_browser_row(list_item, &item);
    });

    factory.connect_unbind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        unbind_browser_row(list_item);
    });

    factory
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

    let factory = browser_row_factory();

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

/// Label of the pane row that ascends one folder level.
const FOLDER_UP_LABEL: &str = "…";

/// Attach the lazy folder-browsing model for the local library and reset
/// the folder pane to its roots level. Called by the window when local
/// library tracks are displayed.
pub fn attach_folder_model(state: &BrowserState, model: FolderBrowser) {
    *state.folder_model.borrow_mut() = Some(model);
    reset_folder_navigation(state);
}

/// Detach the folder model (a pathless source became active): the pane
/// shows the explicit omission notice instead of stale local folders.
pub fn clear_folder_model(state: &BrowserState) {
    *state.folder_model.borrow_mut() = None;
    reset_folder_navigation(state);
}

fn reset_folder_navigation(state: &BrowserState) {
    *state.folder_location.borrow_mut() = FolderLocation::Roots;
    *state.folder_prefix.borrow_mut() = None;
    state.updating.set(true);
    populate_folder_pane(
        &state.folder_store,
        state.folder_model.borrow().as_ref(),
        &FolderLocation::Roots,
    );
    state.folder_selection.set_selected(0);
    state.updating.set(false);
}

/// Repopulate the folder pane for `location`, returning the track-filter
/// prefix it selects (`None` = no folder filter at the roots level or while
/// detached). This is the lazy navigation step: exactly one level is
/// derived from the model per call.
fn populate_folder_pane(
    store: &gio::ListStore,
    model: Option<&FolderBrowser>,
    location: &FolderLocation,
) -> Option<String> {
    let mut rows: Vec<(String, u32)> = Vec::new();
    let mut prefix: Option<String> = None;
    match (model, location) {
        (None, _) => {
            rows.push((
                "Folder browsing follows the local library sources".to_string(),
                0,
            ));
        }
        (Some(browser), FolderLocation::Roots) => {
            let labels = browser.disambiguated_root_labels();
            for (root, label) in browser.roots().iter().zip(labels) {
                let label = match root.availability_suffix() {
                    Some(suffix) => format!("{label}{suffix}"),
                    None => label,
                };
                rows.push((label, 0));
            }
            if rows.is_empty() {
                rows.push(("No library folders configured".to_string(), 0));
            }
        }
        (Some(browser), FolderLocation::Inside { root_id, dir }) => {
            rows.push((FOLDER_UP_LABEL.to_string(), 0));
            match browser.children(root_id, dir) {
                Ok(children) => {
                    for child in children {
                        rows.push((child.name, child.track_count as u32));
                    }
                }
                Err(RootBrowseError::Unavailable { reason }) => {
                    rows.push((format!("(unavailable: {reason})"), 0));
                }
                Err(RootBrowseError::Renamed { previous_path }) => {
                    rows.push((format!("(renamed from {previous_path})"), 0));
                }
                Err(RootBrowseError::UnknownRoot) => {}
            }
            if let Some(root) = browser.roots().iter().find(|r| &r.root_id == root_id) {
                prefix = Some(join_root_prefix(&root.root_path.to_string_lossy(), dir));
            }
        }
    }
    let items: Vec<BrowserItem> = rows
        .iter()
        .map(|(label, count)| BrowserItem::new(label, *count))
        .collect();
    store.splice(0, store.n_items(), &items);
    prefix
}

/// Join a root path and a root-relative directory into the filter prefix
/// (no trailing separator; the window's URI comparison appends one).
fn join_root_prefix(root: &str, dir: &str) -> String {
    if dir.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{dir}")
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

// macOS is excluded at the module level: GTK's Quartz backend panics when
// initialized from the test harness worker thread. Gating only the test
// function instead would leave `use super::*` unused on macOS and fail the
// `-D warnings` clippy pass there (observed in run 33921896331).
//
// The contract under test stays in ONE `#[test]` function (GTK must be
// exercised from a single thread, and `gtk::init` must not race itself);
// its sections live in small helpers below, which also keeps each function
// under Codacy's 50-lines-of-code method limit.
#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

    /// Skips on headless machines BEFORE initializing GTK: headless GTK
    /// can still come up via its Broadway fallback, and a test process
    /// that initialized GTK without a real display session segfaults in
    /// GTK teardown at exit (observed as SIGSEGV after all tests passed
    /// on headless Linux CI, run 33921896331). A display session
    /// (`$WAYLAND_DISPLAY` or `$DISPLAY`) is required both to exercise
    /// the contract meaningfully and to exit cleanly. Returns `false`
    /// (after printing why) when the caller must skip.
    fn display_session_available() -> bool {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
            eprintln!(
                "browser_row widget test: no display session \
                 ($WAYLAND_DISPLAY/$DISPLAY unset); skipping. Re-run inside \
                 a desktop session to exercise the contract."
            );
            return false;
        }

        if let Err(e) = gtk::init() {
            eprintln!(
                "browser_row widget test: GTK unavailable ({e}); skipping. \
                 Re-run on a box with a display session (or under a Broadway \
                 headless server) to exercise the contract."
            );
            return false;
        }

        true
    }

    /// Drives the real factory setup signal on a standalone `GtkListItem`.
    /// (`ListItem:item` is read-only and set by the ListView, so the bind
    /// closure itself cannot be exercised outside a realized list;
    /// `browser_row_factory` / `bind_browser_row` / `unbind_browser_row`
    /// are the exact functions the closure calls.)
    fn make_setup_list_item() -> (gtk::ListItem, BrowserRow) {
        let factory = browser_row_factory();
        let list_item: gtk::ListItem = glib::Object::new();
        factory.emit_by_name::<()>("setup", &[&list_item]);
        let row = list_item
            .child()
            .and_downcast::<BrowserRow>()
            .expect("setup must attach a BrowserRow child");
        (list_item, row)
    }

    /// Setup attaches the BrowserRow; its child labels must be
    /// presentational so they are not announced individually.
    fn assert_row_roles_presentational(row: &BrowserRow) {
        assert_eq!(
            row.label().accessible_role(),
            gtk::AccessibleRole::Presentation,
            "primary label must be presentational"
        );
        assert_eq!(
            row.count().accessible_role(),
            gtk::AccessibleRole::Presentation,
            "count label must be presentational"
        );
    }

    /// Bind must publish the combined utterance on the GtkListItem itself
    /// — the row boundary assistive technology actually navigates — and
    /// populate the visible texts.
    fn assert_combined_bind_contract(list_item: &gtk::ListItem, row: &BrowserRow) {
        let item = BrowserItem::new("Miles Davis", 12);
        bind_browser_row(list_item, &item);
        assert_eq!(
            list_item.accessible_label(),
            "Miles Davis, (12)",
            "combined accessible label must be set on the GtkListItem boundary"
        );
        assert_eq!(row.label().text(), "Miles Davis");
        assert_eq!(row.count().text(), "(12)");
    }

    /// Zero-count rows (folder navigation and status) carry no count:
    /// render and announce only the label, never a meaningless "(0)".
    fn assert_zero_count_contract(list_item: &gtk::ListItem, row: &BrowserRow) {
        let nav_item = BrowserItem::new("Folder browsing follows the local library sources", 0);
        bind_browser_row(list_item, &nav_item);
        assert_eq!(
            row.count().text(),
            "",
            "zero-count rows must not render a count"
        );
        assert_eq!(
            list_item.accessible_label(),
            "Folder browsing follows the local library sources",
            "zero-count accessible label must be the bare label"
        );
        assert_eq!(
            row.label().text(),
            "Folder browsing follows the local library sources"
        );
    }

    /// Unbind must clear the accessible name and visible texts so a
    /// recycled list item never announces a stale label.
    fn assert_unbind_reset(list_item: &gtk::ListItem, row: &BrowserRow) {
        unbind_browser_row(list_item);
        assert_eq!(
            list_item.accessible_label(),
            "",
            "unbind must reset the accessible label"
        );
        assert_eq!(row.label().text(), "");
        assert_eq!(row.count().text(), "");
    }

    /// The combined row label ("Label, (Count)") must be exposed on the
    /// `GtkListItem` — the list-row boundary — not on an inner widget, the
    /// child labels must be presentational so the row is announced as a
    /// single utterance ("Artist Name, (123)"), zero-count rows must
    /// announce only the label, and unbind must reset everything.
    ///
    /// Asserts on the `GtkListItem:accessible-label` property (GTK 4.12),
    /// which GTK uses as the row's accessible name — the widget-level
    /// equivalent of an Orca row-announcement smoke test.
    #[test]
    fn browser_row_accessible_label_exposed_on_list_item_boundary() {
        if !display_session_available() {
            return;
        }

        let (list_item, row) = make_setup_list_item();
        assert_row_roles_presentational(&row);
        assert_combined_bind_contract(&list_item, &row);
        assert_unbind_reset(&list_item, &row);
        assert_zero_count_contract(&list_item, &row);
        assert_unbind_reset(&list_item, &row);
    }
}
