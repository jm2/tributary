//! The album pane's artwork resolution tree.
//!
//! Split verbatim from `album_pane_art.rs` so each module stays under
//! the file-size budget, then extended with an explicit authority
//! classification (2026-09-12 review finding). Every boundary is
//! unchanged in spirit: a row that carries a complete registry identity
//! resolves its `file://` artwork through the retained local-media
//! authority (exact retained file capability, never a reopened
//! pathname) and its remote artwork through the lease-isolated
//! `SourceRegistry::resolve_artwork`; the built-in local library row
//! resolves through the built-in local retained authority; only rows
//! with no authority chain at all keep the transitional direct path,
//! and every failure fails closed.

use crate::architecture::media::ResolvedHttpRequest;
use crate::architecture::SourceId;

use crate::ui::album_art;
use crate::ui::objects::AlbumArtCandidate;

pub enum ResolvedArtKind {
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

/// The authority chain one album-pane row actually carries.
///
/// The former code collapsed "the row carries a source identity but the
/// registry/epoch is missing" into "no identity", so an incomplete or
/// revoked retained identity could fall through to the row's stale raw
/// `cover_art_url` (or reopen a raw `file://` pathname). Truly external
/// rows with no source identity are the only class that may use the
/// transitional direct path; every source-bearing class must resolve
/// through its authority or leave the placeholder (2026-09-12 review
/// finding).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PaneAuthority {
    /// Registry handle, source id, and a current source session epoch.
    Registry,
    /// The built-in local library: `SourceId::local()` with no session
    /// epoch (`arch_track_to_object` never mints one). Resolves through
    /// the built-in local retained authority.
    BuiltinLocal,
    /// A retained source identity is present, but the registry handle or
    /// the session epoch is missing. Never fall back to a raw path/URL.
    IncompleteRetained,
    /// No source identity at all (e.g., OS-opened external files).
    External,
}

/// Classify one row's authority chain from its raw candidate inputs.
pub(super) fn classify_pane_authority(
    has_registry: bool,
    source_id: Option<SourceId>,
    source_epoch: Option<u64>,
) -> PaneAuthority {
    match (has_registry, source_id, source_epoch) {
        // The built-in local library is identified by its reserved source
        // id, independent of whether a registry handle is attached: it
        // has no registry session to resolve through.
        (_, Some(id), _) if id == SourceId::local() => PaneAuthority::BuiltinLocal,
        (true, Some(_), Some(_)) => PaneAuthority::Registry,
        (_, Some(_), _) => PaneAuthority::IncompleteRetained,
        (_, None, _) => PaneAuthority::External,
    }
}

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

pub async fn resolve_kind(
    source_registry: Option<crate::source_registry::SourceRegistry>,
    source_id: Option<SourceId>,
    source_epoch: Option<u64>,
    configured_roots: Vec<String>,
    rt_handle: Option<tokio::runtime::Handle>,
    candidate: &AlbumArtCandidate,
    liveness: &album_art::ScopedArtFetch,
) -> ResolvedArtKind {
    match classify_pane_authority(source_registry.is_some(), source_id, source_epoch) {
        PaneAuthority::Registry => {
            let (registry, id, epoch) = (
                source_registry
                    .as_ref()
                    .expect("registry authority requires a handle"),
                source_id.expect("registry authority requires a source id"),
                source_epoch.expect("registry authority requires a session epoch"),
            );
            // Retained local-media authority first, chosen by the live
            // adapter's authoritative capability — NOT by the row's raw
            // locator: production registry rows are pathless (their URI
            // is empty), yet a mounted removable album still resolves to a
            // retained file beneath its mount authority. The capability
            // check is cheap and synchronous, so it mints no stream
            // credential and reopens no pathname merely to discover the
            // source kind (2026-09-14 review finding).
            if registry.retains_file_streams(id, epoch) {
                if let Some(resolved) =
                    resolve_retained_file_art(registry, &id, epoch, candidate).await
                {
                    return resolved;
                }
                // `Ok(Http)` or an error means the retained local route
                // did not produce a file capability; fall through to the
                // lease-isolated remote resolver for this registry-backed
                // row. Never a raw pathname.
            }
            // Lease-isolated remote resolver. For a registry-backed row
            // this is TERMINAL: an explicit no-artwork or a refused
            // resolution leaves the placeholder rather than falling
            // through to the row's stale snapshot URL (2026-09-12 review
            // finding).
            resolve_remote_artwork(registry, &id, epoch, candidate).await
        }
        PaneAuthority::BuiltinLocal => {
            resolve_builtin_local_art_on_runtime(rt_handle, candidate, &configured_roots, liveness)
                .await
        }
        PaneAuthority::IncompleteRetained => {
            // A retained source identity whose registry handle or session
            // epoch is missing (revoked/refused/stale). Do NOT fall back
            // to the raw snapshot URL or reopen a raw file URI: leave the
            // placeholder.
            tracing::debug!(
                track_id = %candidate.track_id,
                "Album pane left placeholder for an incomplete retained identity"
            );
            ResolvedArtKind::NoArtwork
        }
        // Truly external rows with no authority chain at all (e.g.,
        // OS-opened external files) keep the transitional direct path.
        PaneAuthority::External => resolve_external_art(candidate),
    }
}

