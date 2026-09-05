//! The atomic-replace writer: temp file (`O_EXCL`, single write,
//! fsync), `rename(2)`, then directory fsync. A concurrent or failed
//! writer never leaves a partial file visible.

/// Write `content` to `path` via the atomic-replace protocol, cleaning
/// up the temp sibling when any step fails.
pub(super) fn write_equalizer_file_atomic(
    path: &std::path::Path,
    content: &str,
) -> std::io::Result<()> {
    let temp_path = temp_sibling(path);
    let write_result = write_temp_then_rename(&temp_path, path, content);
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result
}

/// Write `content` to `temp_path` (O_EXCL, single write, fsync), then
/// `rename(2)` it onto `path` and fsync the directory entry.
fn write_temp_then_rename(
    temp_path: &std::path::Path,
    path: &std::path::Path,
    content: &str,
) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = open_temp_file(temp_path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temp_path, path)?;
    // The rename alone is not durable until the directory entry is.
    sync_parent_dir(path);
    Ok(())
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
fn sync_parent_dir(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

/// The temp sibling of `path`: the same file name with a `.tmp`
/// suffix, so the rename stays within one directory.
pub(super) fn temp_sibling(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_sibling() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");

        assert!(write_equalizer_file_atomic(&path, "first=\"1\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first=\"1\"\n");
        assert!(!temp_sibling(&path).exists());

        assert!(write_equalizer_file_atomic(&path, "second=\"2\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second=\"2\"\n");
        assert!(!temp_sibling(&path).exists());
    }

    #[test]
    fn temp_sibling_sits_beside_the_destination() {
        let path = std::path::Path::new("state/tributary/equalizer.cfg");
        assert_eq!(
            temp_sibling(path),
            std::path::Path::new("state/tributary/equalizer.cfg.tmp")
        );
    }
}
