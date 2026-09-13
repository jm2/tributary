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

use std::io::Write as _;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use super::authority;
use crate::local::write_authority::{
    CommitDisposition, CommitError, CommitOutcome, ConflictPolicy, ConflictResolution,
};

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

/// A concurrent writer hammering the destination name must land the
/// replace publish in a state the caller can always undo — never in the
/// historical blind spot. When attempts EXHAUST with a completed binding
/// retained, the commit must surface a state-carrying verified-publication
/// failure whose outcome records the retained backup of the pre-transfer
/// occupant (rollback restores or disposes it) — never a plain I/O error,
/// which would strand the backup as an unrecorded hidden orphan. When a
/// later attempt WINS, every superseded rebind backup must be disposed
/// with identity-verified deletion: exactly one backup — the first
/// binding's, holding the pre-transfer occupant — may survive.
/// Spawns the concurrent writer that keeps re-creating `target` the instant
/// it is vacated, so the replace loop must re-bind within its bound. Each
/// writer file is uniquely named so the caller can tell the pre-transfer
/// occupant (w0) from every interposer. Returns the running flag (set it to
/// `false` to stop the writer) and the thread handle.
fn spawn_recreating_writer(target: PathBuf) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let running = Arc::new(AtomicBool::new(true));
    let writer_running = Arc::clone(&running);
    let handle = std::thread::spawn(move || {
        let mut round = 0u32;
        while writer_running.load(Ordering::Relaxed) {
            round += 1;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
            {
                let _ = writeln!(file, "w{round}");
            }
            std::thread::yield_now();
        }
    });
    (running, handle)
}

/// Every hidden backup sibling currently retained under `root`, sorted.
fn retained_backup_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .expect("read root")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(".tributary-backup-"))
        .collect();
    names.sort();
    names
}

/// The winning publish reported exactly the FIRST binding — the
/// pre-transfer occupant — and disposed every superseded one: exactly one
/// backup may survive, it must be the reported one, and it must hold the
/// pre-transfer occupant's bytes.
fn assert_winning_publish_keeps_only_the_reported_backup(root: &Path, committed: CommitOutcome) {
    assert_eq!(
        committed.disposition,
        CommitDisposition::Published,
        "a winning reboot publish lands the staged bytes"
    );
    let backup = committed
        .replaced_original
        .expect("a completed binding must be reported");
    let names = retained_backup_names(root);
    assert_eq!(
        names.len(),
        1,
        "a successful multi-bind publish must dispose every superseded backup — \
         exactly the reported one may survive: {names:?}"
    );
    assert!(
        root.join(&backup).file_name().is_some_and(|name| names
            .iter()
            .any(|survivor| { std::ffi::OsStr::new(survivor.as_str()) == name })),
        "the reported backup must be the surviving one: {backup:?} vs {names:?}"
    );
    assert_eq!(
        std::fs::read(root.join(&backup)).expect("read surviving backup"),
        b"w0",
        "the surviving backup must hold the pre-transfer occupant, not an interposer"
    );
}

/// Exhaustion with a retained binding must carry state: a plain I/O error
/// here is the historical defect this suite exists to prevent — it would
/// strand the backup unrecorded. The outcome must name the retained backup
/// with its bind-time identity, record nothing as published, and the
/// backup must survive on disk holding the pre-transfer occupant.
fn assert_exhausted_rebind_records_the_retained_backup(root: &Path, outcome: CommitOutcome) {
    assert_eq!(outcome.resolution, ConflictResolution::Overwrite);
    assert_eq!(
        outcome.disposition,
        CommitDisposition::DisplacedOnly,
        "exhaustion published nothing, so it must be recorded as displaced-only, \
         never as an identity-less publication"
    );
    let backup = outcome
        .replaced_original
        .expect("the exhausted rebind must record the retained backup");
    assert!(
        outcome.replaced_original_leaf.is_some(),
        "the retained backup must carry its bind-time identity"
    );
    assert!(
        outcome.published_leaf.is_none(),
        "nothing of the transfer's may be recorded as published after exhaustion"
    );
    let names = retained_backup_names(root);
    assert!(
        !names.is_empty(),
        "the recorded backup must survive on disk: {names:?}"
    );
    assert_eq!(
        std::fs::read(root.join(&backup)).expect("read retained backup"),
        b"w0",
        "the recorded backup must hold the pre-transfer occupant"
    );
}

#[test]
fn racing_rebinds_never_strand_a_backup_or_publish_a_plain_io_failure() {
    let root = tempfile::tempdir().expect("temporary root");
    let destination = root.path().join("song.flac");
    std::fs::write(&destination, b"w0").expect("write the pre-transfer occupant");

    let (running, writer) = spawn_recreating_writer(destination.clone());

    let authority = authority(&root);
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Overwrite)
        .expect("prepare overwrite");
    staged.write_all(b"new bytes").expect("write new");
    let outcome = staged.commit();

    running.store(false, Ordering::Relaxed);
    writer.join().expect("join the concurrent writer");

    match outcome {
        Ok(committed) => {
            assert_winning_publish_keeps_only_the_reported_backup(root.path(), committed);
        }
        Err(CommitError::PublishVerification { outcome, .. }) => {
            assert_exhausted_rebind_records_the_retained_backup(root.path(), outcome);
        }
        Err(error) => panic!(
            "an exhausted rebind with a retained binding must surface the state-carrying \
             error, got: {error}"
        ),
    }
}
