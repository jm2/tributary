//! Regressions for the mounted write authority: staged writes, conflict
//! policies, rollback, drop cleanup, and boundary refusal.

use std::io;
use std::path::Path;

use super::{
    CommitError, ConflictPolicy, ConflictResolution, MountedWriteAuthority, ReversalOutcome,
};

/// Acquire a write authority on a fresh temporary root; dropping the guard
/// removes the tree.
fn authority(root: &tempfile::TempDir) -> MountedWriteAuthority {
    MountedWriteAuthority::acquire(root.path()).expect("acquire write authority")
}

#[test]
fn fresh_write_commits_atomically() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
        .expect("prepare staged file");
    staged.write_all(b"audio payload").expect("write payload");

    let outcome = staged.commit().expect("commit staged file");
    assert_eq!(outcome.resolution, ConflictResolution::Fresh);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read final"),
        b"audio payload"
    );
}

#[test]
fn skip_policy_rejects_when_destination_exists() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"existing").expect("write existing");

    let authority = authority(&root);
    let error = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Skip)
        .expect_err("skip policy must reject existing destination");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
}

#[test]
fn overwrite_policy_replaces_final_file() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"old").expect("write existing");

    let authority = authority(&root);
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Overwrite)
        .expect("prepare overwrite");
    staged.write_all(b"new").expect("write new");
    staged.commit().expect("commit overwrite");

    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read final"),
        b"new"
    );
}

#[test]
fn preserve_policy_writes_to_disambiguated_name() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"first").expect("write first");

    let authority = authority(&root);
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
        .expect("prepare preserve");
    staged.write_all(b"second").expect("write second");
    let outcome = staged.commit().expect("commit preserve");

    assert_eq!(outcome.resolution, ConflictResolution::Preserved);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read original"),
        b"first"
    );
    assert_eq!(
        std::fs::read(root.path().join(&outcome.relative_path)).expect("read preserved"),
        b"second"
    );
}

#[test]
fn rollback_removes_staged_file() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
        .expect("prepare staged");
    staged.write_all(b"partial").expect("write partial");
    let staged_path = staged.staged_path().to_path_buf();
    // Sanity: the staged file actually exists before rollback.
    assert!(staged_path.exists());
    staged.rollback().expect("rollback staged");
    assert!(!staged_path.exists());
    assert!(!root.path().join("song.flac").exists());
}

#[test]
fn dropped_target_removes_staged_file() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let staged_path = {
        let mut staged = authority
            .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
            .expect("prepare staged");
        staged.write_all(b"partial").expect("write partial");
        staged.staged_path().to_path_buf()
        // Dropped without an explicit rollback: the staged handle must be
        // closed before the staged file is removed, or Windows refuses
        // the delete with a sharing violation and the temp file leaks.
    };
    assert!(!staged_path.exists());
    assert!(!root.path().join("song.flac").exists());
}

#[test]
fn cross_mount_path_is_rejected() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let error = authority
        .prepare_write_relative_file(Path::new("../outside.flac"), ConflictPolicy::Fail)
        .expect_err("parent path must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    let error = authority
        .prepare_write_relative_file(Path::new("/etc/passwd"), ConflictPolicy::Fail)
        .expect_err("absolute path must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn directory_creation_and_file_writes_combine() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let bound = authority
        .create_relative_directory(Path::new("album"), ConflictPolicy::Fail)
        .expect("create album dir");
    assert_eq!(bound.relative_path(), Path::new("album"));

    let mut staged = bound
        .prepare_write_in_directory("song.flac", ConflictPolicy::Fail)
        .expect("prepare file under dir");
    staged.write_all(b"nested").expect("write nested");
    staged.commit().expect("commit nested");

    assert_eq!(
        std::fs::read(root.path().join("album/song.flac")).expect("read nested"),
        b"nested"
    );
}

#[test]
fn prepared_target_resolves_only_one_preserved_name() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"original").expect("write original");

    let authority = authority(&root);

    let mut first = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
        .expect("prepare first preserve");
    first.write_all(b"a").expect("write first");
    let first_outcome = first.commit().expect("commit first");

    let mut second = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
        .expect("prepare second preserve");
    second.write_all(b"b").expect("write second");
    let second_outcome = second.commit().expect("commit second");

    assert_ne!(first_outcome.relative_path, second_outcome.relative_path);
    let names: Vec<String> = std::fs::read_dir(root.path())
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert_eq!(names.len(), 3);
    assert!(names.iter().any(|name| name == "song.flac"));
    assert!(names.iter().any(|name| name == "song (1).flac"));
    assert!(names.iter().any(|name| name == "song (2).flac"));
}

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

// ── No-replace publish and backup restore ───────────────────────────────

/// An Overwrite commit must bind the replaced occupant's bytes to a hidden
/// backup sibling and report it as `replaced_original`, so a caller's
/// rollback can restore exactly what was destroyed.
#[test]
fn overwrite_commit_binds_backup_of_replaced_original() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"original bytes")
        .expect("write existing original");

    let authority = authority(&root);
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Overwrite)
        .expect("prepare overwrite");
    staged.write_all(b"new bytes").expect("write new");
    let outcome = staged.commit().expect("commit overwrite");

    assert_eq!(outcome.resolution, ConflictResolution::Overwrite);
    let backup = outcome
        .replaced_original
        .expect("replaced original must be bound");
    assert_ne!(backup, std::path::Path::new("song.flac"));
    assert_eq!(
        std::fs::read(root.path().join(&backup)).expect("read bound backup"),
        b"original bytes",
        "the backup must hold the replaced occupant's bytes"
    );
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read final"),
        b"new bytes"
    );
}

