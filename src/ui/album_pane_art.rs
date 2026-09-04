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
use std::collections::HashMap;
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

// The bounded display-side texture cache lives in `album_art_cache`;
// re-exported here so the pane's controller (and the browser module's
// established `super::album_pane_art` import path) keep one stable home.
pub use super::album_art_cache::AlbumArtCache;

// The row widget tree and per-row bind/cancellation state live in
// `album_art_cell`; re-exported here so the established
// `super::album_pane_art` import path keeps one stable home.
pub use super::album_art_cell::{AlbumArtCell, AlbumArtCellState, BindGeneration};

/// Stable placeholder icon name used when the controller is wired but
/// has not been told what icon to use. The factory overrides this on
/// every cell construction with the per-controller icon string.
pub const FALLBACK_PLACEHOLDER_ICON: &str = "audio-x-generic-symbolic";

/// Coordinator handed to the album pane's bind factory. Owns the cache
/// plus a per-pane source registry handle; the bind factory only needs
/// the lightweight [`AlbumArtController::bind`] entry point.
#[derive(Clone)]
pub struct AlbumArtController {
    cache: AlbumArtCache,
    source_registry: Rc<RefCell<Option<crate::source_registry::SourceRegistry>>>,
    /// Side length (in device pixels) of each rendered thumbnail.
    /// Wired in from the browser's `BrowserState::album_pane_artwork_size`
    /// cell so the bind factory and the cache probe both read the
    /// live preference, not a hardcoded default. `None` until the
    /// browser's setup step attaches a source; until then the
    /// controller falls back to [`AlbumArtController::default_pixel_size`]
    /// so a late wiring (e.g., tests) still renders at a sensible size.
    pixel_size: Rc<RefCell<Option<Rc<Cell<i32>>>>>,
    placeholder_icon: &'static str,
}

