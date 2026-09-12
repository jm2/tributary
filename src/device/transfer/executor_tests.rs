//! Execution regressions for [`TransferExecutor`]: commits, recursive
//! directory copies, progress reporting, and conflict policies.

use std::path::PathBuf;
use std::sync::Arc;

use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{Stage, TransferItem, TransferProgress, TransferRequest, TransferSummary};
use super::{TransferExecutor, TransferPlanner};
use crate::local::root_authority::MountedRootAuthority;
use crate::local::write_authority::{ConflictPolicy, MountedWriteAuthority};
use crate::source_lifecycle::CancellationObserver;

/// Build a recursive transfer request in one call.
fn transfer_request(
    source: Arc<MountedRootAuthority>,
    destination: MountedWriteAuthority,
    items: Vec<TransferItem>,
    conflict_policy: ConflictPolicy,
) -> TransferRequest {
    TransferRequest {
        source,
        destination,
        items,
        conflict_policy,
        capacity_budget: None,
        recurse_directories: true,
    }
}

/// Run a freshly planned request through the executor.
fn run(request: TransferRequest) -> TransferSummary {
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("run")
}

#[test]
fn executor_copies_a_single_file() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"copy me");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Preserve,
    );
    let summary = run(request);
    assert!(summary.completed);
    let final_path = destination_root.path().join("song.flac");
    let bytes = std::fs::read(&final_path).expect("read final");
    assert_eq!(bytes, b"copy me");
}

#[test]
fn executor_recursive_directory_copy() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "album/a.flac", b"a");
    write_source_file(source_root.path(), "album/b.flac", b"b");
    write_source_file(source_root.path(), "album/nested/c.flac", b"c");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        ConflictPolicy::Preserve,
    );
    let summary = run(request);
    assert!(summary.completed);
    assert_eq!(
        std::fs::read(destination_root.path().join("imported/a.flac")).expect("read a"),
        b"a"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("imported/b.flac")).expect("read b"),
        b"b"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("imported/nested/c.flac")).expect("read c"),
        b"c"
    );
}

/// Records every progress callback so chunk counts can be asserted.
struct ProgressRecorder {
    stage_starts: u32,
    stage_completes: u32,
    byte_chunks: u32,
}

impl ProgressRecorder {
    fn new() -> Self {
        Self {
            stage_starts: 0,
            stage_completes: 0,
            byte_chunks: 0,
        }
    }
}

impl TransferProgress for ProgressRecorder {
    fn on_stage_started(&mut self, _stage: &Stage, _index: u32, _total: u32) {
        self.stage_starts = self.stage_starts.saturating_add(1);
    }
    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        _index: u32,
        _total: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        self.stage_completes = self.stage_completes.saturating_add(1);
    }
    fn on_bytes_copied(
        &mut self,
        _stage_index: u32,
        _total_stages: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        self.byte_chunks = self.byte_chunks.saturating_add(1);
    }
}

#[test]
fn progress_callback_reports_every_stage_and_chunk() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    // 200 KiB so the 64 KiB chunked copy yields multiple progress reports.
    let payload = vec![0u8; 200 * 1024];
    write_source_file(source_root.path(), "payload.bin", &payload);
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("payload.bin"))],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ProgressRecorder::new();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("run");
    assert!(summary.completed);
    assert_eq!(progress.stage_starts, 1, "one copy stage should start");
    assert_eq!(
        progress.stage_completes, 1,
        "one copy stage should complete"
    );
    assert!(
        progress.byte_chunks >= 3,
        "at least three progress chunks for 200 KiB"
    );
}

#[test]
fn overwrite_policy_replaces_existing_destination() {
    let source_root = tempfile::tempdir().expect("temporary source root");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    write_source_file(source_root.path(), "song.flac", b"new");
    std::fs::write(destination_root.path().join("song.flac"), b"old").expect("write existing");
    let source = read_authority(source_root.path());
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        source,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Overwrite,
    );
    let _ = run(request);
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read final"),
        b"new"
    );
}
