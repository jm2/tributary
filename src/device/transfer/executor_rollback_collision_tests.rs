//! Post-plan collision resolution and replaced-leaf rollback refusals for
//! [`TransferExecutor`](super::TransferExecutor).
//!
//! A destination that appears after planning is resolved by the recorded
//! policy (Skip, Fail, one Preserve re-resolution, or an Overwrite
//! replacement), never by a silent unbacked replace, and a skipped stage
//! must neither count nor report its discarded bytes as copied. A leaf a
//! concurrent writer substituted for a published or replaced destination
//! must survive rollback untouched, with the reversal refused fail-closed.

use std::path::PathBuf;

use super::executor::PRESERVE_REALLOCATION_ATTEMPTS;
use super::executor_rollback_tests::entry_names;
use super::executor_tests::transfer_request;
use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{
    Stage, TransferError, TransferItem, TransferPlan, TransferProgress, TransferRequest,
};
use super::{TransferExecutor, TransferPlanner};
use crate::local::write_authority::ConflictPolicy;
use crate::source_lifecycle::CancellationObserver;

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
    // The destination appears after planning; Preserve must re-resolve to
    // a disambiguated sibling without touching the destination.
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

/// A destination that appears after planning under an Overwrite request is
/// replaced, exactly as the stated policy demands: the fresh-planned
/// no-replace publish refused to destroy the interposed occupant, and the
/// re-resolution binds it to a commit-time backup before publishing the
/// transfer's bytes — the collision must not surface as a generic commit
/// failure.
#[test]
fn post_plan_collision_under_overwrite_policy_replaces_the_destination() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"planned song");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Overwrite,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    // The destination appears after planning; Overwrite must replace it,
    // with the interposed bytes backed up at commit time rather than
    // destroyed unbacked.
    std::fs::write(destination_root.path().join("song.flac"), b"racer")
        .expect("write post-plan destination");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("overwrite re-resolution must not fail the transfer");
    assert_eq!(summary.committed_stages, 1);
    assert!(summary.completed);
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read replaced song"),
        b"planned song",
        "the transfer's bytes must replace the post-plan destination"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        vec!["song.flac".to_string()],
        "no backup litter may survive a successful overwrite"
    );
}

/// A progress sink that replaces one destination file from inside the
/// stage-completed callback — a deterministic stand-in for a concurrent
/// writer landing a new object at a path the transfer has already
/// published, between the transfer's commit and its rollback.
struct ReplacingProgress {
    destination_root: PathBuf,
    replace_at_stage: u32,
    relative_path: PathBuf,
    replacement: &'static [u8],
}

impl TransferProgress for ReplacingProgress {
    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        index: u32,
        _total: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        if index == self.replace_at_stage {
            // A concurrent writer's replacement lands as a NEW object at
            // the published path: unlink first, then create — writing over
            // the published file in place would only truncate the very
            // inode the transfer recorded.
            std::fs::remove_file(self.destination_root.join(&self.relative_path))
                .expect("concurrent-writer unlink");
            std::fs::write(
                self.destination_root.join(&self.relative_path),
                self.replacement,
            )
            .expect("concurrent-writer replacement write");
        }
    }
}

/// Shared harness for the replaced-published-leaf races: a two-item
/// transfer whose first stage commits and whose published destination is
/// then replaced by a concurrent writer from the stage-completed callback,
/// with the second source removed so its stage fails after the first
/// committed. Returns both roots (which must outlive the run and every
/// assertion against them), the request/plan pair, and the progress sink.
fn replaced_published_leaf_race(
    policy: ConflictPolicy,
    destination_preexisting: Option<&[u8]>,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    TransferRequest,
    TransferPlan,
    ReplacingProgress,
) {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"new one");
    write_source_file(source_root.path(), "two.flac", b"new two");
    if let Some(bytes) = destination_preexisting {
        std::fs::write(destination_root.path().join("one.flac"), bytes)
            .expect("write existing one");
    }
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![
            TransferItem::same(PathBuf::from("one.flac")),
            TransferItem::same(PathBuf::from("two.flac")),
        ],
        policy,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    std::fs::remove_file(source_root.path().join("two.flac")).expect("remove source two");
    let progress = ReplacingProgress {
        destination_root: destination_root.path().to_path_buf(),
        replace_at_stage: 0,
        relative_path: PathBuf::from("one.flac"),
        replacement: b"racer bytes",
    };
    (source_root, destination_root, request, plan, progress)
}

