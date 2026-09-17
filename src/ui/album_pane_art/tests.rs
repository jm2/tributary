//! Regression suite for the album pane artwork resolver and pane fetch.
//!
//! Split verbatim from `album_pane_art.rs` so that module stays under the
//! file-size budget; no assertion, authority check, cache-admission
//! contract, or generation gate was altered. Every test here still drives
//! the same seam as before.
use super::*;
use crate::architecture::SourceId;
use crate::ui::preferences::AlbumArtSize;
use resolver::{
    classify_pane_authority, resolve_kind, LocalLibrary, PaneAuthority, ResolvedArtKind,
};

/// A `file://` row that carries a complete registry identity
/// (source id + session epoch + attached registry) must resolve
/// retained authority — the former code returned the raw file:// URI
/// and freshly opened its pathname, silently bypassing retained
/// removable-media authority (2026-09-10 review finding). This
/// decision seam is the gate: the raw path is unreachable for
/// identity rows.
#[test]
fn pane_file_rows_with_source_identity_resolve_retained_authority() {
    assert_eq!(
        classify_pane_authority(true, Some(SourceId::radio_browser()), Some(7)),
        PaneAuthority::Registry,
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
        classify_pane_authority(false, None, None),
        PaneAuthority::External,
        "authority-less rows keep the transitional direct path"
    );
}

