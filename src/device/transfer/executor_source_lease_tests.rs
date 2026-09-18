//! Source-lease publication regressions for
//! [`TransferExecutor`](super::TransferExecutor).
//!
//! The source authority is read-only, and its retained descriptor keeps
//! serving bytes to EOF even after the source root has been renamed aside
//! and a replacement directory installed at the old name. A lease lost
//! removed, the failed stage reports no completion, and rollback reverses
//! the publication (removing a fresh destination, restoring an overwritten
//! original). A valid-root control pins that the added revalidation does
//! not fail an ordinary transfer. A directory-only plan never copies a
//! byte, so its regressions race the same replacement on the first stage
//! start and pin the created-directory publication boundary: the transfer
//! fails, reports no completion, and rollback removes the created
//! directory; a valid-root control proves an ordinary directory creation
//! still completes.
//!
//! The replacement interposition is platform-specific by necessity. On Unix
//! a concurrent writer can rename the leased root aside, so the tests race
//! the copy exactly that way and assert the full authority-loss path: the
//! typed failure, no completion callback, and correct rollback. On Windows
//! the outcome depends on what the interposition touches. During a file
//! copy the open source-file descendant pins the tree, so the OS refuses
//! to rename the root aside (access denied 5 / sharing violation 32) while
//! the transfer holds it — the retained lease prevents the interposition
//! outright, and the tests assert that refusal (the platform's authority
//! evidence) and that the legitimate transfer still completes correctly;
//! delete-share protection is never weakened to simulate a loss the
//! platform prevents. At a bare directory stage nothing below the root is
//! open and the mounted root's unmount-friendly sharing lets the rename
//! through, so that regression branches on the observed outcome: refusal
//! asserts the legitimate completion, a completed rename asserts the same
//! publication-boundary loss as Unix.

use std::path::PathBuf;

use super::executor_rollback_tests::entry_names;
use super::executor_tests::transfer_request;
use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{
    Stage, TransferError, TransferItem, TransferProgress, TransferRequest, TransferSummary,
};
use super::{TransferExecutor, TransferPlanner};
use crate::local::write_authority::ConflictPolicy;
use crate::source_lifecycle::CancellationObserver;

/// A source root that is not a bare `tempfile::TempDir`: the authority is
/// rooted at `outer/source` so a test can rename that directory aside and
/// install a replacement at the old path — exactly the concurrent-writer
/// interposition the source lease must catch — without disturbing the
/// guard that owns the outer tree.
struct RenameableSource {
    /// Owns the whole tree; kept alive for the test's duration.
    _outer: tempfile::TempDir,
    root: PathBuf,
    moved: PathBuf,
}

impl RenameableSource {
    fn new() -> Self {
        let outer = tempfile::tempdir().expect("temporary source parent");
        let root = outer.path().join("source");
        std::fs::create_dir(&root).expect("create source root");
        let moved = outer.path().join("source-moved-aside");
        Self {
            _outer: outer,
            root,
            moved,
        }
    }

    fn write(&self, relative: &str, contents: &[u8]) {
        write_source_file(&self.root, relative, contents);
    }
}

/// A progress sink that, on the first real byte report, renames the source
/// root aside and installs an empty replacement directory at the old path.
/// The retained source descriptor keeps serving the original bytes, so
/// only an explicit publication-boundary revalidation can refuse the run.
/// It also counts completion callbacks so a test can assert the failed
/// stage never reported success.
struct ReplaceSourceRootDuringCopy {
    source_root: PathBuf,
    moved_root: PathBuf,
    fired: bool,
    /// Set when the platform refused the replacement outright: the
    /// retained source lease omits delete sharing, so the OS rejects
    /// renaming the root aside while the transfer holds it (Windows).
    replacement_refused: bool,
    stage_completes: u32,
}

