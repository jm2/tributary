//! Concurrent-writer interposition regressions for
//! [`TransferExecutor`](super::TransferExecutor) progress callbacks.
//!
//! A concurrent writer racing the transfer through a progress callback is
//! the adversarial window every committed change must survive: a swapped
//! overwrite backup is neither restored over nor silently discarded, a
//! refused reversal never abandons the remaining changes behind it, and a
//! Preserve sibling lost to a concurrent allocation is re-allocated onto
//! the next available name instead of failing the run.

use std::path::{Path, PathBuf};

use super::executor::PRESERVE_REALLOCATION_ATTEMPTS;
use super::executor_rollback_tests::entry_names;
use super::executor_tests::transfer_request;
use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{
    Stage, TransferError, TransferItem, TransferPlan, TransferProgress, TransferRequest,
    TransferSummary,
};
use super::{TransferExecutor, TransferPlanner};
use crate::local::write_authority::ConflictPolicy;
use crate::source_lifecycle::CancellationObserver;

/// Source/destination fixture for an Overwrite transfer of `names`: each
/// source file holds `new <stem>` bytes, and each name in `existing` is
/// pre-created at the destination with `old <stem>` bytes. Returns the
/// rooted temporary directories together with the planned request and plan.
fn overwrite_transfer_fixture(
    names: &[&str],
    existing: &[&str],
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    TransferRequest,
    TransferPlan,
) {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    for name in names {
        let stem = name.split_once('.').expect("named file has an extension").0;
        write_source_file(source_root.path(), name, format!("new {stem}").as_bytes());
    }
    for name in existing {
        let stem = name.split_once('.').expect("named file has an extension").0;
        std::fs::write(destination_root.path().join(name), format!("old {stem}"))
            .expect("write existing original");
    }
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let items: Vec<_> = names
        .iter()
        .map(|name| TransferItem::same(PathBuf::from(name)))
        .collect();
    let request = transfer_request(source, destination, items, ConflictPolicy::Overwrite);
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    (source_root, destination_root, request, plan)
}

/// Source/destination fixture for a Preserve transfer of one file whose
/// source holds `planned <stem>` bytes. Returns the rooted temporary
/// directories together with the planned request and plan.
fn preserve_single_file_fixture(
    name: &str,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    TransferRequest,
    TransferPlan,
) {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let stem = name.split_once('.').expect("named file has an extension").0;
    write_source_file(
        source_root.path(),
        name,
        format!("planned {stem}").as_bytes(),
    );
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from(name))],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    (source_root, destination_root, request, plan)
}

/// The single hidden backup sibling in `destination_root`. A committed
/// overwrite must retain exactly one, found by its reserved name prefix.
fn swapped_backup_path(destination_root: &Path, context: &str) -> PathBuf {
    let backup = std::fs::read_dir(destination_root)
        .expect("read destination root")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .find(|name| name.to_string_lossy().starts_with(".tributary-backup-"))
        .expect(context);
    destination_root.join(backup)
}

/// Assert the destination retains exactly one hidden backup sibling and
/// that it still holds `expected` bytes — the refused leaf's saved
/// original that the rollback could not consume.
fn assert_single_retained_backup(destination_root: &Path, expected: &[u8]) {
    let survivors = entry_names(destination_root);
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
            destination_root.join(
                survivors
                    .iter()
                    .find(|n| n.starts_with(".tributary-backup-"))
                    .expect("backup")
            )
        )
        .expect("read retained backup"),
        expected,
        "the retained backup must hold the refused leaf's saved original"
    );
}

/// A progress sink that swaps the committed overwrite's backup sibling for
/// a foreign object when a given stage completes.
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
            let path = swapped_backup_path(
                &self.destination_root,
                "a committed overwrite must retain a backup sibling",
            );
            std::fs::remove_file(&path).expect("concurrent-writer backup unlink");
            std::fs::write(&path, b"swapped backup").expect("concurrent-writer backup swap");
        }
    }
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

/// An overwrite-committed file whose SAVED BACKUP was replaced by a
/// concurrent writer before a later stage failed must be refused by
/// rollback: the replacement is neither installed as the original nor
/// deleted, the published bytes stay intact, and the refusal surfaces as
/// a rollback failure. Restoring the swap would install a foreign object
/// over the transfer's own history.
#[test]
fn swapped_overwrite_backup_refuses_rollback_restoration() {
    let (source_root, destination_root, request, plan) =
        overwrite_transfer_fixture(&["one.flac", "two.flac"], &["one.flac"]);
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
    let backup = swapped_backup_path(
        destination_root.path(),
        "the swapped backup must be retained for diagnosis",
    );
    assert_eq!(
        std::fs::read(&backup).expect("read swapped backup"),
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
    let (_source_root, destination_root, request, plan) =
        overwrite_transfer_fixture(&["one.flac", "two.flac"], &["one.flac"]);
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
    let backup = swapped_backup_path(
        destination_root.path(),
        "the swapped backup must survive the refused cleanup",
    );
    assert_eq!(
        std::fs::read(&backup).expect("read swapped backup"),
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

/// A refused reversal must not abandon the remaining changes: when the
/// most recently committed leaf was replaced by a concurrent writer and a
/// later stage fails, rollback retains the first reversal failure while
/// STILL reversing the earlier committed change, and the surfaced error
/// names the refused path. Abandoning the loop on the first refusal would
/// strand avoidable destination changes behind the foreign leaf.
#[test]
fn rollback_continues_reversing_after_a_refused_leaf() {
    let (source_root, destination_root, request, plan) = overwrite_transfer_fixture(
        &["one.flac", "two.flac", "three.flac"],
        &["one.flac", "two.flac", "three.flac"],
    );
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
    assert_single_retained_backup(destination_root.path(), b"old two");
}

/// A concurrent allocation racing a Preserve transfer for the freshly
/// chosen disambiguated sibling is normal contention: the transfer must
/// re-allocate onto the NEXT available sibling and succeed, leaving every
/// racing writer's file intact. A single collision resolution (as the
/// historical single retry performed) fails the whole run.
#[test]
fn preserve_sibling_allocation_race_re_allocates_to_the_next_sibling() {
    let (_source_root, destination_root, request, plan) = preserve_single_file_fixture("song.flac");
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
    let summary: TransferSummary = TransferExecutor::new(request, plan)
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
    let (_source_root, destination_root, request, plan) = preserve_single_file_fixture("song.flac");
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
