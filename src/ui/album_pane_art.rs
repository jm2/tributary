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
//!   and consumed by the persistent art worker. Authority-backed local
//!   rows resolve an exact retained file capability through the registry
//!   and extract through that handle (`update_resolved_file_album_art_scoped`);
//!   only rows with no authority chain keep the transitional raw-`file://`
//!   path. URLs from the track's `cover_art_url` go through
//!   `fetch_remote_album_art`. None of these paths invent new
//!   credential-isolation seams.
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
            let cell_state = cell_states
                .borrow()
                .get(&key)
                .cloned()
                .expect("setup closure must register a cell state for this list_item");

            let pixel_size = rebind_cell(&controller, &cell_state);

            let candidate = item.artwork_candidate();
            let label_text = item.display();
            let accessible_label = item.label();
            if paint_cached_texture(
                &controller,
                &cell_state,
                candidate.as_ref(),
                pixel_size,
                &label_text,
                &accessible_label,
            ) {
                return;
            }
            stage_placeholder_fetch(
                &controller,
                &cell_state,
                candidate,
                &label_text,
                &accessible_label,
            );
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
                disconnect_paintable_listener(&state.cell.image, &state);
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
                disconnect_paintable_listener(&state.cell.image, &state);
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
        // The bind factory revoked this cell's PREVIOUS fetch before
        // handing it to us; clear the latch (and any leftover paintable
        // listener) so THIS fetch runs un-revoked. This is race-free:
        // everything up to this point ran synchronously on the main
        // loop, so the previous fetch can only resume after this reset
        // — where its stale generation token and revoked liveness token
        // still block it from painting or caching (checked at every
        // resume point in the orchestration below).
        cell_state.clear();

        // Mint this fetch's worker-side liveness token and store it on
        // the cell. Rebind/unbind/teardown/factory-swap revokes flip it,
        // which stops the persistent art worker before the network read
        // and closes the reply — independently scoped per row, so
        // concurrent visible rows (and the now-playing header) never
        // cancel one another.
        let liveness = album_art::ScopedArtFetch::new();
        *cell_state.fetch_liveness.borrow_mut() = Some(liveness.clone());

        let fetch = PaneFetch {
            image: cell_state.cell.image.clone(),
            cache: self.cache.clone(),
            source_registry: self.source_registry.clone(),
            album_source: candidate.source_id,
            source_epoch: candidate.source_session_epoch,
            album_key: candidate.track_id.clone(),
            pixel_size: self.current_pixel_size(),
        };
        orchestrate_pane_fetch(fetch, cell_state, candidate, generation, liveness);
    }
}

/// Everything one pane fetch needs to paint and (on success) admit its
/// texture into the cache. Grouped so the orchestration function takes a
/// single identity argument instead of a long parameter list.
struct PaneFetch {
    image: gtk::Image,
    cache: AlbumArtCache,
    source_registry: Rc<RefCell<Option<crate::source_registry::SourceRegistry>>>,
    album_source: Option<SourceId>,
    source_epoch: Option<u64>,
    album_key: String,
    pixel_size: i32,
}

/// The spawned half of one pane fetch: resolve, paint, and install the
/// cache probe. Split out of `spawn_fetch` (Codacy function-size limit) —
/// the liveness/generation gates in [`finish_pane_fetch`] are the
/// post-await half of the rebind contract documented at the bind
/// factory's `revoke` call.
///
/// Deliberately a synchronous function that spawns the async body onto
/// the GTK main context: the carried cell state and registry handle are
/// main-thread-only (`Rc`/`RefCell`), so the future is intentionally not
/// `Send`. The registry handle is cloned out of its `RefCell` HERE —
/// synchronously, on the main thread, before the task is queued — so the
/// guard is never held across a suspension point.
fn orchestrate_pane_fetch(
    fetch: PaneFetch,
    cell_state: AlbumArtCellState,
    candidate: AlbumArtCandidate,
    generation: BindGeneration,
    liveness: album_art::ScopedArtFetch,
) {
    let registry_handle = fetch.source_registry.borrow().clone();
    glib::MainContext::default().spawn_local(async move {
        // Step 1: resolve the artwork path. The decision tree mirrors
        // the playback-time resolver: authority-backed local rows extract
        // through a retained file capability; remote sources go through
        // the lease-isolated HTTP path; legacy tracks with an embedded
        // cover URL fall back to the direct URL path.
        let resolved = resolve_kind(
            registry_handle,
            fetch.album_source,
            fetch.source_epoch,
            &candidate,
            candidate.cover_art_url.clone(),
            candidate.uri.clone(),
        )
        .await;
        finish_pane_fetch(fetch, cell_state, generation, liveness, resolved);
    });
}

