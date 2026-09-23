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

use super::album_pane_art::{
    AlbumArtBinder, AlbumArtCache, AlbumArtController, FALLBACK_PLACEHOLDER_ICON,
};
use super::objects::{AlbumArtCandidate, BrowserItem, FolderRowKind, TrackObject};
use crate::ui::folder_browser::{join_native, FolderBrowser, RootBrowseError};
use tracing::debug;

/// Callback invoked when the browser selection changes.
/// Receives (selected_genre, selected_artist, selected_album, folder_prefix, search_text) —
/// `None` = "All" / no folder filter.
pub type FilterCallback =
    Box<dyn Fn(Option<String>, Option<String>, Option<String>, Option<String>, String)>;

/// Opaque handle to the browser's internal track snapshot.
/// Passed back to [`reset_browser_data`] / [`refresh_browser_data`]
/// when the library changes.
#[derive(Clone)]
pub struct BrowserState {
    tracks: Rc<RefCell<Vec<TrackSnapshot>>>,
    /// Shared selection axes — the single source of truth every handler
    /// mutates and every emit composes from (issue #250). Handlers
    /// capture clones of the whole state so a mutation and the
    /// subsequent emit always read and write the same cells.
    selected_genre: Rc<RefCell<Option<String>>>,
    selected_artist: Rc<RefCell<Option<String>>>,
    selected_album: Rc<RefCell<Option<String>>>,
    /// Current search text for the realtime filter.
    search_text: Rc<RefCell<String>>,
    /// Generation counter invalidating pending search debounces when
    /// the shared state is reset (source replacement / full sync).
    search_debounce_gen: Rc<Cell<u32>>,
    /// THE single composition rule: every filter change emits through
    /// [`BrowserState::emit`], which reads all five axes from this
    /// shared state and invokes the callback.
    on_filter_changed: Rc<FilterCallback>,
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
    /// In-memory texture cache keyed by
    /// `(source, track_id, pixel_size)`. Shared with the album pane bind
    /// factory and exposed so callers can clear it on layout/preference
    /// changes.
    album_art_cache: Rc<AlbumArtCache>,
    /// Most-recently installed binder for the album pane. Held by the
    /// state so the rebuild path can revoke every in-flight fetch
    /// before swapping the bind factory, instead of waiting for each
    /// row's `unbind` to fire.
    album_art_binder: Rc<RefCell<Option<AlbumArtBinder>>>,
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

impl BrowserState {
    /// Compose the current filter from the shared axes and invoke the
    /// filter callback — the ONE composition rule (issue #250). Every
    /// mutation path — pane selection, debounced search, album-artist
    /// regrouping, data reset/refresh — emits through here, so the
    /// evaluated result always matches the state the panes display.
    pub fn emit(&self) {
        (self.on_filter_changed)(
            self.selected_genre.borrow().clone(),
            self.selected_artist.borrow().clone(),
            self.selected_album.borrow().clone(),
            self.folder_prefix.borrow().clone(),
            self.search_text.borrow().clone(),
        );
    }

    /// True when any filter axis is set: the window must recompose the
    /// visible list through the browser instead of appending raw rows
    /// (issue #250 — an upsert under an active filter leaked an
    /// unfiltered row into the track list).
    pub fn is_filter_active(&self) -> bool {
        self.selected_genre.borrow().is_some()
            || self.selected_artist.borrow().is_some()
            || self.selected_album.borrow().is_some()
            || self.folder_prefix.borrow().is_some()
            || !self.search_text.borrow().is_empty()
    }

    /// Clear every shared filter axis and invalidate a pending search
    /// debounce. Used when the data the axes point into is replaced
    /// wholesale (source replacement / full sync).
    fn reset_selections(&self) {
        *self.selected_genre.borrow_mut() = None;
        *self.selected_artist.borrow_mut() = None;
        *self.selected_album.borrow_mut() = None;
        *self.search_text.borrow_mut() = String::new();
        *self.folder_prefix.borrow_mut() = None;
        // A debounce timer armed before the reset must never fire with
        // pre-reset text: the generation check inside the timer drops it.
        self.search_debounce_gen
            .set(self.search_debounce_gen.get().wrapping_add(1));
    }

    /// Current search text (window and test introspection).
    pub fn search_text(&self) -> String {
        self.search_text.borrow().clone()
    }
}

/// Build the 3-pane browser.
///
/// Returns `(gtk::Box, BrowserState)`.  The caller must keep the
/// `BrowserState` and pass it to [`reset_browser_data`] (source
/// replacement) or [`refresh_browser_data`] (same-source refresh) as
/// the library changes.
pub fn build_browser(
    all_tracks: &[TrackObject],
    use_album_artist: bool,
    initial_album_pane_artwork: bool,
    initial_album_pane_artwork_size: i32,
    on_filter_changed: FilterCallback,
) -> (gtk::Box, BrowserState) {
    let album_pane_artwork: Rc<Cell<bool>> = Rc::new(Cell::new(initial_album_pane_artwork));
    let album_pane_artwork_size: Rc<Cell<i32>> =
        Rc::new(Cell::new(initial_album_pane_artwork_size));
    // Wrap callback in Rc for sharing between the state and the handlers
    let on_filter_changed: Rc<FilterCallback> = Rc::new(on_filter_changed);

    // Album-art coordinator: virtualized, accessible, bounded cache for
    // the album pane's per-row thumbnails. The source registry is wired
    // in later by the window so the controller can resolve credential-
    // isolated remote artwork without exposing endpoints here.
    let album_art_controller = Rc::new(AlbumArtController::new(FALLBACK_PLACEHOLDER_ICON));

    // Stores for each pane
    let genre_store = gio::ListStore::new::<BrowserItem>();
    let artist_store = gio::ListStore::new::<BrowserItem>();
    let album_store = gio::ListStore::new::<BrowserItem>();
    let folder_store = gio::ListStore::new::<BrowserItem>();

    // Shared mutable track snapshot — updated by reset/refresh.
    let tracks: Rc<RefCell<Vec<TrackSnapshot>>> = Rc::new(RefCell::new(
        all_tracks.iter().map(TrackSnapshot::from_object).collect(),
    ));

    // Re-entrancy guard: when one handler repopulates a sibling store,
    // the sibling's selection_changed fires.  The guard prevents that
    // from cascading into further repopulation.
    let updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Folder-browsing state. The model is attached later by the window
    // (once the local library is known); until then the pane shows the
    // explicit omission notice.
    let folder_model: Rc<RefCell<Option<FolderBrowser>>> = Rc::new(RefCell::new(None));
    let folder_location: Rc<RefCell<FolderLocation>> = Rc::new(RefCell::new(FolderLocation::Roots));
    let folder_prefix: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

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
        None,
    );
    let folder_pane = build_pane("Folder", &folder_store);

    // Initial population — unfiltered, handlers are not connected yet,
    // so autoselect lands every pane on "All".
    {
        let borrowed = tracks.borrow();
        let flag = use_album_artist;
        populate_genres(&genre_store, &borrowed, &None, &None, flag);
        populate_artists(&artist_store, &borrowed, &None, &None, flag);
        populate_albums(&album_store, &borrowed, &None, &None, flag);
    }
    // Folder pane starts detached: the notice row explains the policy
    // until the window attaches the local-library model.
    populate_folder_pane(&folder_store, None, &FolderLocation::Roots);

    // The shared browser state: every filter axis, the search debounce
    // generation, and the composition callback live here — handlers
    // capture clones of it so a mutation and the subsequent emit always
    // touch the same cells (issue #250).
    let state = BrowserState {
        tracks,
        selected_genre: Rc::new(RefCell::new(None)),
        selected_artist: Rc::new(RefCell::new(None)),
        selected_album: Rc::new(RefCell::new(None)),
        search_text: Rc::new(RefCell::new(String::new())),
        search_debounce_gen: Rc::new(Cell::new(0)),
        on_filter_changed,
        use_album_artist: Rc::new(Cell::new(use_album_artist)),
        folder_model,
        folder_location,
        folder_prefix,
        folder_store,
        folder_selection: get_selection(&folder_pane),
        updating,
        album_pane_artwork,
        album_pane_artwork_size,
        album_art_cache: Rc::new(album_art_controller.cache().clone()),
        album_art_controller,
        album_art_binder: Rc::new(RefCell::new(None)),
    };

    // ── Genre selection ──────────────────────────────────────────────
    // User picks a genre → clear the downstream axes, repopulate the
    // panes cross-filtered by the new axes, emit the composed filter.
    {
        let sel = get_selection(&genre_pane);
        let state = state.clone();
        let genre_pane = genre_pane.clone();
        let artist_pane = artist_pane.clone();
        let album_pane = album_pane.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if state.updating.get() {
                return;
            }
            let genre = get_selected_label(sel);
            debug!("Browser: genre changed");
            *state.selected_genre.borrow_mut() = genre.clone();
            *state.selected_artist.borrow_mut() = None;
            *state.selected_album.borrow_mut() = None;

            state.updating.set(true);
            repopulate_panes(
                &state,
                &genre_pane,
                &artist_pane,
                &album_pane,
                &genre,
                &None,
                &None,
            );
            state.updating.set(false);

            state.emit();
        });
    }

    // ── Artist selection ─────────────────────────────────────────────
    // User picks an artist → clear the album axis, repopulate the panes
    // cross-filtered by the new axes, emit the composed filter.
    {
        let sel = get_selection(&artist_pane);
        let state = state.clone();
        let genre_pane = genre_pane.clone();
        let artist_pane = artist_pane.clone();
        let album_pane = album_pane.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if state.updating.get() {
                return;
            }
            let artist = get_selected_label(sel);
            debug!("Browser: artist changed");
            *state.selected_artist.borrow_mut() = artist.clone();
            *state.selected_album.borrow_mut() = None;
            let genre = state.selected_genre.borrow().clone();

            state.updating.set(true);
            // Repopulating the artist store with the album axis cleared
            // also restores the full genre-filtered artist list when the
            // user clears the artist filter ("All", issue #30) — and
            // after any artist pick, so the pane never keeps a list
            // narrowed by an album that was just deselected.
            repopulate_panes(
                &state,
                &genre_pane,
                &artist_pane,
                &album_pane,
                &genre,
                &artist,
                &None,
            );
            state.updating.set(false);

            state.emit();
        });
    }

    // ── Album selection ──────────────────────────────────────────────
    // User picks an album → repopulate the panes cross-filtered by the
    // new axes (album does not narrow itself), emit the composed filter.
    {
        let sel = get_selection(&album_pane);
        let state = state.clone();
        let genre_pane = genre_pane.clone();
        let artist_pane = artist_pane.clone();
        let album_pane = album_pane.clone();

        sel.connect_selection_changed(move |sel, _, _| {
            if state.updating.get() {
                return;
            }
            let album = get_selected_label(sel);
            debug!("Browser: album changed");
            *state.selected_album.borrow_mut() = album.clone();
            let genre = state.selected_genre.borrow().clone();
            let artist = state.selected_artist.borrow().clone();

            state.updating.set(true);
            repopulate_panes(
                &state,
                &genre_pane,
                &artist_pane,
                &album_pane,
                &genre,
                &artist,
                &album,
            );
            state.updating.set(false);

            state.emit();
        });
    }

    // ── Folder activation / navigation ───────────────────────────────
    // Navigation fires on explicit row ACTIVATION — a double-click with
    // the pointer, Enter on the focused row with the keyboard — through
    // the production `ListView::activate` signal, and NEVER on selection
    // changes (issue #251). The sole/first root and the Up row are
    // auto-selected by the model, so a selection-driven handler could
    // never re-navigate them: activating the row that is already
    // selected fires no selection-changed. Activation is independent of
    // selection. Rows carry typed identities (root / directory / up /
    // status), so navigation never interprets display labels — a genuine
    // directory named `…` descends where the old label comparison
    // ascended. The pane's tree is still derived lazily from the
    // attached model: nothing is walked until a level is displayed.
    {
        let state = state.clone();
        let selection = state.folder_selection.clone();
        let list_view = pane_list_view(&folder_pane).expect("folder pane ListView");
        list_view.connect_activate(move |_, position| {
            navigate_folder_row(&state, &selection, position);
        });
    }

    // GtkSearchEntry turns Escape into `stop-search` and leaves the text in
    // place; clearing it makes Escape clear the search.
    search_entry.connect_stop_search(|entry| entry.set_text(""));

    // ── Search entry handler (debounced 100ms) ───────────────────────
    {
        let entry_state = state.clone();

        search_entry.connect_search_changed(move |entry| {
            let text = entry.text().to_string();
            // Programmatic echo: reset_browser_data cleared the entry
            // after clearing the shared state, so the text already
            // matches. Without the guard the late (GTK search-delay)
            // echo would re-arm a debounce and emit a filter the reset
            // just cleared.
            if entry_state.search_text() == text {
                return;
            }
            debug!("Browser: search changed");
            *entry_state.search_text.borrow_mut() = text.clone();

            // Debounce: invalidate any pending timer and schedule a new
            // one. 100ms is short enough to feel responsive but prevents
            // the expensive filter callback from firing on every
            // keystroke during fast typing.
            let gen = entry_state.search_debounce_gen.get().wrapping_add(1);
            entry_state.search_debounce_gen.set(gen);

            // Arm the timer on the context the browser itself lives on —
            // the thread-default one. The free `glib::timeout_add_local*`
            // functions attach to the GLOBAL default context, which the
            // widget test session deliberately never pumps
            // (widget_test_session); production runs both as the same
            // context, so behavior there is unchanged.
            let debounce_context =
                glib::MainContext::thread_default().unwrap_or_else(glib::MainContext::default);
            let timer_state = entry_state.clone();
            debounce_context.spawn_local(async move {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                if timer_state.search_debounce_gen.get() != gen {
                    return; // Superseded by a newer keystroke or a data reset.
                }
                // Compose at fire time from the CURRENT shared axes —
                // the selected album (and every other axis) must ride
                // along with the search text (issue #250).
                timer_state.emit();
            });
        });
    }

    // ── Layout ───────────────────────────────────────────────────────
    // Spacing must stay 0: the separators appended below are the gutter,
    // and Box spacing adds pixels on both sides of each separator,
    // widening every 1px gutter to 3px (2026-09-07 review rejection).
    let panes_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
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

    (browser_box, state)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Switch the album pane between the artwork thumbnail bind factory and