/// A published file that a concurrent writer replaced after its stage
/// committed must NOT be deleted by rollback: the recorded publish-time
/// leaf identity no longer matches the path's occupant, so the reversal is
/// refused and the rollback fails closed with the writer's file intact.
#[test]
fn rollback_refuses_to_remove_a_replaced_published_leaf() {
    let (_source_root, destination_root, request, plan, mut progress) =
        replaced_published_leaf_race(ConflictPolicy::Preserve, None);
    let observer = CancellationObserver::never_cancelled();
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("the second stage must fail");
    assert!(
        matches!(error, TransferError::RollbackFailed { .. }),
        "rollback must refuse the foreign leaf, not succeed silently: {error:?}"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("one.flac")).expect("read racer file"),
        b"racer bytes",
        "the concurrent writer's file must survive the refused rollback"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        vec!["one.flac".to_string()],
        "the writer's file only: no staged litter may survive"
    );
}

/// An overwritten destination whose PUBLISHED file (not the saved
/// original) was replaced by a concurrent writer after the stage committed
/// must not be restored over: the reversal is refused, the writer's file
/// survives, and the backup is retained unconsumed for diagnosis.
#[test]
fn rollback_refuses_to_restore_over_a_replaced_published_leaf() {
    let (_source_root, destination_root, request, plan, mut progress) =
        replaced_published_leaf_race(ConflictPolicy::Overwrite, Some(b"old one"));
    let observer = CancellationObserver::never_cancelled();
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("the second stage must fail");
    assert!(
        matches!(error, TransferError::RollbackFailed { .. }),
        "rollback must refuse the foreign leaf, not succeed silently: {error:?}"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("one.flac")).expect("read racer file"),
        b"racer bytes",
        "the concurrent writer's file must survive the refused rollback"
    );
    let survivors = entry_names(destination_root.path());
    assert_eq!(
        survivors
            .iter()
            .filter(|name| name.starts_with(".tributary-backup-"))
            .count(),
        1,
        "the refused restore must retain the unconsumed backup: {survivors:?}"
    );
    assert_eq!(
        survivors.len(),
        2,
        "racer file plus backup only: {survivors:?}"
    );
}

/// A progress sink that records every reported running byte count.
struct RecordingProgress {
    bytes_reports: Vec<u64>,
    last_stage_completed_bytes: Option<u64>,
}

impl TransferProgress for RecordingProgress {
    fn on_bytes_copied(
        &mut self,
        _stage_index: u32,
        _total_stages: u32,
        bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        self.bytes_reports.push(bytes_so_far);
    }
    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        _index: u32,
        _total: u32,
        bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        self.last_stage_completed_bytes = Some(bytes_so_far);
    }
}

/// A post-plan collision under Skip discards the staged copy: the running
/// byte count must be restored to the pre-attempt value (and re-reported)
/// so a skip never leaves discarded bytes counted as copied and a retry
/// never double-counts.
#[test]
fn skipped_collision_restores_the_reported_byte_count() {
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
    std::fs::write(destination_root.path().join("song.flac"), b"racer")
        .expect("write post-plan destination");
    let mut progress = RecordingProgress {
        bytes_reports: Vec::new(),
        last_stage_completed_bytes: None,
    };
    let observer = CancellationObserver::never_cancelled();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("skip must not fail the transfer");
    assert_eq!(
        progress.bytes_reports.first(),
        Some(&12),
        "the staged copy reports its copied bytes: {:?}",
        progress.bytes_reports
    );
    assert_eq!(
        progress.bytes_reports.last(),
        Some(&0),
        "the collision restore must re-report the pre-attempt count: {:?}",
        progress.bytes_reports
    );
    assert_eq!(
        progress.last_stage_completed_bytes,
        Some(0),
        "the skipped stage must complete with the restored count"
    );
    assert_eq!(
        summary.bytes_copied, 0,
        "discarded staged bytes must not be counted as copied"
    );
}

