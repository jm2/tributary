//! One pane fetch: orchestration, painting, and the cache-admission
//! probe.
//!
//! Split verbatim from `album_pane_art.rs` so each module stays under
//! the file-size budget. The liveness/generation gates, the
//! per-row scoped fetch tokens, and the three-way cache-admission
//! agreement (not revoked, current generation, live token) are
//! unchanged.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::architecture::SourceId;
use crate::ui::album_art;
use crate::ui::objects::AlbumArtCandidate;

use super::resolver::{resolve_kind, ResolvedArtKind};
use super::{
    AlbumArtCache, AlbumArtCellState, AlbumArtController, BindGeneration, FALLBACK_PLACEHOLDER_ICON,
};

/// Everything one pane fetch needs to paint and (on success) admit its
/// texture into the cache. Grouped so the orchestration function takes a
/// single identity argument instead of a long parameter list.
pub(super) struct PaneFetch {
    pub(super) image: gtk::Image,
    pub(super) cache: AlbumArtCache,
    pub(super) source_registry: Rc<RefCell<Option<crate::source_registry::SourceRegistry>>>,
    pub(super) album_source: Option<SourceId>,
    pub(super) source_epoch: Option<u64>,
    /// Configured library roots, snapshotted on the main thread when the
    /// fetch was scheduled. The built-in local library's retained
    /// authority resolves against exactly these roots; the async
    /// resolver must not re-read GTK/preferences state.
    pub(super) configured_roots: Vec<String>,
    pub(super) album_key: String,
    pub(super) pixel_size: i32,
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
pub(super) fn orchestrate_pane_fetch(
    fetch: PaneFetch,
    cell_state: AlbumArtCellState,
    candidate: AlbumArtCandidate,
    generation: BindGeneration,
    liveness: album_art::ScopedArtFetch,
) {
    let registry_handle = fetch.source_registry.borrow().clone();
    // Capture the catalogue content generation SYNCHRONOUSLY, before the
    // resolver's first await. `finish_pane_fetch` used to sample
    // `cache.content_generation()` only AFTER awaiting the resolver and
    // dispatching the paint, so a FullSync that landed during that await
    // labelled a texture resolved from the OLD catalogue with the NEW
    // generation — exactly the schedule-time/admission-time disagreement
    // `CacheAdmission` documents against (2026-09-12 review finding). The
    // schedule-time generation is carried through paint and admission, and
    // admission additionally rejects the result if the generation moved.
    let content_generation = fetch.cache.content_generation();
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
            fetch.configured_roots.clone(),
            &candidate,
            candidate.cover_art_url.clone(),
            candidate.uri.clone(),
        )
        .await;
        finish_pane_fetch(
            fetch,
            cell_state,
            generation,
            liveness,
            resolved,
            content_generation,
        );
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
    content_generation: u64,
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

    // Install the cache-admission listener BEFORE dispatching the paint.
    // The painter hands work to the persistent album-art worker, and the
    // worker's reply is delivered on the main context; connecting the
    // listener first makes the "no synchronous paint can precede listener
    // installation" invariant explicit instead of relying on every
    // current reply path happening to be deferred (2026-09-12 review
    // finding). The probe admits only a `gdk::Texture` — the placeholder
    // icon is a `GtkIconPaintable`, so the explicit placeholder set in
    // [`paint_resolved_art`] cannot seed the cache.
    install_cache_probe(
        CacheAdmission {
            cache,
            source: album_source,
            source_epoch,
            // Schedule-time catalogue identity, captured before the
            // resolver awaited; admission rejects the result if the live
            // generation has since moved.
            content_generation,
            album_key,
            pixel_size,
        },
        image.clone(),
        cell_state.clone(),
        generation,
        liveness.clone(),
    );