/// The post-resolve half of one pane fetch: honor the mid-flight
/// cancellation gates, paint the outcome, and install the cache-admission
/// probe.
fn finish_pane_fetch(
    fetch: PaneFetch,
    cell_state: AlbumArtCellState,
    generation: BindGeneration,
    liveness: album_art::ScopedArtFetch,
    resolved: ResolvedArtKind,
) {
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

    let PaneFetch {
        image,
        cache,
        album_source,
        source_epoch,
        album_key,
        pixel_size,
        ..
    } = fetch;
    paint_resolved_art(resolved, &image, &liveness);

    // Cache the texture only once the worker publishes it. The
    // worker delivers bytes through `gdk::Texture::from_bytes`
    // synchronously on the GTK main loop, so we listen for the
    // resulting `paintable` property change.
    let content_generation = cache.content_generation();
    install_cache_probe(
        CacheAdmission {
            cache,
            source: album_source,
            source_epoch,
            content_generation,
            album_key,
            pixel_size,
        },
        image,
        cell_state,
        generation,
        liveness,
    );
}

/// Cache-hit half of the bind path: paint straight from the display cache
/// and advance the cell's generation. Returns `true` when a cached texture
/// was painted, completing the bind. The cache is keyed by
/// (source, source_epoch, content_generation, album_key, pixel_size), so
/// two remote sources that happen to expose the same track id never
/// collide on the same texture, a source reactivated under a new session
/// epoch never serves artwork decoded under a previous epoch's identity,
/// and artwork decoded before a library content change is never served
/// afterwards.
fn paint_cached_texture(
    controller: &AlbumArtController,
    cell_state: &AlbumArtCellState,
    candidate: Option<&AlbumArtCandidate>,
    pixel_size: i32,
    label_text: &str,
    accessible_label: &str,
) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let Some(texture) = controller.cache().get(
        candidate.source_id.as_ref(),
        candidate.source_session_epoch,
        &candidate.track_id,
        pixel_size,
    ) else {
        return false;
    };
    let generation = cell_state.current_generation().next();
    cell_state.generation.set(generation);
    cell_state
        .cell
        .show_texture(&texture, label_text, Some(accessible_label));
    *cell_state.bound_album_key.borrow_mut() = Some(candidate.track_id.clone());
    *cell_state.bound_source.borrow_mut() = candidate.source_id;
    true
}

/// Cache-miss half of the bind path: paint the placeholder, advance the
/// cell's generation, and schedule the fetch.
///
/// The placeholder is set unconditionally — even for the synthetic "All"
/// row and for rows whose album has no resolvable candidate. The bind
/// factory is the canonical storage point for the placeholder state, so
/// every cell leaves the bind path with a stable visual fallback (icon
/// set, paintable cleared, accessible label set).
fn stage_placeholder_fetch(
    controller: &AlbumArtController,
    cell_state: &AlbumArtCellState,
    candidate: Option<AlbumArtCandidate>,
    label_text: &str,
    accessible_label: &str,
) {
    let generation = cell_state.current_generation().next();
    cell_state.generation.set(generation);
    cell_state
        .cell
        .show_placeholder(label_text, Some(accessible_label));
    *cell_state.bound_album_key.borrow_mut() = candidate.as_ref().map(|cand| cand.track_id.clone());
    *cell_state.bound_source.borrow_mut() = None;

    if let Some(candidate) = candidate {
        controller.spawn_fetch(cell_state.clone(), candidate, generation);
    }
}

/// Paint one resolved artwork outcome onto the row's image. Extracted
/// from `spawn_fetch` so the spawned future stays a thin orchestration
/// shell: each arm maps one [`ResolvedArtKind`] variant onto the
/// matching album-art worker entry point — always through the supplied
/// per-request liveness token, never the process-wide header generation,
/// so concurrent rows fetch independently — and the no-artwork case
/// leaves the existing placeholder visible.
fn paint_resolved_art(
    resolved: ResolvedArtKind,
    image: &gtk::Image,
    liveness: &album_art::ScopedArtFetch,
) {
    match resolved {
        ResolvedArtKind::NoArtwork => {
            // Leave the placeholder visible.
        }
        ResolvedArtKind::ResolvedFile { media } => {
            // Retained-authority extraction: the worker clones the
            // already-authorized file handle; it never reopens a
            // pathname. The fetch's scoped token stops the extractor
            // and drops the reply if the row is re-bound mid-flight.
            album_art::update_resolved_file_album_art_scoped(image, media, liveness);
        }
        ResolvedArtKind::DirectFile { uri } => {
            // Transitional path for rows with NO retained authority
            // chain (e.g., OS-opened external files). Rows carrying a
            // source identity never reach this arm — see
            // [`resolve_kind`] and [`LocalFileArtRoute`].
            album_art::update_direct_file_album_art_scoped(image, &uri, liveness);
        }
        ResolvedArtKind::DirectUrl { url } => {
            album_art::fetch_remote_album_art_scoped(image, &url, liveness);
        }
        ResolvedArtKind::ResolvedRequest(request) => {
            image.set_icon_name(Some(FALLBACK_PLACEHOLDER_ICON));
            album_art::fetch_resolved_album_art_scoped(image, *request, liveness);
        }
    }
}

