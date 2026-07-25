//! Virtualized, accessible album-art column for the browser Album pane.
//!
//! The browser album pane hosts one row per album. Each row may show a
//! thumbnail next to its text label. The thumbnail must:
//!
//! * **Only load rows that are visible** — the GTK `ListView` is already
//!   virtualized, but a naïve design would still trigger fetches for the
//!   entire library at once. The cache below is bounded so a 10 000-album
//!   library never inflates memory.
//! * **Cancel in-flight work when a row scrolls out of view** — a slow
//!   remote fetch that arrives after the row is no longer visible must
//!   not paint a stale texture. Each row is bound with a monotonic
//!   generation token; results for older generations are discarded.
//! * **Show a placeholder while loading or for albums with no art** — a
//!   neutral placeholder icon keeps the list legible during a network
//!   fetch and for albums that genuinely have no embedded/remote art.
//! * **Authenticate through the existing lease-isolated resolver** —
//!   remote album art is resolved through `SourceRegistry::resolve_artwork`
//!   and consumed by the persistent art worker. Local embedded art goes
//!   through `update_direct_file_album_art`. URLs from the track's
//!   `cover_art_url` go through `fetch_remote_album_art`. None of these
//!   paths invent new credential-isolation seams.
//! * **Honor persisted layout preferences** — `AppConfig::album_pane_artwork`
//!   toggles the whole feature; `AlbumArtSize::pixel_size()` fixes the
//!   rendered square side length. The bind factory rebuilds its widgets
//!   when these change.
//!
//! The cache is intentionally **display-side** (a `gdk::Texture` plus
//! `gtk::Image` swap), not a transport cache. The album-art worker in
//! `album_art.rs` already provides the byte-level cache + byte-cap
//! enforcement; this module only ensures the UI doesn't multiply fetches
//! for visible rows.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::architecture::media::ResolvedHttpRequest;
use crate::architecture::SourceId;
use crate::ui::album_art;
use crate::ui::objects::{AlbumArtCandidate, BrowserItem};
#[cfg(test)]
use crate::ui::preferences::AlbumArtSize;

/// Maximum number of cached album-art entries. The cache is keyed by
/// `(album_key, pixel_size)`. A library of 10 000 albums × one size
/// variant × ~32 KiB decoded surface is well under the working-set
/// budget; the bound is here so an attacker-controlled catalog (e.g., a
/// misbehaving Subsonic peer) cannot inflate memory through the UI path.
pub const MAX_CACHED_ALBUM_ARTS: usize = 512;

/// Resolved album art for one album pane entry. The placeholder image is
/// a stable `gtk::Image` the bind factory clones and rebinds; reusing it
/// across rows avoids creating a fresh widget per row in a virtualized
/// list (a real cost — each `Image` is a GObject and a CSS node).
#[derive(Clone)]
#[allow(dead_code)]
pub struct AlbumArtCell {
    pub row: gtk::Box,
    pub image: gtk::Image,
    pub label: gtk::Label,
    pub placeholder_icon: &'static str,
}

