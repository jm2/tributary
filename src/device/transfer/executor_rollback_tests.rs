//! Adversarial rollback and cancellation regressions for
//! [`TransferExecutor`](super::TransferExecutor).
//!
//! Every committed stage must be undone when a later stage fails or the
//! transfer is cancelled: fresh/preserved publishes are removed by their
//! actual published path, overwritten destinations get their saved
//! originals restored, and directories the transfer created are removed.
//! No staged or backup litter may survive a failed run, and a pre-existing
//! destination must never be destroyed by rollback. The suite also covers
//! post-plan collision re-resolution, mount/binder replacement, and
//! directory-creation rollback.

use std::path::{Path, PathBuf};

use super::executor_tests::transfer_request;
use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{
    Stage, TransferError, TransferItem, TransferPlan, TransferProgress, TransferRequest,
};
use super::{TransferExecutor, TransferPlanner};
use crate::local::write_authority::ConflictPolicy;
use crate::source_lifecycle::{CancellationObserver, CancellationTrigger};

// ── Adversarial integrity suite ─────────────────────────────────────────

/// Sorted file names directly inside `dir`, for litter assertions.
pub(super) fn entry_names(dir: &Path) -> Vec<String> {
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
pub(super) fn run_plan_expect_failure(
    request: TransferRequest,
    plan: TransferPlan,
) -> TransferError {
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
fn zero_length_source_grown_after_plan_fails_size_verification() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "empty.flac", b"");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("empty.flac"))],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    // Zero is a known declared size, not an unknown-size sentinel: a
    // source planned as empty and grown before execution must fail the
    // size comparison, so the grown bytes can neither slip past a
    // zero-byte capacity budget nor skew progress totals.
    std::fs::write(source_root.path().join("empty.flac"), b"grown")
        .expect("grow source after planning");
    let error = run_plan_expect_failure(request, plan);
    assert!(
        matches!(error, TransferError::Io { .. }),
        "unexpected error: {error:?}"
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

/// A destination that existed at planning, was deleted, and was recreated
/// by a concurrent writer before the overwrite stage ran: the commit must
/// bind the racer's file as the replaced original, so rollback restores the
/// racer's bytes instead of deleting data the transfer does not own.
#[test]
fn overwrite_rollback_restores_racer_recreated_destination() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "one.flac", b"new one");
    write_source_file(source_root.path(), "two.flac", b"new two");
    std::fs::write(
        destination_root.path().join("one.flac"),
        b"planned occupant",
    )
    .expect("write destination occupied at planning");
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
    // The planned occupant vanishes and a racer recreates the name before
    // execution; the second source is then removed so stage two fails after
    // stage one committed over the racer.
    std::fs::remove_file(destination_root.path().join("one.flac")).expect("occupant vanishes");
    std::fs::write(destination_root.path().join("one.flac"), b"racer bytes")
        .expect("racer recreates destination");
    std::fs::remove_file(source_root.path().join("two.flac")).expect("remove source two");
    let _error = run_plan_expect_failure(request, plan);
    assert_eq!(
        std::fs::read(destination_root.path().join("one.flac")).expect("read restored"),
        b"racer bytes",
        "rollback must restore the concurrent writer's file, never delete it"
    );
    assert_eq!(
        entry_names(destination_root.path()),
        vec!["one.flac".to_string()],
        "restored racer file only: no backups, no published copies, no staged files"
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