enum ResolvedArtKind {
    NoArtwork,
    /// A retained exact-file capability resolved through the source
    /// registry — the pane's authority-correct local extraction input.
    ResolvedFile {
        media: crate::local::resolver::ResolvedLocalMedia,
    },
    /// Transitional raw `file://` path for rows with no retained
    /// authority chain. Never produced for rows that carry a source
    /// identity.
    DirectFile {
        uri: String,
    },
    DirectUrl {
        url: String,
    },
    ResolvedRequest(Box<ResolvedHttpRequest>),
}

/// Which extraction route a `file://` album row must take.
///
/// A row that carries a registry identity (source + session epoch) MUST
/// resolve its artwork through the retained local-media authority: the
/// former code returned the raw `file://` URI and freshly opened its
/// pathname, silently bypassing retained removable-media authority
/// (2026-09-10 review finding). Only rows with no authority chain at all
/// (external OS-opened files) keep the transitional direct path.
#[derive(Debug, PartialEq, Eq)]
enum LocalFileArtRoute {
    /// Resolve `ResolvedLocalMedia` through the registry, then extract
    /// through the retained handle.
    RetainedAuthority,
    /// No authority chain exists; keep the transitional direct path.
    TransitionalDirect,
}

fn local_file_art_route(has_registry_identity: bool) -> LocalFileArtRoute {
    if has_registry_identity {
        LocalFileArtRoute::RetainedAuthority
    } else {
        LocalFileArtRoute::TransitionalDirect
    }
}

/// The pane's registry-backed identity for one row: the registry handle,
/// the source id, and the source session epoch.
type PaneRegistryIdentity<'a> = (&'a crate::source_registry::SourceRegistry, SourceId, u64);

/// Build the architecture `TrackId` for one pane resolution, logging and
/// rejecting the row when its track id is invalid.
fn pane_track_id(candidate: &AlbumArtCandidate) -> Option<crate::architecture::TrackId> {
    match crate::architecture::TrackId::new(candidate.track_id.clone()) {
        Ok(track_id) => Some(track_id),
        Err(error) => {
            tracing::debug!(
                %error,
                track_id = %candidate.track_id,
                "Album pane skipped invalid track id while resolving artwork"
            );
            None
        }
    }
}

async fn resolve_kind(
    source_registry: Option<crate::source_registry::SourceRegistry>,
    source_id: Option<SourceId>,
    source_epoch: Option<u64>,
    candidate: &AlbumArtCandidate,
    cover_art_url: String,
    uri: String,
) -> ResolvedArtKind {
    let registry_identity = match (source_registry.as_ref(), source_id, source_epoch) {
        (Some(registry), Some(id), Some(epoch)) => Some((registry, id, epoch)),
        _ => None,
    };
    // Retained local-media authority first: a row whose playable locator
    // is a file:// URI keeps its authority through the album pane — no
    // remote resolver is consulted first, no opaque credentials are
    // minted, and no pathname is reopened.
    if uri.starts_with("file://") {
        if let Some(resolved) =
            resolve_local_file_art(registry_identity.as_ref(), candidate, &uri).await
        {
            return resolved;
        }
    }
    // Lease-isolated remote resolver.
    if let Some(resolved) =
        resolve_remote_artwork(registry_identity.as_ref(), candidate, &cover_art_url).await
    {
        return resolved;
    }
    // Legacy direct URL fallback for rows that ship one and do not
    // resolve through any source registry.
    if !cover_art_url.is_empty() {
        return ResolvedArtKind::DirectUrl { url: cover_art_url };
    }
    ResolvedArtKind::NoArtwork
}