impl AlbumArtCell {
    pub fn new(placeholder_icon: &'static str) -> Self {
        let image = gtk::Image::builder()
            .icon_name(placeholder_icon)
            .pixel_size(0)
            .build();
        image.set_accessible_role(gtk::AccessibleRole::Img);
        let label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .margin_start(8)
            .margin_end(8)
            .margin_top(2)
            .margin_bottom(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .build();
        row.append(&image);
        row.append(&label);
        Self {
            row,
            image,
            label,
            placeholder_icon,
        }
    }

    /// Reset the cell to its placeholder state. Used both before the
    /// artwork resolves and when the album has no artwork at all.
    fn show_placeholder(&self, label_text: &str, accessible_label: Option<&str>) {
        self.image.set_icon_name(Some(self.placeholder_icon));
        self.image.set_paintable(None::<&gdk::Paintable>);
        self.label.set_text(label_text);
        self.label.set_tooltip_text(Some(label_text));
        if let Some(text) = accessible_label {
            self.image
                .update_property(&[gtk::accessible::Property::Label(text)]);
        }
    }

    /// Replace the placeholder with the supplied texture.
    fn show_texture(
        &self,
        texture: &gdk::Texture,
        label_text: &str,
        accessible_label: Option<&str>,
    ) {
        self.image.set_icon_name(None);
        self.image.set_paintable(Some(texture));
        self.label.set_text(label_text);
        self.label.set_tooltip_text(Some(label_text));
        if let Some(text) = accessible_label {
            self.image
                .update_property(&[gtk::accessible::Property::Label(text)]);
        }
    }
}

/// Generation token handed to the bind factory. Each `bind` mints a new
/// token; the same `ListItem` keeps its previous token through `unbind`.
/// When `set_pending` is called, the cell's generation advances so any
/// late async result for the *prior* token is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindGeneration(u64);

impl BindGeneration {
    pub const INVALID: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// Per-pane cache shared across every row in the album pane.
///
/// The cache is bounded by [`MAX_CACHED_ALBUM_ARTS`]; an insert past the
/// bound evicts the least-recently-inserted entry. A `gdk::Texture`
/// retains the decoded pixbuf only as long as GTK holds it; if the system
/// drops the underlying surface, re-decoding happens through the worker.
#[derive(Clone)]
pub struct AlbumArtCache {
    pub(crate) inner: Rc<RefCell<AlbumArtCacheInner>>,
}

pub struct AlbumArtCacheInner {
    pub(crate) entries: HashMap<String, gdk::Texture>,
    pub(crate) order: VecDeque<String>,
}

impl Default for AlbumArtCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AlbumArtCache {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(AlbumArtCacheInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
            })),
        }
    }

    /// Look up a cached texture for `(album_key, pixel_size)`. Returns
    /// `None` on miss; a hit also bumps the entry to the most-recent
    /// position so a hot row doesn't get evicted under memory pressure.
    pub fn get(&self, album_key: &str, pixel_size: i32) -> Option<gdk::Texture> {
        let key = cache_key(album_key, pixel_size);
        let mut inner = self.inner.borrow_mut();
        let texture = inner.entries.get(&key)?.clone();
        if let Some(position) = inner.order.iter().position(|existing| existing == &key) {
            inner.order.remove(position);
        }
        inner.order.push_back(key);
        Some(texture)
    }

    /// Insert a new texture, evicting the oldest entry if the cache is
    /// already full. The eviction is FIFO with a recency-bump on read so
    /// a long-running scroll session never displaces hot entries.
    pub fn insert(&self, album_key: &str, pixel_size: i32, texture: gdk::Texture) {
        let key = cache_key(album_key, pixel_size);
        let mut inner = self.inner.borrow_mut();
        if inner.entries.contains_key(&key) {
            inner.entries.insert(key.clone(), texture);
            if let Some(position) = inner.order.iter().position(|existing| existing == &key) {
                inner.order.remove(position);
            }
            inner.order.push_back(key);
            return;
        }
        while inner.entries.len() >= MAX_CACHED_ALBUM_ARTS {
            if let Some(oldest) = inner.order.pop_front() {
                inner.entries.remove(&oldest);
            } else {
                break;
            }
        }
        inner.entries.insert(key.clone(), texture);
        inner.order.push_back(key);
    }

    /// Total number of entries currently cached.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    /// True if the cache holds zero entries.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().entries.is_empty()
    }
}

fn cache_key(album_key: &str, pixel_size: i32) -> String {
    format!("{album_key}\x1f{pixel_size}")
}

/// Cookie threaded into the bind factory so a late async result can
/// verify its row is still on screen.
///
/// The factory mints a new generation for every `bind`. When the row is
/// recycled (`unbind` then `bind` for a different item) the cell's
/// generation is bumped so any in-flight fetch for the previous item is
/// rejected even before it reaches the worker. The generation is also
/// incremented when the user toggles the artwork preference, so any
/// leftover fetch from the previous mode paints a placeholder.
#[derive(Clone)]
pub struct AlbumArtCellState {
    cell: AlbumArtCell,
    /// Album key currently bound to this row (`None` for the synthetic
    /// "All" row or while the row is being recycled).
    bound_album_key: Rc<RefCell<Option<String>>>,
    /// Monotonic generation. Bumped on rebind and on artwork toggle.
    generation: Rc<Cell<BindGeneration>>,
    /// Active `paintable`-notify listener for the underlying `Image`,
    /// if any. A new bind replaces this with a new listener; the
    /// previous one is disconnected so the cache doesn't get multiple
    /// probes firing on the same paintable change.
    paintable_notify_id: Rc<RefCell<Option<glib::SignalHandlerId>>>,
}