/// A progress sink that swaps the overwrite commit's saved backup for a
/// fresh foreign object the instant a given stage completes: the backup
/// sibling is unlinked and rewritten, so the name no longer holds the
/// object whose identity the commit bound at bind time.
struct SwapBackupAtStageCompletion {
    destination_root: std::path::PathBuf,
    replace_at_stage: u32,
}
impl TransferProgress for SwapBackupAtStageCompletion {
    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        index: u32,
        _total: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        if index == self.replace_at_stage {
            let backup = std::fs::read_dir(&self.destination_root)
                .expect("read destination root")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name())
                .find(|name| name.to_string_lossy().starts_with(".tributary-backup-"))
                .expect("a committed overwrite must retain a backup sibling");
            let path = self.destination_root.join(backup);
            std::fs::remove_file(&path).expect("concurrent-writer backup unlink");
            std::fs::write(&path, b"swapped backup").expect("concurrent-writer backup swap");
        }
    }
}

/// An overwrite-committed file whose SAVED BACKUP was replaced by a
/// concurrent writer before a later stage failed must be refused by
/// rollback: the replacement is neither installed as the original nor
/// deleted, the published bytes stay intact, and the refusal surfaces as
/// a rollback failure. Restoring the swap would install a foreign object
/// over the transfer's own history.
#[test]
fn swapped_overwrite_backup_refuses_rollback_restoration() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"new one");
    write_source_file(source_root.path(), "two.flac", b"new two");
    std::fs::write(destination_root.path().join("one.flac"), b"old one")
        .expect("write existing original");
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
    let mut progress = SwapBackupAtStageCompletion {
        destination_root: destination_root.path().to_path_buf(),
        replace_at_stage: 0,
    };
    let observer = CancellationObserver::never_cancelled();
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("the second stage must fail");
    assert!(
        matches!(error, TransferError::RollbackFailed { .. }),
        "rollback must refuse the swapped backup: {error:?}"
    );
    let backup = std::fs::read_dir(destination_root.path())
        .expect("read destination root")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .find(|name| name.to_string_lossy().starts_with(".tributary-backup-"))
        .expect("the swapped backup must be retained for diagnosis");
    assert_eq!(
        std::fs::read(destination_root.path().join(&backup)).expect("read swapped backup"),
        b"swapped backup",
        "the foreign replacement must never be deleted or installed as the original"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("one.flac")).expect("read published"),
        b"new one",
        "the published bytes must remain untouched by the refused restore"
    );
}

/// A backup swapped after the LAST stage commits must refuse the
/// successful transfer's cleanup: the transfer cannot report success
/// while discarding an object it does not own, so the refusal surfaces
/// and the swapped backup survives unconsumed.
#[test]
fn swapped_overwrite_backup_refuses_successful_transfer_cleanup() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"new one");
    write_source_file(source_root.path(), "two.flac", b"new two");
    std::fs::write(destination_root.path().join("one.flac"), b"old one")
        .expect("write existing original");
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
    let mut progress = SwapBackupAtStageCompletion {
        destination_root: destination_root.path().to_path_buf(),
        replace_at_stage: 1,
    };
    let observer = CancellationObserver::never_cancelled();
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("cleanup must refuse a swapped backup and fail the transfer");
    assert!(
        matches!(error, TransferError::RollbackFailed { .. }),
        "cleanup refusal must surface as a rollback failure: {error:?}"
    );
    assert!(
        error.to_string().contains("refusing to discard"),
        "the surfaced error must name the refused disposal: {error}"
    );
    let backup = std::fs::read_dir(destination_root.path())
        .expect("read destination root")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .find(|name| name.to_string_lossy().starts_with(".tributary-backup-"))
        .expect("the swapped backup must survive the refused cleanup");
    assert_eq!(
        std::fs::read(destination_root.path().join(&backup)).expect("read swapped backup"),
        b"swapped backup",
        "the foreign replacement must never be silently deleted by cleanup"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("one.flac")).expect("read published"),
        b"new one"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("two.flac")).expect("read published two"),
        b"new two"
    );
}