/// The `file://` arm of the pane resolver. A row that carries a registry
/// identity resolves an exact retained file capability and extracts
/// through it — never a reopened pathname (2026-09-10 review finding).
/// Only rows with no authority chain at all (external OS-opened files)
/// take the transitional direct path.
///
/// Returns `None` when resolution should fall through to the remote
/// artwork resolver: the authority reported the stream as remote, or the
/// retained authority refused and the lease-isolated route may still
/// serve the artwork. Any `Some(_)` is terminal.
async fn resolve_local_file_art(
    registry_identity: Option<&PaneRegistryIdentity<'_>>,
    candidate: &AlbumArtCandidate,
    uri: &str,
) -> Option<ResolvedArtKind> {
    match local_file_art_route(registry_identity.is_some()) {
        LocalFileArtRoute::RetainedAuthority => {
            let Some((registry, id, epoch)) = registry_identity else {
                // Unreachable by construction — the route above was chosen
                // from this very predicate — but fail closed regardless:
                // never fall back to a raw pathname open.
                return Some(ResolvedArtKind::NoArtwork);
            };
            resolve_retained_file_art(registry, id, *epoch, candidate).await
        }
        LocalFileArtRoute::TransitionalDirect => {
            // Transitional path for rows with NO retained authority
            // chain (e.g., OS-opened external files).
            Some(ResolvedArtKind::DirectFile {
                uri: uri.to_string(),
            })
        }
    }
}

/// Resolve one identity-carrying `file://` row through the retained
/// local-media authority: an exact retained file capability is resolved
/// and the artwork extracted through it — never a reopened pathname.
///
/// Returns `None` when resolution should fall through to the remote
/// artwork resolver: the authority reported the stream as remote, or the
/// retained authority refused and the lease-isolated route may still
/// serve the artwork. Any `Some(_)` is terminal.
async fn resolve_retained_file_art(
    registry: &crate::source_registry::SourceRegistry,
    id: &SourceId,
    epoch: u64,
    candidate: &AlbumArtCandidate,
) -> Option<ResolvedArtKind> {
    let Some(track_id) = pane_track_id(candidate) else {
        // Fail closed for an invalid id — never fall back to a raw
        // pathname open.
        return Some(ResolvedArtKind::NoArtwork);
    };
    match registry.resolve_stream(*id, epoch, track_id).await {
        Ok(crate::source_registry::ResolvedSourceStream::File(media)) => {
            Some(ResolvedArtKind::ResolvedFile { media })
        }
        Ok(crate::source_registry::ResolvedSourceStream::Http(_)) => {
            // The source's at-use resolution says this track's
            // stream is remote, so the file:// locator is stale;
            // defer to the remote artwork resolution below
            // instead of opening the stale path.
            tracing::debug!(
                source_id = %id,
                track_id = %candidate.track_id,
                "Album pane file row resolved to a remote stream; deferring to remote artwork"
            );
            None
        }
        Err(error) => {
            // The retained authority refused resolution. Fail
            // closed for the local path — never fall back to a
            // raw pathname open — and let the remote artwork
            // resolution below try the lease-isolated route.
            tracing::debug!(
                %error,
                source_id = %id,
                track_id = %candidate.track_id,
                "Album pane retained artwork authority unavailable"
            );
            None
        }
    }
}

/// The remote arm of the pane resolver: one lease-isolated
/// [`SourceRegistry::resolve_artwork`] call for a registry-backed row.
/// Returns `None` when resolution should fall through to the legacy
/// direct-URL path (no registry identity, or the backend errored);
/// any `Some(_)` is terminal.
async fn resolve_remote_artwork(
    registry_identity: Option<&PaneRegistryIdentity<'_>>,
    candidate: &AlbumArtCandidate,
    cover_art_url: &str,
) -> Option<ResolvedArtKind> {
    let (registry, id, epoch) = registry_identity?;
    let Some(track_id) = pane_track_id(candidate) else {
        return Some(ResolvedArtKind::NoArtwork);
    };
    match registry.resolve_artwork(*id, *epoch, track_id).await {
        Ok(Some(request)) => Some(ResolvedArtKind::ResolvedRequest(Box::new(request))),
        Ok(None) => {
            // Remote source returned no artwork for this track — try
            // the legacy embedded cover URL on the row before giving
            // up, so a row that has both a remote and a URL still
            // gets a thumbnail.
            if !cover_art_url.is_empty() {
                return Some(ResolvedArtKind::DirectUrl {
                    url: cover_art_url.to_string(),
                });
            }
            Some(ResolvedArtKind::NoArtwork)
        }
        Err(error) => {
            tracing::debug!(
                %error,
                source_id = %id,
                track_id = %candidate.track_id,
                "Album pane artwork resolver fell back after backend error"
            );
            None
        }
    }
}

