//! Source-side directory-item boundary regressions for the transfer
//! planner.
//!
//! A directory item is classified by an absolute-path `symlink_metadata`
//! lookup whose resolution follows symlink/reparse ancestors, so the
//! retained root must re-probe the item directory itself — exactly like
//! every walked subdirectory — before any destination ancestor stage is
//! planned, in BOTH recursion modes: a non-recursive directory item
//! beneath a replaced ancestor would otherwise plan a creation that
//! executes entirely outside the retained boundary. They share the
//! fixtures and request builder in `test_support` with the rest of the
//! planning suite.

use std::path::PathBuf;

use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{TransferError, TransferItem, TransferRequest};
use super::TransferPlanner;
use crate::local::write_authority::ConflictPolicy;

/// Build a directory-item transfer request with an explicit recursion
/// mode: the planner's conflict policy does not consult the destination
/// for directory items, so the default policy is carried verbatim.
fn directory_request(
    source: std::sync::Arc<crate::local::root_authority::MountedRootAuthority>,
    destination: crate::local::write_authority::MountedWriteAuthority,
    item: TransferItem,
    recurse_directories: bool,
) -> TransferRequest {
    TransferRequest {
        source,
        destination,
        items: vec![item],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories,
    }
}

/// Plan `link/album` in one recursion mode against the shared out-of-bounds
/// fixture: an in-bounds `album`, an external tree holding its out-of-bounds
/// twin, and a `link` symlink inside the source root pointing at the
/// external tree. Returns the planning outcome for the caller to assert on.
fn plan_through_symlinked_ancestor(
    recurse_directories: bool,
    source_root: &std::path::Path,
    destination_root: &std::path::Path,
) -> Result<super::types::TransferPlan, TransferError> {
    let source = read_authority(source_root);
    let (_, destination) = authority_pair(destination_root);
    let request = directory_request(
        source,
        destination,
        TransferItem::same(PathBuf::from("link/album")),
        recurse_directories,
    );
    TransferPlanner::new().plan(&request)
}

/// The Unix counterpart of the reparse regressions below: a non-recursive
/// directory item beneath a symlinked ancestor must be rejected at planning
/// time rather than followed through the classification lookup into the
/// external tree. Skipped where the environment forbids creating a symlink.
#[cfg(unix)]
#[test]
fn nonrecursive_directory_beneath_symlinked_ancestor_is_rejected_at_planning_time() {
    use std::os::unix::fs::symlink;

    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let outside = tempfile::tempdir().expect("temporary outside root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    std::fs::create_dir(outside.path().join("album")).expect("create external album twin");
    if symlink(outside.path(), source_root.path().join("link")).is_err() {
        // The runner forbids symlink creation; the traversal cannot be
        // exercised here.
        return;
    }

    let error = plan_through_symlinked_ancestor(false, source_root.path(), destination_root.path())
        .expect_err(
            "a non-recursive directory beneath a symlinked ancestor must be rejected at \
             planning time",
        );
    assert!(
        matches!(error, TransferError::Io { .. }),
        "the retained-root probe must refuse the symlinked ancestor, got {error:?}"
    );
}

/// The recursive twin: the same out-of-bounds item must be rejected in the
/// recursion mode too. The walked-directory probes already refused this
/// shape before the item itself was probed; the assertion pins that the
/// added pre-ancestor probe keeps both modes refusing identically.
#[cfg(unix)]
#[test]
fn recursive_directory_beneath_symlinked_ancestor_is_rejected_at_planning_time() {
    use std::os::unix::fs::symlink;

    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let outside = tempfile::tempdir().expect("temporary outside root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    std::fs::create_dir(outside.path().join("album")).expect("create external album twin");
    if symlink(outside.path(), source_root.path().join("link")).is_err() {
        // The runner forbids symlink creation; the traversal cannot be
        // exercised here.
        return;
    }

    let error = plan_through_symlinked_ancestor(true, source_root.path(), destination_root.path())
        .expect_err(
            "a recursive directory beneath a symlinked ancestor must be rejected at planning \
             time",
        );
    assert!(
        matches!(error, TransferError::Io { .. }),
        "the retained-root probe must refuse the symlinked ancestor, got {error:?}"
    );
}

/// The Windows counterpart of
/// `nonrecursive_directory_beneath_symlinked_ancestor_is_rejected_at_planning_time`:
/// a directory item whose ancestor is a directory reparse point must be
/// refused at planning time in both recursion modes rather than followed
/// through an absolute-path lookup that re-resolves every component.
/// Skipped where the environment forbids creating a reparse point (no
/// developer mode or privilege).
#[cfg(windows)]
#[test]
fn directory_beneath_reparse_ancestor_is_rejected_at_planning_time_in_both_modes() {
    use std::os::windows::fs::symlink_dir;

    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let outside = tempfile::tempdir().expect("temporary outside root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    std::fs::create_dir(outside.path().join("album")).expect("create external album twin");
    if symlink_dir(outside.path(), source_root.path().join("link")).is_err() {
        // The runner forbids reparse-point creation (no developer mode or
        // privilege); the traversal cannot be exercised here.
        return;
    }

    for recurse_directories in [false, true] {
        let error = plan_through_symlinked_ancestor(
            recurse_directories,
            source_root.path(),
            destination_root.path(),
        )
        .expect_err(
            "a directory beneath a reparse ancestor must be rejected at planning time in both \
             recursion modes",
        );
        assert!(
            matches!(error, TransferError::Io { .. }),
            "the retained-root probe must refuse the reparse ancestor, got {error:?}"
        );
    }
}

/// The control: a directory item inside the retained root must plan in
/// BOTH recursion modes, with the recursion mode deciding the stage shape —
/// a bare directory creation when non-recursive, the directory plus every
/// contained file when recursive. Proves the added probe does not refuse
/// an ordinary in-bounds directory item.
#[test]
fn in_bounds_directory_item_plans_in_both_recursion_modes() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/a.flac", b"a");

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = directory_request(
        source,
        destination,
        TransferItem::same(PathBuf::from("album")),
        false,
    );
    let plan = TransferPlanner::new()
        .plan(&request)
        .expect("an in-bounds directory item must plan non-recursively");
    assert_eq!(plan.directory_count(), 1, "the directory itself is staged");
    assert_eq!(plan.file_count(), 0, "non-recursive mode stages no files");

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = directory_request(
        source,
        destination,
        TransferItem::same(PathBuf::from("album")),
        true,
    );
    let plan = TransferPlanner::new()
        .plan(&request)
        .expect("an in-bounds directory item must plan recursively");
    assert_eq!(plan.directory_count(), 1, "the directory itself is staged");
    assert_eq!(
        plan.file_count(),
        1,
        "recursive mode stages the contained file"
    );
}
