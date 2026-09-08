//! Execution regressions for [`TransferExecutor`]: commits, recursive
//! directory copies, progress reporting, and conflict policies.

use std::path::PathBuf;
use std::sync::Arc;

use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{
    Stage, TransferItem, TransferPlan, TransferProgress, TransferRequest, TransferSummary,
};
use super::{TransferExecutor, TransferPlanner};
use crate::local::root_authority::MountedRootAuthority;
use crate::local::write_authority::{ConflictPolicy, MountedWriteAuthority};
use crate::source_lifecycle::CancellationObserver;

/// Build a recursive transfer request in one call.
fn transfer_request(
    source: Arc<MountedRootAuthority>,
    destination: MountedWriteAuthority,
    items: Vec<TransferItem>,
    conflict_policy: ConflictPolicy,
) -> TransferRequest {
    TransferRequest {
        source,
        destination,
        items,
        conflict_policy,
        capacity_budget: None,
        recurse_directories: true,
    }
}

/// Run a freshly planned request through the executor.
fn run(request: TransferRequest) -> TransferSummary {
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("run")
}

#[test]
fn executor_copies_a_single_file() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"copy me");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Preserve,
    );
    let summary = run(request);
    assert!(summary.completed);
    let final_path = destination_root.path().join("song.flac");
    let bytes = std::fs::read(&final_path).expect("read final");
    assert_eq!(bytes, b"copy me");
}

#[test]
fn executor_recursive_directory_copy() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    write_source_file(source_root.path(), "album/b.flac", b"b");
    write_source_file(source_root.path(), "album/nested/c.flac", b"c");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        ConflictPolicy::Preserve,
    );
    let summary = run(request);
    assert!(summary.completed);
    assert_eq!(
        std::fs::read(destination_root.path().join("imported/a.flac")).expect("read a"),
        b"a"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("imported/b.flac")).expect("read b"),
        b"b"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("imported/nested/c.flac")).expect("read c"),
        b"c"
    );
}

/// Records every progress callback so chunk counts can be asserted.
struct ProgressRecorder {
    stage_starts: u32,
    stage_completes: u32,
    byte_chunks: u32,
}

impl ProgressRecorder {
    fn new() -> Self {
        Self {
            stage_starts: 0,
            stage_completes: 0,
            byte_chunks: 0,
        }
    }
}

impl TransferProgress for ProgressRecorder {
    fn on_stage_started(&mut self, _stage: &Stage, _index: u32, _total: u32) {
        self.stage_starts = self.stage_starts.saturating_add(1);
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
    fn on_bytes_copied(
        &mut self,
        _stage_index: u32,
        _total_stages: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        self.byte_chunks = self.byte_chunks.saturating_add(1);
    }
}

#[test]
fn progress_callback_reports_every_stage_and_chunk() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    // 200 KiB so the 64 KiB chunked copy yields multiple progress reports.
    let payload = vec![0u8; 200 * 1024];
    write_source_file(source_root.path(), "payload.bin", &payload);
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("payload.bin"))],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ProgressRecorder::new();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("run");
    assert!(summary.completed);
    assert_eq!(progress.stage_starts, 1, "one copy stage should start");
    assert_eq!(
        progress.stage_completes, 1,
        "one copy stage should complete"
    );
    assert!(
        progress.byte_chunks >= 3,
        "at least three progress chunks for 200 KiB"
    );
}

#[test]
fn overwrite_policy_replaces_existing_destination() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"new");
    std::fs::write(destination_root.path().join("song.flac"), b"old").expect("write existing");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Overwrite,
    );
    let _ = run(request);
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read final"),
        b"new"
    );
}

// ── Adversarial integrity suite ─────────────────────────────────────────
//
// Every committed stage must be undone when a later stage fails or the
// transfer is cancelled: fresh/preserved publishes are removed by their
// actual published path, overwritten destinations get their saved originals
// restored, and directories the transfer created are removed. No staged or
// backup litter may survive a failed run, and a pre-existing destination
// must never be destroyed by rollback.

use std::path::Path;

use super::types::TransferError;
use crate::source_lifecycle::CancellationTrigger;

/// Sorted file names directly inside `dir`, for litter assertions.
fn entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read directory")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// Run an already-planned request expecting failure; returns the executor
/// error. Tests that disturb the source tree after planning must pass the
/// pre-made plan here instead of re-planning.
fn run_plan_expect_failure(request: TransferRequest, plan: TransferPlan) -> TransferError {
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("run must fail")
}

#[test]
fn stage_failure_rolls_back_committed_stages() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"one");
    write_source_file(source_root.path(), "two.flac", b"two");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![
            TransferItem::same(PathBuf::from("one.flac")),
            TransferItem::same(PathBuf::from("two.flac")),
        ],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    // The plan staged both copies; removing the second source makes its
    // stage fail after the first stage already committed.
    std::fs::remove_file(source_root.path().join("two.flac")).expect("remove source two");
    let error = run_plan_expect_failure(request, plan);
    assert!(
        matches!(error, TransferError::Io { .. }),
        "unexpected error: {error:?}"
    );
    assert!(
        !destination_root.path().join("one.flac").exists(),
        "committed first stage must be rolled back"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        Vec::<String>::new(),
        "no staged or committed litter may survive"
    );
}