impl AlbumArtController {
    pub fn new(placeholder_icon: &'static str) -> Self {
        Self {
            cache: AlbumArtCache::new(),
            source_registry: Rc::new(RefCell::new(None)),
            pixel_size: Rc::new(RefCell::new(None)),
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

    /// Wire the live size knob in. The bind factory and the cache probe
    /// read this cell on every bind, so a subsequent
    /// [`crate::ui::browser::set_album_pane_artwork_size`] takes effect
    /// for any row that scrolls into view afterwards. The cell is shared
    /// with the `BrowserState` so a write through the public setter is
    /// observed here without any further wiring.
    #[allow(dead_code)]
    pub fn attach_pixel_size(&self, pixel_size: Rc<Cell<i32>>) {
        *self.pixel_size.borrow_mut() = Some(pixel_size);
    }

    pub fn cache(&self) -> &AlbumArtCache {
        &self.cache
    }

    #[allow(dead_code)]
    pub fn placeholder_icon(&self) -> &'static str {
        self.placeholder_icon
    }

    /// Resolve the side length to render at. Reads the live size knob
    /// when the controller has been wired to one, otherwise returns
    /// [`AlbumArtController::default_pixel_size`].
    fn current_pixel_size(&self) -> i32 {
        if let Some(cell) = self.pixel_size.borrow().as_ref() {
            cell.get()
        } else {
            Self::default_pixel_size()
        }
    }

    /// Build the bind factory closures for the album pane, additionally
    /// wiring the controller to the supplied size knob.
    ///
    /// `unbind` and `teardown` are exposed alongside `setup` + `bind` so
    /// callers can wire them to the factory. The bind factory:
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
    /// can install a fresh one without leaking observers; `teardown`
    /// removes the cell state entirely when GTK discards the list item
    /// (row recycled out of the view, view destroyed, or the factory
    /// swapped), releasing the widget tree and stopping any fetch still
    /// attached to it.
    #[allow(clippy::type_complexity)]
    pub fn build_binder_with_size(
        &self,
        pixel_size: Rc<Cell<i32>>,
    ) -> (
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        AlbumArtBinder,
    ) {
        self.build_binder_with_size_internal(Some(pixel_size))
    }

    #[allow(clippy::type_complexity)]
    fn build_binder_with_size_internal(
        &self,
        pixel_size: Option<Rc<Cell<i32>>>,
    ) -> (
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static,
        AlbumArtBinder,
    ) {
        if let Some(cell) = pixel_size {
            *self.pixel_size.borrow_mut() = Some(cell);
        }
        let placeholder_icon = self.placeholder_icon;
        let binder = AlbumArtBinder::new(self.clone());
        let cell_states = binder.cell_states.clone();

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
        let teardown = binder.teardown_fn();
        (setup, bind, unbind, teardown, binder)
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

            // Apply the live pixel size to the cell's image before
            // deciding between cache hit, cache miss, or no candidate.
            // `AlbumArtCell::new` requests the icon-theme default, so a
            // bind here is what tells GTK the actual side length the
            // placeholder and the eventual texture should render at.
            let pixel_size = controller.current_pixel_size();
            cell_state.cell.image.set_pixel_size(pixel_size);

            // Revoke any in-flight fetch for the previous bind BEFORE we
            // mint a new generation. The flag is the cancellation
            // primitive the spawned future checks between `.await`
            // points; the new generation token is the post-await gate
            // that decides whether a result (if any races past the flag)
            // is allowed to paint. Both must be set, in this order, on
            // every rebind.
            cell_state.revoke();

            // Cache hit: paint straight from the cache. The cache is
            // keyed by (source, album_key, pixel_size), so two remote
            // sources that happen to expose the same track id never
            // collide on the same texture.
            if let Some(ref candidate) = candidate {
                if let Some(texture) = controller.cache.get(
                    candidate.source_id.as_ref(),
                    &candidate.track_id,
                    pixel_size,
                ) {
                    let generation = cell_state.current_generation().next();
                    cell_state.generation.set(generation);
                    cell_state
                        .cell
                        .show_texture(&texture, &label_text, Some(&accessible_label));
                    *cell_state.bound_album_key.borrow_mut() = Some(candidate.track_id.clone());
                    *cell_state.bound_source.borrow_mut() = candidate.source_id;
                    return;
                }
            }

            // Cache miss: paint placeholder + schedule fetch.
            //
            // The placeholder is set unconditionally — even for the
            // synthetic "All" row and for rows whose album has no
            // resolvable candidate. The bind factory is the canonical
            // storage point for the placeholder state, so every cell
            // leaves the bind path with a stable visual fallback (icon
            // set, paintable cleared, accessible label set).
            let generation = cell_state.current_generation().next();
            cell_state.generation.set(generation);
            cell_state
                .cell
                .show_placeholder(&label_text, Some(&accessible_label));
            *cell_state.bound_album_key.borrow_mut() =
                candidate.as_ref().map(|cand| cand.track_id.clone());
            *cell_state.bound_source.borrow_mut() = None;

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
                // Bump generation AND flip the revocation flag so any
                // in-flight fetch for this row exits on its next poll.
                // The next `bind` clears the flag back to false before
                // allocating a new generation.
                state.revoke();
                state.generation.set(state.generation.get().next());
                *state.bound_album_key.borrow_mut() = None;
                *state.bound_source.borrow_mut() = None;
                if let Some(handler_id) = state.paintable_notify_id.borrow_mut().take() {
                    state.cell.image.disconnect(handler_id);
                }
            }
        }
    }

    fn teardown_fn(&self) -> impl Fn(&gtk::SignalListItemFactory, &glib::Object) + 'static {
        let cell_states = self.cell_states.clone();
        move |_factory: &gtk::SignalListItemFactory, list_item: &glib::Object| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let key = list_item.as_ptr() as usize;
            if let Some(state) = cell_states.borrow_mut().remove(&key) {
                // The row widget leaves the tree for good here. Revoke
                // any in-flight fetch and drop the paintable listener,
                // then drop the state itself — without this, every
                // discarded row's `AlbumArtCellState` (and the widget
                // tree it holds) survived until the next pane rebuild,
                // so artwork toggles and size changes accumulated dead
                // entries in `cell_states`.
                state.revoke();
                if let Some(handler_id) = state.paintable_notify_id.borrow_mut().take() {
                    state.cell.image.disconnect(handler_id);
                }
            }
        }
    }

    /// Walk every live cell state and revoke its in-flight fetch. Used
    /// by the rebuild path when the user toggles the artwork preference
    /// or resizes the thumbnails — every cell will be re-bound to a new
    /// factory, so revoking here is cheaper than waiting for each
    /// cell's `unbind` to fire (which only happens once GTK hands the
    /// row to a different item, possibly much later).
    pub fn revoke_all(&self) {
        for state in self.cell_states.borrow().values() {
            state.revoke();
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
        let album_source = candidate.source_id;
        let pixel_size = self.current_pixel_size();
        let cover_art_url = candidate.cover_art_url.clone();
        let uri = candidate.uri.clone();
        let source_id = candidate.source_id;
        let source_epoch = candidate.source_session_epoch;

        // The bind factory revoked this cell's PREVIOUS fetch before
        // handing it to us; clear the latch (and any leftover paintable
        // listener) so THIS fetch runs un-revoked. This is race-free:
        // everything up to this point ran synchronously on the main
        // loop, so the previous fetch can only resume after this reset
        // — where its stale generation token still blocks it from
        // painting or caching (checked at every resume point below).
        cell_state.clear();

        glib::MainContext::default().spawn_local(async move {
            // Step 1: resolve the artwork path. The decision tree mirrors
            // the playback-time resolver: local files keep their
            // authority and go straight to embedded extraction; remote
            // sources go through the lease-isolated HTTP path; legacy
            // tracks with an embedded cover URL fall back to the direct
            // URL path. Drop the RefCell guard before any `.await` so
            // we never hold a borrowed reference across a suspension
            // point.
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

            // Mid-flight cancellation: if the row was unbound or
            // re-bound while the resolver was running, exit silently.
            // This check is independent of the generation token so a
            // rebind that hasn't yet published its new token still
            // short-circuits the old future.
            if cell_state.is_revoked() {
                return;
            }
            if cell_state.current_generation() != generation {
                return;
            }

            paint_resolved_art(resolved, &image);

            // Cache the texture only once the worker publishes it. The
            // worker delivers bytes through `gdk::Texture::from_bytes`
            // synchronously on the GTK main loop, so we listen for the
            // resulting `paintable` property change.
            install_cache_probe(
                cache,
                image,
                album_source,
                album_key,
                pixel_size,
                cell_state,
                generation,
            );
        });
    }
}

