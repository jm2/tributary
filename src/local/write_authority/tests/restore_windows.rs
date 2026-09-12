//! Windows backup-restore regressions: the object-coupled restore
//! (delete-capable pinned backup handle → change-exact handle verification
//! → slot re-verification → tombstoned handle-bound deletion of the
//! verified publication → no-replace hard-link install → post-install
//! proof → verified link consumption). All tests are `#[cfg(windows)]`:
//! the machinery under test exists only there, exactly like the
//! production arms.
//!
//! The historical Windows restore performed an ordinary path-based
//! REPLACING rename with no post-move verification at all: an interposer
//! landing between the destination gate and the rename was destroyed
//! unverified, and nothing rechecked that the object installed at the
//! destination was the verified backup object. The object-coupled restore
//! refuses — never destroys — at every unprovable point.
#![cfg(windows)]

use std::path::Path;

use super::authority;
use crate::local::write_authority::ReversalOutcome;

/// Seed a destination/backup pair: `published` occupies the destination,
/// `original` the backup sibling, both captured for identity coupling.
fn seed_pair(root: &tempfile::TempDir, published: &[u8], original: &[u8]) {
    std::fs::write(root.path().join("song.flac"), published).expect("write published file");
    std::fs::write(root.path().join(".tributary-backup-test.tmp"), original).expect("write backup");
}

/// An empty destination slot takes the pinned backup object through the
/// no-replace install, the install is verified to BE the backup object,
/// and the redundant backup link is consumed.
#[test]
fn restore_into_empty_slot_installs_and_consumes_the_backup() {
    let root = tempfile::tempdir().expect("temporary root");
    seed_pair(&root, b"published bytes", b"original bytes");
    std::fs::remove_file(root.path().join("song.flac")).expect("empty the destination slot");
    let authority = authority(&root);
    let backup_leaf = authority
        .relative_leaf_identity(Path::new(".tributary-backup-test.tmp"))
        .expect("capture the backup identity");

    let outcome = authority
        .restore_relative_file_verified(
            Path::new(".tributary-backup-test.tmp"),
            Path::new("song.flac"),
            None,
            Some(&backup_leaf),
        )
        .expect("restore into an empty slot");
    assert_eq!(outcome, ReversalOutcome::Reversed);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read restored"),
        b"original bytes",
        "the verified backup object must be installed"
    );
    assert!(
        !root.path().join(".tributary-backup-test.tmp").exists(),
        "the redundant backup link must be consumed"
    );
}

/// Restoring over the exact recorded publication lands the original and
/// consumes the backup: the object-coupled replace tombstones the
/// publication, deletes its name through a verified handle, installs the
/// backup no-replace, and proves the installed object.
#[test]
fn restore_over_verified_publication_lands_the_original() {
    let root = tempfile::tempdir().expect("temporary root");
    seed_pair(&root, b"published bytes", b"original bytes");
    let authority = authority(&root);
    let expected = authority
        .relative_leaf_identity(Path::new("song.flac"))
        .expect("capture the published identity");
    let backup_leaf = authority
        .relative_leaf_identity(Path::new(".tributary-backup-test.tmp"))
        .expect("capture the backup identity");

    let outcome = authority
        .restore_relative_file_verified(
            Path::new(".tributary-backup-test.tmp"),
            Path::new("song.flac"),
            Some(&expected),
            Some(&backup_leaf),
        )
        .expect("restore over the verified publication");
    assert_eq!(outcome, ReversalOutcome::Reversed);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read restored"),
        b"original bytes"
    );
    assert!(
        !root.path().join(".tributary-backup-test.tmp").exists(),
        "the backup must be consumed by the restore"
    );
    let survivors: Vec<String> = std::fs::read_dir(root.path())
        .expect("read root")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert_eq!(
        survivors.len(),
        1,
        "no tombstone or staged litter may survive the restore: {survivors:?}"
    );
}