/// Cache-admission identity for one bind: everything the cache probe
/// needs to key and charge an admitted texture. Grouped into a struct so
/// the probe installer takes one identity argument instead of four
/// loose ones (clippy::too_many_arguments) and so a future key field has
/// exactly one place to be added.
struct CacheAdmission {
    cache: AlbumArtCache,
    source: Option<SourceId>,
    source_epoch: Option<u64>,
    /// Library content generation captured at fetch-schedule time. A
    /// FullSync that lands mid-flight bumps the cache's generation, and
    /// this older-generation texture then inserts under a key the new
    /// generation can never query — changed covers re-resolve instead of
    /// serving the pre-sync pixels (2026-09-10 review finding).
    content_generation: u64,
    album_key: String,
    pixel_size: i32,
}

/// Apply the live pixel size to a cell's image and revoke any in-flight
/// fetch from the previous bind, in that order, returning the pixel size.
///
/// `AlbumArtCell::new` requests the icon-theme default, so a bind is
/// what tells GTK the actual side length the placeholder and the
/// eventual texture should render at. The revoke must happen before the
/// rebind mints a new generation: the flag is the cancellation primitive
/// the spawned future checks between `.await` points, and the new
/// generation token is the post-await gate that decides whether a result
/// (if any races past the flag) is allowed to paint. Both must be set,
/// in this order, on every rebind.
fn rebind_cell(controller: &AlbumArtController, cell_state: &AlbumArtCellState) -> i32 {
    let pixel_size = controller.current_pixel_size();
    cell_state.cell.image.set_pixel_size(pixel_size);
    cell_state.revoke();
    pixel_size
}

/// Disconnect the cell's previous `paintable` listener, if any.
///
/// The same `gtk::Image` is reused across multiple binds in a virtualized
/// list, so without this every rebind would leave its predecessor's
/// listener attached and the cache would observe the same paintable
/// change N times.
fn disconnect_paintable_listener(image: &gtk::Image, cell_state: &AlbumArtCellState) {
    if let Some(previous) = cell_state.paintable_notify_id.borrow_mut().take() {
        image.disconnect(previous);
    }
}

fn install_cache_probe(
    admission: CacheAdmission,
    image: gtk::Image,
    cell_state: AlbumArtCellState,
    generation: BindGeneration,
    liveness: album_art::ScopedArtFetch,
) {
    // The album-art worker calls `set_paintable` synchronously from the
    // GTK main thread when its fetch succeeds. Listening for the
    // `paintable` property change is therefore the cheapest way to know
    // a fresh texture is installed — no additional worker plumbing.
    disconnect_paintable_listener(&image, &cell_state);

    let CacheAdmission {
        cache,
        source,
        source_epoch,
        content_generation,
        album_key,
        pixel_size,
    } = admission;
    let gen = generation;
    let state = cell_state.clone();
    let handler_id = image.connect_notify_local(Some("paintable"), move |img, _| {
        // All three gates must agree before we cache: a stale generation,
        // a revoked fetch, or a revoked worker-side token must not
        // pollute the cache with a texture the user never sees.
        if state.is_revoked() || state.current_generation() != gen || !liveness.is_live() {
            return;
        }
        if let Some(paintable) = img.paintable() {
            if let Ok(texture) = paintable.downcast::<gdk::Texture>() {
                cache.insert(
                    source.as_ref(),
                    source_epoch,
                    content_generation,
                    &album_key,
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

    /// A `file://` row that carries a registry identity (source id +
    /// session epoch) must resolve retained authority — the former code
    /// returned the raw file:// URI and freshly opened its pathname,
    /// silently bypassing retained removable-media authority
    /// (2026-09-10 review finding). This decision seam is the gate: the
    /// raw path is unreachable for identity rows.
    #[test]
    fn pane_file_rows_with_source_identity_resolve_retained_authority() {
        assert_eq!(
            local_file_art_route(true),
            LocalFileArtRoute::RetainedAuthority,
            "identity rows must extract through retained authority, never the raw path"
        );
    }

    /// Rows with no authority chain (e.g., OS-opened external files)
    /// keep the transitional direct path: there is no retained
    /// capability to resolve, and the raw path is their documented
    /// transitional mechanism.
    #[test]
    fn pane_file_rows_without_identity_keep_the_transitional_direct_path() {
        assert_eq!(
            local_file_art_route(false),
            LocalFileArtRoute::TransitionalDirect,
            "authority-less rows keep the transitional direct path"
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