/// the plain label bind factory used by genre and artist.
///
/// The pane widget itself stays put: the swap goes through
/// `ListView::set_factory`, which keeps the `SingleSelection` — and
/// with it the selection-changed handler that drives the genre/artist
/// cross-filter chain — alive. An earlier revision replaced the whole
/// pane widget, which orphaned that handler and silently froze
/// cross-filtering from the first toggle onward.
pub fn set_album_pane_artwork(browser_box: &gtk::Box, state: &BrowserState, enabled: bool) {
    if state.album_pane_artwork.get() == enabled {
        return;
    }
    state.album_pane_artwork.set(enabled);
    rebuild_album_pane(browser_box, state);
}

/// Update the album-pane thumbnail size. The bind factory reads the
/// size knob on every bind, so the change applies to the next set of
/// rows that scroll into view. We swap the bind factory here so the
/// cached textures (keyed by `(album_key, pixel_size)`) are dropped
/// alongside the old factory — a stale entry from the previous
/// size would never be queried again, and leaving it in the cache
/// would still consume the bounded-memory budget.
pub fn set_album_pane_artwork_size(browser_box: &gtk::Box, state: &BrowserState, pixel_size: i32) {
    if state.album_pane_artwork_size.get() == pixel_size {
        return;
    }
    state.album_pane_artwork_size.set(pixel_size);
    rebuild_album_pane(browser_box, state);
}

/// Swap the album pane's bind factory in place for one wired to the
/// current `(album_pane_artwork, album_pane_artwork_size)` knobs.
///
/// The pane widget, its `ListView`, and — critically — its
/// `SingleSelection` all survive the swap: the selection-changed
/// handler that drives the genre/artist cross-filter chain is wired
/// exactly once, in `build_browser`, onto that selection object.
/// Replacing the whole pane (as an earlier revision did) silently
/// orphaned that handler, so the first artwork toggle or size change
/// froze cross-filtering for the rest of the session.
/// `ListView::set_factory` is GTK's supported way to change how rows
/// are built at runtime: the old rows are unbound and torn down, and
/// the new factory rebuilds them against the same model.
///
/// The existing `gio::ListStore` and its selection are likewise left
/// completely untouched: this is a presentation-only factory swap, so
/// the store keeps its CURRENT (genre/artist-filtered) content and the
/// selected album keeps its highlight. An earlier revision repopulated
/// the store with the filters cleared and reset the selection to "All",
/// which expanded the pane to unrelated albums and — through the
/// selection-changed callback — silently cleared the user's active
/// album filter (2026-09-08 PR #171 review). Fresh artwork candidates
/// are not a layout concern: every library change repopulates the store
/// through [`reset_browser_data`].
///
/// The cache is cleared because every entry was decoded at the previous
/// size and would never match a new `(album_key, pixel_size)` lookup
/// under the recompiled bind factory.
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

    // Album pane is the 3rd child (index 2). Keep the pane in place and
    // swap only its bind factory.
    let album_pane = panes[2].clone();
    let Some(list_view) = pane_list_view(&album_pane) else {
        return;
    };

    // Revoke every in-flight fetch on the previous bind factory before
    // we swap it out. The worker-side liveness tokens stop stale
    // results from painting, but they do not stop a fetch from
    // running to completion and burning CPU; revoking here short-
    // circuits the future at its next poll. The `album_art_binder`
    // slot is cleared so the factory build below records the
    // freshly-built binder.
    if let Some(binder) = state.album_art_binder.borrow_mut().take() {
        binder.revoke_all();
    }

    // Clear the cache so the new bind factory doesn't serve stale
    // textures from before the layout change — they're decoded at the
    // old size, and a stale hit would bypass the new bind path
    // entirely.
    state.album_art_cache.clear();

    let new_factory = build_album_factory(
        state.album_art_controller.clone(),
        state.album_pane_artwork.clone(),
        state.album_pane_artwork_size.clone(),
        Some(&state.album_art_binder),
    );
    list_view.set_factory(Some(&new_factory));

    // Deliberately NO store repopulation and NO selection reset here:
    // the model keeps its filtered content and the user's selected album
    // row keeps its highlight across the swap. See the function docs.
}

