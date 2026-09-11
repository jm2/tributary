//! The atomic-replace writer: a unique, exclusively-created temp file
//! (single write, fsync), `rename(2)`, then directory fsync. Each
//! writer owns exactly the one temp sibling it created — a concurrent
//! writer, or a sibling stranded by a crashed writer, is never touched,
//! so a stale file can never block a later save or be destroyed by
//! another writer's cleanup.

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process sequence that makes each temp sibling unique
/// within this process; the process id separates concurrent writers.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How many exclusive-create collisions one write tolerates before
/// giving up. A collision means another process generated the same
/// `<pid>.<sequence>` name — realistically only a crashed writer whose
/// pid the OS later reused at the same sequence position — and each
/// retry simply advances the per-process sequence to a fresh name.
const TEMP_COLLISION_RETRIES: usize = 8;

/// How many times one replace retries when the destination name is
/// momentarily held by a concurrent replacement. Windows'
/// `MOVEFILE_REPLACE_EXISTING` is not an atomic-replace queue: while
/// another thread's replacement of the same destination is in flight,
/// the losing rename is refused with a sharing violation instead of
/// being serialized behind the winner. POSIX `rename(2)` serializes
/// concurrent replacements in the kernel and never reports this shape,
/// so only the Windows backend ever hits the retries — but the policy
/// itself is platform-independent and unit-tested on every platform
/// through an injected replace backend.
const REPLACE_RACE_RETRIES: usize = 8;

/// Base pause between replace retries; each retry waits a multiple of
/// this (`base × attempt`) so a name held under heavier contention
/// gets proportionally more room before the save honestly gives up.
const REPLACE_RACE_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(2);

/// Write `content` to `path` via the atomic-replace protocol. The temp
/// sibling is created exclusively under a name unique to this write, so
/// concurrent writers never collide and a sibling left behind by a
/// crashed writer never blocks a later save. On any failure before the
/// rename, this writer removes *its own* temp and nothing else.
pub(super) fn write_equalizer_file_atomic(
    path: &std::path::Path,
    content: &str,
) -> std::io::Result<()> {
    // Exclusive create: if this open fails, this writer owns no file
    // and must not remove whatever beat it to the name. A collision
    // with a sibling stranded under the same `<pid>.<sequence>` name
    // (crashed writer, later pid reuse) costs one retry with the next
    // sequence value — the save is only discarded when every retry
    // collides, and a file this writer did not create is never removed.
    let (mut file, temp_path) = create_temp_exclusively(path)?;
    let staged = write_and_sync(&mut file, content);
    drop(file);
    match staged.and_then(|()| rename_over(&temp_path, path)) {
        Ok(()) => {
            // POSIX flushes the directory entry and reports a failure to
            // the caller; platforms whose std layer cannot express the
            // equivalent either carry the durability in the rename
            // itself (Windows MOVEFILE_WRITE_THROUGH) or skip the flush
            // entirely, so the rename's success is the save's outcome.
            #[cfg(unix)]
            {
                sync_parent_dir(path)
            }
            #[cfg(not(unix))]
            {
                sync_parent_dir(path);
                Ok(())
            }
        }
        // Ownership guard: the temp exists only because this call
        // created it exclusively, and the rename has not consumed it
        // yet, so this is the only writer entitled to remove it.
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

/// Create the temp sibling for exclusive single-writer access,
/// retrying with the next per-process sequence value when the name
/// collides with an existing file (crashed writer plus pid reuse).
/// Returns the exclusively-created handle together with the winning
/// path, so the caller's cleanup can never touch a file it did not
/// create.
fn create_temp_exclusively(
    path: &std::path::Path,
) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    let mut last_error = None;
    for _ in 0..TEMP_COLLISION_RETRIES {
        let temp_path = unique_temp_sibling(path);
        match open_temp_file(&temp_path) {
            Ok(file) => return Ok((file, temp_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "every temp-sibling name collided",
        )
    }))
}

/// Replace the destination with the staged temp. On Windows the rename
/// must request `MOVEFILE_WRITE_THROUGH`: `std::fs::rename` does not,
/// so a reported success could still be lost to a sudden power loss
/// while the destination-directory update is only in flight. The
/// `MOVEFILE_REPLACE_EXISTING` flag preserves `rename(2)`'s
/// replace-the-destination semantics, and the shared retry policy
/// absorbs the transient refusals a concurrent replacement of the same
/// destination produces on Windows.
#[cfg(windows)]
fn rename_over(temp_path: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    let mut from: Vec<u16> = temp_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    retry_transient_replace_race(
        move || {
            // Safety: both pointers reference NUL-terminated wide buffers owned
            // by this call and `MoveFileExW` keeps them valid for its duration.
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                    from.as_mut_ptr(),
                    to.as_mut_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            };
            if ok == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        },
        is_replace_race_refusal,
    )
}

