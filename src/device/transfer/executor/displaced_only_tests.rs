//! Windows regression for the displaced-only rollback state emitted by the
//! replace publish's rebind exhaustion (F6, tr-0na / PR #175).
//!
//! On exhaustion nothing of the transfer's was published: the destination
//! still holds a concurrent writer's occupant (or is vacant) and only the
//! pre-transfer occupant was displaced into the retained backup. Rollback
//! must restore that backup ONLY into an absent slot and refuse any occupant
//! intact — never reverse it as an identity-less replacement and delete a
//! file the transfer never published.
//!
//! The live exhaustion race cannot be reproduced deterministically in a unit
//! test, so these regressions feed the exact [`CommitOutcome`] the replace
//! loop emits through [`owned_change_for_copy`] and the executor's real
//! reversal path.
#![cfg(windows)]

use std::path::{Path, PathBuf};

use super::rollback::{owned_change_for_copy, OwnedChange};
use super::TransferExecutor;
use crate::device::transfer::executor_tests::transfer_request;
use crate::device::transfer::test_support::{authority_pair, read_authority, write_source_file};
use crate::device::transfer::types::{TransferError, TransferItem};
use crate::device::transfer::TransferPlanner;
use crate::local::root_authority::LeafIdentity;
use crate::local::write_authority::{
    CommitDisposition, CommitOutcome, ConflictPolicy, ConflictResolution,
};

/// Build an executor whose destination authority is rooted at
/// `destination_root`, and capture the bind-time identity of the retained
/// backup. The caller keeps `source_root` alive so its authority handle is
/// never closed underneath the executor.
fn executor_for(
    source_root: &Path,
    destination_root: &Path,
    backup_relative: &Path,
) -> (TransferExecutor, LeafIdentity) {
    write_source_file(source_root, "song.flac", b"planned");
    let source = read_authority(source_root);
    let (_, destination) = authority_pair(destination_root);
    let backup_identity = destination
        .relative_leaf_identity(backup_relative)
        .expect("capture the displaced backup identity");
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Overwrite,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    (TransferExecutor::new(request, plan), backup_identity)
}

/// The displaced-only outcome the Windows replace loop emits on exhaustion:
/// nothing published, the pre-transfer occupant retained at its backup.
fn displaced_only_outcome(backup_relative: &Path, backup_identity: LeafIdentity) -> CommitOutcome {
    CommitOutcome {
        relative_path: PathBuf::from("song.flac"),
        resolution: ConflictResolution::Overwrite,
        disposition: CommitDisposition::DisplacedOnly,
        replaced_original: Some(backup_relative.to_path_buf()),
        replaced_original_leaf: Some(backup_identity),
        published_leaf: None,
    }
}

/// An occupied destination must be refused intact — a concurrent writer's
/// file the transfer never published must survive — and the verified backup
/// must be retained for a later, unblocked restore.
#[test]
fn displaced_only_rollback_refuses_a_concurrent_occupant() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let backup_relative = PathBuf::from(".tributary-backup-displaced.tmp");
    let backup_path = destination_root.path().join(&backup_relative);
    std::fs::write(&backup_path, b"w0").expect("write displaced backup");
    std::fs::write(destination_root.path().join("song.flac"), b"racer")
        .expect("write concurrent occupant");
    let (executor, backup_identity) = executor_for(
        source_root.path(),
        destination_root.path(),
        &backup_relative,
    );

    let change = owned_change_for_copy(displaced_only_outcome(&backup_relative, backup_identity));
    assert!(
        matches!(change, OwnedChange::DisplacedOnlyFile { .. }),
        "the displaced-only outcome must classify distinctly, not as a replacement"
    );

    let error = executor
        .rollback_change_for_test(change)
        .expect_err("an occupied slot must be refused");
    assert!(
        matches!(error, TransferError::RollbackFailed { .. }),
        "the refusal must surface as a rollback failure: {error:?}"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read racer"),
        b"racer",
        "the concurrent writer's file must survive the refused reversal"
    );
    assert_eq!(
        std::fs::read(&backup_path).expect("read retained backup"),
        b"w0",
        "the refusal must retain the verified backup, never delete it"
    );
}

/// The vacant case: once the slot is empty, the retained backup is restored
/// (a no-replace install) and consumed.
#[test]
fn displaced_only_rollback_restores_into_a_vacant_slot() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let backup_relative = PathBuf::from(".tributary-backup-displaced.tmp");
    let backup_path = destination_root.path().join(&backup_relative);
    std::fs::write(&backup_path, b"w0").expect("write displaced backup");
    let (executor, backup_identity) = executor_for(
        source_root.path(),
        destination_root.path(),
        &backup_relative,
    );

    let change = owned_change_for_copy(displaced_only_outcome(&backup_relative, backup_identity));
    executor
        .rollback_change_for_test(change)
        .expect("a vacant slot must take the retained backup");
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read restored"),
        b"w0",
        "the displaced pre-transfer occupant must return to the destination"
    );
    assert!(
        !backup_path.exists(),
        "a successful restore consumes the backup"
    );
}

/// A backup name a concurrent writer swapped for a different object must be
/// refused fail-closed: the foreign object is never installed as the
/// displaced original and never deleted.
#[test]
fn displaced_only_rollback_refuses_a_swapped_backup() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let backup_relative = PathBuf::from(".tributary-backup-displaced.tmp");
    let backup_path = destination_root.path().join(&backup_relative);
    std::fs::write(&backup_path, b"w0").expect("write displaced backup");
    let (executor, backup_identity) = executor_for(
        source_root.path(),
        destination_root.path(),
        &backup_relative,
    );

    // The backup name is swapped for a NEW object after its identity was
    // recorded; restoring it would install a foreign object as "the
    // original".
    std::fs::remove_file(&backup_path).expect("concurrent-writer unlink");
    std::fs::write(&backup_path, b"swapped").expect("swap the backup object");

    let change = owned_change_for_copy(displaced_only_outcome(&backup_relative, backup_identity));
    let error = executor
        .rollback_change_for_test(change)
        .expect_err("a swapped backup must be refused");
    assert!(
        matches!(error, TransferError::RollbackFailed { .. }),
        "the refusal must surface as a rollback failure: {error:?}"
    );
    assert!(
        !destination_root.path().join("song.flac").exists(),
        "nothing may be installed from a swapped backup"
    );
    assert_eq!(
        std::fs::read(&backup_path).expect("read swapped object"),
        b"swapped",
        "the foreign object at the backup name must never be deleted"
    );
}