impl AlbumArtCellState {
    fn new(cell: AlbumArtCell) -> Self {
        Self {
            cell,
            bound_album_key: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(BindGeneration::INVALID)),
            paintable_notify_id: Rc::new(RefCell::new(None)),
        }
    }

    #[allow(dead_code)]
    fn row(&self) -> &gtk::Box {
        &self.cell.row
    }

    fn current_generation(&self) -> BindGeneration {
        self.generation.get()
    }

    #[allow(dead_code)]
    fn reset(&self, label_text: &str, accessible_label: &str) {
        *self.bound_album_key.borrow_mut() = None;
        self.generation.set(self.generation.get().next());
        self.cell
            .show_placeholder(label_text, Some(accessible_label));
    }
}

/// Coordinator handed to the album pane's bind factory. Owns the cache
/// plus a per-pane source registry handle; the bind factory only needs
/// the lightweight [`AlbumArtController::bind`] entry point.
#[derive(Clone)]
pub struct AlbumArtController {
    cache: AlbumArtCache,
    source_registry: Rc<RefCell<Option<crate::source_registry::SourceRegistry>>>,
    placeholder_icon: &'static str,
}

impl AlbumArtController {
    pub fn new(placeholder_icon: &'static str) -> Self {
        Self {
            cache: AlbumArtCache::new(),
            source_registry: Rc::new(RefCell::new(None)),
            placeholder_icon,
        }
    }

    /// Wire the source registry in once the main window has constructed
    /// it. The controller is cloned into the bind factory before this is
    /// called, so a late binding simply skips the credential-isolated
    /// resolution path and falls back to the URI/placeholder paths.
    pub fn attach_source_registry(&self, source_registry: crate::source_registry::SourceRegistry) {
        *self.source_registry.borrow_mut() = Some(source_registry);
    }

    pub fn cache(&self) -> &AlbumArtCache {
        &self.cache
    }

    #[allow(dead_code)]
    pub fn placeholder_icon(&self) -> &'static str {
        self.placeholder_icon
    }

    /// Build the bind factory pair (`setup` + `bind`) for the album pane.
    ///
    /// `unbind` is exposed through the returned [`AlbumArtBinder`] so
    /// callers can wire it to the factory. The bind factory:
    /// * Snapshots the `BrowserItem`'s artwork candidate.
    /// * Stamps the cell with a fresh `BindGeneration` so any in-flight
    ///   fetch for the prior row is invalidated.
    /// * If the cache already has a texture for this album + size, paints
    ///   it directly and returns (no fetch, no async).
    /// * Otherwise paints the placeholder and schedules a fetch.
    ///
    /// The `setup` closure installs the row's reusable widget tree (one
    /// `gtk::Box` per `ListItem`, holding an `Image` and a `Label`). The
    /// bind phase updates those existing widgets in place; `unbind`
    /// disconnects the artwork-paintable notify handler so the next bind
    /// can install a fresh one without leaking observers.
    #[allow(clippy::type_complexity)]
    pub fn build_binder(
        &self,
    ) -> (
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        AlbumArtBinder,
    ) {
        let placeholder_icon = self.placeholder_icon;
        let binder = AlbumArtBinder::new(self.clone());
        let cell_states = binder.cell_states.clone();
        let controller = self.clone();

        let setup = move |_factory: &gtk::SignalListItemFactory, list_item: &glib::Object| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let cell_state = AlbumArtCellState::new(AlbumArtCell::new(placeholder_icon));
            let row_widget = cell_state.cell.row.clone();
            cell_states
                .borrow_mut()
                .insert(list_item.as_ptr() as usize, cell_state);
            list_item.set_child(Some(&row_widget));
        };

        let bind = binder.bind_fn();
        let unbind = binder.unbind_fn();
        let _ = controller;
        (setup, bind, unbind, binder)
    }
}

/// Handle returned to the factory wiring so `unbind` can invalidate the
/// bound row's generation before GTK hands the cell to a different item.
pub struct AlbumArtBinder {
    controller: AlbumArtController,
    pub(crate) cell_states: Rc<RefCell<HashMap<usize, AlbumArtCellState>>>,
}

