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
                // The retained route reaches adapter code that polls Tokio
                // time/blocking APIs (`acquire_retained_probe_permit`
                // constructs a `tokio::time::timeout_at`), which panics on
                // the runtime-less GTK main context this fetch is driven on.
                // Dispatch it onto the application runtime and abort it if
                // the row is revoked mid-flight; a revoked or
                // missing-runtime row fails closed (2026-09-17 review
                // finding). Only a genuine remote/refused result may fall
                // through to the lease-isolated remote resolver below.
                let owned_registry = registry.clone();
                let owned_candidate = candidate.clone();
                match resolve_on_application_runtime(rt_handle.clone(), liveness, async move {
                    resolve_retained_file_art(&owned_registry, &id, epoch, &owned_candidate).await
                })
                .await
                {
                    RuntimeResolution::Completed(Some(resolved)) => return resolved,
                    // `Ok(Http)` or an error means the retained local route
                    // did not produce a file capability; fall through to the
                    // lease-isolated remote resolver for this registry-backed
                    // row. Never a raw pathname.
                    RuntimeResolution::Completed(None) => {}
                    RuntimeResolution::Aborted => return ResolvedArtKind::NoArtwork,
                }
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

/// Outcome of one resolution dispatched onto the application runtime.
enum RuntimeResolution<T> {
    /// The runtime-hosted work ran to completion and produced `T`.
    Completed(T),
    /// The row was revoked before or while the work ran, or no application
    /// runtime was attached. The caller must fail closed.
    Aborted,
}

/// Run one resolution on the application's Tokio runtime, cancelling it the
/// moment the row's liveness token is revoked.
///
/// The retained registry route reaches adapter code that awaits Tokio
/// time/blocking APIs ([`crate::local::resolver::acquire_retained_probe_permit`]
/// constructs a `tokio::time::timeout_at`), which panics on the runtime-less
/// GTK main context the pane fetch is driven on (`src/main.rs` parks the
/// runtime on a background thread). Merely carrying a runtime handle does not
/// enter it, so the work is spawned onto the handle. A revoked row aborts the
/// pending task, and a missing runtime fails closed instead of polling those
/// APIs from the main context (2026-09-17 review finding).
async fn resolve_on_application_runtime<T>(
    rt_handle: Option<tokio::runtime::Handle>,
    liveness: &album_art::ScopedArtFetch,
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> RuntimeResolution<T>
where
    T: Send + 'static,
{
    // Bound admission: a row whose fetch was already revoked (rebind,
    // unbind, teardown, factory swap) never schedules resolution work at
    // all, so rapid scrolling cannot pile up authority probes for rows
    // that can no longer paint (2026-09-14 review finding).
    if !liveness.is_live() {
        return RuntimeResolution::Aborted;
    }
    let Some(rt_handle) = rt_handle else {
        // No application runtime is attached (the controller was built
        // without one). Retained extraction needs the runtime's timer and
        // blocking pool, so fail closed to the placeholder instead of
        // polling those APIs from the main context and panicking.
        return RuntimeResolution::Aborted;
    };
    match run_until_revoked(&rt_handle, liveness, work).await {
        Some(value) => RuntimeResolution::Completed(value),
        None => RuntimeResolution::Aborted,
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
mod tests;
