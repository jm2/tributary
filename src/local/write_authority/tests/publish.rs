//! Overwrite-publish regressions: the replaced occupant's backup binding,
//! racer and vanished-occupant degradation, and no-replace refusals.

use std::io;
use std::path::Path;

use super::authority;
use crate::local::write_authority::{CommitError, ConflictPolicy, ConflictResolution};

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
