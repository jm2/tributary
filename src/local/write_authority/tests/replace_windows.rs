//! Windows Overwrite replace regressions: the object-coupled bind
//! primitive (delete-capable handle → handle-verified identity →
//! handle-bound deletion → no-replace publish). All tests are
//! `#[cfg(windows)]`: the machinery under test exists only there, exactly
//! like the production arms.
//!
//! The historical compare-then-rename replaced whatever occupied the
//! destination path at rename time: an interposer landing between the
//! verification and the rename was destroyed unbacked while the backup
//! still named the previous occupant. The object-coupled primitive binds
//! both the backup and the deletion to the verified OBJECT, so a path
//! replacement can never redirect them.
#![cfg(windows)]

use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use super::authority;
use crate::local::write_authority::{ConflictPolicy, ConflictResolution};

/// An Overwrite commit must report the replaced occupant's bind-time
/// identity together with the backup path, so rollback and cleanup can
/// couple the backup to the exact object it holds.
#[test]
fn overwrite_commit_reports_the_bound_backup_identity() {
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
    assert!(
        outcome.replaced_original_leaf.is_some(),
        "the bind must capture the replaced occupant's identity"
    );
    assert_eq!(
        std::fs::read(root.path().join(&backup)).expect("read backup"),
        b"original bytes",
        "the backup must hold the replaced occupant's bytes"
    );
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read published"),
        b"new bytes"
    );
}

/// An occupant pinned by another handle (opened without
/// `FILE_SHARE_DELETE`) can be neither opened for deletion nor replaced:
/// the commit must fail closed, never destroy the pinned occupant, and
/// never leave a backup or staged litter behind. This is the
/// fail-closed guarantee the object-coupled primitive owes when no
/// coupled primitive can be obtained.
#[test]
fn pinned_occupant_fails_closed_without_destruction() {
    let root = tempfile::tempdir().expect("temporary root");
    let destination = root.path().join("song.flac");
    std::fs::write(&destination, b"pinned original").expect("write existing original");

    // A concurrent writer holds the occupant open without delete sharing,
    // pinning it against the handle-conditioned deletion. The share mode
    // must be explicit: the std default grants `FILE_SHARE_DELETE`, which
    // would let the bind's delete-capable open succeed and the replace
    // proceed — no pin at all.
    let pin = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
        .open(&destination)
        .expect("open the pinned occupant");

    let authority = authority(&root);
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Overwrite)
        .expect("prepare overwrite");
    staged.write_all(b"new bytes").expect("write new");
    let outcome = staged.commit();

    assert!(
        outcome.is_err(),
        "the replace must fail closed against a pinned occupant"
    );
    assert_eq!(
        std::fs::read(&destination).expect("read pinned occupant"),
        b"pinned original",
        "the pinned occupant must survive the failed replace untouched"
    );
    let survivors: Vec<String> = std::fs::read_dir(root.path())
        .expect("read root")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert_eq!(
        survivors.len(),
        1,
        "no backup or staged litter may survive the failed replace: {survivors:?}"
    );
    drop(pin);
}