impl TransferProgress for ReplaceSourceRootDuringCopy {
    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        _index: u32,
        _total: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        self.stage_completes = self.stage_completes.saturating_add(1);
    }

    fn on_bytes_copied(
        &mut self,
        _stage_index: u32,
        _total_stages: u32,
        bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        // The collision bookkeeping report restores the count to the
        // attempt's pre-copy value (zero for a first attempt); only a real
        // chunk report should race the root.
        if self.fired || bytes_so_far == 0 {
            return;
        }
        self.fired = true;
        match std::fs::rename(&self.source_root, &self.moved_root) {
            Ok(()) => {
                std::fs::create_dir(&self.source_root).expect("create replacement source root");
            }
            Err(error) => {
                // On Unix the rename must succeed: a concurrent writer CAN
                // replace a read-leased root, which is exactly the
                // interposition the publication boundary must catch.
                #[cfg(unix)]
                {
                    panic!("unix must permit the source-root replacement interposition: {error}");
                }
                // On Windows the retained root handle omits delete sharing,
                // so the OS refuses to rename the root aside while the
                // transfer holds it. That refusal is the platform's
                // authority evidence: the lease itself prevents the
                // replacement, so the legitimate transfer must remain
                // correct. Access denied (5) and sharing violation (32) are
                // the refusals Windows reports for an open directory
                // without delete sharing.
                #[cfg(windows)]
                {
                    assert!(
                        matches!(error.raw_os_error(), Some(5 | 32)),
                        "windows must refuse the root replacement via the retained lease: {error}"
                    );
                    self.replacement_refused = true;
                }
            }
        }
    }
}

fn replacement_sink(source: &RenameableSource) -> ReplaceSourceRootDuringCopy {
    ReplaceSourceRootDuringCopy {
        source_root: source.root.clone(),
        moved_root: source.moved.clone(),
        fired: false,
        replacement_refused: false,
        stage_completes: 0,
    }
}

/// Everything the two root-replacement regressions share: the executed
/// single-final-stage run with the replacement sink attached, plus the
/// state their assertions inspect. Extracted so each regression stays
/// within the static-analysis method-length budget without dropping a
/// single interposition or rollback assertion.
struct ReplacementRace {
    run: Result<TransferSummary, TransferError>,
    progress: ReplaceSourceRootDuringCopy,
    /// Windows only: the source (and the outer tree owning it) must stay
    /// alive through the refusal assertions; on Unix nothing after the run
    /// reads it, so the helper's drop of the tree is harmless.
    #[cfg(windows)]
    source: RenameableSource,
    destination_root: tempfile::TempDir,
}

/// Executes the single-final-stage transfer with the root-replacement sink
/// attached — the shared body of both interposition regressions. Pass
/// `existing_destination` (with `ConflictPolicy::Overwrite`) to pre-seed
/// the destination with an original, as the overwrite regression requires.
fn race_source_root_replacement(
    source_bytes: &[u8],
    policy: ConflictPolicy,
    existing_destination: Option<&[u8]>,
) -> ReplacementRace {
    let source = RenameableSource::new();
    source.write("song.flac", source_bytes);
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    if let Some(original) = existing_destination {
        std::fs::write(destination_root.path().join("song.flac"), original)
            .expect("write existing original");
    }
    let read = read_authority(&source.root);
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        read,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        policy,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = replacement_sink(&source);
    let run = TransferExecutor::new(request, plan).run(&mut progress, &observer);
    ReplacementRace {
        run,
        progress,
        #[cfg(windows)]
        source,
        destination_root,
    }
}

/// The Unix authority-loss prefix shared by both regressions: the run must
/// fail with the typed authority loss and the failed stage must never have
/// reported a completion callback. Returns the error so each test keeps its
/// own publication-message and rollback assertions.
#[cfg(unix)]
fn assert_authority_lost_with_no_completion(
    run: Result<TransferSummary, TransferError>,
    context: &str,
    progress: &ReplaceSourceRootDuringCopy,
) -> TransferError {
    let error = run.expect_err(context);
    assert!(
        matches!(error, TransferError::AuthorityLost { .. }),
        "the failure must be the typed authority loss: {error:?}"
    );
    assert_eq!(
        progress.stage_completes, 0,
        "the failed stage must not report a completion callback"
    );
    error
}

