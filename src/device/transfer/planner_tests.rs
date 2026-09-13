//! Planning regressions for [`TransferPlanner`].

use std::path::PathBuf;
use std::sync::Arc;

use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{Stage, TransferError, TransferItem, TransferRequest};
use super::TransferPlanner;
use crate::local::root_authority::MountedRootAuthority;
use crate::local::write_authority::{ConflictPolicy, MountedWriteAuthority};

/// Build a recursive, budgeted transfer request in one call.
fn plan_request(
    source: Arc<MountedRootAuthority>,
    destination: MountedWriteAuthority,
    items: Vec<TransferItem>,
    conflict_policy: ConflictPolicy,
    capacity_budget: Option<u64>,
) -> TransferRequest {
    TransferRequest {
        source,
        destination,
        items,
        conflict_policy,
        capacity_budget,
        recurse_directories: true,
    }
}

#[test]
fn plan_resolves_file_into_single_copy_stage() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/song.flac", b"audio");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("album/song.flac"))],
        ConflictPolicy::Preserve,
        None,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert_eq!(plan.file_count(), 1);
    assert!(
        plan.directory_count() >= 1,
        "album directory must be staged"
    );
    let total = plan.total_bytes();
    assert!(total >= 5);
}

#[test]
fn capacity_budget_rejects_oversized_plan() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "big.flac", &[0u8; 100]);
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("big.flac"))],
        ConflictPolicy::Preserve,
        Some(10),
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("oversized plan must be rejected");
    assert!(matches!(error, TransferError::CapacityExceeded { .. }));
}

#[test]
fn plan_walks_directory_recursively() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    write_source_file(source_root.path(), "album/nested/b.flac", b"b");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        ConflictPolicy::Preserve,
        None,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert_eq!(plan.file_count(), 2);
    assert!(
        plan.directory_count() >= 2,
        "album and nested must be staged"
    );
}

#[test]
fn conflict_fail_rejects_existing_destination() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"new");
    std::fs::write(destination_root.path().join("song.flac"), b"old").expect("write existing");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Fail,
        None,
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("fail policy must reject existing destination");
    assert!(matches!(error, TransferError::ConflictRejected { .. }));
}

#[test]
fn skip_policy_skips_existing_destination() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"new");
    std::fs::write(destination_root.path().join("song.flac"), b"old").expect("write existing");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Skip,
        None,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert_eq!(
        plan.file_count(),
        0,
        "skip policy should produce no copy stages"
    );
}

/// A walked file skipped under the Skip policy must stage no parent
/// directory work either: conflict resolution happens before parent
/// staging, so never-copied files emit no `CreateDirectory` stages for
/// their ancestors (and execution creates no empty destination directories
/// for them). The explicit directory item's own creation stage is the
/// directory-item contract and is still staged exactly once.
#[test]
fn skip_walk_stages_no_work_for_skipped_files() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/sub/a.flac", b"new");
    std::fs::create_dir_all(destination_root.path().join("imported/sub")).expect("create dest");
    std::fs::write(destination_root.path().join("imported/sub/a.flac"), b"old")
        .expect("write existing destination");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        ConflictPolicy::Skip,
        None,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert_eq!(
        plan.file_count(),
        0,
        "a fully skipped walk must stage no copy work"
    );
    assert_eq!(
        plan.directory_count(),
        1,
        "only the directory item itself may be staged; the skipped \
         file's walked ancestors must not be"
    );
    let directories: Vec<&PathBuf> = plan
        .stages()
        .iter()
        .filter_map(|stage| match stage {
            Stage::CreateDirectory {
                destination_relative_path,
            } => Some(destination_relative_path),
            _ => None,
        })
        .collect();
    assert_eq!(
        directories,
        vec![&PathBuf::from("imported")],
        "the walked ancestor 'imported/sub' of the skipped file must not be staged"
    );
}

#[test]
fn empty_request_plans_to_no_stages() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(source, destination, vec![], ConflictPolicy::Preserve, None);
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert!(plan.is_empty());
    assert_eq!(plan.stage_count(), 0);
    assert_eq!(plan.total_bytes(), 0);
}

#[test]
fn absolute_path_in_request_is_rejected() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("/etc/passwd"))],
        ConflictPolicy::Preserve,
        None,
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("absolute path must be rejected");
    assert!(matches!(error, TransferError::InvalidItemPath { .. }));
}