/// The built-in local library row (`SourceId::local()`, no epoch) is
/// neither external nor denied: it resolves through the built-in
/// local retained authority (2026-09-12 review finding).
#[test]
fn pane_local_library_rows_resolve_builtin_local_authority() {
    assert_eq!(
        classify_pane_authority(false, Some(SourceId::local()), None),
        PaneAuthority::BuiltinLocal,
        "built-in local rows must resolve through local retained authority"
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

// ---------------------------------------------------------------------
// Production projection through the real removable adapter
//
// Registry-backed album rows are pathless: `arch_remote_track_to_object`
// constructs every adopted-session row with an empty URI, and
// `populate_albums` copies that into the `AlbumArtCandidate`. The pane
// resolver must therefore choose the retained-file route from the live
// adapter's authoritative capability, not the row's raw locator
// (2026-09-14 review finding).
// ---------------------------------------------------------------------

use crate::source_lifecycle::SourceProvenance;
use crate::source_registry::SourceRegistry;
use std::time::Duration;

/// Copy the deterministic FLAC fixture, tag it, and embed one cover
/// picture so a retained-file extraction has real artwork to find.
pub(super) fn tagged_flac_with_embedded_art(path: &std::path::Path, art: &[u8]) {
    use lofty::config::WriteOptions;
    use lofty::file::{FileType, TaggedFileExt};
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, Tag, TagExt};
    use std::io::BufReader;

    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/audio/silence.flac"
        ),
        path,
    )
    .expect("copy deterministic FLAC fixture");
    let fixture_file = std::fs::File::open(path).expect("open FLAC fixture");
    let mut tagged = Probe::with_file_type(BufReader::new(fixture_file), FileType::Flac)
        .read()
        .expect("read FLAC fixture through handle");
    if tagged.primary_tag_mut().is_none() {
        let tag_type = tagged.primary_tag_type();
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged.primary_tag_mut().expect("FLAC primary tag");
    tag.set_title("Removable Album".to_string());
    tag.set_artist("Removable Artist".to_string());
    tag.set_album("Removable Album".to_string());
    tag.push_picture(
        Picture::unchecked(art.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    tag.save_to_path(path, WriteOptions::default())
        .expect("write FLAC tags and embedded art");
}

/// Adopt one real removable mount into a fresh registry and return the
/// live session epoch plus the accepted catalogue track.
async fn adopted_removable(
    mount_root: &std::path::Path,
    source_id: SourceId,
) -> (SourceRegistry, u64, crate::architecture::models::Track) {
    let registry = SourceRegistry::new(tokio::runtime::Handle::current());
    registry
        .claim_provenance(source_id, SourceProvenance::Removable)
        .expect("claim removable source");
    registry
        .connect_removable(source_id, mount_root.to_path_buf(), |_| {})
        .expect("removable connection admitted");
    let epoch = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(catalogue) = registry
                .snapshot(source_id)
                .and_then(|snapshot| snapshot.catalogue)
            {
                return catalogue.session_epoch;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("removable catalogue accepted");
    let track = registry
        .snapshot(source_id)
        .and_then(|snapshot| snapshot.catalogue)
        .and_then(|catalogue| catalogue.value.tracks().first().cloned())
        .expect("accepted removable track");
    (registry, epoch, track)
}

/// Build the album-pane candidate exactly as the production browser
/// does: `arch_remote_track_to_object` (pathless row) -> `TrackSnapshot`
/// -> `populate_albums`.
fn pane_candidate_for(
    track: &crate::architecture::models::Track,
    source_id: SourceId,
    epoch: u64,
) -> AlbumArtCandidate {
    let object = crate::ui::window::arch_remote_track_to_object(track, source_id, epoch, 1);
    assert_eq!(object.uri(), "", "the production projection is pathless");
    let snapshots = vec![crate::ui::browser::TrackSnapshot::from_object(&object)];
    let store = gtk::gio::ListStore::new::<BrowserItem>();
    crate::ui::browser::populate_albums(&store, &snapshots, &None, &None, false);
    (0..store.n_items())
        .filter_map(|index| store.item(index))
        .filter_map(|item| item.downcast::<BrowserItem>().ok())
        .find_map(|item| item.artwork_candidate())
        .expect("album row must carry an artwork candidate")
}

/// The P2 regression (2026-09-14 review finding; production shape from the
/// 2026-09-17 review finding): a mounted removable album reaches the pane as
/// a production registry row with an empty URI. Its retained-file capability
/// must route it to the retained file extractor, and the embedded art must
/// come back through the exact retained capability — never through a raw
/// path.
///
/// This drives the resolver from a GLib main context on a thread with NO
/// entered Tokio runtime, exactly as `orchestrate_pane_fetch` does in
/// production (`src/main.rs` parks the runtime on a background thread). The
/// direct await this replaced constructed `tokio::time::timeout_at` on the
/// main context and panicked before extraction; a `#[tokio::test]` would
/// mask that.
#[test]
fn pathless_removable_album_row_resolves_retained_embedded_art_from_glib_context() {
    let runtime = tokio::runtime::Runtime::new().expect("application tokio runtime");
    let application_handle = runtime.handle().clone();

    let mount = tempfile::tempdir().expect("temporary removable mount");
    let art = b"retained-removable-cover-art";
    tagged_flac_with_embedded_art(&mount.path().join("cover.flac"), art);
    let source_id =
        SourceId::removable("pane:test:retained-art").expect("removable source identity");
    let (registry, epoch, track) = runtime.block_on(adopted_removable(mount.path(), source_id));

    let candidate = pane_candidate_for(&track, source_id, epoch);
    assert_eq!(candidate.uri, "", "production registry rows are pathless");
    assert_eq!(candidate.source_id, Some(source_id));
    assert_eq!(candidate.source_session_epoch, Some(epoch));

    let shutdown_registry = registry.clone();
    let resolution_registry = registry.clone();
    let liveness = album_art::ScopedArtFetch::new();

    // The runtime lives on `runtime` above and is reachable only through the
    // supplied handle, mirroring production: the resolving thread has no
    // entered runtime, so any main-context Tokio API call panics.
    let resolved = std::thread::spawn(move || {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the pane resolution thread must have no entered Tokio runtime"
        );
        let context = gtk::glib::MainContext::new();
        context.block_on(resolve_kind(
            Some(resolution_registry),
            Some(source_id),
            Some(epoch),
            Vec::new(),
            Some(application_handle),
            LocalLibrary::Shared,
            &candidate,
            &liveness,
        ))
    })
    .join()
    .expect("pane resolution thread");

    let ResolvedArtKind::ResolvedFile { media } = resolved else {
        panic!("a pathless removable row must reach retained-file artwork");
    };
    assert_eq!(
        album_art::extract_resolved_file_album_art_bytes(&media).as_deref(),
        Some(art.as_slice()),
        "embedded art must extract through the retained file capability"
    );

    drop(media);
    runtime.block_on(shutdown_registry.shutdown().wait());
}

/// A retained-file-capable registry row whose token was revoked before its
/// fetch was driven must fail closed: the runtime resolution is refused at
/// admission, so no authority probe is scheduled for a row that can no
/// longer paint (2026-09-17 review finding).
#[test]
fn pre_revoked_removable_registry_row_fails_closed() {
    let runtime = tokio::runtime::Runtime::new().expect("application tokio runtime");
    let application_handle = runtime.handle().clone();

    let mount = tempfile::tempdir().expect("temporary removable mount");
    tagged_flac_with_embedded_art(&mount.path().join("cover.flac"), b"art");
    let source_id =
        SourceId::removable("pane:test:pre-revoked").expect("removable source identity");
    let (registry, epoch, track) = runtime.block_on(adopted_removable(mount.path(), source_id));
    let candidate = pane_candidate_for(&track, source_id, epoch);

    let liveness = album_art::ScopedArtFetch::new();
    liveness.revoke();

    let resolved = std::thread::spawn({
        let registry = registry.clone();
        let liveness = liveness.clone();
        move || {
            assert!(tokio::runtime::Handle::try_current().is_err());
            gtk::glib::MainContext::new().block_on(resolve_kind(
                Some(registry),
                Some(source_id),
                Some(epoch),
                Vec::new(),
                Some(application_handle),
                LocalLibrary::Shared,
                &candidate,
                &liveness,
            ))
        }
    })
    .join()
    .expect("pane resolution thread");

    assert!(
        matches!(resolved, ResolvedArtKind::NoArtwork),
        "a pre-revoked registry row must leave the placeholder"
    );

    runtime.block_on(registry.shutdown().wait());
}

/// A retained-file-capable registry row with no attached application
/// runtime must fail closed: the retained route polls Tokio time/blocking
/// APIs, so resolving it on the runtime-less main context would panic
/// (2026-09-17 review finding).
#[test]
fn removable_registry_row_without_runtime_fails_closed() {
    let runtime = tokio::runtime::Runtime::new().expect("application tokio runtime");
    let mount = tempfile::tempdir().expect("temporary removable mount");
    tagged_flac_with_embedded_art(&mount.path().join("cover.flac"), b"art");
    let source_id = SourceId::removable("pane:test:no-runtime").expect("removable source identity");
    let (registry, epoch, track) = runtime.block_on(adopted_removable(mount.path(), source_id));
    let candidate = pane_candidate_for(&track, source_id, epoch);

    let resolved = std::thread::spawn({
        let registry = registry.clone();
        move || {
            assert!(tokio::runtime::Handle::try_current().is_err());
            let liveness = album_art::ScopedArtFetch::new();
            gtk::glib::MainContext::new().block_on(resolve_kind(
                Some(registry),
                Some(source_id),
                Some(epoch),
                Vec::new(),
                None,
                LocalLibrary::Shared,
                &candidate,
                &liveness,
            ))
        }
    })
    .join()
    .expect("pane resolution thread");

    assert!(
        matches!(resolved, ResolvedArtKind::NoArtwork),
        "a retained row without a runtime must leave the placeholder"
    );

    runtime.block_on(registry.shutdown().wait());
}

/// A superseded session epoch must leave the placeholder: the live
/// adapter no longer carries retained-file authority for that epoch,
/// and no raw locator may be reopened to compensate.
#[tokio::test]
async fn superseded_removable_epoch_stays_on_the_placeholder() {
    let mount = tempfile::tempdir().expect("temporary removable mount");
    tagged_flac_with_embedded_art(&mount.path().join("cover.flac"), b"art");
    let source_id = SourceId::removable("pane:test:superseded").expect("removable source identity");
    let (registry, epoch, track) = adopted_removable(mount.path(), source_id).await;

    let candidate = pane_candidate_for(&track, source_id, epoch);
    let liveness = album_art::ScopedArtFetch::new();
    let resolved = resolve_kind(
        Some(registry.clone()),
        Some(source_id),
        Some(epoch + 1),
        Vec::new(),
        Some(tokio::runtime::Handle::current()),
        LocalLibrary::Shared,
        &candidate,
        &liveness,
    )
    .await;
    assert!(
        matches!(resolved, ResolvedArtKind::NoArtwork),
        "a superseded epoch must not resolve artwork"
    );

    registry.shutdown().wait().await;
}

/// A retained-file-capable adapter that refuses the exact track (a
/// well-formed identity the scan never accepted) must fall through to
/// the remote resolver's authoritative no-artwork rather than reopening
/// a raw path. This also covers the remote `Ok(None)` contract: the
/// removable adapter inherits the default no-artwork resolver.
#[tokio::test]
async fn refused_retained_authority_stays_on_the_placeholder() {
    let mount = tempfile::tempdir().expect("temporary removable mount");
    tagged_flac_with_embedded_art(&mount.path().join("cover.flac"), b"art");
    let source_id = SourceId::removable("pane:test:refused").expect("removable source identity");
    let (registry, epoch, _track) = adopted_removable(mount.path(), source_id).await;

    // A file that appeared after the accepted scan is a well-formed but
    // unaccepted removable identity: the adapter advertises retained-file
    // capability yet refuses this exact track at resolution.
    let unseen = mount.path().join("appeared-later.flac");
    tagged_flac_with_embedded_art(&unseen, b"art");
    let unaccepted = crate::architecture::TrackId::removable_relative(mount.path(), &unseen)
        .expect("well-formed relative identity");
    let candidate = AlbumArtCandidate {
        track_id: unaccepted.as_str().to_string(),
        uri: String::new(),
        cover_art_url: String::new(),
        source_id: Some(source_id),
        source_session_epoch: Some(epoch),
    };

    let liveness = album_art::ScopedArtFetch::new();
    let resolved = resolve_kind(
        Some(registry.clone()),
        Some(source_id),
        Some(epoch),
        Vec::new(),
        Some(tokio::runtime::Handle::current()),
        LocalLibrary::Shared,
        &candidate,
        &liveness,
    )
    .await;
    match resolved {
        ResolvedArtKind::NoArtwork => {}
        ResolvedArtKind::DirectFile { .. } => {
            panic!("a refused retained row must not reopen a raw path")
        }
        _ => panic!("refused retained authority must leave the placeholder"),
    }

    registry.shutdown().wait().await;
}