/// The Windows lease-refusal assertion block shared by both regressions:
/// the retained lease must have refused the interposed replacement (the
/// platform's authority evidence) and the legitimate transfer must have
/// completed exactly once, publishing `published_bytes` and no litter.
/// Messages are threaded through verbatim so each regression keeps its
/// own wording.
#[cfg(windows)]
fn assert_lease_refusal_completion(
    race: ReplacementRace,
    completion: &str,
    published_bytes: &[u8],
    read_message: &str,
    published_message: &str,
    survivors_message: &str,
) {
    let summary = race.run.expect(completion);
    assert!(
        race.progress.replacement_refused,
        "the interposition must have been attempted and refused by the lease"
    );
    assert_eq!(
        race.progress.stage_completes, 1,
        "the completed stage must report exactly one completion callback"
    );
    assert!(summary.completed, "the transfer must complete: {summary:?}");
    assert_eq!(summary.committed_stages, 1);
    assert_eq!(
        std::fs::read(race.destination_root.path().join("song.flac")).expect(read_message),
        published_bytes,
        "{}",
        published_message
    );
    assert!(
        race.source.root.is_dir(),
        "the source root must still stand at its original path"
    );
    assert!(
        !race.source.moved.exists(),
        "the lease must have prevented any move of the source root"
    );
    let survivors = entry_names(race.destination_root.path());
    assert_eq!(
        survivors,
        vec!["song.flac".to_string()],
        "{}: {survivors:?}",
        survivors_message
    );
}

/// A root replaced during `on_bytes_copied` on a single final stage to a
/// FRESH destination must fail the transfer (the retained source descriptor
/// alone cannot prove the lease). The failed stage reports no completion
/// callback and rollback removes the just-published file, leaving no staged
/// or backup litter behind.
///
/// On Windows the retained lease refuses the replacement outright (the root
/// handle omits delete sharing), so the same interposition asserts the
/// refusal and that the legitimate transfer still completes correctly.
#[test]
fn source_root_replaced_during_copy_fails_a_fresh_publish() {
    let race = race_source_root_replacement(b"copy me", ConflictPolicy::Preserve, None);
    #[cfg(unix)]
    {
        let error = assert_authority_lost_with_no_completion(
            race.run,
            "a source lease lost during the copy must fail the transfer",
            &race.progress,
        );
        assert!(
            error
                .to_string()
                .contains("source not current at publication"),
            "the error must name the publication-boundary source loss: {error}"
        );
        assert!(
            !race.destination_root.path().join("song.flac").exists(),
            "rollback must remove the fresh publication"
        );
        let survivors = entry_names(race.destination_root.path());
        assert!(
            survivors.is_empty(),
            "no published file or staged/backup litter may survive: {survivors:?}"
        );
    }
    #[cfg(windows)]
    {
        assert_lease_refusal_completion(
            race,
            "the retained lease refuses the replacement, so the legitimate transfer must \
             complete",
            b"copy me",
            "read published file",
            "the legitimate publication must hold the transferred bytes",
            "the published file only — no litter",
        );
    }
}

/// The same interposition against an OVERWRITE destination must fail and
/// roll back by restoring the saved original, consuming its backup — a
/// failed transfer must never destroy the pre-existing destination.
///
/// On Windows the retained lease refuses the replacement outright, so the
/// same interposition asserts the refusal and that the legitimate overwrite
/// still completes correctly.
#[test]
fn source_root_replaced_during_copy_restores_an_overwritten_destination() {
    let race =
        race_source_root_replacement(b"new song", ConflictPolicy::Overwrite, Some(b"old song"));
    #[cfg(unix)]
    {
        assert_authority_lost_with_no_completion(
            race.run,
            "a source lease lost during the copy must fail the overwrite",
            &race.progress,
        );
        assert_eq!(
            std::fs::read(race.destination_root.path().join("song.flac"))
                .expect("read restored original"),
            b"old song",
            "rollback must restore the overwritten original"
        );
        let survivors = entry_names(race.destination_root.path());
        assert_eq!(
            survivors,
            vec!["song.flac".to_string()],
            "the restored original only — no backup litter: {survivors:?}"
        );
    }
    #[cfg(windows)]
    {
        assert_lease_refusal_completion(
            race,
            "the retained lease refuses the replacement, so the legitimate overwrite must \
             complete",
            b"new song",
            "read final",
            "the legitimate overwrite must hold the transferred bytes",
            "the overwritten file only — no backup litter",
        );
    }
}

