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
/// `cover_art_url`, and never reopen the raw `file://` pathname.
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
        LocalLibrary::Shared,
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
        LocalLibrary::Shared,
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
/// fabricating one.
#[tokio::test]
async fn external_rows_keep_the_transitional_direct_path() {
    let liveness = album_art::ScopedArtFetch::new();
    let file_row = candidate("file:///tmp/external.flac", "", None, None);
    let resolved = resolve_kind(
        None,
        None,
        None,
        Vec::new(),
        None,
        LocalLibrary::Shared,
        &file_row,
        &liveness,
    )
    .await;
    assert!(matches!(resolved, ResolvedArtKind::DirectFile { .. }));

    let url_row = candidate("", "https://example.test/cover.jpg", None, None);
    let resolved = resolve_kind(
        None,
        None,
        None,
        Vec::new(),
        None,
        LocalLibrary::Shared,
        &url_row,
        &liveness,
    )
    .await;
    assert!(matches!(resolved, ResolvedArtKind::DirectUrl { .. }));

    let empty_row = candidate("", "", None, None);
    let resolved = resolve_kind(
        None,
        None,
        None,
        Vec::new(),
        None,
        LocalLibrary::Shared,
        &empty_row,
        &liveness,
    )
    .await;
    assert!(matches!(resolved, ResolvedArtKind::NoArtwork));
}

// The built-in local resolution tests live in `super::local_library_tests`.
// They resolve against an injected in-memory library with an authorized
// temporary root, never the production `init_db()` seam (which would
// create, migrate, and open the real user library), and assert a successful
// retained-file resolution with real embedded-art extraction.

/// Regression: a built-in-local row whose token is revoked mid-resolution must
/// not continue its resolution. This drives the runtime-handoff cancellation
/// seam with a controllable future, so cancellation — not merely the final
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

/// The registry retained-file arm relies on the same runtime dispatch
/// primitive as the built-in local arm: work hosted on the application
/// runtime must be cancelled the moment the row's token is revoked, so
/// a rebind that lands while a retained authority probe is still
/// waiting does not run the probe to completion for a row that can no
/// longer paint.
#[test]
fn revoked_registry_resolution_on_runtime_is_cancelled() {
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
        // Park where the real retained authority probe awaits its
        // permit/blocking work.
        let _ = release_rx.await;
        work_completed.store(true, Ordering::SeqCst);
        0usize
    };

    // Revoke only once the work is definitely running, exactly like a
    // rebind that lands while the row's resolution is in flight.
    let revoker_token = liveness.clone();
    let revoker_started = started.clone();

    let outcome = runtime.block_on(async move {
        let revoker = tokio::spawn(async move {
            while !revoker_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            revoker_token.revoke();
        });
        let outcome = resolve_on_application_runtime(Some(handle), &liveness, work).await;
        revoker.await.expect("revoker task");
        outcome
    });

    assert!(
        matches!(outcome, RuntimeResolution::Aborted),
        "a revoked registry resolution must abort rather than complete"
    );
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
    // An injected (empty, in-memory) library keeps even a regressed
    // admission order from touching the real user library: the revoked
    // check fires before any database access, and the injected path can
    // never reach `init_db` at all.
    let library = LocalLibrary::Injected(
        tokio::runtime::Runtime::new()
            .expect("fixture setup runtime")
            .block_on(super::local_library_tests::memory_library()),
    );
    let context = glib::MainContext::new();
    let resolved = context.block_on(resolve_kind(
        None,
        row.source_id,
        row.source_session_epoch,
        Vec::new(),
        None,
        library,
        &row,
        &liveness,
    ));
    assert!(matches!(resolved, ResolvedArtKind::NoArtwork));
}