/// POSIX and everything else: `rename(2)` atomically replaces the
/// destination and the kernel serializes concurrent replacements, so
/// the shared retry policy never triggers here; the directory-entry
/// flush remains the caller's separate `sync_parent_dir` step.
#[cfg(not(windows))]
fn rename_over(temp_path: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    retry_transient_replace_race(|| std::fs::rename(temp_path, path), is_replace_race_refusal)
}

/// Windows error values (the `windows-sys` `Win32::Foundation` names)
/// that mean "the destination name is momentarily held by a concurrent
/// replacement" rather than "this save cannot succeed":
/// `ERROR_ACCESS_DENIED` observes the target's brief delete-pending
/// state, `ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION` find the
/// name still held by the winner's in-flight replacement. Kept as local
/// constants so the classifier reads without a platform-gated import.
#[cfg(windows)]
const ERROR_ACCESS_DENIED: i32 = 5;
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

/// Classify one replace error for the shared retry policy. Windows:
/// exactly the in-flight-replacement refusals above. POSIX: nothing —
/// `rename(2)` never refuses a replacement because another one is in
/// flight, so every error is reported on the first attempt.
#[cfg(windows)]
fn is_replace_race_refusal(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

#[cfg(not(windows))]
fn is_replace_race_refusal(_error: &std::io::Error) -> bool {
    false
}

/// Run one destination replace under the bounded transient-race policy:
/// a refusal that means "another replacement of this same destination
/// is in flight" is retried with a `REPLACE_RACE_RETRY_BASE × attempt`
/// pause, up to `REPLACE_RACE_RETRIES` retries, before the last error
/// is honestly reported; every other error is returned on the first
/// attempt. A save is only ever reported successful when the replace
/// itself reported success.
fn retry_transient_replace_race<F, C>(mut replace: F, is_transient: C) -> std::io::Result<()>
where
    F: FnMut() -> std::io::Result<()>,
    C: Fn(&std::io::Error) -> bool,
{
    let mut attempts = 0;
    loop {
        match replace() {
            Ok(()) => return Ok(()),
            Err(error) => {
                attempts += 1;
                if attempts > REPLACE_RACE_RETRIES || !is_transient(&error) {
                    return Err(error);
                }
                std::thread::sleep(REPLACE_RACE_RETRY_BASE * attempts as u32);
            }
        }
    }
}

/// One writer's work between exclusive creation and rename: a single
/// write of the full content, then a file fsync so the renamed result
/// is crash-durable.
fn write_and_sync(file: &mut std::fs::File, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

/// Create the temp sibling for exclusive single-writer access.
fn open_temp_file(temp_path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(temp_path)
}

/// Flush the directory entry so the completed rename survives a crash.
///
/// Post-rename failure policy: the destination already holds the new
/// content and the rename consumed the temp, so there is nothing to
/// clean up — the error is reported to the caller as-is. A caller that
/// treats the save as failed loses nothing that was on disk before (the
/// replaced file stays in place), and the next save rewrites the file.
#[cfg(unix)]
fn sync_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let dir = std::fs::File::open(parent)?;
    dir.sync_all()
}

/// Non-POSIX platforms have no std directory handle to flush; the file
/// fsync and the rename carry the durability the platform's std layer
/// can express, so the entry flush is a no-op that cannot fail — the
/// caller's cfg arm treats the rename's success as final.
#[cfg(not(unix))]
fn sync_parent_dir(_path: &std::path::Path) {}

/// The temp sibling for one write: the destination name extended with
/// the writing process's id and a per-process sequence, then `.tmp`.
/// Unique per write, always in the destination's directory so the
/// rename stays within one filesystem. A name collision — only
/// possible when a crashed writer's pid is later reused at the same
/// sequence position — is retried with the next sequence value by
/// [`create_temp_exclusively`].
fn unique_temp_sibling(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    path.with_file_name(name)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// Serializes every test whose outcome depends on the shared
    /// per-process `TEMP_SEQUENCE`: a parallel test consuming sequence
    /// values mid-test would break the collision test's name
    /// prediction. Locking this in each sequence-consuming test keeps
    /// the predictions deterministic.
    static SEQUENCE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The temp files a directory holds, sorted for stable assertions.
    fn dir_entries(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("readable directory")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// Hold while consuming or predicting sequence values (see
    /// [`SEQUENCE_TEST_LOCK`]).
    fn lock_sequence() -> MutexGuard<'static, ()> {
        SEQUENCE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_sibling() {
        let _sequence_guard = lock_sequence();
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");

        assert!(write_equalizer_file_atomic(&path, "first=\"1\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first=\"1\"\n");
        assert_eq!(dir_entries(base.path()), vec!["equalizer.cfg".to_string()]);

        assert!(write_equalizer_file_atomic(&path, "second=\"2\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second=\"2\"\n");
        assert_eq!(dir_entries(base.path()), vec!["equalizer.cfg".to_string()]);
    }

    #[test]
    fn unique_temp_siblings_differ_and_stay_beside_the_destination() {
        let _sequence_guard = lock_sequence();
        let path = std::path::Path::new("state/tributary/equalizer.cfg");
        let first = unique_temp_sibling(path);
        let second = unique_temp_sibling(path);

        assert_ne!(first, second, "every write gets its own sibling");
        assert_eq!(
            first.parent(),
            path.parent(),
            "same directory as the destination"
        );
        // Separator-agnostic: rebuild the destination-with-suffix prefix
        // from the same path so the comparison holds on POSIX and Windows
        // (which renders the separators as backslashes).
        let destination_prefix = path.with_file_name("equalizer.cfg.");
        assert!(
            first
                .to_string_lossy()
                .starts_with(destination_prefix.to_string_lossy().as_ref()),
            "sibling keeps the destination name as its prefix"
        );
        assert!(first.to_string_lossy().ends_with(".tmp"));
    }

    /// Regression (operator P1 + review thread on stale temp recovery):
    /// a sibling stranded by a crashed writer must neither block a
    /// later save nor be removed by it — only the writer that created
    /// a temp may ever delete it.
    #[test]
    fn a_stale_sibling_from_a_crashed_writer_never_blocks_or_dies() {
        let _sequence_guard = lock_sequence();
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");
        let stale = base.path().join("equalizer.cfg.999.0.tmp");
        std::fs::write(&stale, "a crashed writer's temp").expect("seed stale sibling");

        assert!(write_equalizer_file_atomic(&path, "fresh=\"1\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh=\"1\"\n");
        // The crashed writer's sibling is still exactly as it was, and
        // this save's own sibling is gone.
        assert_eq!(
            std::fs::read_to_string(&stale).unwrap(),
            "a crashed writer's temp"
        );
        assert_eq!(
            dir_entries(base.path()),
            vec![
                "equalizer.cfg".to_string(),
                "equalizer.cfg.999.0.tmp".to_string(),
            ]
        );
    }

    /// Regression (review thread on PID-reuse collisions): when the
    /// writer's first `<pid>.<sequence>` name is already taken — the
    /// crashed-writer-plus-pid-reuse shape — the save retries with the
    /// next sequence value and succeeds instead of discarding the
    /// settings, and the pre-existing sibling (not this writer's) is
    /// left exactly as it was.
    #[test]
    fn a_colliding_first_sibling_name_costs_one_retry_not_the_save() {
        // The sequence and the pid must not move under the test while
        // it predicts the writer's first name.
        let _sequence_guard = SEQUENCE_TEST_LOCK.lock().unwrap();
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");

        let next_sequence = TEMP_SEQUENCE.load(Ordering::Relaxed);
        let colliding = base.path().join(format!(
            "equalizer.cfg.{}.{}.tmp",
            std::process::id(),
            next_sequence
        ));
        std::fs::write(&colliding, "a reused pid's stranded temp").expect("seed colliding sibling");

        assert!(
            write_equalizer_file_atomic(&path, "fresh=\"1\"\n").is_ok(),
            "a single collision must cost a retry, never the save"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh=\"1\"\n");
        // The colliding sibling belongs to someone else: untouched.
        assert_eq!(
            std::fs::read_to_string(&colliding).unwrap(),
            "a reused pid's stranded temp"
        );
        // Exactly the destination and the foreign sibling remain.
        assert_eq!(
            dir_entries(base.path()),
            vec![
                "equalizer.cfg".to_string(),
                format!("equalizer.cfg.{}.{}.tmp", std::process::id(), next_sequence),
            ]
        );
    }

    /// Regression (operator P1, concurrent-writer harness): concurrent
    /// writers all succeed — no fixed-name `create_new` collision — and
    /// the destination always holds one complete payload, never a
    /// partial or interleaved write.
    #[test]
    fn concurrent_writers_all_succeed_and_never_interleave() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");
        let iterations = 25;

        let handles: Vec<_> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|name| {
                let dest = path.clone();
                std::thread::spawn(move || {
                    let payload = format!("writer-{name}\n").repeat(iterations);
                    for _ in 0..iterations {
                        write_equalizer_file_atomic(&dest, &payload)
                            .expect("every concurrent write succeeds");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("writer thread");
        }

        // The file is exactly one writer's payload: a whole run of one
        // writer's identical lines, never a mix or a torn write.
        let final_content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = final_content.lines().collect();
        assert_eq!(lines.len(), iterations);
        assert!(
            lines.iter().all(|line| *line == lines[0]),
            "destination must hold one complete payload, not a mix"
        );
        // Every writer cleaned up exactly its own siblings.
        assert_eq!(dir_entries(base.path()), vec!["equalizer.cfg".to_string()]);
    }

    /// P2: a parent directory that cannot be opened is an error, not a
    /// silently-swallowed durability failure. (Unix-only: elsewhere the
    /// flush is a no-op that cannot fail.)
    #[cfg(unix)]
    #[test]
    fn sync_parent_dir_propagates_an_unopenable_parent() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("no-such-dir").join("equalizer.cfg");
        assert!(sync_parent_dir(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sync_parent_dir_succeeds_on_a_real_directory() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");
        assert!(sync_parent_dir(&path).is_ok());
    }

    /// The raw OS error a Windows replace refusal carries (the
    /// `windows-sys` `ERROR_SHARING_VIOLATION` value); `from_raw_os_error`
    /// stores it verbatim on every platform, so the shared policy below
    /// is exercisable without a Windows host.
    const TEST_SHARING_VIOLATION: i32 = 32;

    /// Regression (Windows CI, concurrent-writers test): a replace
    /// refused because a concurrent replacement of the same destination
    /// name is in flight costs a bounded retry — never the save. The
    /// policy is driven through an injected backend so every platform's
    /// test run exercises it; only the per-platform classification
    /// differs (`is_replace_race_refusal`).
    #[test]
    fn a_transient_replace_race_costs_a_retry_not_the_save() {
        let mut calls = 0;
        let result = retry_transient_replace_race(
            || {
                calls += 1;
                if calls <= 2 {
                    Err(std::io::Error::from_raw_os_error(TEST_SHARING_VIOLATION))
                } else {
                    Ok(())
                }
            },
            |error| error.raw_os_error() == Some(TEST_SHARING_VIOLATION),
        );
        assert!(
            result.is_ok(),
            "a transient refusal must cost a retry, never the save"
        );
        assert_eq!(calls, 3);
    }

    /// A non-race failure is returned on the first attempt: the retry
    /// policy exists for in-flight-replacement refusals only, never to
    /// paper over a replace that cannot succeed.
    #[test]
    fn a_permanent_replace_failure_is_returned_on_the_first_attempt() {
        let mut calls = 0;
        let result = retry_transient_replace_race(
            || {
                calls += 1;
                // An error outside the race family (ENOENT-shaped here).
                Err(std::io::Error::from_raw_os_error(2))
            },
            |error| error.raw_os_error() == Some(TEST_SHARING_VIOLATION),
        );
        assert!(result.is_err(), "a non-race failure must not be retried");
        assert_eq!(calls, 1, "a non-race error is returned immediately");
    }

    /// Exhausted retries report the save as failed with the last
    /// refusal — the durability-honest outcome the caller's trailing
    /// edge persistence re-arms on.
    #[test]
    fn exhausted_race_retries_return_the_last_refusal() {
        let mut calls = 0;
        let result = retry_transient_replace_race(
            || {
                calls += 1;
                Err(std::io::Error::from_raw_os_error(TEST_SHARING_VIOLATION))
            },
            |error| error.raw_os_error() == Some(TEST_SHARING_VIOLATION),
        );
        assert!(
            result.is_err(),
            "exhausted retries must honestly report the save as failed"
        );
        assert_eq!(calls, REPLACE_RACE_RETRIES + 1);
    }
}
