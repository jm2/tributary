//! Staged-write lifecycle regressions: commit, conflict policies at
//! staging, rollback, drop cleanup, and boundary refusal.

use std::io;
use std::path::Path;

use super::authority;
use crate::local::write_authority::{ConflictPolicy, ConflictResolution};

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

    let (bound, created) = authority
        .create_relative_directory(Path::new("album"), ConflictPolicy::Fail)
        .expect("create album dir");
    assert_eq!(bound.relative_path(), Path::new("album"));
    assert_eq!(
        created,
        vec![Path::new("album").to_path_buf()],
        "the created-component report must name exactly what this call created"
    );

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

/// An adopted directory is never reported as created: the created-component
/// report of a creation over an existing leaf is empty, so a concurrently
/// created (or pre-existing) directory can never be recorded — or rolled
/// back — as the transfer's own.
#[test]
fn adopted_directory_is_not_reported_as_created() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);
    std::fs::create_dir(root.path().join("album")).expect("pre-create album dir");

    let (_, created) = authority
        .create_relative_directory(Path::new("album"), ConflictPolicy::Overwrite)
        .expect("adopt existing album dir");
    assert!(
        created.is_empty(),
        "an adopted leaf owns nothing: {created:?}"
    );
}

/// Only the components the authority actually created are reported: an
/// existing intermediate component is adopted silently while the missing
/// components below it are named exactly, so ownership recording claims
/// neither more nor less than the transfer created.
#[test]
fn created_component_report_names_only_created_components() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);
    std::fs::create_dir(root.path().join("x")).expect("pre-create intermediate x");

    let (_, created) = authority
        .create_relative_directory(Path::new("x/y/z"), ConflictPolicy::Preserve)
        .expect("create nested chain under adopted x");
    assert_eq!(
        created,
        vec![
            Path::new("x/y").to_path_buf(),
            Path::new("x/y/z").to_path_buf(),
        ],
        "the adopted ancestor must be absent from the report: {created:?}"
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
