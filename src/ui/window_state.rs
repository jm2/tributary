//! Shared UI state passed across window sub-modules.
//!
//! [`WindowState`] bundles the `Rc`/`RefCell`-wrapped state that the
//! main window owns and that extracted modules need read/write access
//! to.  It is constructed once in [`super::window::build_window`] and
//! passed by reference to each sub-module's setup function.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::local::engine::LibraryEvent;

use super::browser::BrowserState;
use super::objects::TrackObject;
use super::preferences;

/// Shared UI state for the main window.
///
/// This is a plain data struct — it carries no business logic of its
/// own.  Each field is documented with the module(s) that depend on it
/// so future maintainers (human or AI) can trace data flow easily.
pub struct WindowState {
    // ── Top-level GTK handles ───────────────────────────────────────
    /// The main application window.
    /// Used by: source_connect (auth dialogs), playlist_actions (dialogs).
    pub window: adw::ApplicationWindow,

    /// Tokio runtime handle for spawning async background work.
    /// Used by: discovery_handler, source_connect, playlist_actions, context_menu.
    pub rt_handle: tokio::runtime::Handle,

    /// Channel to send events to the library engine.
    /// Used by: source_connect (RemoteSync after auth).
    pub engine_tx: async_channel::Sender<LibraryEvent>,

    // ── Track data ──────────────────────────────────────────────────
    /// Backing store for the tracklist `ColumnView`.
    /// Used by: discovery_handler, source_connect, context_menu, window (library events).
    pub track_store: gtk::gio::ListStore,

    /// The current "master" track list — unfiltered snapshot of the
    /// active source, kept in sync with `track_store`.
    /// Used by: discovery_handler, source_connect, context_menu, window.
    pub master_tracks: Rc<RefCell<Vec<TrackObject>>>,

    /// Per-source track cache.  Key: `"local"` for local filesystem,
    /// server URL for remote, `"playlist:<id>"` for playlists,
    /// backend type string for radio.
    /// Used by: discovery_handler, source_connect, context_menu, window.
    pub source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,

    /// Key identifying the currently active source in `source_tracks`.
    /// Used by: discovery_handler, source_connect, context_menu, window.
    pub active_source_key: Rc<RefCell<String>>,

    /// Index of the currently-playing track in the sorted model.
    /// `None` when nothing is playing.
    /// Used by: source_connect, window (playback wiring).
    pub current_pos: Rc<Cell<Option<u32>>>,

    // ── Sidebar ─────────────────────────────────────────────────────
    /// Sidebar backing store (list of `SourceObject`s with headers).
    /// Used by: discovery_handler, source_connect, playlist_actions, context_menu, window.
    pub sidebar_store: gtk::gio::ListStore,

    /// Sidebar selection model.
    /// Used by: discovery_handler, source_connect, window.
    pub sidebar_selection: gtk::SingleSelection,

    // ── Browser + tracklist widgets ─────────────────────────────────
    /// The 3-pane genre/artist/album browser widget.
    /// Used by: discovery_handler, source_connect, context_menu, window.
    pub browser_widget: gtk::Box,

    /// Opaque browser state for rebuilding pane data.
    /// Used by: discovery_handler, source_connect, context_menu, window.
    pub browser_state: BrowserState,

    /// The "N tracks — HH:MM total" status label under the tracklist.
    /// Used by: discovery_handler, source_connect, context_menu, window.
    pub status_label: gtk::Label,

    /// The tracklist `ColumnView`.
    /// Used by: discovery_handler, source_connect, context_menu, window.
    pub column_view: gtk::ColumnView,

    /// Sorted model wrapping `track_store`, used for playback indexing.
    /// Used by: context_menu, window (playback wiring).
    pub sort_model: gtk::SortListModel,

    // ── Preferences ─────────────────────────────────────────────────
    /// Application configuration (column visibility, browser views, etc.).
    /// Used by: source_connect, window (preferences dialog).
    pub app_config: Rc<RefCell<preferences::AppConfig>>,

    // ── Connection guard ────────────────────────────────────────────
    /// URL of the server currently being connected to, if any.
    /// Prevents duplicate connection attempts.
    /// Used by: source_connect, window (library events).
    pub pending_connection: Rc<RefCell<Option<String>>>,

    /// Sidebar position to revert to if a connection attempt fails.
    /// Used by: source_connect.
    pub pre_connect_selection: Rc<Cell<u32>>,
}
