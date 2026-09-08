//! Directory-rollback and mount-boundary regressions for
//! [`TransferExecutor`](super::TransferExecutor).
//!
//! Directories the transfer created are removed on failure while
//! pre-existing destination directories — and the foreign data inside
//! them — survive rollback untouched. A destination root replaced behind
//! the retained authority mid-run (unmount/remount or binder swap) fails
//! the transfer closed instead of authorising a partial publish.

use std::path::PathBuf;

use super::executor_rollback_tests::{entry_names, run_plan_expect_failure};
use super::executor_tests::transfer_request;
use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{Stage, TransferError, TransferItem, TransferProgress};
use super::{TransferExecutor, TransferPlanner};
use crate::local::write_authority::ConflictPolicy;
use crate::source_lifecycle::CancellationObserver;

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