/// An occupied slot that no longer names the recorded publication is a
/// concurrent writer's interposition: the restore refuses, and BOTH the
/// foreign occupant and the backup survive untouched.
#[test]
fn restore_refuses_a_foreign_slot_without_touching_it() {
    let root = tempfile::tempdir().expect("temporary root");
    seed_pair(&root, b"published bytes", b"original bytes");
    let authority = authority(&root);
    let expected = authority
        .relative_leaf_identity(Path::new("song.flac"))
        .expect("capture the published identity");
    let backup_leaf = authority
        .relative_leaf_identity(Path::new(".tributary-backup-test.tmp"))
        .expect("capture the backup identity");
    // A concurrent writer interposes before the restore runs.
    std::fs::write(root.path().join("song.flac"), b"interposer bytes")
        .expect("interpose at the destination");

    let outcome = authority
        .restore_relative_file_verified(
            Path::new(".tributary-backup-test.tmp"),
            Path::new("song.flac"),
            Some(&expected),
            Some(&backup_leaf),
        )
        .expect("a refusal is an outcome, not an error");
    assert_eq!(outcome, ReversalOutcome::RefusedForeignLeaf);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read interposer"),
        b"interposer bytes",
        "the foreign occupant must survive the refused restore untouched"
    );
    assert_eq!(
        std::fs::read(root.path().join(".tributary-backup-test.tmp")).expect("read backup"),
        b"original bytes",
        "the backup must survive for a later, unblocked restore"
    );
}

/// A swapped backup is refused before anything moves: the replacement is
/// never installed as the original and never deleted, and the destination
/// keeps the transfer's publication.
#[test]
fn restore_refuses_a_swapped_backup() {
    let root = tempfile::tempdir().expect("temporary root");
    seed_pair(&root, b"published bytes", b"original bytes");
    let authority = authority(&root);
    let expected = authority
        .relative_leaf_identity(Path::new("song.flac"))
        .expect("capture the published identity");
    let backup_leaf = authority
        .relative_leaf_identity(Path::new(".tributary-backup-test.tmp"))
        .expect("capture the backup identity");
    // A concurrent writer swaps the backup sibling for a foreign object.
    std::fs::remove_file(root.path().join(".tributary-backup-test.tmp"))
        .expect("unlink the backup");
    std::fs::write(
        root.path().join(".tributary-backup-test.tmp"),
        b"swapped backup",
    )
    .expect("swap in a foreign object");

    let outcome = authority
        .restore_relative_file_verified(
            Path::new(".tributary-backup-test.tmp"),
            Path::new("song.flac"),
            Some(&expected),
            Some(&backup_leaf),
        )
        .expect("a refusal is an outcome, not an error");
    assert_eq!(outcome, ReversalOutcome::RefusedForeignLeaf);
    assert_eq!(
        std::fs::read(root.path().join(".tributary-backup-test.tmp")).expect("read swapped backup"),
        b"swapped backup",
        "the foreign replacement must never be deleted or installed"
    );
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read published"),
        b"published bytes",
        "the destination must be untouched by the refused restore"
    );
}

/// The documented uncoupled-identity degradation still restores: with no
/// recorded identities the occupied slot is replaced by the backup — but
/// through the object-coupled replace (the observed occupant is tombstoned
/// and deletable only through a handle that re-verifies it), and the
/// installed object is still verified post-move.
#[test]
fn legacy_uncoupled_restore_still_replaces_the_slot() {
    let root = tempfile::tempdir().expect("temporary root");
    seed_pair(&root, b"published bytes", b"original bytes");
    let authority = authority(&root);

    let outcome = authority
        .restore_relative_file_verified(
            Path::new(".tributary-backup-test.tmp"),
            Path::new("song.flac"),
            None,
            None,
        )
        .expect("restore with uncaptured identities");
    assert_eq!(outcome, ReversalOutcome::Reversed);
    assert_eq!(
        std::fs::read(root.path().join("song.flac")).expect("read restored"),
        b"original bytes"
    );
    assert!(
        !root.path().join(".tributary-backup-test.tmp").exists(),
        "the backup must be consumed by the restore"
    );
}
