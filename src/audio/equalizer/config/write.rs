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

/// Write `content` to `path` via the atomic-replace protocol. The temp
/// sibling is created exclusively under a name unique to this write, so
/// concurrent writers never collide and a sibling left behind by a
/// crashed writer never blocks a later save. On any failure before the
/// rename, this writer removes *its own* temp and nothing else.
pub(super) fn write_equalizer_file_atomic(
    path: &std::path::Path,
    content: &str,
) -> std::io::Result<()> {
    let temp_path = unique_temp_sibling(path);
    // Exclusive create: if this open fails, this writer owns no file
    // and must not remove whatever beat it to the name.
    let mut file = open_temp_file(&temp_path)?;
    let staged = write_and_sync(&mut file, content);
    drop(file);
    match staged.and_then(|()| std::fs::rename(&temp_path, path)) {
        Ok(()) => {
            // POSIX flushes the directory entry and reports a failure to
            // the caller; platforms without a std directory handle skip
            // the flush entirely (see sync_parent_dir), so the rename's
            // success is the save's outcome.
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
/// rename stays within one filesystem.
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
    use super::*;

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

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_sibling() {
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
}