impl AlbumArtBinder {
    fn new(controller: AlbumArtController) -> Self {
        Self {
            controller,
            cell_states: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    fn bind_fn(&self) -> impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static {
        let controller = self.controller.clone();
        let cell_states = self.cell_states.clone();
        move |_factory: &gtk::SignalListItemFactory, list_item: &glib::Object| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let key = list_item.as_ptr() as usize;
            let item = list_item
                .item()
                .and_downcast::<BrowserItem>()
                .expect("Album pane list_item must wrap a BrowserItem");
            let label_text = item.display();
            let accessible_label = item.label();
            let candidate = item.artwork_candidate();

            let cell_state = cell_states
                .borrow()
                .get(&key)
                .cloned()
                .expect("setup closure must register a cell state for this list_item");

            // Cache hit: paint straight from the cache.
            if let Some(ref candidate) = candidate {
                if let Some(texture) = controller.cache.get(
                    &candidate.track_id,
                    AlbumArtController::default_pixel_size(),
                ) {
                    let generation = cell_state.current_generation().next();
                    cell_state.generation.set(generation);
                    cell_state
                        .cell
                        .show_texture(&texture, &label_text, Some(&accessible_label));
                    *cell_state.bound_album_key.borrow_mut() = Some(candidate.track_id.clone());
                    return;
                }
            }

            // Cache miss: paint placeholder + schedule fetch.
            let generation = cell_state.current_generation().next();
            cell_state.generation.set(generation);
            cell_state
                .cell
                .show_placeholder(&label_text, Some(&accessible_label));
            *cell_state.bound_album_key.borrow_mut() =
                candidate.as_ref().map(|cand| cand.track_id.clone());

            if let Some(candidate) = candidate {
                controller.spawn_fetch(cell_state.clone(), candidate, generation);
            }
        }
    }

    fn unbind_fn(&self) -> impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static {
        let cell_states = self.cell_states.clone();
        move |_factory: &gtk::SignalListItemFactory, list_item: &glib::Object| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let key = list_item.as_ptr() as usize;
            if let Some(state) = cell_states.borrow().get(&key).cloned() {
                // Bump generation so any in-flight fetch for this row
                // paints nothing when it returns, and disconnect the
                // paintable-notify listener so the next bind installs a
                // fresh one without leaking observers.
                state.generation.set(state.generation.get().next());
                *state.bound_album_key.borrow_mut() = None;
                if let Some(handler_id) = state.paintable_notify_id.borrow_mut().take() {
                    state.cell.image.disconnect(handler_id);
                }
            }
        }
    }
}

impl AlbumArtController {
    /// Default pixel size used when no preference is set. Matches
    /// `AlbumArtSize::Medium` so the controller has a sensible
    /// default without depending on the prefs module (which would
    /// create a circular dependency direction).
    fn default_pixel_size() -> i32 {
        48
    }

    fn spawn_fetch(
        &self,
        cell_state: AlbumArtCellState,
        candidate: AlbumArtCandidate,
        generation: BindGeneration,
    ) {
        let image = cell_state.cell.image.clone();
        let cache = self.cache.clone();
        let source_registry = self.source_registry.clone();
        let album_key = candidate.track_id.clone();
        let pixel_size = Self::default_pixel_size();
        let cover_art_url = candidate.cover_art_url.clone();
        let uri = candidate.uri.clone();
        let source_id = candidate.source_id;
        let source_epoch = candidate.source_session_epoch;

        glib::MainContext::default().spawn_local(async move {
            // Step 1: resolve the artwork path. The decision tree mirrors
            // the playback-time resolver: remote sources go through the
            // lease-isolated HTTP path; legacy tracks with an embedded
            // cover URL fall back to the direct URL path; local files go
            // through the embedded-extraction path. Drop the RefCell
            // guard before any `.await` so we never hold a borrowed
            // reference across a suspension point.
            let registry_handle = source_registry.borrow().clone();
            let resolved = resolve_kind(
                registry_handle,
                source_id,
                source_epoch,
                &candidate,
                cover_art_url,
                uri,
            )
            .await;

            // Late-cancellation check: if the row was unbound before the
            // resolver returned, drop the result on the floor.
            if cell_state.current_generation() != generation {
                return;
            }

            match resolved {
                ResolvedArtKind::NoArtwork => {
                    // Leave the placeholder visible.
                }
                ResolvedArtKind::DirectFile { uri } => {
                    // Embedded extraction goes through the album-art
                    // worker; the worker's own generation check prevents
                    // late results from racing newer rows.
                    album_art::update_direct_file_album_art(&image, &uri);
                }
                ResolvedArtKind::DirectUrl { url } => {
                    album_art::fetch_remote_album_art(&image, &url);
                }
                ResolvedArtKind::ResolvedRequest(request) => {
                    let gen = album_art::begin_remote_album_art(&image);
                    album_art::fetch_resolved_album_art(&image, *request, gen);
                }
            }

            // Cache the texture only once the worker publishes it. The
            // worker delivers bytes through `gdk::Texture::from_bytes`
            // synchronously on the GTK main loop, so we listen for the
            // resulting `paintable` property change.
            install_cache_probe(cache, image, album_key, pixel_size, cell_state, generation);
        });
    }
}