/// Extract the `ListView` from an album pane Box — the shared anchor
/// for the factory-swap path.
fn pane_list_view(pane: &gtk::Box) -> Option<gtk::ListView> {
    let scrolled = pane.last_child()?.downcast::<gtk::ScrolledWindow>().ok()?;
    scrolled.child()?.downcast::<gtk::ListView>().ok()
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

/// Attach the live application config to the album-art coordinator. Must
/// be called once after `build_browser`. The built-in local library's
/// retained artwork authority resolves against the configured library
/// roots, which live in `AppConfig`; the controller snapshots them per
/// bind. The pointer is intentional (mirrors
/// [`attach_source_registry`]): only the resolver path needs it, and a
/// later preferences edit is observed through the shared cell.
pub fn attach_app_config(
    state: &BrowserState,
    app_config: Rc<RefCell<crate::ui::preferences::AppConfig>>,
) {
    state.album_art_controller.attach_app_config(app_config);
}

/// Attach the application's Tokio runtime to the album-art coordinator.
/// Must be called once after `build_browser`. The built-in local
/// library's retained artwork authority polls Tokio time/blocking APIs,
/// which panic on the runtime-less GTK main context the pane fetch is
/// driven on, so the controller runs that arm on this handle
/// (2026-09-13 review finding).
pub fn attach_runtime(state: &BrowserState, rt_handle: tokio::runtime::Handle) {
    state.album_art_controller.attach_runtime(rt_handle);
}

/// Lightweight snapshot of track fields for filtering (avoids borrowing GObjects).
#[derive(Clone)]
pub struct TrackSnapshot {
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
    pub fn from_object(t: &TrackObject) -> Self {
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

/// Build the album pane's bind factory for the current
/// `(album_pane_artwork, album_pane_artwork_size)` knob values: the
/// artwork-thumbnail factory when artwork is enabled, the standard
/// browser-row factory otherwise. Extracted from [`build_album_pane`] so
/// the layout-change path can rebuild the factory and swap it into the
/// existing `ListView` (keeping the pane, its model, and the
/// selection-changed wiring alive) instead of replacing the pane
/// widget. When artwork is enabled the freshly-built
/// [`AlbumArtBinder`] is recorded into `binder_slot` so the swap path
/// can revoke its in-flight fetches before building the next factory.
fn build_album_factory(
    album_art_controller: Rc<AlbumArtController>,
    album_pane_artwork_visible: Rc<Cell<bool>>,
    album_pane_artwork_size: Rc<Cell<i32>>,
    binder_slot: Option<&Rc<RefCell<Option<AlbumArtBinder>>>>,
) -> gtk::SignalListItemFactory {
    if !album_pane_artwork_visible.get() {
        // Artwork disabled: reuse the SAME browser-row factory the genre
        // and artist panes use, so the album pane keeps the established
        // list-row accessibility contract — presentational label/count
        // children plus the combined album/count accessible name on the
        // GtkListItem — instead of an ad-hoc `gtk::Label` that lost both
        // (2026-09-13 review finding). The visible text now matches the
        // sibling panes (primary label + dimmed count), not a bespoke
        // "label (count)" string.
        return browser_row_factory();
    }

    let factory = gtk::SignalListItemFactory::new();
    let (setup, bind, unbind, teardown, binder) =
        album_art_controller.build_binder_with_size(album_pane_artwork_size.clone());
    factory.connect_setup(setup);
    factory.connect_bind(bind);
    factory.connect_unbind(unbind);
    // Release each row's cell state when GTK discards the list
    // item — otherwise toggles and size changes accumulate dead
    // `AlbumArtCellState` entries (widget tree included) until the
    // next rebuild.
    factory.connect_teardown(teardown);
    // Stash the binder so the factory-swap path can revoke every
    // in-flight fetch on this pane before swapping the factory.
    // Without this, a quick toggle would leave the old fetches
    // racing the new bind factory until each cell's `unbind`
    // eventually fires — and the worker-side generation check
    // alone does not stop a fetch from running to completion.
    if let Some(slot) = binder_slot {
        slot.replace(Some(binder));
    }

    factory
}

/// Build the album pane with an optional artwork column.
///
/// When `album_pane_artwork_visible` is on, the bind factory uses the
/// `AlbumArtController` to fetch a thumbnail for each row. When off,
/// the factory falls back to the plain label used by genre and artist.
///
/// `binder_slot` is supplied on rebuild so the freshly-built binder can
/// be recorded into [`BrowserState::album_art_binder`]. The first build
/// path passes `None` because the state is already being constructed.
fn build_album_pane(
    store: &gio::ListStore,
    album_art_controller: Rc<AlbumArtController>,
    album_pane_artwork_visible: Rc<Cell<bool>>,
    album_pane_artwork_size: Rc<Cell<i32>>,
    binder_slot: Option<&Rc<RefCell<Option<AlbumArtBinder>>>>,
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

    let factory = build_album_factory(
        album_art_controller,
        album_pane_artwork_visible,
        album_pane_artwork_size,
        binder_slot,
    );

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

pub fn populate_albums(
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
// Public API for replacing or refreshing browser data
// ---------------------------------------------------------------------------

/// The three selection panes of a browser box, in display order (genre,
/// artist, album) — extracted from the widget tree shared by the reset,
/// refresh, and regrouping paths.
fn browser_panes(browser_box: &gtk::Box) -> Option<[gtk::Box; 3]> {
    // The browser_box layout is: SearchEntry, panes_box (horizontal Box).
    // The panes_box contains the four panes with gutter separators
    // between them.
    let panes_box = browser_box.last_child()?.downcast::<gtk::Box>().ok()?;
    let mut panes = [None, None, None];
    let mut child = panes_box.first_child();
    while let Some(widget) = child {
        if let Some(pane) = widget.downcast_ref::<gtk::Box>() {
            if let Some(slot) = panes.iter_mut().find(|slot| slot.is_none()) {
                *slot = Some(pane.clone());
            }
        }
        child = widget.next_sibling();
    }
    Some([panes[0].clone()?, panes[1].clone()?, panes[2].clone()?])
}

/// Repopulate all three selection panes cross-filtered by the given
/// axes (each pane ignores its own axis, mirroring the selection
/// handlers) and restore the visual selections. Callers must hold
/// [`BrowserState::updating`] so the programmatic repopulation does not
/// cascade into the selection handlers.
fn repopulate_panes(
    state: &BrowserState,
    genre_pane: &gtk::Box,
    artist_pane: &gtk::Box,
    album_pane: &gtk::Box,
    genre: &Option<String>,
    artist: &Option<String>,
    album: &Option<String>,
) {
    let borrowed = state.tracks.borrow();
    let use_aa = state.use_album_artist.get();
    if let Some(store) = get_store_from_pane(genre_pane) {
        populate_genres(&store, &borrowed, artist, album, use_aa);
    }
    if let Some(store) = get_store_from_pane(artist_pane) {
        populate_artists(&store, &borrowed, genre, album, use_aa);
    }
    if let Some(store) = get_store_from_pane(album_pane) {
        populate_albums(&store, &borrowed, genre, artist, use_aa);
    }
    restore_selection(genre_pane, genre);
    restore_selection(artist_pane, artist);
    restore_selection(album_pane, album);
}

/// Replace the browser's data wholesale and reset every filter axis.
///
/// This is the SOURCE-REPLACEMENT path (a different library became
/// active, or a full sync replaced the data): the previous selections
/// point at values that may no longer exist, so every axis — including
/// the search text and any pending search debounce — resets to "All",
/// and the panes repopulate unfiltered so the displayed selections
/// agree with the reset state (issue #250: the panes used to show "All"
/// while the pre-reset selections silently rode along in the filter
/// callback).
///
/// The folder pane joins the reset: clearing `folder_prefix` alone left
/// `folder_location`, the folder store, and the folder selection on the
/// pre-reset directory, so the pane kept displaying a stale directory
/// inconsistent with the now-empty composed filter (issue #250 rework
/// finding — hit by the album-artist toggle and every reset path that
/// does not explicitly clear or re-attach the folder model).
///
/// Does NOT emit: callers that replace the source splice the full,
/// unfiltered track set themselves (see `window.rs` `display_tracks`),
/// and the reset state composes to exactly that.
pub fn reset_browser_data(browser_box: &gtk::Box, state: &BrowserState, tracks: &[TrackObject]) {
    // The track set just changed (FullSync, source switch, snapshot
    // refresh). Bump the album-art cache's content generation so covers
    // changed by the new data are re-resolved within the SAME source
    // session instead of serving the previous contents of
    // `(source, epoch, album)` (2026-09-10 review finding — the key
    // carried the source epoch but no artwork/content generation, so a
    // same-session FullSync left changed covers stale). Old-generation
    // entries become unqueryable and age out through the bounded
    // eviction.
    state.album_art_controller.cache().bump_content_generation();

    // Update the shared snapshot that selection handlers reference.
    let snapshots: Vec<TrackSnapshot> = tracks.iter().map(TrackSnapshot::from_object).collect();
    *state.tracks.borrow_mut() = snapshots;

    // Clear every shared axis and kill a pending search debounce before
    // the entry's (GTK search-delayed) echo can re-arm it.
    state.reset_selections();

    // Clear the search entry widget if present (first child of
    // browser_box). The resulting search-changed is a no-op: the guard
    // sees the text already matches the cleared shared state.
    if let Some(first) = browser_box.first_child() {
        if let Some(entry) = first.downcast_ref::<gtk::SearchEntry>() {
            entry.set_text("");
        }
    }

    // Reset the folder pane to the roots level so its displayed
    // directory, store rows, and selection agree with the cleared
    // folder_prefix. Runs before the pane extraction below so the
    // folder state resets even when the widget tree is unreachable
    // (idempotent with the explicit clear/attach calls the display
    // paths make right after this function).
    reset_folder_navigation(state);

    let Some(panes) = browser_panes(browser_box) else {
        return;
    };
    state.updating.set(true);
    repopulate_panes(state, &panes[0], &panes[1], &panes[2], &None, &None, &None);
    state.updating.set(false);
}

/// Refresh the browser's data from the SAME source (incremental library
/// events) while preserving still-valid selections.
///
/// Each selection axis is validated against the refreshed snapshot and
/// the most specific one is dropped first (album → artist → genre): a
/// value that vanished falls back to "All" exactly as if the user had
/// cleared it. The panes repopulate cross-filtered by the surviving
/// axes and the visual selections are restored, then the composed
/// filter is emitted — recomputing the evaluated result from the shared
/// state so the track list and status agree with the panes (issue
/// #250).
pub fn refresh_browser_data(browser_box: &gtk::Box, state: &BrowserState, tracks: &[TrackObject]) {
    let snapshots: Vec<TrackSnapshot> = tracks.iter().map(TrackSnapshot::from_object).collect();
    *state.tracks.borrow_mut() = snapshots;

    // Validate the axes against the refreshed snapshot, dropping the
    // most specific axis first (album → artist → genre).
    let mut genre = state.selected_genre.borrow().clone();
    let mut artist = state.selected_artist.borrow().clone();
    let mut album = state.selected_album.borrow().clone();
    {
        let borrowed = state.tracks.borrow();
        let use_aa = state.use_album_artist.get();
        if !snapshot_matches(&borrowed, &genre, &artist, &album, use_aa) {
            album = None;
        }
        if !snapshot_matches(&borrowed, &genre, &artist, &album, use_aa) {
            artist = None;
        }
        if !snapshot_matches(&borrowed, &genre, &artist, &album, use_aa) {
            genre = None;
        }
    }
    *state.selected_genre.borrow_mut() = genre.clone();
    *state.selected_artist.borrow_mut() = artist.clone();
    *state.selected_album.borrow_mut() = album.clone();

    let Some(panes) = browser_panes(browser_box) else {
        // No pane widgets reachable — still recompose the evaluated
        // result so the track list and status agree with the state.
        state.emit();
        return;
    };
    state.updating.set(true);
    repopulate_panes(
        state, &panes[0], &panes[1], &panes[2], &genre, &artist, &album,
    );
    state.updating.set(false);

    state.emit();
}

/// True when any track in `tracks` matches all three axes at once
/// (with the browser's album-artist grouping applied to the artist
/// axis). Mirrors the `populate_*` filters.
fn snapshot_matches(
    tracks: &[TrackSnapshot],
    genre: &Option<String>,
    artist: &Option<String>,
    album: &Option<String>,
    use_album_artist: bool,
) -> bool {
    tracks.iter().any(|t| {
        genre.as_ref().is_none_or(|g| t.genre == *g)
            && artist
                .as_ref()
                .is_none_or(|a| t.browser_artist(use_album_artist) == a)
            && album.as_ref().is_none_or(|al| t.album == *al)
    })
}

/// Display label of the pane row that ascends one folder level. Purely
/// presentational: navigation keys off the row's typed
/// [`FolderRowKind::Up`] identity, never off this string (issue #251 —
/// a genuine directory named `…` must descend).
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
    // Purely visual now: navigation fires on row activation, not on
    // selection changes (issue #251), so resetting the selection to the
    // first row cannot re-trigger navigation.
    state.folder_selection.set_selected(0);
    state.updating.set(true);
    populate_folder_pane(
        &state.folder_store,
        state.folder_model.borrow().as_ref(),
        &FolderLocation::Roots,
    );
    state.updating.set(false);
}

/// Repopulate the folder pane for `location`, returning the track-filter
/// prefix it selects (`None` = no folder filter at the roots level or while
/// detached). This is the lazy navigation step: exactly one level is
/// derived from the model per call.
///
/// Every row carries its typed [`FolderRowKind`] identity — root,
/// directory, up, or status — so the activation handler never has to
/// interpret display labels (issue #251).
fn populate_folder_pane(
    store: &gio::ListStore,
    model: Option<&FolderBrowser>,
    location: &FolderLocation,
) -> Option<String> {
    let mut rows: Vec<(String, u32, FolderRowKind)> = Vec::new();
    let mut prefix: Option<String> = None;
    match (model, location) {
        (None, _) => {
            rows.push((
                "Folder browsing follows the local library sources".to_string(),
                0,
                FolderRowKind::Status,
            ));
        }
        (Some(browser), FolderLocation::Roots) => {
            let labels = browser.disambiguated_root_labels();
            for (root, label) in browser.roots().iter().zip(labels) {
                let label = match root.availability_suffix() {
                    Some(suffix) => format!("{label}{suffix}"),
                    None => label,
                };
                // Root rows are pushed in model order, so the row's
                // position within the store IS the root's index — the
                // activation handler resolves the root positionally,
                // never by label.
                rows.push((label, 0, FolderRowKind::Root));
            }
            if rows.is_empty() {
                rows.push((
                    "No library folders configured".to_string(),
                    0,
                    FolderRowKind::Status,
                ));
            }
        }
        (Some(browser), FolderLocation::Inside { root_id, dir }) => {
            rows.push((FOLDER_UP_LABEL.to_string(), 0, FolderRowKind::Up));
            match browser.children(root_id, dir) {
                Ok(children) => {
                    for child in children {
                        // The label of a Directory row is its final path
                        // component — data attached to a typed row, not a
                        // string the handler interprets.
                        rows.push((
                            child.name,
                            child.track_count as u32,
                            FolderRowKind::Directory,
                        ));
                    }
                }
                Err(RootBrowseError::Unavailable { reason }) => {
                    rows.push((format!("(unavailable: {reason})"), 0, FolderRowKind::Status));
                }
                Err(RootBrowseError::Renamed { previous_path }) => {
                    rows.push((
                        format!("(renamed from {previous_path})"),
                        0,
                        FolderRowKind::Status,
                    ));
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
        .map(|(label, count, kind)| BrowserItem::with_folder_kind(label, *count, *kind))
        .collect();
    store.splice(0, store.n_items(), &items);
    prefix
}

/// Apply one folder navigation step for the row activated at `position`
/// (issue #251): resolve the row's typed identity against the current
/// location, move to the next location, repopulate the pane, reset the
/// visual selection, and emit the recomposed filter. Status rows and
/// unresolvable targets are a no-op — activation is the ONLY navigation
/// trigger, independent of selection changes.
fn navigate_folder_row(state: &BrowserState, selection: &gtk::SingleSelection, position: u32) {
    if state.updating.get() {
        return;
    }
    let Some(item) = selection.item(position).and_downcast::<BrowserItem>() else {
        return;
    };
    let Some(next) = resolve_folder_activation(state, &item, position) else {
        return;
    };

    *state.folder_location.borrow_mut() = next;
    state.updating.set(true);
    let new_prefix = populate_folder_pane(
        &state.folder_store,
        state.folder_model.borrow().as_ref(),
        &state.folder_location.borrow(),
    );
    selection.set_selected(0);
    state.updating.set(false);
    *state.folder_prefix.borrow_mut() = new_prefix;

    state.emit();
}

/// Resolve an activated folder row to the location it navigates to, or
/// `None` when the activation must not navigate (status rows, detached
/// model, refused roots). Root rows resolve positionally — at the roots
/// level the row position IS the root index, by construction of
/// [`populate_folder_pane`] — so no display label is ever interpreted.
fn resolve_folder_activation(
    state: &BrowserState,
    item: &BrowserItem,
    position: u32,
) -> Option<FolderLocation> {
    let current = state.folder_location.borrow().clone();
    match (&current, item.folder_kind()) {
        // Informational rows (detached notice, empty-roots notice,
        // unavailable / renamed markers) never navigate.
        (_, FolderRowKind::Status) => None,
        (FolderLocation::Roots, FolderRowKind::Root) => {
            let model_ref = state.folder_model.borrow();
            let browser = model_ref.as_ref()?;
            let root = browser.roots().get(position as usize)?;
            if !root.browsable() {
                // Listed for visibility, but navigation is refused.
                return None;
            }
            Some(FolderLocation::Inside {
                root_id: root.root_id.clone(),
                dir: String::new(),
            })
        }
        // Inside a root, the Up row ascends one level (back to the roots
        // level from the root itself).
        (FolderLocation::Inside { root_id, dir }, FolderRowKind::Up) => {
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
        }
        // Any other row inside a root is a typed Directory: descend by
        // the row's name. (Root rows cannot occur at this level, and
        // Status rows were refused above.)
        (FolderLocation::Inside { root_id, dir }, FolderRowKind::Directory) => {
            let child_dir = if dir.is_empty() {
                item.label()
            } else {
                format!("{}/{}", dir, item.label())
            };
            Some(FolderLocation::Inside {
                root_id: root_id.clone(),
                dir: child_dir,
            })
        }
        // Directory / Up rows cannot occur at the roots level; refuse
        // rather than guess.
        (FolderLocation::Roots, FolderRowKind::Directory | FolderRowKind::Up) => None,
        (FolderLocation::Inside { .. }, FolderRowKind::Root) => None,
    }
}

/// Join a root path and a root-relative directory into the filter prefix
/// (no trailing separator; the window's URI comparison appends one).
/// `dir` uses the portable `/`-separated representation and is normalized
/// with the model's no-escape rules before the native join, so a Windows
/// root mixes no separators and a crafted dir cannot climb above the root.
fn join_root_prefix(root: &str, dir: &str) -> String {
    join_native(root, dir).to_string_lossy().into_owned()
}

/// Toggle album-artist grouping and rebuild the browser panes.
///
/// Updates the shared flag, then refreshes all three panes from the
/// current snapshot.  Selections reset to "All" because the artist
/// pane's contents are about to change; the composed filter is emitted
/// at the end so the evaluated result follows the reset (issue #250).
pub fn set_album_artist_grouping(browser_box: &gtk::Box, state: &BrowserState, enabled: bool) {
    state.use_album_artist.set(enabled);
    *state.selected_genre.borrow_mut() = None;
    *state.selected_artist.borrow_mut() = None;
    *state.selected_album.borrow_mut() = None;

    let Some(panes) = browser_panes(browser_box) else {
        state.emit();
        return;
    };
    state.updating.set(true);
    repopulate_panes(state, &panes[0], &panes[1], &panes[2], &None, &None, &None);
    state.updating.set(false);

    state.emit();
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
// This module holds the crate's SINGLE GTK-initializing `#[test]` (GTK
// must be exercised from a single thread, and `gtk::init` must not race
// itself). The `ui::widget_test_session` mutex serializes GTK-initializing
// tests but does not give them thread affinity, so a second GTK-touching
// `#[test]` would still run on a different libtest worker thread than the
// one that ran `gtk::init` and construct widgets off the initializing
// thread (2026-09-09 review rejection, PR #179). Every GTK-touching
// contract in the crate — including the context-menu ones — therefore runs
// inside that one test's body, via small helpers here and in
// `context_menu::tests`, which also keeps each function under Codacy's
// 50-lines-of-code method limit. The test funnels its display gate, the
// single `gtk::init`, and the whole widget-exercising body through the
// crate-wide `ui::widget_test_session` lock.
#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

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

    // ── Q4 engine-loop/GTK responsiveness lane (tr-am6qr) ──────────────

    /// Deterministic synthetic snapshot spread over 4 genres, 5 artists,
    /// and 4 album-per-artist groups, so browser pane cardinality is a
    /// known function of the row count.
    fn q4_bench_track_objects(count: usize) -> Vec<TrackObject> {
        (0..count)
            .map(|index| {
                TrackObject::new(
                    (index % 12) as u32,
                    &format!("Q4 Bench Track {index:05}"),
                    180,
                    &format!("Q4 Artist {}", index % 5),
                    &format!("Q4 Album {}", index % 4),
                    &format!("Q4 Genre {}", index % 4),
                    "",
                    2026,
                    "2026-09-18",
                    320,
                    44_100,
                    0,
                    "FLAC",
                    &format!("file:///q4-bench/{index:05}.flac"),
                )
            })
            .collect()
    }

    /// Raw committed rows carrying the same synthetic identity as
    /// [`q4_bench_track_objects`], so the timed FullSync unit starts where
    /// production starts: from `architecture::Track` rows with the
    /// Track→TrackObject conversion inside the measured region, not from
    /// prebuilt `TrackObject`s.
    fn q4_bench_arch_tracks(count: usize) -> Vec<crate::architecture::models::Track> {
        let date_modified = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 9, 18, 0, 0, 0)
            .single()
            .expect("fixed bench date");
        (0..count)
            .map(|index| crate::architecture::models::Track {
                id: uuid::Uuid::from_u128(index as u128),
                native_track_id: None,
                title: format!("Q4 Bench Track {index:05}"),
                artist_name: format!("Q4 Artist {}", index % 5),
                album_artist_name: None,
                artist_id: None,
                album_title: format!("Q4 Album {}", index % 4),
                album_id: None,
                track_number: Some((index % 12) as u32),
                disc_number: None,
                duration_secs: Some(180),
                composer: None,
                genre: Some(format!("Q4 Genre {}", index % 4)),
                year: Some(2026),
                file_path: Some(format!("/q4-bench/{index:05}.flac")),
                stream_url: None,
                cover_art_url: None,
                date_added: None,
                date_modified: Some(date_modified),
                bitrate_kbps: Some(320),
                sample_rate_hz: Some(44_100),
                format: Some("FLAC".to_string()),
                play_count: Some(0),
                rating: crate::architecture::models::TrackRating::Unsupported,
                last_played: None,
            })
            .collect()
    }

    /// The genre/artist/album ListStores behind the browser panes, in pane
    /// order (same traversal contract as `reset_browser_data`).
    fn q4_browser_pane_stores(browser_box: &gtk::Box) -> Vec<gio::ListStore> {
        let mut stores = Vec::new();
        let Some(panes_box) = browser_box.last_child().and_downcast::<gtk::Box>() else {
            return stores;
        };
        let mut child = panes_box.first_child();
        while let Some(widget) = child {
            if let Some(pane) = widget.downcast_ref::<gtk::Box>() {
                if let Some(store) = get_store_from_pane(pane) {
                    stores.push(store);
                }
            }
            child = widget.next_sibling();
        }
        stores
    }

    /// FullSync publication contract plus the opt-in measurement for the
    /// GTK-side endpoints this lane owns (source publication / browser
    /// rebuild latency after the scan settles, and the main-loop stall the
    /// synchronous publication inflicts).
    ///
    /// The contract half always runs: a `display_tracks` publication over a
    /// small snapshot must replace the track store, the master rows, the
    /// browser snapshot, and repopulate the genre pane. The measurement
    /// half runs only when `TRIBUTARY_Q4_UI_BENCH_TRACKS` is set to a row
    /// count — an explicit measurement run, recorded per runner in
    /// `docs/engine-ui-responsiveness.md`. Runs inside the crate's single
    /// GTK session (see the consolidated test below).
    fn q4_publication_contract_and_bench() {
        let objects = q4_bench_track_objects(60);
        let (browser_box, browser_state) =
            build_browser(&[], false, false, 48, Box::new(|_, _, _, _, _| {}));
        let track_store = gio::ListStore::new::<TrackObject>();
        // The production tracklist drives a ColumnView over a selection
        // model wrapping the store; mirror that so `display_tracks`'s
        // scroll_to target exists.
        let selection = gtk::SingleSelection::new(Some(track_store.clone()));
        let column_view = gtk::ColumnView::new(Some(selection));
        let master_tracks = RefCell::new(Vec::new());
        let status_label = gtk::Label::default();

        crate::ui::window::display_tracks(
            &objects,
            &track_store,
            &master_tracks,
            &browser_box,
            &browser_state,
            &status_label,
            &column_view,
        );
        assert_eq!(
            track_store.n_items() as usize,
            objects.len(),
            "publication must replace the visible track store"
        );
        assert_eq!(
            master_tracks.borrow().len(),
            objects.len(),
            "publication must replace the master row set"
        );
        assert_eq!(
            browser_state.tracks.borrow().len(),
            objects.len(),
            "publication must replace the browser snapshot"
        );
        let pane_stores = q4_browser_pane_stores(&browser_box);
        // populate_genres prepends the "All" row: 1 + the 4 synthetic genres.
        assert_eq!(
            pane_stores.first().map(|store| store.n_items()),
            Some(5),
            "publication must repopulate the genre pane from the snapshot"
        );

        if let Ok(bench_rows) = std::env::var("TRIBUTARY_Q4_UI_BENCH_TRACKS") {
            if let Ok(rows) = bench_rows.parse::<usize>() {
                if rows > 0 {
                    q4_measure_publication_rebuild(rows);
                }
            }
        }
    }

    /// Owned GTK fixture for the publication/rebuild benchmark: the
    /// widgets and shared state one FullSync publication touches, built
    /// empty so the first publication mirrors the startup FullSync path
    /// (empty panes → full snapshot).
    struct Q4PublicationBench {
        browser_box: gtk::Box,
        browser_state: BrowserState,
        track_store: gio::ListStore,
        column_view: gtk::ColumnView,
        master_tracks: Rc<RefCell<Vec<TrackObject>>>,
        status_label: gtk::Label,
        active_source_key: Rc<RefCell<String>>,
        playback_session: Rc<RefCell<crate::ui::playback::PlaybackSession>>,
        source_tracks: Rc<RefCell<std::collections::HashMap<String, Vec<TrackObject>>>>,
        app_config: Rc<RefCell<crate::ui::preferences::AppConfig>>,
    }

    impl Q4PublicationBench {
        fn new() -> Self {
            let (browser_box, browser_state) =
                build_browser(&[], false, false, 48, Box::new(|_, _, _, _, _| {}));
            let track_store = gio::ListStore::new::<TrackObject>();
            let selection = gtk::SingleSelection::new(Some(track_store.clone()));
            let column_view = gtk::ColumnView::new(Some(selection));
            // One configured library root covering every synthetic track
            // path, so the folder-model rebuild does real root matching
            // per row the way production does.
            let app_config = Rc::new(RefCell::new(crate::ui::preferences::AppConfig {
                library_paths: vec!["/q4-bench".to_string()],
                ..crate::ui::preferences::AppConfig::default()
            }));
            Self {
                browser_box,
                browser_state,
                track_store,
                column_view,
                master_tracks: Rc::new(RefCell::new(Vec::new())),
                status_label: gtk::Label::default(),
                active_source_key: Rc::new(RefCell::new("local".to_string())),
                playback_session: Rc::new(RefCell::new(
                    crate::ui::playback::PlaybackSession::default(),
                )),
                source_tracks: Rc::new(RefCell::new(std::collections::HashMap::<
                    String,
                    Vec<TrackObject>,
                >::new())),
                app_config,
            }
        }

        /// The exact unit the production FullSync arm calls (a
        /// source-structure test pins the arm to it).
        fn publish(&self, tracks: &[crate::architecture::models::Track]) {
            crate::ui::window::apply_full_sync_publication(
                tracks,
                &self.active_source_key,
                &self.playback_session,
                &self.source_tracks,
                &self.master_tracks,
                &self.track_store,
                &self.browser_box,
                &self.browser_state,
                &self.status_label,
                &self.column_view,
                &self.app_config,
            );
        }
    }

    /// The unit must have published everything it is contractually
    /// responsible for, not merely have run fast.
    fn q4_assert_publication_published(bench: &Q4PublicationBench, rows: usize) {
        assert_eq!(
            bench.track_store.n_items() as usize,
            rows,
            "full-sync publication must replace the visible track store"
        );
        assert_eq!(
            bench.master_tracks.borrow().len(),
            rows,
            "full-sync publication must replace the master row set"
        );
        assert_eq!(
            bench.browser_state.tracks.borrow().len(),
            rows,
            "full-sync publication must replace the browser snapshot"
        );
        assert_eq!(
            bench.source_tracks.borrow().get("local").map(Vec::len),
            Some(rows),
            "full-sync publication must store the per-source projection"
        );
    }

    /// Display-only lower bound at `rows` scale: a second browser built
    /// empty, objects preconverted outside the timer, `display_tracks`
    /// alone — then `reset_browser_data` alone. Returns
    /// (`display_only_ms`, `rebuild_ms`).
    fn q4_measure_display_only(rows: usize) -> (f64, f64) {
        let objects = q4_bench_track_objects(rows);
        let (display_box, display_state) =
            build_browser(&[], false, false, 48, Box::new(|_, _, _, _, _| {}));
        let display_store = gio::ListStore::new::<TrackObject>();
        let display_selection = gtk::SingleSelection::new(Some(display_store.clone()));
        let display_view = gtk::ColumnView::new(Some(display_selection));
        let display_master = RefCell::new(Vec::new());
        let display_status = gtk::Label::default();
        let display_started = std::time::Instant::now();
        crate::ui::window::display_tracks(
            &objects,
            &display_store,
            &display_master,
            &display_box,
            &display_state,
            &display_status,
            &display_view,
        );
        let display_only_ms = display_started.elapsed().as_micros() as f64 / 1_000.0;
        let rebuild_started = std::time::Instant::now();
        reset_browser_data(&display_box, &display_state, &objects);
        let rebuild_ms = rebuild_started.elapsed().as_micros() as f64 / 1_000.0;
        (display_only_ms, rebuild_ms)
    }

    /// Timed publication at `rows` scale, on a browser built empty so the
    /// first publication mirrors the startup FullSync path (empty panes →
    /// full snapshot). Times the complete production publication unit
    /// ([`crate::ui::window::apply_full_sync_publication`]) — arch
    /// Track→TrackObject conversion, playlist/queue refresh, the
    /// per-source clone, and `display_local_tracks` (display + folder
    /// model rebuild), everything the main loop blocks on during a
    /// FullSync — alongside the display-only lower bound (`display_tracks`
    /// alone on a second empty browser at the same scale, objects
    /// preconverted outside the timer). Prints `Q4_UI_METRIC` lines.
    fn q4_measure_publication_rebuild(rows: usize) {
        let tracks = q4_bench_arch_tracks(rows);
        let bench = Q4PublicationBench::new();

        let fullsync_started = std::time::Instant::now();
        bench.publish(&tracks);
        let fullsync_first_ms = fullsync_started.elapsed().as_micros() as f64 / 1_000.0;
        let resync_started = std::time::Instant::now();
        bench.publish(&tracks);
        let fullsync_resync_ms = resync_started.elapsed().as_micros() as f64 / 1_000.0;

        q4_assert_publication_published(&bench, rows);

        // The full-path unit does more work than the display-only bound by
        // construction (conversion, clone, folder-model rebuild on top of
        // `display_tracks`), but both are single wall-clock samples on
        // separate GTK fixtures, so scheduling noise can invert their
        // recorded order at any scale without invalidating either number.
        // The difference is therefore reported as measurement data, not
        // gated on an ordering assertion; coverage and publication stay
        // asserted by `q4_assert_publication_published` above.
        let (display_only_ms, rebuild_ms) = q4_measure_display_only(rows);
        let fullsync_margin_ms = fullsync_first_ms - display_only_ms;

        println!(
            "Q4_UI_METRIC name=fullsync_publication_first_ms rows={rows} value={fullsync_first_ms:.3}"
        );
        println!(
            "Q4_UI_METRIC name=fullsync_publication_resync_ms rows={rows} value={fullsync_resync_ms:.3}"
        );
        println!(
            "Q4_UI_METRIC name=publication_display_only_ms rows={rows} value={display_only_ms:.3}"
        );
        println!("Q4_UI_METRIC name=browser_rebuild_ms rows={rows} value={rebuild_ms:.3}");
        println!(
            "Q4_UI_NOTE full-path publication minus display-only bound at {rows} rows: \
             {fullsync_margin_ms:.3} ms (single-sample comparison; ordering may invert \
             under load and is reported, not asserted)"
        );
    }

    /// Compact track fixture for the pane tests (the real constructor
    /// takes 14 arguments; only these five vary here).
    fn art_fixture_track(
        number: u32,
        artist: &str,
        album: &str,
        genre: &str,
        uri: &str,
    ) -> TrackObject {
        TrackObject::new(
            number, "T", 60, artist, album, genre, "", 0, "", 0, 0, 0, "", uri,
        )
    }

    /// Collect the four pane boxes from the browser widget tree
    /// (browser_box = [SearchEntry, panes_box], panes_box = [genre,
    /// artist, album, folder] with separators between them — mirrors
    /// `reset_browser_data`). The separators are `gtk::Separator`s, so
    /// filtering to `gtk::Box` children drops them and the album pane
    /// stays at index 2.
    fn collect_browser_panes(browser_box: &gtk::Box) -> Vec<gtk::Box> {
        let panes_box = browser_box
            .last_child()
            .and_then(|w| w.downcast::<gtk::Box>().ok())
            .expect("panes box");
        let mut child = panes_box.first_child();
        let mut panes = Vec::new();
        while let Some(widget) = child {
            if let Some(pane) = widget.downcast_ref::<gtk::Box>() {
                panes.push(pane.clone());
            }
            child = widget.next_sibling();
        }
        // `build_browser` appends four pane boxes (genre, artist, album,
        // folder) with 1px separators between them; the separators are not
        // `gtk::Box` children, so exactly four panes remain and the album
        // pane is still indexed at position 2.
        assert_eq!(panes.len(), 4, "genre, artist, album and folder panes");
        panes
    }

    /// A presentation-only factory swap must keep the album store
    /// filtered and the album/genre selections in place.
    fn assert_album_pane_preserved(panes: &[gtk::Box], album_store: &gio::ListStore) {
        assert_eq!(
            album_store.n_items(),
            3,
            "the genre-filtered album store must survive the swap"
        );
        assert_eq!(
            get_selection(&panes[2]).selected(),
            2,
            "the selected album row must survive the swap"
        );
        assert_eq!(get_selection(&panes[0]).selected(), 1);
    }

    /// Codex P2 (PR #171 discussion r3962112844): toggling album-pane
    /// artwork or changing its thumbnail size is a presentation-only
    /// factory swap. The rebuild must keep the album store filtered to
    /// the active genre selection and keep the selected album row; the
    /// previous implementation repopulated the store with the filters
    /// cleared and reset the selection to "All", which expanded the pane
    /// to unrelated albums and — through the selection-changed callback
    /// — silently cleared the user's active album filter.
    fn factory_swap_preserves_album_filters_and_selection() {
        let tracks = vec![
            art_fixture_track(1, "AR", "A1", "G1", "file:///t1.flac"),
            art_fixture_track(2, "AR", "A2", "G1", "file:///t2.flac"),
            art_fixture_track(3, "AR2", "A3", "G2", "file:///t3.flac"),
        ];
        let (browser_box, state) =
            build_browser(&tracks, false, false, 48, Box::new(|_, _, _, _, _| {}));
        let panes = collect_browser_panes(&browser_box);

        // Filter to genre G1 (genre store: "All", "G1", "G2" → index 1).
        // The album store narrows to G1's albums: "All", "A1", "A2".
        get_selection(&panes[0]).set_selected(1);
        let album_store = get_store_from_pane(&panes[2]).expect("album store");
        assert_eq!(
            album_store.n_items(),
            3,
            "the genre filter must narrow the album pane before the swap"
        );
        // Select the second album row ("A2").
        get_selection(&panes[2]).set_selected(2);

        // Layout toggle: swap the artwork factory in place.
        set_album_pane_artwork(&browser_box, &state, true);
        assert_album_pane_preserved(&panes, &album_store);

        // Thumbnail size change: same contract.
        set_album_pane_artwork_size(&browser_box, &state, 72);
        assert_album_pane_preserved(&panes, &album_store);
    }

    /// FullSync and any other full data rebuild must invalidate cached
    /// covers: the rebuild bumps the album-art cache's content
    /// generation, so artwork decoded before the rebuild can never be
    /// queried afterwards (2026-09-10 review finding — same-session
    /// FullSync used to leave changed covers stale).
    fn rebuild_bumps_album_art_content_generation() {
        let tracks = vec![art_fixture_track(1, "AR", "A1", "G1", "file:///t1.flac")];
        let (browser_box, state) =
            build_browser(&tracks, false, false, 48, Box::new(|_, _, _, _, _| {}));
        let before = state.album_art_controller.cache().content_generation();
        reset_browser_data(&browser_box, &state, &[]);
        assert!(
            state.album_art_controller.cache().content_generation() > before,
            "a full data rebuild must advance the album-art content generation"
        );
    }

    /// With artwork disabled the album pane must keep the standard
    /// browser-row accessibility contract: its factory setup must attach
    /// the shared [`BrowserRow`] (presentational label/count children)
    /// that bind publishes the combined accessible name on, not an ad-hoc
    /// `gtk::Label` that regressed both (2026-09-13 review finding).
    fn album_artwork_disabled_keeps_browser_row_contract() {
        let controller = Rc::new(AlbumArtController::new(FALLBACK_PLACEHOLDER_ICON));
        let factory = build_album_factory(
            controller,
            Rc::new(Cell::new(false)),
            Rc::new(Cell::new(48)),
            None,
        );
        let list_item: gtk::ListItem = glib::Object::new();
        factory.emit_by_name::<()>("setup", &[&list_item]);
        let row = list_item
            .child()
            .and_downcast::<BrowserRow>()
            .expect("disabled album factory must reuse the shared BrowserRow");
        assert_row_roles_presentational(&row);
        bind_browser_row(&list_item, &BrowserItem::new("Kind of Blue", 9));
        assert_eq!(
            list_item.accessible_label(),
            "Kind of Blue, (9)",
            "disabled album rows must publish the combined accessible name"
        );
    }

    // ── Browser data lifecycle contracts (issue #250) ─────────────────
    // Search, source replacement, and incremental refresh must keep the
    // displayed pane selections and the composed filter in sync.

    /// Fixture track with the given grouping fields (no album-artist
    /// tag, so browser grouping falls back to the track artist).
    fn fixture_track(genre: &str, artist: &str, album: &str, title: &str) -> TrackObject {
        TrackObject::new(
            1,
            title,
            60,
            artist,
            album,
            genre,
            "",
            0,
            "",
            0,
            0,
            0,
            "flac",
            &format!("file:///lib/{artist}/{album}/{title}.flac"),
        )
    }

    /// Log of composed filter emits: (genre, artist, album, folder,
    /// search).
    type EmitLog = Rc<
        RefCell<
            Vec<(
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
            )>,
        >,
    >;

    /// A filter callback that records every composed emit.
    fn recorder() -> (EmitLog, FilterCallback) {
        let log: EmitLog = Rc::new(RefCell::new(Vec::new()));
        let sink = log.clone();
        let cb = Box::new(move |g, a, al, f, s| {
            sink.borrow_mut().push((g, a, al, f, s));
        });
        (log, cb)
    }

    /// The most recent composed emit.
    fn composed(
        log: &EmitLog,
    ) -> (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) {
        log.borrow().last().expect("at least one emit").clone()
    }

    /// The most recent search text, if anything was emitted yet (a
    /// reset legitimately emits nothing, so pumps must tolerate an
    /// empty log).
    fn last_search(log: &EmitLog) -> Option<String> {
        log.borrow().last().map(|entry| entry.4.clone())
    }

    fn search_entry_of(browser_box: &gtk::Box) -> gtk::SearchEntry {
        browser_box
            .first_child()
            .and_downcast::<gtk::SearchEntry>()
            .expect("search entry is the browser box's first child")
    }

    /// The browser's composed-filter entry point: type into the search
    /// entry and deliver `search-changed` directly. GTK's own
    /// search-delay timer is a C-side source on the GLOBAL default main
    /// context, which the widget test session deliberately never pumps,
    /// so real typing would never surface here — fire the signal the
    /// delay would eventually call, with the entry's new text.
    fn type_search(browser_box: &gtk::Box, text: &str) {
        let entry = search_entry_of(browser_box);
        entry.set_text(text);
        entry.emit_by_name::<()>("search-changed", &[]);
    }

    /// Pump the session's thread-default main context until `done` or
    /// the deadline. Typing surfaces synchronously via
    /// [`type_search`]; the browser's 100ms debounce then runs on this
    /// context before an emit can be observed. Polls without blocking
    /// (a context with no ready sources must not park the test past
    /// the deadline).
    fn pump_until(
        context: &glib::MainContext,
        deadline: std::time::Instant,
        done: impl Fn() -> bool,
    ) {
        while !done() && std::time::Instant::now() < deadline {
            if !context.iteration(false) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    /// Pump the session's context for a fixed duration.
    fn pump_for(context: &glib::MainContext, duration: std::time::Duration) {
        pump_until(context, std::time::Instant::now() + duration, || false);
    }

    fn session_context() -> glib::MainContext {
        glib::MainContext::thread_default().expect("widget session pushed a main context")
    }

    /// THE reported defect: with an album selected, typing into search
    /// and clearing it again must keep the album axis — the composed
    /// filter must carry the album the panes still display (issue
    /// #250).
    fn album_selection_survives_typing_and_clearing() {
        let context = session_context();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let tracks = vec![
            fixture_track("Jazz", "Alpha", "Album One", "T1"),
            fixture_track("Jazz", "Alpha", "Album One", "T2"),
            fixture_track("Jazz", "Alpha", "Album Two", "T3"),
        ];
        let (log, cb) = recorder();
        let (browser_box, _state) = build_browser(&tracks, false, false, 48, cb);
        let panes = browser_panes(&browser_box).expect("panes");

        // Rows are ["All", "Album One", "Album Two"] ("All" is
        // prepended, album rows follow in sorted order).
        get_selection(&panes[2]).set_selected(1);
        assert_eq!(
            composed(&log).2.as_deref(),
            Some("Album One"),
            "album pick must compose album axis"
        );

        type_search(&browser_box, "zzz");
        pump_until(&context, deadline, || composed(&log).4 == "zzz");
        let (genre, artist, album, folder, search) = composed(&log);
        assert_eq!(search, "zzz");
        assert_eq!(
            album.as_deref(),
            Some("Album One"),
            "searching must not drop the album selection (issue #250)"
        );
        assert_eq!((genre, artist, folder), (None, None, None));

        type_search(&browser_box, "");
        pump_until(&context, deadline, || composed(&log).4.is_empty());
        let (_, _, album, _, _) = composed(&log);
        assert_eq!(
            album.as_deref(),
            Some("Album One"),
            "clearing the search must not drop the album selection"
        );
    }

    /// Escape reaches a focused GtkSearchEntry as `stop-search`, which GTK
    /// leaves to the application: it must clear the text, and the composed
    /// filter must follow.
    fn escape_clears_the_search() {
        let context = session_context();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let tracks = vec![fixture_track("Jazz", "Alpha", "Album One", "T1")];
        let (log, cb) = recorder();
        let (browser_box, _state) = build_browser(&tracks, false, false, 48, cb);

        type_search(&browser_box, "zzz");
        pump_until(&context, deadline, || {
            last_search(&log).as_deref() == Some("zzz")
        });
        assert_eq!(last_search(&log).as_deref(), Some("zzz"));

        let entry = search_entry_of(&browser_box);
        entry.emit_by_name::<()>("stop-search", &[]);
        assert_eq!(entry.text(), "", "Escape must clear the search text");
        // Deliver the search-changed that GTK's delay timer would (see
        // `type_search`); a duplicate of GTK's own emission is ignored.
        entry.emit_by_name::<()>("search-changed", &[]);
        pump_until(&context, deadline, || {
            last_search(&log).as_deref() == Some("")
        });
        assert_eq!(
            last_search(&log).as_deref(),
            Some(""),
            "clearing the text must clear the search filter"
        );
    }

    /// Source replacement (A → B) resets every axis and the panes agree:
    /// the panes display "All" AND the composed filter carries no stale
    /// artist/album — the issue's exact probe (issue #250).
    fn source_replacement_resets_every_filter_axis() {
        let context = session_context();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let source_a = vec![
            fixture_track("Rock", "Alpha", "Old Album", "T1"),
            fixture_track("Rock", "Alpha", "Old Album", "T2"),
        ];
        let source_b = vec![
            fixture_track("Folk", "Beta", "New Album", "U1"),
            fixture_track("Folk", "Beta", "New Album", "U2"),
        ];
        let (log, cb) = recorder();
        let (browser_box, state) = build_browser(&source_a, false, false, 48, cb);
        let panes = browser_panes(&browser_box).expect("panes");

        get_selection(&panes[1]).set_selected(1);
        assert_eq!(composed(&log).1.as_deref(), Some("Alpha"));

        reset_browser_data(&browser_box, &state, &source_b);

        assert_eq!(
            search_entry_of(&browser_box).text(),
            "",
            "the search entry must clear on source replacement"
        );
        assert_eq!(
            get_selected_label(&get_selection(&panes[1])),
            None,
            "artist pane must display All after a source replacement"
        );

        // Type after the replacement: the composed filter must carry NO
        // stale artist and the freshly typed text.
        type_search(&browser_box, "new");
        pump_until(&context, deadline, || composed(&log).4 == "new");
        let (genre, artist, album, folder, search) = composed(&log);
        assert_eq!(search, "new");
        assert_eq!(
            (genre, artist, album, folder),
            (None, None, None, None),
            "a search after a source replacement must compose from the reset axes"
        );

        // And a later album pick rides along with the surviving search.
        get_selection(&panes[2]).set_selected(1);
        let (genre, artist, album, _, search) = composed(&log);
        assert_eq!(album.as_deref(), Some("New Album"));
        assert_eq!(artist, None, "artist axis must stay reset");
        assert_eq!(genre, None);
        assert_eq!(search, "new", "search text survives a later selection");
    }

    /// A production browser arranged for the same-source refresh
    /// contracts: built on three Jazz/Alpha tracks with artist Alpha
    /// and album `Album A` selected (album rows are `All`, `Album A`,
    /// `Album B` — `All` prepended, albums sorted), and the emit log
    /// cleared so the next `composed` reflects only the refresh under
    /// test.
    struct RefreshArrangement {
        log: EmitLog,
        browser_box: gtk::Box,
        state: BrowserState,
        panes: [gtk::Box; 3],
    }

    /// Build [`RefreshArrangement`].
    fn arranged_artist_and_album_selection() -> RefreshArrangement {
        let initial = vec![
            fixture_track("Jazz", "Alpha", "Album A", "T1"),
            fixture_track("Jazz", "Alpha", "Album A", "T2"),
            fixture_track("Jazz", "Alpha", "Album B", "T3"),
        ];
        let (log, cb) = recorder();
        let (browser_box, state) = build_browser(&initial, false, false, 48, cb);
        let panes = browser_panes(&browser_box).expect("panes");
        get_selection(&panes[1]).set_selected(1);
        get_selection(&panes[2]).set_selected(1);
        log.borrow_mut().clear();
        RefreshArrangement {
            log,
            browser_box,
            state,
            panes,
        }
    }

    /// Same-source refresh: a still-valid album selection survives an
    /// upsert of a non-matching track, and the emit must still carry
    /// both axes (issue #250).
    fn refresh_preserves_matching_selection_through_upsert() {
        let arrangement = arranged_artist_and_album_selection();

        // Upsert a non-matching track: the selections must survive the
        // refresh and the emit must still carry both axes.
        let upserted = vec![
            fixture_track("Jazz", "Alpha", "Album A", "T1"),
            fixture_track("Jazz", "Alpha", "Album A", "T2"),
            fixture_track("Jazz", "Alpha", "Album B", "T3"),
            fixture_track("Jazz", "Alpha", "Album C", "T4"),
        ];
        refresh_browser_data(&arrangement.browser_box, &arrangement.state, &upserted);
        let (_, artist, album, _, _) = composed(&arrangement.log);
        assert_eq!(
            artist.as_deref(),
            Some("Alpha"),
            "refresh must preserve a valid artist"
        );
        assert_eq!(
            album.as_deref(),
            Some("Album A"),
            "refresh must preserve a still-valid album selection"
        );
        let album_store = get_store_from_pane(&arrangement.panes[2]).expect("album store");
        assert_eq!(album_store.n_items(), 4, "All + the three albums");
        assert_eq!(
            get_selected_label(&get_selection(&arrangement.panes[2])).as_deref(),
            Some("Album A"),
            "the pane must keep displaying the preserved selection"
        );
    }

    /// Same-source refresh: deleting every track of the selected album
    /// drops the album axis to All exactly as if the user had cleared
    /// it, while the still-valid artist axis and the panes' displayed
    /// selections keep agreeing (issue #250).
    fn refresh_drops_vanished_album_and_keeps_surviving_artist() {
        let arrangement = arranged_artist_and_album_selection();

        // Delete every "Album A" track: the album axis must drop to All
        // while the still-valid artist axis survives, and the panes must
        // display the agreement.
        let after_delete = vec![fixture_track("Jazz", "Alpha", "Album C", "T4")];
        refresh_browser_data(&arrangement.browser_box, &arrangement.state, &after_delete);
        let (_, artist, album, _, _) = composed(&arrangement.log);
        assert_eq!(
            album, None,
            "a vanished album must drop the axis to All (issue #250)"
        );
        assert_eq!(
            artist.as_deref(),
            Some("Alpha"),
            "a still-valid artist must survive the album's drop"
        );
        assert_eq!(
            get_selected_label(&get_selection(&arrangement.panes[2])),
            None,
            "the album pane must display All after its selection vanished"
        );
        assert_eq!(
            get_selected_label(&get_selection(&arrangement.panes[1])).as_deref(),
            Some("Alpha"),
            "the artist pane must keep displaying the surviving selection"
        );
    }

    /// A full-sync-style replacement clears every axis AND the entry,
    /// emits nothing itself (the caller splices the full set), and the
    /// next interaction composes from fully cleared axes (issue #250).
    fn full_sync_reset_clears_every_axis_and_the_entry() {
        let context = session_context();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let source_a = vec![fixture_track("Rock", "Alpha", "Old", "T1")];
        let source_b = vec![fixture_track("Folk", "Beta", "New", "U1")];
        let (log, cb) = recorder();
        let (browser_box, state) = build_browser(&source_a, false, false, 48, cb);
        let panes = browser_panes(&browser_box).expect("panes");

        get_selection(&panes[0]).set_selected(1);
        get_selection(&panes[1]).set_selected(1);
        get_selection(&panes[2]).set_selected(1);
        type_search(&browser_box, "query");
        pump_until(&context, deadline, || composed(&log).4 == "query");
        log.borrow_mut().clear();

        reset_browser_data(&browser_box, &state, &source_b);

        assert_eq!(state.search_text(), "", "shared search text must reset");
        assert_eq!(
            search_entry_of(&browser_box).text(),
            "",
            "entry widget must clear"
        );
        assert!(
            log.borrow().is_empty(),
            "reset must not emit (display_tracks splices the full set itself)"
        );
        for pane in &panes {
            assert_eq!(
                get_selected_label(&get_selection(pane)),
                None,
                "every pane must display All after a full-sync reset"
            );
        }

        get_selection(&panes[0]).set_selected(1);
        let (genre, artist, album, folder, search) = composed(&log);
        assert_eq!(
            (
                genre.as_deref(),
                artist.as_deref(),
                album.as_deref(),
                folder.as_deref(),
                search.as_str()
            ),
            (Some("Folk"), None, None, None, ""),
            "post-reset selection must compose from fully cleared axes"
        );
    }

    /// THE reported race: a pending search debounce armed before a
    /// source replacement must never fire with the pre-reset text, and
    /// the handler must keep working for later typing (issue #250).
    fn pending_search_debounce_never_fires_after_source_replacement() {
        let context = session_context();
        let source_a = vec![fixture_track("Rock", "Alpha", "Old", "T1")];
        let source_b = vec![fixture_track("Folk", "Beta", "New", "U1")];
        let (log, cb) = recorder();
        let (browser_box, state) = build_browser(&source_a, false, false, 48, cb);

        // Type and wait only until GTK's search-delay delivered the
        // text to the handler — the browser's own 100ms debounce is
        // still pending.
        type_search(&browser_box, "stale");
        pump_until(
            &context,
            std::time::Instant::now() + std::time::Duration::from_secs(3),
            || state.search_text() == "stale",
        );

        // Replace the source while the debounce timer is still pending.
        reset_browser_data(&browser_box, &state, &source_b);
        assert_eq!(state.search_text(), "");

        // Pump well past the debounce window: no timer may fire.
        pump_for(&context, std::time::Duration::from_millis(600));
        assert!(
            log.borrow().iter().all(|entry| entry.4 != "stale"),
            "a pending search debounce must die with the replaced source (issue #250)"
        );

        // The generation invalidation must not brick later typing.
        type_search(&browser_box, "fresh");
        pump_until(
            &context,
            std::time::Instant::now() + std::time::Duration::from_secs(3),
            || last_search(&log).as_deref() == Some("fresh"),
        );
        assert_eq!(composed(&log).4, "fresh");
        assert_eq!(
            log.borrow().last().unwrap().1,
            None,
            "post-reset search must compose from the reset axes"
        );
    }

    /// Unique scratch tree for the folder-navigation contracts: two real
    /// browsable roots (`ga`, `gb`), each holding `sub/01.flac`, so the
    /// production `from_configured`/`place_tracks` pipeline sees two
    /// available roots with a navigable child directory. Scratch lives
    /// under `${TMPDIR:-/var/tmp}` (never /tmp) and is removed on drop.
    struct FolderScratch {
        base: std::path::PathBuf,
    }

    impl FolderScratch {
        fn new(tag: &str) -> Self {
            let base = Self::base_dir(tag);
            for root_name in ["ga", "gb"] {
                let dir = base.join(root_name).join("sub");
                std::fs::create_dir_all(&dir).expect("create scratch root dir");
                std::fs::write(dir.join("01.flac"), b"").expect("create scratch track file");
            }
            Self { base }
        }

        /// Scratch tree for the activation contracts (issue #251): ONE
        /// root (`sole`), a directory literally named `…` holding
        /// `deep/01.flac`, and a sibling leaf `leafonly/01.flac`.
        /// `deep` itself is an empty leaf — a track directly inside and
        /// no subdirectories — so navigating into it leaves the Up row
        /// as the pane's ONLY row. The pane's case-insensitive lowercase
        /// byte sort puts `leafonly` (ASCII 'l') before `…` (U+2026,
        /// lead byte 0xE2), so the display layout is deterministic:
        /// [Up, leafonly, …].
        fn new_ellipsis_tree(tag: &str) -> Self {
            let base = Self::base_dir(tag);
            let deep = base.join("sole").join("…").join("deep");
            std::fs::create_dir_all(&deep).expect("create ellipsis scratch tree");
            std::fs::write(deep.join("01.flac"), b"").expect("create deep track file");
            let leaf = base.join("sole").join("leafonly");
            std::fs::create_dir_all(&leaf).expect("create leafonly scratch dir");
            std::fs::write(leaf.join("01.flac"), b"").expect("create leafonly track file");
            Self { base }
        }

        fn base_dir(tag: &str) -> std::path::PathBuf {
            std::env::var_os("TMPDIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("/var/tmp"))
                .join(format!("tr-2xstt-folder-{tag}-{}", std::process::id()))
        }

        fn root_path(&self, name: &str) -> std::path::PathBuf {
            self.base.join(name)
        }
    }

    impl Drop for FolderScratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// One source track rooted in the scratch tree: genre/artist/album
    /// are grouping fixtures; the URI points at the real scratch file so
    /// the production placement pipeline can bucket it.
    fn folder_scratch_track(scratch: &FolderScratch, root: &str, title: &str) -> TrackObject {
        let uri = url::Url::from_file_path(scratch.root_path(root).join("sub/01.flac"))
            .expect("valid file uri")
            .to_string();
        TrackObject::new(
            1, title, 60, "Alpha", "Album A", "Jazz", "", 0, "", 0, 0, 0, "flac", &uri,
        )
    }

    /// A source track whose file sits at `rel` beneath the scratch base —
    /// the nesting depth the folder tree derives its levels from.
    fn folder_scratch_track_at(scratch: &FolderScratch, rel: &str, title: &str) -> TrackObject {
        let uri = url::Url::from_file_path(scratch.root_path(rel))
            .expect("valid file uri")
            .to_string();
        TrackObject::new(
            1, title, 60, "Alpha", "Album A", "Jazz", "", 0, "", 0, 0, 0, "flac", &uri,
        )
    }

    /// Attach the production folder model the way `display_local_tracks`
    /// does: two real configured roots, real track placement, a real
    /// `FolderBrowser`.
    fn attach_two_root_folder_model(state: &BrowserState, scratch: &FolderScratch) {
        let roots = vec![
            crate::ui::folder_browser::BrowsableRoot::from_configured(
                scratch.root_path("ga").to_str().expect("utf8 root path"),
                None,
            ),
            crate::ui::folder_browser::BrowsableRoot::from_configured(
                scratch.root_path("gb").to_str().expect("utf8 root path"),
                None,
            ),
        ];
        let inputs = vec![
            crate::ui::folder_browser::TrackPathInput {
                source_label: "local".to_string(),
                path: Some(scratch.root_path("ga").join("sub/01.flac")),
            },
            crate::ui::folder_browser::TrackPathInput {
                source_label: "local".to_string(),
                path: Some(scratch.root_path("gb").join("sub/01.flac")),
            },
        ];
        let (placed, _report) = crate::ui::folder_browser::place_tracks(&roots, &inputs);
        attach_folder_model(
            state,
            crate::ui::folder_browser::FolderBrowser::new(roots, placed),
        );
    }

    /// Attach the single-root scratch tree with the `…`-named directory
    /// and the `leafonly` leaf, the layout the activation contracts
    /// navigate (issue #251).
    fn attach_sole_root_ellipsis_model(state: &BrowserState, scratch: &FolderScratch) {
        let roots = vec![crate::ui::folder_browser::BrowsableRoot::from_configured(
            scratch.root_path("sole").to_str().expect("utf8 root path"),
            None,
        )];
        let inputs = vec![
            crate::ui::folder_browser::TrackPathInput {
                source_label: "local".to_string(),
                path: Some(scratch.root_path("sole").join("…").join("deep/01.flac")),
            },
            crate::ui::folder_browser::TrackPathInput {
                source_label: "local".to_string(),
                path: Some(scratch.root_path("sole").join("leafonly/01.flac")),
            },
        ];
        let (placed, _report) = crate::ui::folder_browser::place_tracks(&roots, &inputs);
        attach_folder_model(
            state,
            crate::ui::folder_browser::FolderBrowser::new(roots, placed),
        );
    }

    /// Drive the production `ListView::activate` signal — the exact path
    /// a pointer double-click and a keyboard Enter both emit — rather
    /// than poking navigation state directly. This is the parity proof:
    /// there is exactly one activation entry point and both input
    /// methods land on it.
    fn emit_folder_activation(list_view: &gtk::ListView, position: u32) {
        list_view.emit_by_name::<()>("activate", &[&position]);
    }

    /// The folder pane's store, asserting row `position` is the typed
    /// row `kind` (and optionally the label) — the typed-identity
    /// backbone navigation keys off (issue #251).
    fn assert_folder_row(
        folder_pane: &gtk::Box,
        position: u32,
        kind: FolderRowKind,
        label: Option<&str>,
    ) {
        let item = get_store_from_pane(folder_pane)
            .and_then(|store| store.item(position))
            .and_downcast::<BrowserItem>()
            .unwrap_or_else(|| panic!("folder row {position} must exist"));
        assert_eq!(
            item.folder_kind(),
            kind,
            "folder row {position} must be a {kind:?} row"
        );
        if let Some(expected) = label {
            assert_eq!(item.label(), expected, "folder row {position} label");
        }
    }

    /// Navigate into the second root through the production activation
    /// handler (the second root row sits at position 1 after the attach
    /// reset) and assert the navigation took hold.
    fn navigate_into_second_root(state: &BrowserState, folder_pane: &gtk::Box) {
        let list_view = pane_list_view(folder_pane).expect("folder pane ListView");
        emit_folder_activation(&list_view, 1);
        assert!(
            matches!(
                &*state.folder_location.borrow(),
                FolderLocation::Inside { .. }
            ),
            "precondition: navigation must be inside a root before the reset"
        );
        assert!(
            state.folder_prefix.borrow().is_some(),
            "precondition: navigation must apply a folder prefix before the reset"
        );
        assert_eq!(
            get_store_from_pane(folder_pane).map(|store| store.n_items()),
            Some(2),
            "precondition: the pane must display the inside-directory rows (up + sub)"
        );
    }

    /// After the reset, the folder pane must display the roots level:
    /// navigation state, prefix, store rows, and selection all agree
    /// with the cleared composed filter.
    fn assert_folder_pane_reset_to_roots(
        state: &BrowserState,
        folder_pane: &gtk::Box,
        folder_sel: &gtk::SingleSelection,
    ) {
        assert!(
            matches!(&*state.folder_location.borrow(), FolderLocation::Roots),
            "folder navigation must return to the roots level on source replacement"
        );
        assert!(
            state.folder_prefix.borrow().is_none(),
            "the folder prefix axis must stay cleared after the reset"
        );
        assert_eq!(
            get_store_from_pane(folder_pane).map(|store| store.n_items()),
            Some(2),
            "the folder pane must display the two roots again, not the stale directory"
        );
        assert_eq!(
            get_store_from_pane(folder_pane)
                .and_then(|store| store.item(0))
                .and_downcast::<BrowserItem>()
                .map(|item| item.label()),
            Some("ga".to_string()),
            "the folder pane's first row must be the first root, not the stale up-row"
        );
        assert_eq!(
            folder_sel
                .selected_item()
                .and_downcast::<BrowserItem>()
                .map(|item| item.label()),
            Some("ga".to_string()),
            "the folder selection must sit on the first root, consistent with the cleared filter"
        );
    }

    /// Evaluated agreement: a post-reset selection composes folder=None —
    /// the displayed roots and the composed filter say the same thing.
    fn assert_post_reset_composition_agrees(panes: &[gtk::Box], log: &EmitLog) {
        get_selection(&panes[0]).set_selected(1);
        let (genre, artist, album, folder, search) = composed(log);
        assert_eq!(
            (
                genre.as_deref(),
                artist.as_deref(),
                album.as_deref(),
                folder.as_deref(),
                search.as_str()
            ),
            (Some("Folk"), None, None, None, ""),
            "post-reset interaction must compose from fully cleared axes including folder"
        );
    }

    /// Source replacement must reset the folder pane along with every
    /// other axis: `folder_location` returns to the roots level and the
    /// folder store/selection display the roots — not the stale
    /// inside-directory rows — so the pane agrees with the now-empty
    /// composed filter (issue #250 rework finding: clearing
    /// `folder_prefix` alone left the pane showing the pre-reset
    /// directory, the very display/filter disagreement the reset path
    /// exists to prevent).
    fn source_replacement_resets_folder_navigation() {
        let scratch = FolderScratch::new("reset");
        let source_a = vec![
            folder_scratch_track(&scratch, "ga", "T1"),
            folder_scratch_track(&scratch, "gb", "T2"),
        ];
        let (log, cb) = recorder();
        let (browser_box, state) = build_browser(&source_a, false, false, 48, cb);
        let panes = collect_browser_panes(&browser_box);
        let folder_pane = &panes[3];

        attach_two_root_folder_model(&state, &scratch);

        // Navigate into the second root through the production
        // activation handler, then clear the log so only post-reset
        // emits are observed.
        let folder_sel = get_selection(folder_pane);
        navigate_into_second_root(&state, folder_pane);
        log.borrow_mut().clear();

        // Source replacement: every axis AND the folder pane must reset.
        let source_b = vec![fixture_track("Folk", "Beta", "New", "U1")];
        reset_browser_data(&browser_box, &state, &source_b);

        assert!(
            log.borrow().is_empty(),
            "the reset itself must not emit (the caller splices the full set)"
        );
        assert_folder_pane_reset_to_roots(&state, folder_pane, &folder_sel);
        assert_post_reset_composition_agrees(&panes, &log);
    }

    /// A fresh browser over the sole-root `…` scratch tree, attached and
    /// showing the roots level: the fixture every folder-activation
    /// contract drives (issue #251). `scratch` is held to the end of the
    /// fixture's life so the URIs the model was built from stay backed by
    /// real files for the whole test; distinct `tag`s keep parallel
    /// scratch trees apart within one process.
    struct FolderActivationFixture {
        browser_box: gtk::Box,
        state: BrowserState,
        panes: Vec<gtk::Box>,
        source: Vec<TrackObject>,
        log: EmitLog,
        _scratch: FolderScratch,
    }

    impl FolderActivationFixture {
        fn new(tag: &str) -> Self {
            let scratch = FolderScratch::new_ellipsis_tree(tag);
            let source = vec![
                folder_scratch_track_at(&scratch, "sole/…/deep/01.flac", "T1"),
                folder_scratch_track_at(&scratch, "sole/leafonly/01.flac", "T2"),
            ];
            let (log, cb) = recorder();
            let (browser_box, state) = build_browser(&source, false, false, 48, cb);
            let panes = collect_browser_panes(&browser_box);
            attach_sole_root_ellipsis_model(&state, &scratch);
            Self {
                browser_box,
                state,
                panes,
                source,
                log,
                _scratch: scratch,
            }
        }
    }

    /// The first reported bug (issue #251): the sole root is
    /// auto-selected, so a selection-changed listener never fires when
    /// the user activates it again — activating the ALREADY-SELECTED row
    /// must still navigate into the root through the production
    /// `ListView::activate` signal, emitting the root's folder prefix.
    fn sole_root_activation_navigates_when_already_selected() {
        let fx = FolderActivationFixture::new("activate-sole-root");
        let folder_pane = &fx.panes[3];
        let list_view = pane_list_view(folder_pane).expect("folder pane ListView");
        let folder_sel = get_selection(folder_pane);

        // Roots level: one typed Root row, auto-selected — activating
        // the ALREADY-SELECTED row must navigate (the reported bug).
        assert_folder_row(folder_pane, 0, FolderRowKind::Root, Some("sole"));
        assert!(
            matches!(&*fx.state.folder_location.borrow(), FolderLocation::Roots),
            "precondition: navigation must start at the roots level"
        );
        fx.log.borrow_mut().clear();
        emit_folder_activation(&list_view, 0);
        assert_eq!(
            composed(&fx.log).3,
            fx.state.folder_prefix.borrow().clone(),
            "activation inside the sole root must emit with the folder prefix"
        );
        assert!(
            fx.state.folder_prefix.borrow().is_some(),
            "activating the sole root must apply the root's folder prefix"
        );
        assert_eq!(
            folder_sel.selected(),
            0,
            "after navigation the selection must sit on the pane's first row"
        );
    }

    /// A genuine directory named `…` and the Up row share a label but
    /// not a typed identity: the directory row must DESCEND on
    /// activation while the same-labelled Up row ASCENDS — the old
    /// label-comparing code could not tell them apart (issue #251).
    fn ellipsis_labelled_rows_keep_typed_identities() {
        let fx = FolderActivationFixture::new("activate-ellipsis");
        let folder_pane = &fx.panes[3];
        let list_view = pane_list_view(folder_pane).expect("folder pane ListView");

        // Into the sole root: [Up("…"), Directory("leafonly"),
        // Directory("…")]. Two rows share the label `…` with DIFFERENT
        // typed identities — the Up row at position 0 and the genuine
        // directory at the end.
        emit_folder_activation(&list_view, 0);
        assert_folder_row(folder_pane, 0, FolderRowKind::Up, Some("…"));
        assert_folder_row(folder_pane, 1, FolderRowKind::Directory, Some("leafonly"));
        assert_folder_row(folder_pane, 2, FolderRowKind::Directory, Some("…"));

        // Activating the `…` DIRECTORY must descend, not ascend: the
        // label-based code compared labels and treated it as Up.
        emit_folder_activation(&list_view, 2);
        let inside_dir = match &*fx.state.folder_location.borrow() {
            FolderLocation::Inside { dir, .. } => Some(dir.clone()),
            FolderLocation::Roots => None,
        };
        assert_eq!(
            inside_dir.as_deref(),
            Some("…"),
            "activating the directory named `…` must descend into it, not go up"
        );
        assert_folder_row(folder_pane, 1, FolderRowKind::Directory, Some("deep"));

        // The same-labelled Up row at position 0 must ASCEND one level —
        // back to the root's top ([Up("…"), leafonly, …]), the same
        // ladder the repeated-Up contract climbs — never a
        // label-interpreted descent that would stay inside `…`.
        emit_folder_activation(&list_view, 0);
        let ascended = match &*fx.state.folder_location.borrow() {
            FolderLocation::Inside { dir, .. } => Some(dir.clone()),
            FolderLocation::Roots => None,
        };
        assert_eq!(
            ascended.as_deref(),
            Some(""),
            "the same-labelled Up row must ascend to the root's top level"
        );
        assert_folder_row(folder_pane, 1, FolderRowKind::Directory, Some("leafonly"));
    }

    /// Inside an empty leaf the folder pane's ONLY row is the
    /// auto-selected Up row — and activating that ALREADY-SELECTED row
    /// must still navigate back up (the second reported bug:
    /// selection-changed never fires for a row the model already
    /// selected) (issue #251).
    fn empty_leaf_up_row_is_only_row_and_still_navigates() {
        let fx = FolderActivationFixture::new("activate-empty-leaf");
        let folder_pane = &fx.panes[3];
        let list_view = pane_list_view(folder_pane).expect("folder pane ListView");

        // Descend sole → `…` → deep, an empty leaf: a track directly
        // inside and no subdirectories.
        emit_folder_activation(&list_view, 0);
        emit_folder_activation(&list_view, 2);
        emit_folder_activation(&list_view, 1);
        let inside_dir = match &*fx.state.folder_location.borrow() {
            FolderLocation::Inside { dir, .. } => Some(dir.clone()),
            FolderLocation::Roots => None,
        };
        assert_eq!(inside_dir.as_deref(), Some("…/deep"));
        assert_eq!(
            get_store_from_pane(folder_pane).map(|store| store.n_items()),
            Some(1),
            "an empty leaf must display exactly one row: Up"
        );

        // The empty-leaf Up row is auto-selected at position 0 —
        // activating the ALREADY-SELECTED row must still ascend (the
        // other reported bug).
        emit_folder_activation(&list_view, 0);
        let reached = match &*fx.state.folder_location.borrow() {
            FolderLocation::Inside { dir, .. } => Some(dir.clone()),
            FolderLocation::Roots => None,
        };
        assert_eq!(
            reached.as_deref(),
            Some("…"),
            "the empty leaf's auto-selected Up row must still navigate back up"
        );
    }

    /// Three consecutive activations of the already-selected Up row must
    /// climb three levels — deep → `…` → the root — back to the roots
    /// level, each emitting the recomposed filter (issue #251).
    fn repeated_up_activations_climb_three_levels_to_roots() {
        let fx = FolderActivationFixture::new("activate-repeated-up");
        let folder_pane = &fx.panes[3];
        let list_view = pane_list_view(folder_pane).expect("folder pane ListView");

        // Descend sole → `…` → deep so three Up activations are needed.
        emit_folder_activation(&list_view, 0);
        emit_folder_activation(&list_view, 2);
        emit_folder_activation(&list_view, 1);

        // The empty-leaf Up row is auto-selected at position 0 —
        // activating the ALREADY-SELECTED row must still ascend (the
        // other reported bug). Three consecutive activations of the
        // auto-selected row 0 climb deep → `…` → root → roots.
        for expected in ["…", "", "ROOTS"] {
            fx.log.borrow_mut().clear();
            emit_folder_activation(&list_view, 0);
            if expected == "ROOTS" {
                assert!(
                    matches!(&*fx.state.folder_location.borrow(), FolderLocation::Roots),
                    "third Up activation must reach the roots level"
                );
            } else {
                let reached = match &*fx.state.folder_location.borrow() {
                    FolderLocation::Inside { dir, .. } => Some(dir.clone()),
                    FolderLocation::Roots => None,
                };
                assert_eq!(
                    reached.as_deref(),
                    Some(expected),
                    "Up activation must climb one level"
                );
            }
            assert!(
                !fx.log.borrow().is_empty(),
                "every Up activation must emit the recomposed filter"
            );
        }
        assert!(
            fx.state.folder_prefix.borrow().is_none(),
            "back at the roots level the folder prefix must be cleared"
        );
        assert_folder_row(folder_pane, 0, FolderRowKind::Root, Some("sole"));
    }

    /// Status rows — the detached-model notice and the empty-roots
    /// notice — must never navigate and never emit, however often they
    /// are activated (issue #251).
    fn folder_status_rows_never_navigate() {
        // Detached model: the pane shows its informational notice row.
        let (log, cb) = recorder();
        let (browser_box, state) = build_browser(&[], false, false, 48, cb);
        let panes = collect_browser_panes(&browser_box);
        let folder_pane = &panes[3];
        let list_view = pane_list_view(folder_pane).expect("folder pane ListView");
        assert_folder_row(
            folder_pane,
            0,
            FolderRowKind::Status,
            Some("Folder browsing follows the local library sources"),
        );
        emit_folder_activation(&list_view, 0);
        assert!(
            matches!(&*state.folder_location.borrow(), FolderLocation::Roots),
            "activating the detached notice row must not navigate"
        );
        assert!(state.folder_prefix.borrow().is_none());
        assert!(log.borrow().is_empty(), "a status activation must not emit");

        // An attached model with zero roots: same contract.
        attach_folder_model(
            &state,
            crate::ui::folder_browser::FolderBrowser::new(vec![], vec![]),
        );
        assert_folder_row(
            folder_pane,
            0,
            FolderRowKind::Status,
            Some("No library folders configured"),
        );
        emit_folder_activation(&list_view, 0);
        assert!(
            matches!(&*state.folder_location.borrow(), FolderLocation::Roots),
            "activating the empty-roots notice row must not navigate"
        );
        assert!(log.borrow().is_empty(), "a status activation must not emit");

        // Regression (issue #251 self-review): the Cell<u8> default a
        // BrowserItem carries when never given a folder kind must decode
        // to Status — the never-navigable identity — so a stray
        // untyped row can never drive folder navigation.
        assert_eq!(
            BrowserItem::new("plain untyped row", 0).folder_kind(),
            FolderRowKind::Status,
            "an untyped BrowserItem must decode as Status, never as a navigable kind"
        );
    }

    /// After a same-source refresh the folder location, prefix, and the
    /// pane's displayed level and rows must all have survived untouched
    /// (issue #251).
    fn assert_refresh_preserves_folder_axis(fx: &FolderActivationFixture, folder_pane: &gtk::Box) {
        let inside_dir = match &*fx.state.folder_location.borrow() {
            FolderLocation::Inside { dir, .. } => Some(dir.clone()),
            FolderLocation::Roots => None,
        };
        assert_eq!(
            inside_dir.as_deref(),
            Some(""),
            "a same-source refresh must preserve the folder location"
        );
        assert!(
            fx.state.folder_prefix.borrow().is_some(),
            "a same-source refresh must preserve the folder prefix"
        );
        assert_eq!(
            get_store_from_pane(folder_pane).map(|store| store.n_items()),
            Some(3),
            "the refresh must not disturb the folder pane's displayed level"
        );
        assert_folder_row(folder_pane, 0, FolderRowKind::Up, Some("…"));
    }

    /// A same-source refresh must preserve the folder axis (location and
    /// pane rows) and leave activation driving navigation afterwards —
    /// the refresh path must not strand the activation handler (issue
    /// #251; the lifecycle groundwork for R6).
    fn folder_navigation_survives_same_source_refresh() {
        let fx = FolderActivationFixture::new("refresh");
        let folder_pane = &fx.panes[3];
        let list_view = pane_list_view(folder_pane).expect("folder pane ListView");

        emit_folder_activation(&list_view, 0);
        assert!(fx.state.folder_prefix.borrow().is_some());
        assert_eq!(
            get_store_from_pane(folder_pane).map(|store| store.n_items()),
            Some(3),
            "precondition: inside the root the pane shows Up + the two directories"
        );

        // Same-source refresh: the folder axis must survive untouched.
        refresh_browser_data(&fx.browser_box, &fx.state, &fx.source);
        assert_refresh_preserves_folder_axis(&fx, folder_pane);

        // Activation must still navigate after the refresh.
        fx.log.borrow_mut().clear();
        emit_folder_activation(&list_view, 0);
        assert!(
            matches!(&*fx.state.folder_location.borrow(), FolderLocation::Roots),
            "activating Up after a refresh must return to the roots level"
        );
        assert!(
            fx.state.folder_prefix.borrow().is_none(),
            "returning to the roots must clear the folder prefix"
        );
        assert!(
            !fx.log.borrow().is_empty(),
            "post-refresh activation must emit the recomposed filter"
        );
    }

    /// The crate's single consolidated GTK widget test, all run on the ONE
    /// thread that owns the GTK session:
    ///
    /// - the combined browser-row label ("Label, (Count)") must be exposed
    ///   on the `GtkListItem` — the list-row boundary — not on an inner
    ///   widget; the child labels must be presentational so the row is
    ///   announced as a single utterance ("Artist Name, (123)"); zero-count
    ///   rows must announce only the label; unbind must reset everything;
    /// - the context-menu popover must attach a visible scrolling child
    ///   with one button per enabled action
    ///   ([`crate::ui::context_menu::tests::popover_from_menu_model_attaches_a_visible_child_widget`]);
    /// - browser gutter separators must join visible panes around hidden
    ///   ones — exactly one surviving gutter between panes that straddle
    ///   a disabled pane, no dangling edge gutters
    ///   ([`crate::ui::preferences::widget_tests::separator_gutters_join_visible_panes_around_hidden_ones`]);
    /// - browser data lifecycle: album selection must survive search
    ///   typing and clearing, source replacement must reset every axis
    ///   so panes and composed filter agree, same-source refresh must
    ///   preserve still-valid selections and drop vanished axes, a
    ///   pending search debounce must die with a replaced source, and
    ///   source replacement must reset the folder pane to its roots so
    ///   the displayed directory agrees with the cleared folder filter
    ///   (issue #250);
    /// - folder rows must navigate on row ACTIVATION — never on
    ///   selection changes — through the production `ListView::activate`
    ///   signal: the already-selected sole root re-navigates, a genuine
    ///   directory named `…` descends while the same-labelled Up row
    ///   ascends (typed identities, not label comparison), an empty
    ///   leaf's auto-selected Up row still climbs out, status rows never
    ///   navigate or emit, and a same-source refresh preserves the
    ///   folder axis with activation still driving it (issue #251);
    /// - tracklist drags must start only from the data row area (folded
    ///   into the popover contract);
    /// - a FullSync publication must replace the visible track store, the
    ///   master rows, the browser snapshot, and repopulate the genre panes;
    ///   when `TRIBUTARY_Q4_UI_BENCH_TRACKS` is set it additionally times
    ///   the publication/rebuild endpoints at that scale
    ///   ([`Self::q4_publication_contract_and_bench`], tr-am6qr);
    /// - the per-row playlist drop target must resolve the row under the
    ///   pointer, drive the production `connect_accept`/`connect_drop`
    ///   handlers, and forward the exact displayed candidate order
    ///   ([`crate::ui::context_menu::tests::per_row_playlist_drop_target_drives_the_production_drop_path`]);
    /// - the keyboard "Add to Playlist" action must carry the identical
    ///   displayed-order candidates as the drag payload and refuse the same
    ///   destinations
    ///   ([`crate::ui::context_menu::tests::keyboard_add_action_matches_the_drag_payload_contract`]).
    ///
    /// This is deliberately the only GTK-initializing `#[test]` in the
    /// crate: the `ui::widget_test_session` mutex serializes but does not
    /// give thread affinity, so a second GTK-touching `#[test]` would
    /// construct widgets off the initializing thread and trip gtk-rs
    /// main-thread checks (2026-09-09 review rejection, PR #179).
    ///
    /// Asserts on the `GtkListItem:accessible-label` property (GTK 4.12),
    /// which GTK uses as the row's accessible name — the widget-level
    /// equivalent of an Orca row-announcement smoke test.
    ///
    /// Q1 gate (`tr-sptyt`, issue #274): the CI `gtk-display-gate` job runs
    /// this test under Xvfb with `TRIBUTARY_GTK_GATE=require`, so a display
    /// that never comes up — or a future refactor that lets this body skip —
    /// fails the job instead of passing vacuously. The corrected R3/R4
    /// selection-restoration, per-row drag/drop destination, settings and
    /// close contracts join THIS body as their implementations land
    /// (`tr-2xstt`, `tr-y72e3`, `tr-hdgwh`); they must not become competing
    /// GTK `#[test]`s.
    #[test]
    fn gtk_widget_contracts_hold_on_one_session() {
        // Run every widget construction and assertion below — including
        // the context-menu and preferences helpers — inside the
        // process-wide GTK test session (display gate + single `gtk::init`
        // + serialization lock + dedicated thread-default main context):
        // GTK requires single-threaded use after initialization. See
        // `ui::widget_test_session`.
        let Some(()) = crate::ui::widget_test_session::with_session(
            "gtk widget contracts test",
            || {
                // Fail-on-skip proof: with the session live GTK must be
                // initialized and the gate must have recorded the
                // establishment. Under `TRIBUTARY_GTK_GATE=require` (the CI
                // display gate) `with_session` would already have panicked
                // instead of returning `None`, so reaching this body means
                // the contracts below really ran.
                assert!(
                    crate::ui::widget_test_session::was_established(),
                    "the widget session must record an established session before contracts run"
                );
                assert!(
                    gtk::is_initialized(),
                    "the widget session must have initialized GTK before contracts run"
                );

                let (list_item, row) = make_setup_list_item();
                assert_row_roles_presentational(&row);
                assert_combined_bind_contract(&list_item, &row);
                assert_unbind_reset(&list_item, &row);
                assert_zero_count_contract(&list_item, &row);
                assert_unbind_reset(&list_item, &row);

                crate::ui::context_menu::tests::popover_from_menu_model_attaches_a_visible_child_widget();
                crate::ui::context_menu::tests::per_row_playlist_drop_target_drives_the_production_drop_path();
                crate::ui::context_menu::tests::keyboard_add_action_matches_the_drag_payload_contract();
                crate::ui::preferences::widget_tests::separator_gutters_join_visible_panes_around_hidden_ones();
                crate::ui::preferences::widget_tests::column_state_is_keyed_by_id_under_a_non_english_locale();
                crate::ui::preferences::widget_tests::radio_columns_keep_their_ids();
                crate::ui::lastfm_settings::widget_tests::render_shows_only_the_offered_actions();
                crate::ui::confirm_dialog::widget_tests::nothing_is_removed_until_the_destructive_response();
                crate::ui::album_art_cell::widget_tests::show_placeholder_keeps_the_missing_art_visible();
                crate::ui::album_art_cell::widget_tests::revoking_a_cell_revokes_its_outstanding_fetch_token();
                crate::ui::album_pane_art::widget_tests::album_art_row_publishes_combined_accessible_name();
                crate::ui::album_pane_art::widget_tests::album_art_row_zero_count_announces_bare_label();
                crate::ui::discovery_handler::widget_tests::discovered_rows_are_keyed_by_identity_not_display_name();
                crate::ui::discovery_handler::widget_tests::republication_refreshes_the_row_at_its_endpoint();
                crate::ui::discovery_handler::widget_tests::airplay_loss_removes_only_that_receivers_row();
                crate::ui::discovery_handler::widget_tests::airplay_rows_are_hidden_without_a_sender();
                crate::ui::equalizer_panel::widget_tests::equalizer_panel_edits_report_consistent_settings();
                crate::ui::equalizer_panel::widget_tests::equalizer_panel_is_disabled_for_unsupported_outputs();
                crate::ui::header_bar::widget_tests::play_button_tooltip_follows_state();
                q4_publication_contract_and_bench();
                factory_swap_preserves_album_filters_and_selection();
                rebuild_bumps_album_art_content_generation();
                album_artwork_disabled_keeps_browser_row_contract();

                // Browser data lifecycle contracts (issue #250).
                album_selection_survives_typing_and_clearing();
                escape_clears_the_search();
                source_replacement_resets_every_filter_axis();
                refresh_preserves_matching_selection_through_upsert();
                refresh_drops_vanished_album_and_keeps_surviving_artist();
                full_sync_reset_clears_every_axis_and_the_entry();
                pending_search_debounce_never_fires_after_source_replacement();
                source_replacement_resets_folder_navigation();

                // Folder activation contracts (issue #251). The
                // R4 activation contract is split into focused
                // tests (Codacy method-size rework round); every
                // split form still runs in this consolidated
                // session block.
                sole_root_activation_navigates_when_already_selected();
                ellipsis_labelled_rows_keep_typed_identities();
                empty_leaf_up_row_is_only_row_and_still_navigates();
                repeated_up_activations_climb_three_levels_to_roots();
                folder_status_rows_never_navigate();
                folder_navigation_survives_same_source_refresh();
            },
        ) else {
            return;
        };
    }
}