    paint_resolved_art(resolved, &image, &liveness);
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
            // [`resolve_kind`] and [`super::resolver::PaneAuthority`].
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

/// Cache-admission identity for one bind: everything the cache probe
/// needs to key and charge an admitted texture. Grouped into a struct so
/// the probe installer takes one identity argument instead of four
/// loose ones (clippy::too_many_arguments) and so a future key field has
/// exactly one place to be added.
struct CacheAdmission {
    pub(super) cache: AlbumArtCache,
    source: Option<SourceId>,
    pub(super) source_epoch: Option<u64>,
    /// Library content generation captured at fetch-schedule time. A
    /// FullSync that lands mid-flight bumps the cache's generation, and
    /// this older-generation texture then inserts under a key the new
    /// generation can never query — changed covers re-resolve instead of
    /// serving the pre-sync pixels (2026-09-10 review finding).
    content_generation: u64,
    pub(super) album_key: String,
    pub(super) pixel_size: i32,
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
pub(super) fn rebind_cell(controller: &AlbumArtController, cell_state: &AlbumArtCellState) -> i32 {
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
pub(super) fn disconnect_paintable_listener(image: &gtk::Image, cell_state: &AlbumArtCellState) {
    if let Some(previous) = cell_state.paintable_notify_id.borrow_mut().take() {
        image.disconnect(previous);
    }
}

/// Whether a texture resolved against `scheduled_generation` may be
/// admitted now that the live catalogue generation is
/// `current_generation`.
///
/// The catalogue generation is sampled synchronously when the fetch is
/// scheduled and carried through resolution and paint. A `FullSync` that
/// lands while the resolver is awaiting advances the cache's generation,
/// so a texture decoded from the pre-sync candidate must not enter the
/// cache under the new catalogue: equality is required. The row still
/// displays the texture it received; only its cache retention is
/// declined, and the next bind re-resolves against the new catalogue
/// (2026-09-12 review finding).
fn admit_scheduled_generation(scheduled_generation: u64, current_generation: u64) -> bool {
    scheduled_generation == current_generation
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
        // All gates must agree before we cache: a stale generation,
        // a revoked fetch, or a revoked worker-side token must not
        // pollute the cache with a texture the user never sees.
        if state.is_revoked() || state.current_generation() != gen || !liveness.is_live() {
            return;
        }
        // Reject a texture resolved from the pre-FullSync catalogue: the
        // schedule-time generation is authoritative for this result, and
        // the live generation moving means the catalogue changed while the
        // resolution was pending (2026-09-12 review finding). The row
        // still displays the texture; it is simply not admitted, so the
        // next bind re-resolves against the new catalogue.
        if !admit_scheduled_generation(content_generation, cache.content_generation()) {
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

    /// A texture resolved before an interposed `FullSync` must not be
    /// admitted once the catalogue generation has moved. The fetch
    /// captures the generation synchronously at schedule time
    /// (`orchestrate_pane_fetch`), the probe compares it against the live
    /// generation at admission (`install_cache_probe`), and this gate is
    /// that comparison. Without it a pre-sync texture would be inserted
    /// under a generation the new catalogue never queries — a silent
    /// stale admission (2026-09-12 review finding).
    #[test]
    fn interposed_fullsync_rejects_a_pre_sync_admission() {
        let cache = AlbumArtCache::new();

        // Schedule time: capture the catalogue generation before the
        // resolver's first await.
        let scheduled = cache.content_generation();
        assert!(
            admit_scheduled_generation(scheduled, cache.content_generation()),
            "an unperturbed fetch admits under its schedule-time generation"
        );

        // FullSync lands while the resolution is still pending and bumps
        // the catalogue generation.
        cache.bump_content_generation();

        assert!(
            !admit_scheduled_generation(scheduled, cache.content_generation()),
            "a pre-FullSync texture must not be admitted under the new catalogue"
        );

        // The next bind schedules against the new generation and admits.
        let rescheduled = cache.content_generation();
        assert!(admit_scheduled_generation(
            rescheduled,
            cache.content_generation()
        ));
    }
}
