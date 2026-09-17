//! Isolated built-in-local artwork resolution tests.
//!
//! R1 (2026-09-17 refinery audit): the replaced
//! `builtin_local_artwork_resolves_on_the_application_runtime` fixture
//! called the production `init_db()` seam, so a normal `cargo test`
//! run created, migrated, and opened the real user library at
//! `dirs::data_dir()/tributary/library.db`; its single `NoArtwork`
//! assertion also accepted a database error, so the test could pass
//! without exercising the retained-authority work at all. These tests
//! resolve through the [`super::LocalLibrary::Injected`] seam against
//! a private in-memory library and an authorized temporary root whose
//! track carries real embedded artwork.

use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use sea_orm_migration::MigratorTrait;

use super::*;
use gtk::glib;

/// A private in-memory library: no database file exists anywhere on
/// disk for this connection, so nothing can be created, migrated, or
/// changed outside the fixture.
pub(super) async fn memory_library() -> DatabaseConnection {
    sea_orm::Database::connect("sqlite::memory:")
        .await
        .expect("open in-memory fixture library")
}

/// The fully migrated private fixture library. Migrations run against
/// the in-memory connection only.
async fn migrated_memory_library() -> DatabaseConnection {
    let db = memory_library().await;
    crate::db::migration::Migrator::up(&db, None)
        .await
        .expect("migrate fixture library");
    db
}

/// Seed one authoritative local root (marker file + authoritative
/// `library_root` row) and one local track with real embedded artwork
/// beneath it. Returns the temporary root and the embedded art bytes.
async fn seed_authorized_local_track(
    db: &DatabaseConnection,
    track_id: &str,
) -> (tempfile::TempDir, Vec<u8>) {
    let root = tempfile::tempdir().expect("temporary library root");
    let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
    std::fs::write(
        root.path().join(".tributary-root-id"),
        format!("{marker}\n"),
    )
    .expect("write root marker");
    crate::db::entities::library_root::ActiveModel {
        path: Set(root.path().to_string_lossy().into_owned()),
        device_id: Set(Some(marker)),
        identity_confirmed: Set(true),
        is_available: Set(true),
        last_scan_complete: Set(true),
        last_checked_at: Set("2026-07-17T00:00:00+00:00".to_string()),
    }
    .insert(db)
    .await
    .expect("insert authoritative fixture root");

    let track_path = root.path().join("01.flac");
    let art = b"retained-local-cover-art".to_vec();
    super::super::tests::tagged_flac_with_embedded_art(&track_path, &art);
    crate::db::entities::track::ActiveModel {
        id: Set(track_id.to_string()),
        file_path: Set(track_path.to_string_lossy().into_owned()),
        title: Set("Title".to_string()),
        artist_name: Set("Artist".to_string()),
        album_title: Set("Album".to_string()),
        play_count: Set(0),
        last_played_at_ms: Set(None),
        date_added: Set("2026-07-17T00:00:00+00:00".to_string()),
        date_modified: Set("2026-07-17T00:00:00+00:00".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert fixture track");

    (root, art)
}

/// A built-in local row as the pane binds them: `SourceId::local()`
/// with no session epoch and a `file://` locator.
fn local_candidate(track_id: &str, uri: &str) -> AlbumArtCandidate {
    AlbumArtCandidate {
        track_id: track_id.to_string(),
        uri: uri.to_string(),
        cover_art_url: String::new(),
        source_id: Some(SourceId::local()),
        source_session_epoch: None,
    }
}

/// Drive one resolution from a GLib main context on a thread with no
/// entered Tokio runtime — exactly the production pane-fetch shape —
/// so [`crate::local::resolver::resolve_track`]'s runtime-bound APIs
/// are reached only through the supplied application handle.
fn resolve_from_glib_without_runtime(
    configured_roots: Vec<String>,
    rt_handle: Option<tokio::runtime::Handle>,
    library: LocalLibrary,
    candidate: &AlbumArtCandidate,
    liveness: &album_art::ScopedArtFetch,
) -> ResolvedArtKind {
    std::thread::spawn({
        let candidate = candidate.clone();
        let liveness = liveness.clone();
        move || {
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "the pane resolution thread must have no entered Tokio runtime"
            );
            let context = glib::MainContext::new();
            context.block_on(resolve_kind(
                None,
                candidate.source_id,
                candidate.source_session_epoch,
                configured_roots,
                rt_handle,
                library,
                &candidate,
                &liveness,
            ))
        }
    })
    .join()
    .expect("pane resolution thread")
}

/// R1 correction (2026-09-17 refinery audit): the built-in local arm
/// must resolve a real track successfully through the retained
/// authority on the application runtime — driven from a GLib main
/// context with no entered runtime — and the embedded artwork must
/// extract through the retained file capability. The fixture track
/// exists only in the injected in-memory library, so a successful
/// retained-file resolution is itself the proof that no library
/// outside the fixture was consulted.
#[test]
fn builtin_local_artwork_resolves_retained_embedded_art_on_the_application_runtime() {
    let runtime = tokio::runtime::Runtime::new().expect("application tokio runtime");
    let handle = runtime.handle().clone();

    let (library, root, art, candidate) = runtime.block_on(async {
        let db = migrated_memory_library().await;
        let (root, art) = seed_authorized_local_track(&db, "track-1").await;
        let candidate = local_candidate("track-1", "file:///media/music/album/01.flac");
        (LocalLibrary::Injected(db), root, art, candidate)
    });
    let configured_roots = vec![root.path().to_string_lossy().into_owned()];
    let liveness = album_art::ScopedArtFetch::new();

    let resolved = resolve_from_glib_without_runtime(
        configured_roots,
        Some(handle),
        library,
        &candidate,
        &liveness,
    );

    let ResolvedArtKind::ResolvedFile { media } = resolved else {
        panic!("a seeded built-in local row must resolve retained-file artwork");
    };
    assert_eq!(
        album_art::extract_resolved_file_album_art_bytes(&media).as_deref(),
        Some(art.as_slice()),
        "embedded art must extract through the retained file capability"
    );
}

/// The missing-track failure path, isolated: a built-in local row whose
/// id is absent from the injected fixture library leaves the
/// placeholder, and the failure is deterministic (row missing, not a
/// database error) — the former R1 fixture could not distinguish the
/// two because it also accepted a database error.
#[test]
fn missing_builtin_local_track_leaves_the_placeholder() {
    let runtime = tokio::runtime::Runtime::new().expect("application tokio runtime");
    let handle = runtime.handle().clone();

    let (library, root, _art, candidate) = runtime.block_on(async {
        let db = migrated_memory_library().await;
        let (root, art) = seed_authorized_local_track(&db, "seeded-track").await;
        let candidate = local_candidate("absent-track", "file:///media/music/album/01.flac");
        (LocalLibrary::Injected(db), root, art, candidate)
    });
    let configured_roots = vec![root.path().to_string_lossy().into_owned()];
    let liveness = album_art::ScopedArtFetch::new();

    let resolved = resolve_from_glib_without_runtime(
        configured_roots,
        Some(handle),
        library,
        &candidate,
        &liveness,
    );

    assert!(
        matches!(resolved, ResolvedArtKind::NoArtwork),
        "a missing track must leave the placeholder"
    );
}
