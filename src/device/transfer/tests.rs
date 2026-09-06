//! Tests for the transfer planner and executor.

use super::*;

use crate::local::write_authority::ConflictPolicy;

fn unique(label: &str) -> PathBuf {
    tempfile::Builder::new()
        .prefix(&format!("tributary-transfer-{label}-"))
        .tempdir()
        .expect("create temp root")
        .keep()
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

fn make_authority_pair(path: &Path) -> (Arc<MountedRootAuthority>, MountedWriteAuthority) {
    let mounted = MountedRootAuthority::acquire(path).expect("acquire mounted");
    let read = Arc::new(mounted);
    let write = MountedWriteAuthority::from_mounted(Arc::clone(&read));
    (read, write)
}

fn make_read_authority(path: &Path) -> Arc<MountedRootAuthority> {
    Arc::new(MountedRootAuthority::acquire(path).expect("acquire read authority"))
}

fn write_source_file(path: &Path, contents: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create parent");
    std::fs::write(path, contents).expect("write source");
}

#[test]
fn plan_resolves_file_into_single_copy_stage() {
    let source_root = unique("plan-src");
    let destination_root = unique("plan-dst");
    write_source_file(&source_root.join("album/song.flac"), b"audio");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);
    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::same(PathBuf::from("album/song.flac"))],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: true,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert_eq!(plan.file_count(), 1);
    assert!(
        plan.directory_count() >= 1,
        "album directory must be staged"
    );
    let total = plan.total_bytes();
    assert!(total >= 5);
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn capacity_budget_rejects_oversized_plan() {
    let source_root = unique("budget-src");
    let destination_root = unique("budget-dst");
    write_source_file(&source_root.join("big.flac"), &[0u8; 100]);
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);

    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::same(PathBuf::from("big.flac"))],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: Some(10),
        recurse_directories: true,
    };
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("oversized plan must be rejected");
    assert!(matches!(error, TransferError::CapacityExceeded { .. }));
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn plan_walks_directory_recursively() {
    let source_root = unique("walk-src");
    let destination_root = unique("walk-dst");
    write_source_file(&source_root.join("album/a.flac"), b"a");
    write_source_file(&source_root.join("album/nested/b.flac"), b"b");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);

    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: true,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert_eq!(plan.file_count(), 2);
    assert!(
        plan.directory_count() >= 2,
        "album and nested must be staged"
    );
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn executor_copies_a_single_file() {
    let source_root = unique("exec-src");
    let destination_root = unique("exec-dst");
    write_source_file(&source_root.join("song.flac"), b"copy me");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);

    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::same(PathBuf::from("song.flac"))],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: true,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("run");
    assert!(summary.completed);
    let final_path = destination_root.join("song.flac");
    let bytes = std::fs::read(&final_path).expect("read final");
    assert_eq!(bytes, b"copy me");
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn executor_recursive_directory_copy() {
    let source_root = unique("rec-src");
    let destination_root = unique("rec-dst");
    write_source_file(&source_root.join("album/a.flac"), b"a");
    write_source_file(&source_root.join("album/b.flac"), b"b");
    write_source_file(&source_root.join("album/nested/c.flac"), b"c");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);

    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::new(
            PathBuf::from("album"),
            PathBuf::from("imported"),
        )],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: true,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let summary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("run");
    assert!(summary.completed);
    assert_eq!(
        std::fs::read(destination_root.join("imported/a.flac")).expect("read a"),
        b"a"
    );
    assert_eq!(
        std::fs::read(destination_root.join("imported/b.flac")).expect("read b"),
        b"b"
    );
    assert_eq!(
        std::fs::read(destination_root.join("imported/nested/c.flac")).expect("read c"),
        b"c"
    );
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn conflict_fail_rejects_existing_destination() {
    let source_root = unique("fail-src");
    let destination_root = unique("fail-dst");
    write_source_file(&source_root.join("song.flac"), b"new");
    std::fs::write(destination_root.join("song.flac"), b"old").expect("write existing");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);

    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::same(PathBuf::from("song.flac"))],
        conflict_policy: ConflictPolicy::Fail,
        capacity_budget: None,
        recurse_directories: true,
    };
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("fail policy must reject existing destination");
    assert!(matches!(error, TransferError::ConflictRejected { .. }));
    cleanup(&source_root);
    cleanup(&destination_root);
}

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
    let source_root = unique("progress-src");
    let destination_root = unique("progress-dst");
    // 200 KiB so the 64 KiB chunked copy yields multiple progress reports.
    let payload = vec![0u8; 200 * 1024];
    write_source_file(&source_root.join("payload.bin"), &payload);
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);
    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::same(PathBuf::from("payload.bin"))],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: true,
    };
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
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn overwrite_policy_replaces_existing_destination() {
    let source_root = unique("overwrite-src");
    let destination_root = unique("overwrite-dst");
    write_source_file(&source_root.join("song.flac"), b"new");
    std::fs::write(destination_root.join("song.flac"), b"old").expect("write existing");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);
    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::same(PathBuf::from("song.flac"))],
        conflict_policy: ConflictPolicy::Overwrite,
        capacity_budget: None,
        recurse_directories: true,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let _ = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("run");
    assert_eq!(
        std::fs::read(destination_root.join("song.flac")).expect("read final"),
        b"new"
    );
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn skip_policy_skips_existing_destination() {
    let source_root = unique("skip-src");
    let destination_root = unique("skip-dst");
    write_source_file(&source_root.join("song.flac"), b"new");
    std::fs::write(destination_root.join("song.flac"), b"old").expect("write existing");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);
    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::same(PathBuf::from("song.flac"))],
        conflict_policy: ConflictPolicy::Skip,
        capacity_budget: None,
        recurse_directories: true,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert_eq!(
        plan.file_count(),
        0,
        "skip policy should produce no copy stages"
    );
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn empty_request_plans_to_no_stages() {
    let source_root = unique("empty-src");
    let destination_root = unique("empty-dst");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);
    let request = TransferRequest {
        source,
        destination,
        items: vec![],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: true,
    };
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    assert!(plan.is_empty());
    assert_eq!(plan.stage_count(), 0);
    assert_eq!(plan.total_bytes(), 0);
    cleanup(&source_root);
    cleanup(&destination_root);
}

#[test]
fn absolute_path_in_request_is_rejected() {
    let source_root = unique("abs-src");
    let destination_root = unique("abs-dst");
    let source = make_read_authority(&source_root);
    let (_, destination) = make_authority_pair(&destination_root);
    let request = TransferRequest {
        source,
        destination,
        items: vec![TransferItem::same(PathBuf::from("/etc/passwd"))],
        conflict_policy: ConflictPolicy::Preserve,
        capacity_budget: None,
        recurse_directories: true,
    };
    let error = TransferPlanner::new()
        .plan(&request)
        .expect_err("absolute path must be rejected");
    assert!(matches!(error, TransferError::InvalidItemPath { .. }));
    cleanup(&source_root);
    cleanup(&destination_root);
}