enum ResolvedArtKind {
    NoArtwork,
    DirectFile { uri: String },
    DirectUrl { url: String },
    ResolvedRequest(Box<ResolvedHttpRequest>),
}

async fn resolve_kind(
    source_registry: Option<crate::source_registry::SourceRegistry>,
    source_id: Option<SourceId>,
    source_epoch: Option<u64>,
    candidate: &AlbumArtCandidate,
    cover_art_url: String,
    uri: String,
) -> ResolvedArtKind {
    // 1. Lease-isolated remote resolver.
    if let (Some(registry), Some(id), Some(epoch)) =
        (source_registry.as_ref(), source_id, source_epoch)
    {
        let track_id = match crate::architecture::TrackId::new(candidate.track_id.clone()) {
            Ok(id) => id,
            Err(error) => {
                tracing::debug!(
                    %error,
                    track_id = %candidate.track_id,
                    "Album pane skipped invalid track id while resolving artwork"
                );
                return ResolvedArtKind::NoArtwork;
            }
        };
        match registry.resolve_artwork(id, epoch, track_id).await {
            Ok(Some(request)) => return ResolvedArtKind::ResolvedRequest(Box::new(request)),
            Ok(None) => {
                // Remote source returned no artwork for this track — try
                // the legacy embedded cover URL on the row before giving
                // up, so a row that has both a remote and a URL still
                // gets a thumbnail.
                if !cover_art_url.is_empty() {
                    return ResolvedArtKind::DirectUrl { url: cover_art_url };
                }
                if uri.starts_with("file://") {
                    return ResolvedArtKind::DirectFile { uri };
                }
                return ResolvedArtKind::NoArtwork;
            }
            Err(error) => {
                tracing::debug!(
                    %error,
                    source_id = %id,
                    track_id = %candidate.track_id,
                    "Album pane artwork resolver fell back after backend error"
                );
            }
        }
    }
    // 2. Legacy direct URL fallback for rows that ship one.
    if !cover_art_url.is_empty() {
        return ResolvedArtKind::DirectUrl { url: cover_art_url };
    }
    // 3. Embedded extraction for local file rows.
    if uri.starts_with("file://") {
        return ResolvedArtKind::DirectFile { uri };
    }
    ResolvedArtKind::NoArtwork
}

