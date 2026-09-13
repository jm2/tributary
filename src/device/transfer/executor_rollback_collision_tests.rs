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
