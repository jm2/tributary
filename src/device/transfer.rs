//! File transfer operations for USB mass storage devices.
//!
//! Provides async file copy from the local music library to mounted
//! USB devices, preserving the directory structure relative to the
//! source library path.

use std::path::{Path, PathBuf};

use tokio::fs;
use tracing::{debug, error, info};

/// Result of a file transfer operation.
#[derive(Debug, Clone)]
pub struct TransferResult {
    /// Number of files successfully copied.
    pub files_copied: u32,
    /// Number of files that failed to copy.
    pub files_failed: u32,
    /// Total bytes written.
    pub bytes_written: u64,
    /// Errors encountered during transfer.
    pub errors: Vec<String>,
}

/// Copy a list of audio files to a USB mass storage device.
///
/// Files are copied to `<mount_point>/Music/<relative_path>`, preserving
/// the directory structure from the source library.  If a file already
/// exists at the destination and has the same size, it is skipped.
///
/// # Arguments
///
/// * `file_paths` — Absolute paths of audio files to copy.
/// * `mount_point` — Root path of the mounted USB device.
/// * `library_root` — Root of the source music library (used to compute
///   relative paths).
/// * `progress_tx` — Optional channel to report progress updates as
///   `(files_done, total_files)`.
pub async fn copy_files_to_device(
    file_paths: &[PathBuf],
    mount_point: &Path,
    library_root: &Path,
    progress_tx: Option<async_channel::Sender<(u32, u32)>>,
) -> TransferResult {
    let total = file_paths.len() as u32;
    let dest_root = mount_point.join("Music");
    let mut result = TransferResult {
        files_copied: 0,
        files_failed: 0,
        bytes_written: 0,
        errors: Vec::new(),
    };

    info!(
        total,
        dest = %dest_root.display(),
        "Starting file transfer to USB device"
    );

    for (i, src_path) in file_paths.iter().enumerate() {
        // Compute relative path from library root.
        let rel_path = src_path
            .strip_prefix(library_root)
            .unwrap_or(src_path.as_path());

        let dest_path = dest_root.join(rel_path);

        // Create parent directory if needed.
        if let Some(parent) = dest_path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                let msg = format!("Failed to create directory {}: {e}", parent.display());
                error!("{}", msg);
                result.errors.push(msg);
                result.files_failed += 1;
                continue;
            }
        }

        // Skip if destination exists with the same size (quick dedup).
        if let Ok(dest_meta) = fs::metadata(&dest_path).await {
            if let Ok(src_meta) = fs::metadata(src_path).await {
                if dest_meta.len() == src_meta.len() {
                    debug!(
                        path = %rel_path.display(),
                        "Skipping (already exists with same size)"
                    );
                    result.files_copied += 1;
                    if let Some(ref tx) = progress_tx {
                        let _ = tx.try_send((i as u32 + 1, total));
                    }
                    continue;
                }
            }
        }

        // Copy the file.
        match fs::copy(src_path, &dest_path).await {
            Ok(bytes) => {
                debug!(
                    path = %rel_path.display(),
                    bytes,
                    "File copied to device"
                );
                result.files_copied += 1;
                result.bytes_written += bytes;
            }
            Err(e) => {
                let msg = format!(
                    "Failed to copy {} → {}: {e}",
                    src_path.display(),
                    dest_path.display()
                );
                error!("{}", msg);
                result.errors.push(msg);
                result.files_failed += 1;
            }
        }

        if let Some(ref tx) = progress_tx {
            let _ = tx.try_send((i as u32 + 1, total));
        }
    }

    info!(
        copied = result.files_copied,
        failed = result.files_failed,
        bytes = result.bytes_written,
        "File transfer to USB device complete"
    );

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_copy_to_nonexistent_mount_fails() {
        let files = vec![PathBuf::from("/nonexistent/song.mp3")];
        let mount = PathBuf::from("/nonexistent/mount");
        let lib_root = PathBuf::from("/nonexistent");

        let result = copy_files_to_device(&files, &mount, &lib_root, None).await;
        // Should fail (source doesn't exist).
        assert_eq!(result.files_failed, 1);
        assert_eq!(result.files_copied, 0);
    }
}
