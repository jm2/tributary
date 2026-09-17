//! Source-lease publication regressions for
//! [`TransferExecutor`](super::TransferExecutor).
//!
//! The source authority is read-only, and its retained descriptor keeps
//! serving bytes to EOF even after the source root has been renamed aside
//! and a replacement directory installed at the old name. A lease lost
//! during the copy must therefore be revalidated at the publication
//! boundary and never reported as a completed transfer. These regressions
//! replace the source root inside `on_bytes_copied` on a single final
//! stage — fresh and overwrite destinations — and assert the publish is
//! failed, the failed stage reports no completion, and rollback reverses
//! the publication (removing a fresh destination, restoring an overwritten
//! original). A valid-root control pins that the added revalidation does
//! not fail an ordinary transfer.

use std::path::PathBuf;

use super::executor_rollback_tests::entry_names;
use super::executor_tests::transfer_request;
use super::test_support::{authority_pair, read_authority, write_source_file};
use super::types::{Stage, TransferError, TransferItem, TransferProgress, TransferSummary};
use super::{TransferExecutor, TransferPlanner};
use crate::local::write_authority::ConflictPolicy;
use crate::source_lifecycle::CancellationObserver;

/// A source root that is not a bare `tempfile::TempDir`: the authority is
/// rooted at `outer/source` so a test can rename that directory aside and
/// install a replacement at the old path — exactly the concurrent-writer
/// interposition the source lease must catch — without disturbing the
/// guard that owns the outer tree.
struct RenameableSource {
    /// Owns the whole tree; kept alive for the test's duration.
    _outer: tempfile::TempDir,
    root: PathBuf,
    moved: PathBuf,
}

impl RenameableSource {
    fn new() -> Self {
        let outer = tempfile::tempdir().expect("temporary source parent");
        let root = outer.path().join("source");
        std::fs::create_dir(&root).expect("create source root");
        let moved = outer.path().join("source-moved-aside");
        Self {
            _outer: outer,
            root,
            moved,
        }
    }

    fn write(&self, relative: &str, contents: &[u8]) {
        write_source_file(&self.root, relative, contents);
    }
}

/// A progress sink that, on the first real byte report, renames the source
/// root aside and installs an empty replacement directory at the old path.
/// The retained source descriptor keeps serving the original bytes, so
/// only an explicit publication-boundary revalidation can refuse the run.
/// It also counts completion callbacks so a test can assert the failed
/// stage never reported success.
struct ReplaceSourceRootDuringCopy {
    source_root: PathBuf,
    moved_root: PathBuf,
    fired: bool,
    stage_completes: u32,
}

impl TransferProgress for ReplaceSourceRootDuringCopy {
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
        bytes_so_far: u64,
        _total_bytes: u64,
    ) {
        // The collision bookkeeping report restores the count to the
        // attempt's pre-copy value (zero for a first attempt); only a real
        // chunk report should race the root.
        if self.fired || bytes_so_far == 0 {
            return;
        }
        self.fired = true;
        std::fs::rename(&self.source_root, &self.moved_root).expect("rename source root aside");
        std::fs::create_dir(&self.source_root).expect("create replacement source root");
    }
}

fn replacement_sink(source: &RenameableSource) -> ReplaceSourceRootDuringCopy {
    ReplaceSourceRootDuringCopy {
        source_root: source.root.clone(),
        moved_root: source.moved.clone(),
        fired: false,
        stage_completes: 0,
    }
}

/// A root replaced during `on_bytes_copied` on a single final stage to a
/// FRESH destination must fail the transfer (the retained source descriptor
/// alone cannot prove the lease). The failed stage reports no completion
/// callback and rollback removes the just-published file, leaving no staged
/// or backup litter behind.
#[test]
fn source_root_replaced_during_copy_fails_a_fresh_publish() {
    let source = RenameableSource::new();
    source.write("song.flac", b"copy me");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let read = read_authority(&source.root);
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        read,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = replacement_sink(&source);
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("a source lease lost during the copy must fail the transfer");
    assert!(
        matches!(error, TransferError::AuthorityLost { .. }),
        "the failure must be the typed authority loss: {error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("source not current at publication"),
        "the error must name the publication-boundary source loss: {error}"
    );
    assert_eq!(
        progress.stage_completes, 0,
        "the failed stage must not report a completion callback"
    );
    assert!(
        !destination_root.path().join("song.flac").exists(),
        "rollback must remove the fresh publication"
    );
    let survivors = entry_names(destination_root.path());
    assert!(
        survivors.is_empty(),
        "no published file or staged/backup litter may survive: {survivors:?}"
    );
}

/// The same interposition against an OVERWRITE destination must fail and
/// roll back by restoring the saved original, consuming its backup — a
/// failed transfer must never destroy the pre-existing destination.
#[test]
fn source_root_replaced_during_copy_restores_an_overwritten_destination() {
    let source = RenameableSource::new();
    source.write("song.flac", b"new song");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    std::fs::write(destination_root.path().join("song.flac"), b"old song")
        .expect("write existing original");
    let read = read_authority(&source.root);
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        read,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Overwrite,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = replacement_sink(&source);
    let error = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect_err("a source lease lost during the copy must fail the overwrite");
    assert!(
        matches!(error, TransferError::AuthorityLost { .. }),
        "the failure must be the typed authority loss: {error:?}"
    );
    assert_eq!(
        progress.stage_completes, 0,
        "the failed stage must not report a completion callback"
    );
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read restored original"),
        b"old song",
        "rollback must restore the overwritten original"
    );
    let survivors = entry_names(destination_root.path());
    assert_eq!(
        survivors,
        vec!["song.flac".to_string()],
        "the restored original only — no backup litter: {survivors:?}"
    );
}

/// The valid-root control: an untouched source root still publishes
/// normally under the added publication-boundary revalidation, proving the
/// check does not fail an ordinary transfer.
#[test]
fn valid_source_root_control_completes() {
    let source = RenameableSource::new();
    source.write("song.flac", b"copy me");
    let destination_root = tempfile::tempdir().expect("temporary destination root");
    let read = read_authority(&source.root);
    let (_, destination) = authority_pair(destination_root.path());
    let request = transfer_request(
        read,
        destination,
        vec![TransferItem::same(PathBuf::from("song.flac"))],
        ConflictPolicy::Preserve,
    );
    let plan = TransferPlanner::new().plan(&request).expect("plan");
    let observer = CancellationObserver::never_cancelled();
    let mut progress = ();
    let summary: TransferSummary = TransferExecutor::new(request, plan)
        .run(&mut progress, &observer)
        .expect("a valid source root must not fail the added revalidation");
    assert!(summary.completed);
    assert_eq!(summary.committed_stages, 1);
    assert_eq!(
        std::fs::read(destination_root.path().join("song.flac")).expect("read final"),
        b"copy me"
    );
}