/// Resolve a truly external row (no authority chain) through the
/// transitional direct path: its raw `file://` locator then its snapshot
/// URL, else the placeholder. Extracted from [`resolve_kind`] so the
/// authority match stays inside the per-method size budget.
fn resolve_external_art(candidate: &AlbumArtCandidate) -> ResolvedArtKind {
    if candidate.uri.starts_with("file://") {
        ResolvedArtKind::DirectFile {
            uri: candidate.uri.clone(),
        }
    } else if !candidate.cover_art_url.is_empty() {
        ResolvedArtKind::DirectUrl {
            url: candidate.cover_art_url.clone(),
        }
    } else {
        ResolvedArtKind::NoArtwork
    }
}

/// The built-in local library's artwork arm.
///
/// A local row carries `SourceId::local()` with no session epoch, so it
/// has no registry session to resolve through. It must still resolve
/// through the built-in local retained authority
/// ([`crate::local::resolver::resolve_track`]) rather than reopening its
/// raw `file://` pathname; a refused/absent resolution leaves the
/// placeholder. This preserves local artwork without classifying every
/// local row as external (2026-09-12 review finding).
async fn resolve_builtin_local_art(
    candidate: &AlbumArtCandidate,
    configured_roots: &[String],
    liveness: &album_art::ScopedArtFetch,
) -> ResolvedArtKind {
    // Cheap admission gate, re-checked by the caller before scheduling and
    // again here so the task performs no work at all for a revoked row.
    if candidate.track_id.is_empty() || !liveness.is_live() {
        return ResolvedArtKind::NoArtwork;
    }
    let db = match crate::db::connection::init_db().await {
        Ok(db) => db,
        Err(error) => {
            tracing::debug!(
                %error,
                track_id = %candidate.track_id,
                "Album pane local library database unavailable"
            );
            return ResolvedArtKind::NoArtwork;
        }
    };
    // Stop before the retained-authority probe if the row was revoked
    // while the database connection was being established. The probe
    // queues `spawn_blocking` work on the shared pool; a revoked row must
    // not add to that backlog (2026-09-14 review finding).
    if !liveness.is_live() {
        return ResolvedArtKind::NoArtwork;
    }
    match crate::local::resolver::resolve_track(&db, candidate.track_id.as_str(), configured_roots)
        .await
    {
        Ok(media) => ResolvedArtKind::ResolvedFile { media },
        Err(error) => {
            tracing::debug!(
                %error,
                track_id = %candidate.track_id,
                "Album pane built-in local artwork authority unavailable"
            );
            ResolvedArtKind::NoArtwork
        }
    }
}

/// Run the built-in local library's retained-authority resolution on the
/// application's Tokio runtime.
///
/// [`crate::local::resolver::resolve_track`] polls `tokio::time::timeout`
/// and `tokio::task::spawn_blocking`. The pane fetch is driven on the GTK
/// main context, which has no entered runtime (`src/main.rs` parks the
/// runtime on a background thread), so polling those APIs there panics and
/// the row silently never loads local artwork (2026-09-13 review finding).
/// The resolution therefore runs on the runtime handle the window attaches
/// at construction, and its result is delivered back to the pane's async
/// task — mirroring the playback resolver's `rt_handle.spawn` hand-off.
async fn resolve_builtin_local_art_on_runtime(
    rt_handle: Option<tokio::runtime::Handle>,
    candidate: &AlbumArtCandidate,
    configured_roots: &[String],
    liveness: &album_art::ScopedArtFetch,
) -> ResolvedArtKind {
    // Bound admission: a row whose fetch was already revoked (rebind,
    // unbind, teardown, factory swap) never schedules resolution work at
    // all, so rapid scrolling cannot pile up authority probes for rows
    // that can no longer paint (2026-09-14 review finding).
    if !liveness.is_live() {
        return ResolvedArtKind::NoArtwork;
    }
    let Some(rt_handle) = rt_handle else {
        // No application runtime is attached (the controller was built
        // without one). Local extraction needs the runtime's timer and
        // blocking pool, so fail closed to the placeholder instead of
        // polling those APIs from the main context and panicking.
        tracing::debug!(
            track_id = %candidate.track_id,
            "Album pane built-in local artwork skipped without an attached runtime"
        );
        return ResolvedArtKind::NoArtwork;
    };
    let candidate = candidate.clone();
    let configured_roots = configured_roots.to_vec();
    let task_liveness = liveness.clone();
    let resolved = run_until_revoked(&rt_handle, liveness, async move {
        resolve_builtin_local_art(&candidate, &configured_roots, &task_liveness).await
    })
    .await;
    resolved.unwrap_or(ResolvedArtKind::NoArtwork)
}