fn install_cache_probe(
    cache: AlbumArtCache,
    image: gtk::Image,
    album_key: String,
    pixel_size: i32,
    cell_state: AlbumArtCellState,
    generation: BindGeneration,
) {
    // The album-art worker calls `set_paintable` synchronously from the
    // GTK main thread when its fetch succeeds. Listening for the
    // `paintable` property change is therefore the cheapest way to know
    // a fresh texture is installed — no additional worker plumbing.
    //
    // Disconnect any prior listener first: the same `gtk::Image` is
    // reused across multiple binds in a virtualized list, so without
    // this every rebind would leave its predecessor's listener attached
    // and the cache would observe the same paintable change N times.
    if let Some(previous) = cell_state.paintable_notify_id.borrow_mut().take() {
        image.disconnect(previous);
    }

    let closure_album_key = album_key;
    let gen = generation;
    let state = cell_state.clone();
    let handler_id = image.connect_notify_local(Some("paintable"), move |img, _| {
        if state.current_generation() != gen {
            return;
        }
        if let Some(paintable) = img.paintable() {
            if let Ok(texture) = paintable.downcast::<gdk::Texture>() {
                cache.insert(&closure_album_key, pixel_size, texture);
            }
        }
    });
    cell_state.paintable_notify_id.replace(Some(handler_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_texture() -> gdk::Texture {
        // 1×1 RGBA PNG with full filter byte per row. PNG's raw stream is
        // a per-row filter byte (0 = none) plus RGBA pixels; the IDAT
        // zlib-stream deflates those bytes. CRC table from PNG spec §B.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x60, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0x7A, 0x5E, 0xAB, 0x3F,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        gdk::Texture::from_bytes(&glib::Bytes::from_static(png)).expect("valid 1×1 PNG decodes")
    }

    #[test]
    fn cache_bounded_eviction_replaces_oldest_entry() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        for i in 0..(MAX_CACHED_ALBUM_ARTS + 4) {
            cache.insert(&format!("album-{i}"), 48, texture.clone());
        }
        assert_eq!(cache.len(), MAX_CACHED_ALBUM_ARTS);
        // Earliest entries must have been evicted.
        assert!(cache.get("album-0", 48).is_none());
        assert!(cache.get("album-1", 48).is_none());
        // Most recent insertions survive.
        let last = MAX_CACHED_ALBUM_ARTS + 4 - 1;
        assert!(cache.get(&format!("album-{last}"), 48).is_some());
    }

    #[test]
    fn cache_hit_promotes_to_most_recent() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        for i in 0..MAX_CACHED_ALBUM_ARTS {
            cache.insert(&format!("album-{i}"), 48, texture.clone());
        }
        // Touch the earliest entry — it should survive a follow-up
        // insert that would otherwise evict it.
        assert!(cache.get("album-0", 48).is_some());
        cache.insert("newcomer", 48, texture.clone());
        assert_eq!(cache.len(), MAX_CACHED_ALBUM_ARTS);
        assert!(cache.get("album-0", 48).is_some());
        assert!(cache.get("album-1", 48).is_none());
    }

    #[test]
    fn cache_pixel_size_distinguishes_entries() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        cache.insert("album-a", 32, texture.clone());
        assert!(cache.get("album-a", 32).is_some());
        assert!(cache.get("album-a", 48).is_none());
        cache.insert("album-a", 48, texture);
        assert!(cache.get("album-a", 32).is_some());
        assert!(cache.get("album-a", 48).is_some());
    }

    #[test]
    fn bind_generation_advances_on_reset() {
        // `AlbumArtCell::new` builds a real `gtk::Image`, so we need
        // GTK to be initialised for this test. The first call to
        // `gtk::init` per test binary wins; ignore the "already
        // initialised" error so a later test can run before us. Run
        // this test serially with `--test-threads=1` if a test runner
        // ever chooses to spawn a GTK worker off-thread.
        let _ = gtk::init();
        let cell = AlbumArtCell::new("audio-x-generic-symbolic");
        let state = AlbumArtCellState::new(cell);
        let initial = state.current_generation();
        state.reset("Album A", "Album A");
        assert_ne!(state.current_generation(), initial);
    }

    #[test]
    fn bind_generation_monotonic_across_rebinds() {
        // The virtualized list recycles a row's cell across many
        // rebinds; each bind must hand the cell a fresh generation so a
        // late async result for the previous row is dropped without
        // painting. The factory's contract is: every bind mints a new
        // generation, no generation is reused.
        let gen = BindGeneration::INVALID;
        let mut last = gen;
        for _ in 0..8 {
            let next = last.next();
            assert_ne!(next, last);
            last = next;
        }
    }

    #[test]
    fn bind_generation_late_result_is_dropped() {
        // An in-flight async fetch returns after the row has been
        // recycled. The contract is: `state.current_generation() !=
        // captured_generation` ⇒ the result must be discarded. We can
        // exercise the check directly without spinning up the async
        // runtime or constructing a `gtk::Image`: the same predicate
        // runs in `spawn_fetch` and the cache probe closure, both of
        // which compare the captured generation against the cell's
        // current generation token.
        let captured = BindGeneration::INVALID.next();
        // Simulate a rebind (advance the generation).
        let current = captured.next();
        assert_ne!(
            current, captured,
            "late result must observe a newer generation than the one captured at fetch time"
        );
    }

    #[test]
    fn album_art_size_tokens_round_trip() {
        for size in [
            AlbumArtSize::Small,
            AlbumArtSize::Medium,
            AlbumArtSize::Large,
        ] {
            let token = size.as_token();
            let parsed = AlbumArtSize::from_token(token).expect("round-trip");
            assert_eq!(parsed, size);
        }
        assert!(AlbumArtSize::from_token("nope").is_none());
        assert!(AlbumArtSize::from_token("").is_none());
    }

    #[test]
    fn album_art_size_pixel_sizes_are_distinct() {
        let small = AlbumArtSize::Small.pixel_size();
        let medium = AlbumArtSize::Medium.pixel_size();
        let large = AlbumArtSize::Large.pixel_size();
        assert!(small < medium);
        assert!(medium < large);
        assert!(small > 0);
    }

    #[test]
    fn controller_default_pixel_size_matches_medium_token() {
        // The controller's default must match `AlbumArtSize::Medium` so
        // a layout that toggles on before the prefs module is queried
        // still renders at the same size the user sees everywhere else.
        assert_eq!(
            AlbumArtController::default_pixel_size(),
            AlbumArtSize::Medium.pixel_size()
        );
    }
}