/// The valid-root control: an untouched source root still publishes
/// normally under the added publication-boundary revalidation, proving the
/// check does not fail an ordinary transfer.
#[test]
fn valid_source_root_control_completes() {
    let source = RenameableSource::new();
    source.write("song.flac", b"copy me");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let read = read_authority(&source.root);
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        read,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let summary: TransferSummary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("a valid source root must not fail the added revalidation");
    assert!(summary.completed);
    assert_eq!(summary.committed_stages, 1);
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read final"),
        b"copy me"
    );
}

// ── Directory-stage publication regressions ─────────────────────────────

/// A progress sink that, on the first stage start, renames the source root
/// aside and installs an empty replacement directory at the old path — the
/// same concurrent-writer interposition the file regressions race through
/// `on_bytes_copied`, moved to `on_stage_started` because a directory-only
/// plan never copies a byte. It also counts completion callbacks so a test
/// can assert the failed stage never reported success.
struct ReplaceSourceRootOnStageStart {
    source_root: PathBuf,
    moved_root: PathBuf,
    fired: bool,
    /// Set when the platform refused the replacement outright: the
    /// retained source lease omits delete sharing, so the OS rejects
    /// renaming the root aside while the transfer holds it (Windows).
    replacement_refused: bool,
    stage_completes: u32,
}

impl TransferProgress for ReplaceSourceRootOnStageStart {
    fn on_stage_started(&mut self, _stage: &Stage, _index: u32, _total: u32) {
        if self.fired {
            return;
        }
        self.fired = true;
        match std::fs::rename(&self.source_root, &self.moved_root) {
            Ok(()) => {
                std::fs::create_dir(&self.source_root).expect("create replacement source root");
            }
            Err(error) => {
                // On Unix the rename must succeed: a concurrent writer CAN
                // replace a read-leased root, which is exactly the
                // interposition the publication boundary must catch.
                #[cfg(unix)]
                {
                    panic!("unix must permit the source-root replacement interposition: {error}");
                }
                // On Windows the retained root handle omits delete sharing,
                // so the OS refuses the replacement outright (access denied
                // 5 / sharing violation 32). That refusal is the platform's
                // authority evidence; the legitimate creation must remain
                // correct.
                #[cfg(windows)]
                {
                    assert!(
                        matches!(error.raw_os_error(), Some(5 | 32)),
                        "windows must refuse the root replacement via the retained lease: {error}"
                    );
                    self.replacement_refused = true;
                }
            }
        }
    }

    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        _index: u32,
        _total: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        self.stage_completes = self.stage_completes.saturating_add(1);
    }
}

/// Everything the directory-stage regressions inspect: the executed
/// directory-only transfer (one non-recursive `album` directory item, one
/// `CreateDirectory` stage) with the stage-start replacement sink
/// attached, plus the rooted trees the assertions read. The source (and
/// the outer tree owning it) must stay alive through the Windows refusal
/// assertions; on Unix nothing after the run reads it.
struct DirectoryReplacementRace {
    run: Result<TransferSummary, TransferError>,
    progress: ReplaceSourceRootOnStageStart,
    #[cfg(windows)]
    source: RenameableSource,
    destination_root: tempfile::TempDir,
}