/// Paint one resolved artwork outcome onto the row's image. Extracted
/// from `spawn_fetch` so the spawned future stays a thin orchestration
/// shell: each arm maps one [`ResolvedArtKind`] variant onto the
/// matching album-art worker entry point, and the no-artwork case
/// leaves the existing placeholder visible.
fn paint_resolved_art(resolved: ResolvedArtKind, image: &gtk::Image) {
    match resolved {
        ResolvedArtKind::NoArtwork => {
            // Leave the placeholder visible.
        }
        ResolvedArtKind::DirectFile { uri } => {
            // Embedded extraction goes through the album-art
            // worker; the worker's own generation check prevents
            // late results from racing newer rows.
            album_art::update_direct_file_album_art(image, &uri);
        }
        ResolvedArtKind::DirectUrl { url } => {
            album_art::fetch_remote_album_art(image, &url);
        }
        ResolvedArtKind::ResolvedRequest(request) => {
            let gen = album_art::begin_remote_album_art(image);
            album_art::fetch_resolved_album_art(image, *request, gen);
        }
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
    // Retained local-media authority: a row whose playable locator is a
    // file:// URI keeps its authority through the album pane — no
    // remote resolver is consulted, no opaque credentials are minted,
    // and the existing embedded-extraction worker is used verbatim.
    // This is the precedence the rejection review asked for: local
    // rows must never silently detour through the remote path on the
    // chance the row also carries a remote source_id.
    if uri.starts_with("file://") {
        return ResolvedArtKind::DirectFile { uri };
    }
    // Lease-isolated remote resolver.
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
    // Legacy direct URL fallback for rows that ship one and do not
    // resolve through any source registry.
    if !cover_art_url.is_empty() {
        return ResolvedArtKind::DirectUrl { url: cover_art_url };
    }
    ResolvedArtKind::NoArtwork
}

fn install_cache_probe(
    cache: AlbumArtCache,
    image: gtk::Image,
    source: Option<SourceId>,
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
    let closure_source = source;
    let gen = generation;
    let state = cell_state.clone();
    let handler_id = image.connect_notify_local(Some("paintable"), move |img, _| {
        // Both gates must agree before we cache: a stale generation or
        // a revoked fetch must not pollute the cache with a texture the
        // user never sees.
        if state.is_revoked() || state.current_generation() != gen {
            return;
        }
        if let Some(paintable) = img.paintable() {
            if let Ok(texture) = paintable.downcast::<gdk::Texture>() {
                cache.insert(
                    closure_source.as_ref(),
                    &closure_album_key,
                    pixel_size,
                    texture,
                );
            }
        }
    });
    cell_state.paintable_notify_id.replace(Some(handler_id));
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn album_art_size_persists_as_stable_token() {
        // The persisted form is the lowercase token, not the serde
        // default variant name — that's the migration guarantee the
        // custom Serialize/Deserialize impls exist to provide.
        assert_eq!(
            serde_json::to_value(AlbumArtSize::Medium).expect("serialize"),
            serde_json::Value::String("medium".into())
        );
        // Older builds wrote the bare variant names (derived enum
        // representation); they must keep loading.
        assert_eq!(
            serde_json::from_value::<AlbumArtSize>(serde_json::Value::String("Small".into()))
                .expect("legacy variant name"),
            AlbumArtSize::Small
        );
        // An unknown token falls back to the default instead of failing
        // the whole AppConfig load.
        assert_eq!(
            serde_json::from_value::<AlbumArtSize>(serde_json::Value::String("huge".into()))
                .expect("unknown token falls back"),
            AlbumArtSize::default()
        );
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

    #[test]
    fn current_pixel_size_falls_back_to_default_without_source() {
        // A controller constructed without a size source must render at
        // the same size the default knob advertises, so an untested
        // call site doesn't see a different thumbnail size than the
        // documented default.
        let controller = AlbumArtController::new("audio-x-generic-symbolic");
        assert_eq!(
            controller.current_pixel_size(),
            AlbumArtController::default_pixel_size()
        );
    }

    #[test]
    fn current_pixel_size_reads_live_source_cell() {
        // The persisted layout preference must reach the bind path:
        // flipping the cell from Small to Large must be observed by the
        // next call into `current_pixel_size`, otherwise the
        // Small/Large selector in the preferences dialog is inert.
        let controller = AlbumArtController::new("audio-x-generic-symbolic");
        let source: Rc<Cell<i32>> = Rc::new(Cell::new(AlbumArtSize::Small.pixel_size()));
        controller.attach_pixel_size(source.clone());
        assert_eq!(
            controller.current_pixel_size(),
            AlbumArtSize::Small.pixel_size()
        );
        source.set(AlbumArtSize::Large.pixel_size());
        assert_eq!(
            controller.current_pixel_size(),
            AlbumArtSize::Large.pixel_size()
        );
        source.set(AlbumArtSize::Medium.pixel_size());
        assert_eq!(
            controller.current_pixel_size(),
            AlbumArtSize::Medium.pixel_size()
        );
    }

    /// Placeholder storage must be idempotent. The bind factory calls
    /// `show_placeholder` for every bind (including for the synthetic
    /// "All" row and for any row whose album has no resolvable
    /// candidate). The two sides of the placeholder state — the icon
    /// name and the (cleared) paintable — must agree after every call.
    /// We can verify the storage contract without GTK by inspecting
    /// the icon-name field that the placeholder setter writes; the
    /// paintable-clear half is exercised on the GTK thread in the
    /// production bind path.
    #[test]
    fn placeholder_icon_name_is_a_stable_string_constant() {
        // The placeholder icon name is the `&'static str` carried by
        // the controller. A row that loses its artwork must show the
        // same icon every time, not whichever icon happened to be on
        // the cell when it was last painted.
        let controller = AlbumArtController::new("audio-x-generic-symbolic");
        assert_eq!(controller.placeholder_icon(), "audio-x-generic-symbolic");
        let controller2 = AlbumArtController::new("image-missing-symbolic");
        assert_eq!(controller2.placeholder_icon(), "image-missing-symbolic");
    }
}