/// A progress sink that replaces one committed destination leaf with a
/// fresh foreign object when a given stage completes.
struct ReplaceLeafAtStageCompletion {
    destination_root: std::path::PathBuf,
    replace_at_stage: u32,
    relative_path: String,
    replacement: &'static [u8],
}
impl TransferProgress for ReplaceLeafAtStageCompletion {
    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        index: u32,
        _total: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        if index == self.replace_at_stage {
            let path = self.destination_root.join(&self.relative_path);
            std::fs::remove_file(&path).expect("concurrent-writer unlink");
            std::fs::write(&path, self.replacement).expect("concurrent-writer replacement");
        }
    }
}

/// A refused reversal must not abandon the remaining changes: when the
/// most recently committed leaf was replaced by a concurrent writer and a
/// later stage fails, rollback retains the first reversal failure while
/// STILL reversing the earlier committed change, and the surfaced error
/// names the refused path. Abandoning the loop on the first refusal would
/// strand avoidable destination changes behind the foreign leaf.
#[test]
fn rollback_continues_reversing_after_a_refused_leaf() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"new one");
    write_source_file(source_root.path(), "two.flac", b"new two");
    write_source_file(source_root.path(), "three.flac", b"new three");
    for (name, bytes) in [
        ("one.flac", &b"old one"[..]),
        ("two.flac", &b"old two"[..]),
        ("three.flac", &b"old three"[..]),
    ] {
        std::fs::write(destination_root.path().join(name), bytes).expect("write existing");
    }
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
    // Stage 2 must fail after stages 0 and 1 committed; the concurrent
    // writer replaces the most recently committed leaf (two) the moment
    // its stage completes.
    std::fs::remove_file(source_root.path().join("three.flac")).expect("remove source three");
    let mut progress = ReplaceLeafAtStageCompletion {
        destination_root: destination_root.path().to_path_buf(),
        replace_at_stage: 1,
        relative_path: "two.flac".to_string(),
        replacement: b"racer two",
    };
    let observer = CancellationObserver::never_cancelled();
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("the third stage must fail");
    assert!(
        matches!(error, TransferError::RollbackFailed { .. }),
        "the refused reversal must fail the rollback: {error:?}"
    );
    assert!(
        error.to_string().contains("two.flac"),
        "the surfaced error must name the refused reversal's path: {error}"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("one.flac")).expect("read rolled-back one"),
        b"old one",
        "the EARLIER committed change must still reverse despite the refused leaf"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("two.flac")).expect("read racer two"),
        b"racer two",
        "the foreign leaf must survive the refused reversal untouched"
    );
    let survivors = entry_names(destination_root.path());
    assert_eq!(
        survivors
            .iter()
            .filter(|name| name.starts_with(".tributary-backup-"))
            .count(),
        1,
        "exactly the refused leaf's backup remains (the earlier one was consumed): {survivors:?}"
    );
    assert_eq!(
        std::fs::read(
            destination_root.path().join(
                survivors
                    .iter()
                    .find(|n| n.starts_with(".tributary-backup-"))
                    .expect("backup")
            )
        )
        .expect("read retained backup"),
        b"old two",
        "the retained backup must hold the refused leaf's saved original"
    );
}

