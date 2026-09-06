//! Regressions for the mounted write authority: staged writes, conflict
//! policies, rollback, drop cleanup, and boundary refusal.

use std::io;
use std::path::Path;

use super::{ConflictPolicy, ConflictResolution, MountedWriteAuthority};

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