/// Executes the directory-only transfer with the root-replacement sink
/// attached — the shared body of the directory-stage regressions.
fn race_source_root_replacement_on_directory_stage() -> DirectoryReplacementRace {
    let source = RenameableSource::new();
    std::fs::create_dir(source.root.join("album")).expect("create source album directory");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let read = read_authority(&source.root);
    let (_, destination) = authority_pair(destination_root.path());
    let request = TransferRequest {
        source: read,
        destination,
        items: vec![TransferItem::same(PathBuf::from("album"))],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: false,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ReplaceSourceRootOnStageStart {
        source_root: source.root.clone(),
        moved_root: source.moved.clone(),
        fired: false,
        replacement_refused: false,
        stage_completes: 0,
    };
    let run = TransferExecutor::new(request, plan).run(&mut progress, &observer);
    DirectoryReplacementRace {
        run,
        progress,
        #[cfg(windows)]
        source,
        destination_root,
    }
}

/// A source root replaced before the (only) directory stage must fail the
/// transfer at the created-directory publication boundary — the stage
/// records the created directory, so only an explicit source revalidation
/// can refuse the run. The failed stage reports no completion callback and
/// rollback removes the created directory, leaving no litter.
///
/// On Windows the outcome of the rename interposition is
/// platform-managed, so the regression branches on it: when the retained
/// lease refuses the replacement outright, the same interposition asserts
/// the refusal and that the legitimate directory creation still completes;
/// when the rename goes through (the mounted root grants delete sharing by
/// design — `MountedRootAuthority::unmount_friendly_sharing`), the
/// publication-loss assertions below apply unchanged.
///
/// Directory-stage publication-loss arm (both platforms): the rename
/// interposition wins, so the run must fail with the typed authority loss
/// naming the publication boundary, report no completion callback, and
/// roll back the created directory without litter. On Unix the rename
/// always wins; on Windows it wins whenever the mounted root's
/// unmount-friendly sharing let the interposition through.
fn assert_directory_stage_publication_loss(race: DirectoryReplacementRace) {
    let error = race
        .run
        .expect_err("a source lease lost before the directory stage must fail the transfer");
    assert!(
        matches!(error, TransferError::AuthorityLost { .. }),
        "the failure must be the typed authority loss: {error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("source not current at publication"),
        "the error must name the publication-boundary source loss: {error}"
    );
    assert_eq!(
        race.progress.stage_completes, 0,
        "the failed stage must not report a completion callback"
    );
    let survivors = entry_names(race.destination_root.path());
    assert!(
        survivors.is_empty(),
        "rollback must remove the created directory: {survivors:?}"
    );
}

/// Windows arm of the directory-stage publication regression. The rename
/// interposition has two legitimate outcomes on Windows, so the arm
/// branches on the observed one: when the retained lease refused the
/// replacement outright (raw access-denied 5 / sharing-violation 32 —
/// the platform's authority evidence), the legitimate directory creation
/// must complete exactly once and the source root must still stand at its
/// original path; when the rename went through, the publication-loss
/// assertions apply exactly as on Unix.
#[cfg(windows)]
fn assert_directory_stage_lease_refusal(race: DirectoryReplacementRace) {
    if !race.progress.replacement_refused {
        // The rename succeeded — the mounted root grants delete sharing by
        // design — so the created-directory publication boundary must have
        // refused the run against the replaced source, exactly as on Unix.
        assert_directory_stage_publication_loss(race);
        return;
    }
    let summary = race.run.expect(
        "the retained lease refuses the replacement, so the legitimate directory creation \
         must complete",
    );
    assert!(
        race.progress.replacement_refused,
        "the interposition must have been attempted and refused by the lease"
    );
    assert_eq!(
        race.progress.stage_completes, 1,
        "the completed stage must report exactly one completion callback"
    );
    assert!(summary.completed, "the transfer must complete: {summary:?}");
    assert_eq!(summary.committed_stages, 1);
    assert!(
        race.destination_root.path().join("album").is_dir(),
        "the legitimate directory creation must have landed"
    );
    assert!(
        race.source.root.is_dir(),
        "the source root must still stand at its original path"
    );
    assert!(
        !race.source.moved.exists(),
        "the lease must have prevented any move of the source root"
    );
}

#[test]
fn source_root_replaced_before_a_directory_stage_fails_and_rolls_back() {
    let race = race_source_root_replacement_on_directory_stage();
    #[cfg(unix)]
    assert_directory_stage_publication_loss(race);
    #[cfg(windows)]
    assert_directory_stage_lease_refusal(race);
}

/// The valid-root control for the directory boundary: an untouched source
/// root still lets a directory-only plan complete under the added
/// publication-boundary revalidation, proving the check does not fail an
/// ordinary directory creation.
#[test]
fn valid_source_root_directory_stage_completes() {
    let source = RenameableSource::new();
    std::fs::create_dir(source.root.join("album")).expect("create source album directory");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let read = read_authority(&source.root);
    let (_, destination) = authority_pair(destination_root.path());
    let request = TransferRequest {
        source: read,
        destination,
        items: vec![TransferItem::same(PathBuf::from("album"))],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: false,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let summary: TransferSummary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("a valid source root must not fail the directory-stage revalidation");
    assert!(summary.completed);
    assert_eq!(summary.committed_stages, 1);
    assert!(
        destination_root.path().join("album").is_dir(),
        "the directory creation must have landed"
    );
}
