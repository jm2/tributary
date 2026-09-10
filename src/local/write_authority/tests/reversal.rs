//! Reversal regressions: plain removal typing, backup restores, and the
//! identity-verified removal/restore outcomes.

use std::io;
use std::path::Path;

use super::authority;
use crate::local::write_authority::{ConflictPolicy, ReversalOutcome};

#[test]
fn remove_relative_file_only_accepts_regular_files() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"data").expect("write file");
    std::fs::create_dir(root.path().join("album")).expect("create album");

    let authority = authority(&root);
    authority
        .remove_relative_file(Path::new("song.flac"))
        .expect("remove file");
    assert!(!root.path().join("song.flac").exists());

    let error = authority
        .remove_relative_file(Path::new("album"))
        .expect_err("directory must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn restore_relative_file_puts_backup_back_and_consumes_it() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"published bytes")
        .expect("write published file");
    let backup = root.path().join(".tributary-backup-test.tmp");
    std::fs::write(&backup, b"original bytes").expect("write backup");
    let authority = authority(&root);

    authority
        .restore_relative_file(
            Path::new(".tributary-backup-test.tmp"),
            Path::new("song.flac"),
        )
        .expect("restore overwritten original");
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read restored"),
        b"original bytes"
    );
    assert!(!backup.exists(), "restore consumes the backup");
}

#[test]
fn restore_requires_sibling_backup() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::create_dir_all(root.path().join("a")).expect("create a");
    std::fs::create_dir_all(root.path().join("other")).expect("create other");
    std::fs::write(root.path().join("other/backup.tmp"), b"backup").expect("write backup");
    let authority = authority(&root);

    let error = authority
        .restore_relative_file(Path::new("other/backup.tmp"), Path::new("a/song.flac"))
        .expect_err("a non-sibling backup must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn verified_removal_removes_exactly_the_recorded_leaf() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
        .expect("prepare staged file");
    staged.write_all(b"published").expect("write payload");
    staged.commit().expect("commit staged file");
    let published_leaf = authority
        .relative_leaf_identity(Path::new("song.flac"))
        .expect("capture published leaf identity");

    let outcome = authority
        .remove_relative_file_verified(Path::new("song.flac"), Some(published_leaf))
        .expect("verified removal of the recorded leaf must succeed");
    assert_eq!(outcome, ReversalOutcome::Reversed);
    assert!(
        !root.path().join("song.flac").exists(),
        "the recorded leaf must be gone"
    );
}

#[test]
fn verified_removal_refuses_a_replaced_published_leaf() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
        .expect("prepare staged file");
    staged.write_all(b"published").expect("write payload");
    staged.commit().expect("commit staged file");
    let published_leaf = authority
        .relative_leaf_identity(Path::new("song.flac"))
        .expect("capture published leaf identity");

    // A concurrent writer replaces the publication: a new object now
    // occupies the recorded path. Reversing by pathname alone would delete
    // bytes the transfer does not own, so the removal must be refused.
    // Unlink-then-create is what makes the occupant a NEW object — writing
    // over the file in place would only truncate the recorded inode.
    std::fs::remove_file(root.path().join("song.flac")).expect("concurrent-writer unlink");
    std::fs::write(root.path().join("song.flac"), b"foreign").expect("replace published file");

    let outcome = authority
        .remove_relative_file_verified(Path::new("song.flac"), Some(published_leaf))
        .expect("a refusal is an outcome, not an error");
    assert_eq!(outcome, ReversalOutcome::RefusedForeignLeaf);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read foreign file"),
        b"foreign",
        "the foreign file must survive the refused reversal untouched"
    );
}

#[test]
fn verified_removal_reports_an_absent_leaf() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
        .expect("prepare staged file");
    staged.write_all(b"published").expect("write payload");
    staged.commit().expect("commit staged file");
    let published_leaf = authority
        .relative_leaf_identity(Path::new("song.flac"))
        .expect("capture published leaf identity");
    std::fs::remove_file(root.path().join("song.flac")).expect("delete the publication");

    let outcome = authority
        .remove_relative_file_verified(Path::new("song.flac"), Some(published_leaf))
        .expect("verified removal of an absent leaf must not fail");
    assert_eq!(outcome, ReversalOutcome::AlreadyAbsent);
}

#[test]
fn verified_restore_refuses_a_foreign_occupant_of_the_slot() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"published bytes")
        .expect("write published file");
    let backup = root.path().join(".tributary-backup-test.tmp");
    std::fs::write(&backup, b"original bytes").expect("write backup");
    let authority = authority(&root);
    let published_leaf = authority
        .relative_leaf_identity(Path::new("song.flac"))
        .expect("capture published leaf identity");

    // A concurrent writer replaces the publication after the transfer
    // committed it; restoring the backup over that writer's file would
    // destroy it. The reversal must be refused and the backup retained.
    // Unlink-then-create makes the occupant a NEW object rather than a
    // truncation of the recorded inode.
    std::fs::remove_file(root.path().join("song.flac")).expect("concurrent-writer unlink");
    std::fs::write(root.path().join("song.flac"), b"racer bytes").expect("replace publication");

    let outcome = authority
        .restore_relative_file_verified(
            Path::new(".tributary-backup-test.tmp"),
            Path::new("song.flac"),
            Some(&published_leaf),
        )
        .expect("a refusal is an outcome, not an error");
    assert_eq!(outcome, ReversalOutcome::RefusedForeignLeaf);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read racer file"),
        b"racer bytes",
        "the concurrent writer's file must survive the refused restore"
    );
    assert_eq!(
        std::fs::read(&backup).expect("read retained backup"),
        b"original bytes",
        "the refused restore must not consume the backup"
    );
}

#[test]
fn verified_restore_puts_the_backup_back_when_the_slot_is_empty() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"published bytes")
        .expect("write published file");
    let backup = root.path().join(".tributary-backup-test.tmp");
    std::fs::write(&backup, b"original bytes").expect("write backup");
    let authority = authority(&root);
    let published_leaf = authority
        .relative_leaf_identity(Path::new("song.flac"))
        .expect("capture published leaf identity");

    // The publication was deleted by a concurrent writer; the slot is the
    // transfer's own and empty, so restoring the original returns the
    // destination to its pre-transfer state.
    std::fs::remove_file(root.path().join("song.flac")).expect("delete the publication");

    let outcome = authority
        .restore_relative_file_verified(
            Path::new(".tributary-backup-test.tmp"),
            Path::new("song.flac"),
            Some(&published_leaf),
        )
        .expect("restore into the empty recorded slot must succeed");
    assert_eq!(outcome, ReversalOutcome::Reversed);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read restored"),
        b"original bytes"
    );
    assert!(!backup.exists(), "restore consumes the backup");
}