/// Lazily unmounts the path when dropped, so a failing assertion cannot
/// leak the bind mount registered by the regression below.
#[cfg(target_os = "linux")]
struct LazyUnmount(std::path::PathBuf);

#[cfg(target_os = "linux")]
impl Drop for LazyUnmount {
    fn drop(&mut self) {
        let _ = std::process::Command::new("umount")
            .arg("-l")
            .arg(&self.0)
            .status();
    }
}

/// The planning-time boundary probe must accept every directory of a
/// normal (non-mount) tree: only an actual boundary crossing may reject,
/// so non-mount staging and accounting are unchanged. Deterministic on
/// every platform, unlike the bind-mount regression below.
#[test]
fn walked_directory_boundary_accepts_normal_nested_directories() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    write_source_file(source_root.path(), "album/nested/a.flac", b"a");
    let source = read_authority(source_root.path());
    source
        .validate_walked_directory_boundary(&source_root.path().join("album"))
        .expect("the walked root directory shares the source boundary");
    source
        .validate_walked_directory_boundary(&source_root.path().join("album/nested"))
        .expect("nested non-mount directories share the source boundary");
}

/// A source subtree containing a nested bind mount must be rejected AT
/// PLANNING TIME with the nested mount path named — never mid-execution,
/// after earlier stages had already committed. The walk's `st_dev`
/// comparison cannot see the bind mount (same device), so without the
/// planner's per-directory boundary check the nested-mount file would be
/// staged, counted into the totals, and then refused by the executor's
/// per-component mount-ID checks. Requires the privilege to create a bind
/// mount; skips when the environment cannot provide it.
#[cfg(target_os = "linux")]
#[test]
fn nested_bind_mount_is_rejected_at_planning_time() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    write_source_file(source_root.path(), "mounted-src/nested.flac", b"nested");
    std::fs::create_dir(source_root.path().join("album/mnt")).expect("create bind target");

    // Bind `mounted-src` onto `album/mnt`. Both sit under the walk root on
    // the same device, so `same_file_system(true)` descends into the mount
    // — exactly the case the planning-time boundary check must catch.
    let mounted = std::process::Command::new("mount")
        .arg("--bind")
        .arg(source_root.path().join("mounted-src"))
        .arg(source_root.path().join("album/mnt"))
        .status()
        .is_ok_and(|status| status.success());
    if !mounted {
        eprintln!("skipping: bind mount unavailable in this environment");
        return;
    }
    let _unmount = LazyUnmount(source_root.path().join("album/mnt"));

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        ConflictPolicy::Preserve,
        None,
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("a nested bind mount must be rejected at planning time");
    let TransferError::NestedMountBoundary { path } = error else {
        panic!("expected NestedMountBoundary, got {error:?}");
    };
    assert_eq!(
        path,
        source_root.path().join("album/mnt"),
        "the typed error must name the nested mount path"
    );
}

/// Bind `source` onto `target` at the driver level. Returns `false` when the
/// environment cannot create a bind mount, so the file-boundary regressions
/// below skip instead of failing on an unprivileged host.
#[cfg(target_os = "linux")]
fn bind_mount(source: &std::path::Path, target: &std::path::Path) -> bool {
    std::process::Command::new("mount")
        .arg("--bind")
        .arg(source)
        .arg(target)
        .status()
        .is_ok_and(|status| status.success())
}

/// A regular file that is itself a bind mount keeps the walk's `st_dev`
/// (same device) and satisfies `entry.file_type().is_file()`, so without a
/// per-file boundary probe the planner would stage and count the
/// mount-foreign bytes and refuse only mid-run, after earlier stages had
/// already committed. The whole plan must be rejected at planning time with
/// the bind-mounted file named.
#[cfg(target_os = "linux")]
#[test]
fn walked_bind_mounted_regular_file_is_rejected_at_planning_time() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "mounted-src/song.flac", b"foreign");
    write_source_file(source_root.path(), "album/song.flac", b"target");

    if !bind_mount(
        &source_root.path().join("mounted-src/song.flac"),
        &source_root.path().join("album/song.flac"),
    ) {
        eprintln!("skipping: bind mount unavailable in this environment");
        return;
    }
    let _unmount = LazyUnmount(source_root.path().join("album/song.flac"));

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        ConflictPolicy::Preserve,
        None,
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("a bind-mounted regular file must be rejected at planning time");
    let TransferError::NestedMountBoundary { path } = error else {
        panic!("expected NestedMountBoundary, got {error:?}");
    };
    assert_eq!(
        path,
        source_root.path().join("album/song.flac"),
        "the typed error must name the bind-mounted file"
    );
}