/// Drive one runtime-hosted resolution to completion, cancelling it the
/// moment the row's liveness token is revoked.
///
/// The built-in local arm runs
/// [`crate::local::resolver::resolve_track`] on the application runtime
/// because it polls Tokio time/blocking APIs. That work is expensive (a
/// database lookup plus a five-second retained-authority probe), so a row
/// that is unbound or re-bound while its resolution is pending must stop
/// it: [`album_art::ScopedArtFetch::wait_revoked`] wakes this waiter and
/// the task is aborted before it continues past its current await. A
/// `revoke` landing while the probe's `spawn_blocking` closure is already
/// running cannot unwind that OS thread, but aborting the task stops the
/// async work that would otherwise queue behind it, and the pre-probe
/// liveness re-check in [`resolve_builtin_local_art`] avoids starting the
/// probe at all once the row is gone (2026-09-14 review finding).
async fn run_until_revoked<T>(
    handle: &tokio::runtime::Handle,
    liveness: &album_art::ScopedArtFetch,
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T>
where
    T: Send + 'static,
{
    let mut task = handle.spawn(work);
    tokio::select! {
        result = &mut task => result.ok(),
        () = liveness.wait_revoked() => {
            task.abort();
            None
        }
    }
}

/// Resolve one retained-file-capable registry row through the retained
/// local-media authority: an exact retained file capability is resolved
/// and the artwork extracted through it — never a reopened pathname.
///
/// The resolution is classed [`StreamResolutionClass::Speculative`] because
/// the album pane resolves artwork for every bound row: an adapter whose
/// resolution performs a blocking mounted probe (retained removable media)
/// must draw on the pane-bounded speculative gate and must never delay a
/// playback resolution (2026-09-14 review finding).
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
    match registry
        .resolve_stream_classified(
            *id,
            epoch,
            track_id,
            crate::source_registry::StreamResolutionClass::Speculative,
        )
        .await
    {
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
///
/// Terminal by construction: `Ok(Some(_))` produces the resolved
/// request; an explicit `Ok(None)` (authoritative no-artwork) and an
/// `Err` (refused/errored authority) both leave the placeholder. A
/// registry-backed row never regains access through its stale snapshot
/// URL (2026-09-12 review finding).
async fn resolve_remote_artwork(
    registry: &crate::source_registry::SourceRegistry,
    id: &SourceId,
    epoch: u64,
    candidate: &AlbumArtCandidate,
) -> ResolvedArtKind {
    let Some(track_id) = pane_track_id(candidate) else {
        return ResolvedArtKind::NoArtwork;
    };
    match registry.resolve_artwork(*id, epoch, track_id).await {
        Ok(Some(request)) => ResolvedArtKind::ResolvedRequest(Box::new(request)),
        Ok(None) => {
            tracing::debug!(
                source_id = %id,
                track_id = %candidate.track_id,
                "Album pane source authority reported no artwork; leaving placeholder"
            );
            ResolvedArtKind::NoArtwork
        }
        Err(error) => {
            tracing::debug!(
                %error,
                source_id = %id,
                track_id = %candidate.track_id,
                "Album pane artwork authority refused; leaving placeholder"
            );
            ResolvedArtKind::NoArtwork
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::glib;

    fn local() -> SourceId {
        SourceId::local()
    }

    /// Any non-local source identity (a network adapter, removable
    /// filesystem, or the built-in radio adapter).
    fn other_source() -> SourceId {
        SourceId::radio_browser()
    }

    /// A row with a complete registry identity resolves through the
    /// registry (retained local authority first for file rows, the
    /// lease-isolated remote resolver otherwise) — never the transitional
    /// direct path.
    #[test]
    fn complete_registry_identity_is_registry_authority() {
        assert_eq!(
            classify_pane_authority(true, Some(other_source()), Some(7)),
            PaneAuthority::Registry
        );
    }

    /// The normal built-in local library row: `SourceId::local()` with no
    /// session epoch. It must resolve through the built-in local retained
    /// authority, not be denied and not be classified as external.
    #[test]
    fn local_library_row_without_epoch_is_builtin_local() {
        assert_eq!(
            classify_pane_authority(false, Some(local()), None),
            PaneAuthority::BuiltinLocal
        );
        // A registry handle being present does not change the local
        // classification: the built-in library has no registry session.
        assert_eq!(
            classify_pane_authority(true, Some(local()), None),
            PaneAuthority::BuiltinLocal
        );
    }

    /// A retained source identity whose session epoch was revoked/stripped
    /// must fail closed, never fall through to a raw URL/path.
    #[test]
    fn retained_identity_without_epoch_is_incomplete() {
        assert_eq!(
            classify_pane_authority(true, Some(other_source()), None),
            PaneAuthority::IncompleteRetained
        );
    }

    /// A retained source identity whose registry handle is missing must
    /// also fail closed — a late/unwired registry is not "no identity".
    #[test]
    fn retained_identity_without_registry_is_incomplete() {
        assert_eq!(
            classify_pane_authority(false, Some(other_source()), Some(7)),
            PaneAuthority::IncompleteRetained
        );
    }

    /// Only rows with no authority chain at all keep the transitional
    /// direct path (external OS-opened files).
    #[test]
    fn no_source_identity_is_external() {
        assert_eq!(
            classify_pane_authority(false, None, None),
            PaneAuthority::External
        );
        assert_eq!(
            classify_pane_authority(true, None, None),
            PaneAuthority::External
        );
        // An epoch without a source id is meaningless and still external.
        assert_eq!(
            classify_pane_authority(false, None, Some(7)),
            PaneAuthority::External
        );
    }

    fn candidate(
        uri: &str,
        cover_art_url: &str,
        source_id: Option<SourceId>,
        epoch: Option<u64>,
    ) -> AlbumArtCandidate {
        AlbumArtCandidate {
            track_id: "track-1".to_string(),
            uri: uri.to_string(),
            cover_art_url: cover_art_url.to_string(),
            source_id,
            source_session_epoch: epoch,
        }
    }

    /// End-to-end resolution for an incomplete retained identity: a row
    /// that still carries a source id but whose session epoch was
    /// revoked/stripped (or whose registry handle is absent) must leave
    /// the placeholder. It must never fall through to the stale snapshot
    /// `cover_art_url`, and never reopen the raw `file://` pathname
    /// (2026-09-12 review finding).
    #[tokio::test]
    async fn incomplete_retained_identity_leaves_the_placeholder() {
        let liveness = album_art::ScopedArtFetch::new();
        // Epoch revoked while the source id survives.
        let no_epoch = candidate(
            "file:///media/music/album/01.flac",
            "https://stale.example/cover.jpg",
            Some(other_source()),
            None,
        );
        let resolved = resolve_kind(
            None,
            no_epoch.source_id,
            no_epoch.source_session_epoch,
            Vec::new(),
            None,
            &no_epoch,
            &liveness,
        )
        .await;
        assert!(
            matches!(resolved, ResolvedArtKind::NoArtwork),
            "a revoked epoch must not regain access through the stale URL"
        );

        // Registry handle missing while the source identity survives.
        let no_registry = candidate(
            "file:///media/music/album/01.flac",
            "https://stale.example/cover.jpg",
            Some(other_source()),
            Some(7),
        );
        let resolved = resolve_kind(
            None,
            no_registry.source_id,
            no_registry.source_session_epoch,
            Vec::new(),
            None,
            &no_registry,
            &liveness,
        )
        .await;
        assert!(
            matches!(resolved, ResolvedArtKind::NoArtwork),
            "an unwired registry is not 'no identity' and must fail closed"
        );
    }

    /// The genuinely external compatibility path survives: a row with NO
    /// source identity at all keeps its transitional direct locator, and
    /// a row with neither locator leaves the placeholder rather than
    /// fabricating one (2026-09-12 review finding).
    #[tokio::test]
    async fn external_rows_keep_the_transitional_direct_path() {
        let liveness = album_art::ScopedArtFetch::new();
        let file_row = candidate("file:///tmp/external.flac", "", None, None);
        let resolved = resolve_kind(None, None, None, Vec::new(), None, &file_row, &liveness).await;
        assert!(matches!(resolved, ResolvedArtKind::DirectFile { .. }));

        let url_row = candidate("", "https://example.test/cover.jpg", None, None);
        let resolved = resolve_kind(None, None, None, Vec::new(), None, &url_row, &liveness).await;
        assert!(matches!(resolved, ResolvedArtKind::DirectUrl { .. }));

        let empty_row = candidate("", "", None, None);
        let resolved =
            resolve_kind(None, None, None, Vec::new(), None, &empty_row, &liveness).await;
        assert!(matches!(resolved, ResolvedArtKind::NoArtwork));
    }

    /// The built-in local library arm must resolve on the application's
    /// Tokio runtime, never the GTK main context: the pane fetch is driven
    /// on a glib main context with no entered runtime, and
    /// [`crate::local::resolver::resolve_track`] polls
    /// `tokio::time::timeout` / `spawn_blocking`, which panic without one
    /// (2026-09-13 review finding). This drives the real resolution path on
    /// a glib context whose only runtime is the supplied handle, so the old
    /// direct call panics while the runtime hand-off resolves.
    #[test]
    fn builtin_local_artwork_resolves_on_the_application_runtime() {
        // The runtime lives on its own thread — exactly the production
        // shape, and NOT entered on the glib context thread below.
        let runtime = tokio::runtime::Runtime::new().expect("application tokio runtime");
        let handle = runtime.handle().clone();

        let row = candidate("file:///media/music/album/01.flac", "", Some(local()), None);
        let liveness = album_art::ScopedArtFetch::new();
        let context = glib::MainContext::new();
        let resolved = context.block_on(resolve_kind(
            None,
            row.source_id,
            row.source_session_epoch,
            Vec::new(),
            Some(handle),
            &row,
            &liveness,
        ));

        // The track is not in this process's library, so the retained
        // authority resolves to no artwork; the contract under test is that
        // it resolves on the runtime rather than panicking on the main
        // context.
        assert!(matches!(resolved, ResolvedArtKind::NoArtwork));
    }

    /// N3 regression (2026-09-14 review finding): a built-in-local row
    /// whose token is revoked mid-resolution must not continue its
    /// resolution. This drives the runtime-handoff cancellation seam with
    /// a controllable future, so cancellation — not merely the final
    /// placeholder — is observable: the parked work is dropped before it
    /// can complete, mirroring a rebind that revokes a row whose
    /// `resolve_track` authority probe is still pending.
    #[test]
    fn revoked_builtin_local_resolution_is_cancelled() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let runtime = tokio::runtime::Runtime::new().expect("application tokio runtime");
        let handle = runtime.handle().clone();
        let liveness = album_art::ScopedArtFetch::new();

        let started = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        let work_started = started.clone();
        let work_completed = completed.clone();
        let work = async move {
            work_started.store(true, Ordering::SeqCst);
            // Park where the real resolution awaits its authority probe.
            let _ = release_rx.await;
            work_completed.store(true, Ordering::SeqCst);
            0usize
        };

        // Revoke only once the work is definitely running, exactly like a
        // rebind that lands while the row's resolution is in flight.
        let revoker_token = liveness.clone();
        let revoker_started = started.clone();

        let result = runtime.block_on(async move {
            let revoker = tokio::spawn(async move {
                while !revoker_started.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                revoker_token.revoke();
            });
            let result = run_until_revoked(&handle, &liveness, work).await;
            revoker.await.expect("revoker task");
            result
        });

        assert_eq!(result, None, "a revoked row yields no resolved value");
        assert!(
            !completed.load(Ordering::SeqCst),
            "the runtime resolution must be cancelled once the row is revoked"
        );

        // Release the parked work so a late completion cannot leak.
        let _ = release_tx.send(());
    }

    /// A pre-revoked token must be refused at admission, before any
    /// runtime work is scheduled: the row is already gone by the time the
    /// fetch is driven.
    #[test]
    fn pre_revoked_builtin_local_row_is_refused_at_admission() {
        let liveness = album_art::ScopedArtFetch::new();
        liveness.revoke();
        let row = candidate("file:///media/music/album/01.flac", "", Some(local()), None);
        let context = glib::MainContext::new();
        let resolved = context.block_on(resolve_kind(
            None,
            row.source_id,
            row.source_session_epoch,
            Vec::new(),
            None,
            &row,
            &liveness,
        ));
        assert!(matches!(resolved, ResolvedArtKind::NoArtwork));
    }
}