#[test]
fn preserve_rollback_never_deletes_pre_existing_destination() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"new song");
    write_source_file(source_root.path(), "second.flac", b"second");
    std::fs::write(destination_root.path().join("song.flac"), b"keep me")
        .expect("write pre-existing destination");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![
            TransferItem::same(PathBuf::from("song.flac")),
            TransferItem::same(PathBuf::from("second.flac")),
        ],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    std::fs::remove_file(source_root.path().join("second.flac")).expect("remove source two");
    let error = run_plan_expect_failure(request, plan);
    assert!(
        matches!(error, TransferError::Io { .. }),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read original"),
        b"keep me",
        "pre-existing destination must survive rollback untouched"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        vec!["song.flac".to_string()],
        "preserved sibling and staged litter must be gone"
    );
}

#[test]
fn overwrite_rollback_restores_originals() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"new one");
    write_source_file(source_root.path(), "two.flac", b"new two");
    write_source_file(source_root.path(), "three.flac", b"new three");
    std::fs::write(destination_root.path().join("one.flac"), b"old one")
        .expect("write existing one");
    std::fs::write(destination_root.path().join("two.flac"), b"old two")
        .expect("write existing two");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![
            TransferItem::same(PathBuf::from("one.flac")),
            TransferItem::same(PathBuf::from("two.flac")),
            TransferItem::same(PathBuf::from("three.flac")),
        ],
        ConflictPolicy::Overwrite,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    std::fs::remove_file(source_root.path().join("three.flac")).expect("remove source three");
    let error = run_plan_expect_failure(request, plan);
    assert!(
        matches!(error, TransferError::Io { .. }),
        "unexpected error: {error:?}"
    );
    // Both overwritten destinations were committed with saved originals;
    // rollback must put the originals back, not delete them.
    assert_eq!(
        std::fs::read(destination_root.path().join("one.flac")).expect("read restored one"),
        b"old one"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("two.flac")).expect("read restored two"),
        b"old two"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        vec!["one.flac".to_string(), "two.flac".to_string(),],
        "restored originals only: no backups, no published copies, no staged files"
    );
}

#[test]
fn overwrite_of_absent_destination_rolls_back_as_published() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"new one");
    write_source_file(source_root.path(), "two.flac", b"new two");
    // two.flac pre-exists; one.flac does not, so its Overwrite commit has
    // no original to save and must roll back as a plain publish.
    std::fs::write(destination_root.path().join("two.flac"), b"old two")
        .expect("write existing two");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![
            TransferItem::same(PathBuf::from("one.flac")),
            TransferItem::same(PathBuf::from("two.flac")),
        ],
        ConflictPolicy::Overwrite,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    std::fs::remove_file(source_root.path().join("two.flac")).expect("remove source two");
    let error = run_plan_expect_failure(request, plan);
    assert!(
        matches!(error, TransferError::Io { .. }),
        "unexpected error: {error:?}"
    );
    assert!(
        !destination_root.path().join("one.flac").exists(),
        "the backup-less overwrite commit must roll back as a plain publish"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("two.flac")).expect("read restored two"),
        b"old two"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        vec!["two.flac".to_string()],
        "restored original only: no backup or staged litter may survive"
    );
}

/// A progress sink that flips a shared cancellation trigger from inside a
/// callback mid-transfer.
struct CancellingProgress {
    trigger: CancellationTrigger,
    flip_on_stage_completed: Option<u32>,
    flip_on_first_chunk_of_stage: Option<u32>,
}

impl TransferProgress for CancellingProgress {
    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        index: u32,
        _total: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        if self.flip_on_stage_completed == Some(index) {
            self.trigger.cancel();
        }
    }
    fn on_bytes_copied(
        &mut self,
        stage_index: u32,
        _total_stages: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        if self.flip_on_first_chunk_of_stage == Some(stage_index) {
            self.trigger.cancel();
        }
    }
}

#[test]
fn cancellation_between_stages_rolls_back_committed_stages() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"one");
    write_source_file(source_root.path(), "two.flac", b"two");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![
            TransferItem::same(PathBuf::from("one.flac")),
            TransferItem::same(PathBuf::from("two.flac")),
        ],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let (trigger, observer) = CancellationTrigger::new();
    let mut progress = CancellingProgress {
        trigger,
        flip_on_stage_completed: Some(0),
        flip_on_first_chunk_of_stage: None,
    };
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("cancelled transfer must fail");
    assert!(matches!(error, TransferError::Cancelled));
    assert!(
        !destination_root.path().join("one.flac").exists(),
        "stage committed before cancellation must be rolled back"
    );
    assert_eq!(entry_names(destination_root.path()), Vec::<String>::new());
}