/// A directly requested single-file item whose leaf is itself a bind mount is
/// never walked, so it relies on the per-file boundary probe rather than the
/// walk's directory probe. It must be refused at planning time with the file
/// named, exactly like a walked bind-mounted file.
#[cfg(target_os = "linux")]
#[test]
fn direct_bind_mounted_file_item_is_rejected_at_planning_time() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "mounted-src/song.flac", b"foreign");
    write_source_file(source_root.path(), "album/song.flac", b"target");

    if !bind_mount(
        &source_root.path().join("mounted-src/song.flac"),
        &source_root.path().join("album/song.flac"),
    ) {
        eprintln!("skipping: bind mount unavailable in this environment");
        return;
    }
    let _unmount = LazyUnmount(source_root.path().join("album/song.flac"));

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("album/song.flac"))],
        ConflictPolicy::Preserve,
        None,
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("a directly requested bind-mounted file must be rejected at planning time");
    let TransferError::NestedMountBoundary { path } = error else {
        panic!("expected NestedMountBoundary, got {error:?}");
    };
    assert_eq!(
        path,
        source_root.path().join("album/song.flac"),
        "the typed error must name the directly requested bind-mounted file"
    );
}

/// A directly requested single-file item whose path traverses a bind-mounted
/// directory must be refused at planning time with the file named. The file
/// itself carries the nested mount's identity even though the walk is not
/// involved at all.
#[cfg(target_os = "linux")]
#[test]
fn direct_file_beneath_nested_bind_mount_is_rejected_at_planning_time() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "mounted-src/nested.flac", b"nested");
    std::fs::create_dir_all(source_root.path().join("album/mnt")).expect("create bind target");

    if !bind_mount(
        &source_root.path().join("mounted-src"),
        &source_root.path().join("album/mnt"),
    ) {
        eprintln!("skipping: bind mount unavailable in this environment");
        return;
    }
    let _unmount = LazyUnmount(source_root.path().join("album/mnt"));

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("album/mnt/nested.flac"))],
        ConflictPolicy::Preserve,
        None,
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("a file beneath a nested bind mount must be rejected at planning time");
    let TransferError::NestedMountBoundary { path } = error else {
        panic!("expected NestedMountBoundary, got {error:?}");
    };
    assert_eq!(
        path,
        source_root.path().join("album/mnt/nested.flac"),
        "the typed error must name the file beneath the nested mount"
    );
}

/// A directly requested source file whose path traverses a symlink ancestor
/// beneath the root must be rejected AT PLANNING TIME. An absolute-path
/// probe follows the ancestor (it only refuses the final component) and
/// accepts the file, so the planner would enumerate metadata outside the
/// retained source root and emit a plan the executor's per-component
/// no-follow traversal rejects mid-run, possibly after earlier stages had
/// committed. The planning probe must traverse from the retained root
/// component by component, exactly like the executor, and refuse the
/// symlink ancestor before any stage is planned. Deterministic on Unix,
/// where creating a symlink needs no privilege.
#[cfg(unix)]
#[test]
fn direct_file_beneath_symlinked_ancestor_is_rejected_at_planning_time() {
    use std::os::unix::fs::symlink;

    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let outside = tempfile::tempdir().expect("temporary outside root");
    write_source_file(outside.path(), "real/song.flac", b"outside");
    // The symlink is an ANCESTOR of the requested leaf, not its immediate
    // parent: the immediate parent is a real directory, so only a
    // per-component traversal can see the crossing.
    symlink(outside.path(), source_root.path().join("link")).expect("create symlinked ancestor");

    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = plan_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("link/real/song.flac"))],
        ConflictPolicy::Preserve,
        None,
    );
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("a file beneath a symlinked ancestor must be rejected at planning time");
    assert!(
        matches!(error, TransferError::Io { .. }),
        "the planning probe must refuse the symlink ancestor (a crossing, not a typed \
         nested-mount error), got {error:?}"
    );
}
