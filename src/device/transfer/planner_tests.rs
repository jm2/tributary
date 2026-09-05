//! Planning regressions for [`TransferPlanner`].

use std::path::PathBuf;
use std::sync::Arc;

use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{TransferError, TransferItem, TransferRequest};
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
