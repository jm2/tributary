//! Destination-leaf boundary regressions for the transfer planner.
//!
//! These exercise the planning-time conflict probe across symlink/reparse
//! ancestors and directory-shaped leaves. They share the fixtures and request
//! builder in `test_support` with the rest of the planning suite and live in
//! their own module so each test file stays within the project's file-length
//! budget.

/// The Windows counterpart of
/// `destination_file_beneath_symlinked_ancestor_is_rejected_at_planning_time`:
/// a destination whose ancestor is a directory reparse point must be refused
/// at planning time rather than followed through an absolute-path lookup that
/// re-resolves every component. Skipped where the environment forbids
/// creating a symlink (no developer mode or privilege).
#[cfg(windows)]
#[test]
fn destination_file_beneath_reparse_ancestor_is_rejected_at_planning_time() {
    use std::os::windows::fs::symlink_dir;
    use std::path::PathBuf;

    use super::test_support::{authority_pair, plan_request, read_authority, write_source_file};
    use super::types::{TransferError, TransferItem};
    use super::TransferPlanner;
    use crate::local::write_authority::ConflictPolicy;

    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let outside = tempfile::tempdir().expect("temporary outside root");
    write_source_file(source_root.path(), "song.flac", b"audio");
    // The external directory already holds a same-named entry: an
    // absolute-path probe would follow the destination's `link` ancestor and
    // classify it as an existing destination, silently skipping the copy.
    std::fs::write(outside.path().join("song.flac"), b"outside").expect("write outside entry");
    if symlink_dir(outside.path(), destination_root.path().join("link")).is_err() {
        // The runner forbids symlink creation (no developer mode/privilege);
        // the reparse traversal cannot be exercised here.
        return;
    }

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("song.flac"),
            PathBuf::from("link/song.flac"),
        )],
        ConflictPolicy::Skip,
        None,
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("a destination beneath a reparse ancestor must be rejected at planning time");
    assert!(
        matches!(error, TransferError::Io { .. }),
        "the planning probe must refuse the reparse ancestor, got {error:?}"
    );
}

/// A destination leaf that already exists as a directory must be classified
/// as present by the planning conflict probe. `open_windows_regular` denies
/// directory opens with `ERROR_ACCESS_DENIED`, so the probe must retry the
/// directory traversal instead of surfacing a spurious I/O error and failing
/// the plan. Exercised on Windows, where the affected probe arm lives.
#[cfg(windows)]
#[test]
fn directory_destination_leaf_is_classified_present_at_planning_time() {
    use std::path::PathBuf;

    use super::test_support::{authority_pair, plan_request, read_authority, write_source_file};
    use super::types::TransferItem;
    use super::TransferPlanner;
    use crate::local::write_authority::ConflictPolicy;

    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"audio");
    // The destination leaf exists, but as a directory.
    std::fs::create_dir(destination_root.path().join("song.flac"))
        .expect("create the directory destination leaf");

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Skip,
        None,
    );
    // A present directory leaf under Skip drops the item; the probe must not
    // turn it into an I/O error.
    let plan = TransferPlanner::new()
        .plan(&request)
        .expect("a directory destination leaf must be classified present, not an I/O error");
    assert_eq!(
        plan.file_count(),
        0,
        "a Skip conflict against a directory leaf must not stage a copy"
    );
}