/// The [C] TOCTOU regression: a destination occupied at planning, deleted,
/// and recreated by a concurrent writer before the commit. The publish must
/// bind the *racer's* bytes as the replaced original — never silently
/// replace-and-delete them as an unbacked publish.
#[test]
fn overwrite_commit_binds_racer_who_recreated_the_destination() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"planned occupant")
        .expect("write planned occupant");

    let authority = authority(&root);
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Overwrite)
        .expect("prepare overwrite");
    staged.write_all(b"new bytes").expect("write new");

    // The planned occupant is removed and a racer recreates the name
    // between staging and commit.
    std::fs::remove_file(root.path().join("song.flac")).expect("remove planned occupant");
    std::fs::write(root.path().join("song.flac"), b"racer bytes")
        .expect("racer recreates destination");

    let outcome = staged.commit().expect("commit overwrite");

    let backup = outcome
        .replaced_original
        .expect("the racer's file must be backed up, not destroyed");
    assert_eq!(
        std::fs::read(root.path().join(&backup)).expect("read bound backup"),
        b"racer bytes",
        "rollback must restore the concurrent writer's file"
    );
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read final"),
        b"new bytes"
    );
}

/// An occupant that vanishes between planning and commit degrades the
/// Overwrite publish to a fresh one: no backup is bound, and the commit
/// must leave no backup litter behind.
#[test]
fn overwrite_commit_publishes_fresh_when_occupant_vanishes() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::write(root.path().join("song.flac"), b"planned occupant")
        .expect("write planned occupant");

    let authority = authority(&root);
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Overwrite)
        .expect("prepare overwrite");
    staged.write_all(b"new bytes").expect("write new");

    std::fs::remove_file(root.path().join("song.flac")).expect("occupant vanishes");

    let outcome = staged.commit().expect("commit overwrite");
    assert_eq!(
        outcome.replaced_original, None,
        "a vanished occupant has no bytes to restore"
    );
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read final"),
        b"new bytes"
    );
    let names: Vec<String> = std::fs::read_dir(root.path())
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert_eq!(
        names,
        vec!["song.flac".to_string()],
        "the fresh-degraded publish must leave no backup litter"
    );
}

#[test]
fn fresh_publish_refuses_post_plan_destination() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    // The destination is absent at staging, so Preserve resolves fresh.
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
        .expect("prepare fresh");
    assert_eq!(staged.resolution(), ConflictResolution::Fresh);
    staged.write_all(b"planned").expect("write staged");
    // The destination appears after the resolution was made.
    std::fs::write(root.path().join("song.flac"), b"racer").expect("write racer");

    let error = staged
        .commit()
        .expect_err("no-replace publish must refuse an existing destination");
    assert!(
        matches!(error, CommitError::Io(ref error) if error.kind() == io::ErrorKind::AlreadyExists),
        "the collision must surface as a plain io AlreadyExists error: {error:?}"
    );
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read racer"),
        b"racer",
        "the fresh publish must never replace the destination"
    );
    let names: Vec<String> = std::fs::read_dir(root.path())
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert_eq!(
        names,
        vec!["song.flac".to_string()],
        "the refused staged file must be discarded"
    );
}

#[cfg(unix)]
#[test]
fn dangling_symlink_destination_is_never_replaced_through() {
    let root = tempfile::tempdir().expect("temporary root");
    std::os::unix::fs::symlink("missing-target.flac", root.path().join("song.flac"))
        .expect("create dangling symlink");
    let authority = authority(&root);

    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
        .expect("prepare fresh");
    staged.write_all(b"payload").expect("write staged");
    let error = staged
        .commit()
        .expect_err("no-replace publish must refuse the symlink entry");
    assert!(
        matches!(error, CommitError::Io(ref error) if error.kind() == io::ErrorKind::AlreadyExists),
        "the collision must surface as a plain io AlreadyExists error: {error:?}"
    );
    let metadata = std::fs::symlink_metadata(root.path().join("song.flac"))
        .expect("destination still present");
    assert!(
        metadata.file_type().is_symlink(),
        "the symlink must be left exactly as found, not replaced through"
    );
}

#[test]
fn overwrite_onto_directory_destination_is_refused() {
    let root = tempfile::tempdir().expect("temporary root");
    std::fs::create_dir(root.path().join("album")).expect("create directory destination");
    let authority = authority(&root);

    let mut staged = authority
        .prepare_write_relative_file(Path::new("album"), ConflictPolicy::Overwrite)
        .expect("prepare overwrite onto directory");
    staged.write_all(b"payload").expect("write staged");
    let error = staged
        .commit()
        .expect_err("a directory destination must not be overwritten by a file");
    let _ = error;
    assert!(
        root.path().join("album").is_dir(),
        "the directory must survive the refused overwrite"
    );
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