#[test]
fn mid_copy_cancellation_rolls_back_prior_stages() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"one");
    // 200 KiB: several 64 KiB chunks so cancellation lands mid-copy.
    let payload = vec![7u8; 200 * 1024];
    write_source_file(source_root.path(), "big.bin", &payload);
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![
            TransferItem::same(PathBuf::from("one.flac")),
            TransferItem::same(PathBuf::from("big.bin")),
        ],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let (trigger, observer) = CancellationTrigger::new();
    let mut progress = CancellingProgress {
        trigger,
        flip_on_stage_completed: None,
        flip_on_first_chunk_of_stage: Some(1),
    };
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("mid-copy cancellation must fail the transfer");
    assert!(matches!(error, TransferError::Cancelled));
    assert!(
        !destination_root.path().join("one.flac").exists(),
        "prior committed stage must be rolled back"
    );
    assert!(
        !destination_root.path().join("big.bin").exists(),
        "partially copied file must not be published"
    );
    assert_eq!(entry_names(destination_root.path()), Vec::<String>::new());
}

#[test]
fn post_plan_collision_re_resolves_to_preserved_sibling() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"planned song");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    // A destination appears after planning but before execution. The
    // staging-time policy re-resolution must preserve it under a sibling,
    // never replace it.
    std::fs::write(destination_root.path().join("song.flac"), b"racer")
        .expect("write post-plan destination");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("preserve re-resolution must not fail the transfer");
    assert_eq!(summary.committed_stages, 1);
    assert!(summary.completed);
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read racer"),
        b"racer",
        "post-plan collision must never replace the destination"
    );
    let published = destination_root.path().join("song (1).flac");
    assert_eq!(
        std::fs::read(&published).expect("read preserved sibling"),
        b"planned song",
        "the copy must land on the disambiguated sibling"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        vec!["song (1).flac".to_string(), "song.flac".to_string()],
        "no staged litter may accompany the preserved sibling"
    );
}

#[test]
fn post_plan_collision_under_skip_policy_skips_the_stage() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"planned song");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Skip,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    // The destination appears after planning; the Skip policy must skip
    // the stage without disturbing the destination or counting it as a
    // committed stage.
    std::fs::write(destination_root.path().join("song.flac"), b"racer")
        .expect("write post-plan destination");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("skip must not fail the transfer");
    assert_eq!(summary.committed_stages, 0);
    assert!(summary.completed);
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read racer"),
        b"racer",
        "skip must never replace the destination"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        vec!["song.flac".to_string()],
        "staged bytes of the skipped stage must be discarded"
    );
}

#[test]
fn directory_rollback_removes_created_directories() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    write_source_file(source_root.path(), "album/b.flac", b"b");
    write_source_file(source_root.path(), "album/sub/c.flac", b"c");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    std::fs::remove_file(source_root.path().join("album/b.flac")).expect("remove source b");
    let _error = run_plan_expect_failure(request, plan);
    assert!(
        !destination_root.path().join("imported").exists(),
        "directories created by the failed transfer must be removed"
    );
    assert_eq!(entry_names(destination_root.path()), Vec::<String>::new());
}

#[test]
fn pre_existing_directory_survives_rollback() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    write_source_file(source_root.path(), "album/b.flac", b"b");
    std::fs::create_dir(destination_root.path().join("imported")).expect("pre-create imported");
    std::fs::write(
        destination_root.path().join("imported/foreign.txt"),
        b"foreign",
    )
    .expect("write foreign entry");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    std::fs::remove_file(source_root.path().join("album/b.flac")).expect("remove source b");
    let _error = run_plan_expect_failure(request, plan);
    // The pre-existing directory holds foreign data; it must survive even
    // though the transfer copied a file into it before failing.
    assert_eq!(
        std::fs::read(destination_root.path().join("imported/foreign.txt")).expect("read foreign"),
        b"foreign"
    );
    assert!(
        !destination_root.path().join("imported/a.flac").exists(),
        "committed copy inside the pre-existing directory must be rolled back"
    );
}

#[cfg(unix)]
#[test]
fn mount_swap_during_execution_fails_closed() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"one");
    write_source_file(source_root.path(), "two.flac", b"two");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![
            TransferItem::same(PathBuf::from("one.flac")),
            TransferItem::same(PathBuf::from("two.flac")),
        ],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    // Replacing the destination directory behind the authority simulates an
    // unmount/remount or binder swap: the retained boundary identity no
    // longer matches the path. Unix only: the retained Windows handles make
    // in-place directory replacement impossible there.
    struct SwapOnFirstCompletion(std::path::PathBuf);
    impl TransferProgress for SwapOnFirstCompletion {
        fn on_stage_completed(
            &mut self,
            _stage: &Stage,
            index: u32,
            _total: u32,
            _bytes_so_far: u64,
            _total_bytes: u64,
        ) {
            if index == 0 {
                std::fs::remove_dir_all(&self.0).expect("remove destination tree");
                std::fs::create_dir(&self.0).expect("recreate empty destination root");
            }
        }
    }
    let mut progress = SwapOnFirstCompletion(destination_root.path().to_path_buf());
    let observer = CancellationObserver::never_cancelled();
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("mount swap must fail the transfer fail-closed");
    assert!(
        matches!(
            error,
            TransferError::AuthorityLost { .. } | TransferError::RollbackFailed { .. }
        ),
        "unexpected error: {error:?}"
    );
}