/// A progress sink that races every preserved-sibling allocation: the
/// first bytes report of each re-allocation attempt (the staged copy has
/// just begun; the sibling name was allocated before it) creates the
/// freshly allocated sibling, so the no-replace publish collides and the
/// executor must re-allocate onto the next available name. Reports made
/// with a restored zero count are the collision bookkeeping, not copies,
/// and are ignored.
struct SiblingAllocationRacer {
    destination_root: std::path::PathBuf,
    leaf: String,
    collide_first_n: u32,
    attempts_seen: u32,
}
impl TransferProgress for SiblingAllocationRacer {
    fn on_bytes_copied(
        &mut self,
        _stage_index: u32,
        _total_stages: u32,
        bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        if bytes_so_far == 0 {
            return;
        }
        self.attempts_seen += 1;
        if self.attempts_seen <= self.collide_first_n {
            let (stem, extension) = self.leaf.rsplit_once('.').expect("leaf has an extension");
            let racer = format!("{stem} ({}).{extension}", self.attempts_seen);
            std::fs::write(self.destination_root.join(racer), b"racer bytes")
                .expect("write racing sibling");
        }
    }
}

/// A concurrent allocation racing a Preserve transfer for the freshly
/// chosen disambiguated sibling is normal contention: the transfer must
/// re-allocate onto the NEXT available sibling and succeed, leaving every
/// racing writer's file intact. A single collision resolution (as the
/// historical single retry performed) fails the whole run.
#[test]
fn preserve_sibling_allocation_race_re_allocates_to_the_next_sibling() {
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
    // The destination appears after planning, forcing the first collision;
    // the racer then takes the next TWO allocated siblings before letting
    // the third allocation through.
    std::fs::write(destination_root.path().join("song.flac"), b"racer")
        .expect("write post-plan destination");
    let mut progress = SiblingAllocationRacer {
        destination_root: destination_root.path().to_path_buf(),
        leaf: "song.flac".to_string(),
        collide_first_n: 2,
        attempts_seen: 0,
    };
    let observer = CancellationObserver::never_cancelled();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("bounded sibling re-allocation must not fail the transfer");
    assert!(summary.completed);
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read racer"),
        b"racer"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("song (1).flac")).expect("read racer one"),
        b"racer bytes",
        "the first raced sibling must survive untouched"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("song (2).flac")).expect("read racer two"),
        b"racer bytes",
        "the second raced sibling must survive untouched"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("song (3).flac"))
            .expect("read published sibling"),
        b"planned song",
        "the copy must land on the next available sibling"
    );
    assert_eq!(
        entry_names(destination_root.path()).len(),
        4,
        "no staged litter"
    );
}

/// Exhausting the bounded re-allocation loop surfaces the contention as
/// the typed I/O error instead of failing silently or spinning: every
/// allocated sibling is raced, the run fails, and no staged litter is
/// left behind.
#[test]
fn preserve_sibling_allocation_race_exhausting_the_bound_fails_typed() {
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
    std::fs::write(destination_root.path().join("song.flac"), b"racer")
        .expect("write post-plan destination");
    // More racing siblings than the reallocation bound allows, so every
    // attempt within the bound collides and the loop must exhaust.
    let mut progress = SiblingAllocationRacer {
        destination_root: destination_root.path().to_path_buf(),
        leaf: "song.flac".to_string(),
        collide_first_n: PRESERVE_REALLOCATION_ATTEMPTS as u32 * 4,
        attempts_seen: 0,
    };
    let observer = CancellationObserver::never_cancelled();
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("exhausted sibling allocation must fail the transfer");
    assert!(
        matches!(error, TransferError::Io { .. }),
        "exhaustion must surface as the typed I/O error: {error:?}"
    );
    assert!(
        error.to_string().contains("preserved sibling allocation"),
        "the error must name the exhausted allocation: {error}"
    );
    // The destination plus one raced sibling per exhausted attempt (the
    // initial publish and every bounded re-allocation) — no staged litter.
    let survivors = entry_names(destination_root.path());
    assert_eq!(
        survivors.len(),
        PRESERVE_REALLOCATION_ATTEMPTS + 2,
        "the destination plus the raced siblings only — no staged litter: {survivors:?}"
    );
    assert!(
        !survivors.iter().any(|name| name.starts_with(".tributary-")),
        "no private staged leaves may survive: {survivors:?}"
    );
}
