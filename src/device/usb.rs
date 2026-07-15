//! USB mass storage device detection.
//!
//! Detects mounted removable drives that may contain music files.
//! Platform-specific implementations scan the appropriate mount points.

use std::path::{Path, PathBuf};

use tracing::{debug, info};

use super::DeviceInfo;

/// Detect mounted USB mass storage devices that may contain music.
///
/// Returns a list of `DeviceInfo` for each detected removable volume. The
/// caller supplies the localized label used when a mount path has no usable
/// final component.
///
/// Discovery is shallow, but mount enumeration and metadata probes can still
/// block indefinitely in the kernel for stale or unresponsive media. Call
/// this from a dedicated worker thread, never from the GTK main thread.
pub fn detect_usb_devices(fallback_label: &str) -> Vec<DeviceInfo> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "linux")]
    collect_linux_candidates(&mut candidates);

    #[cfg(target_os = "macos")]
    collect_macos_candidates(&mut candidates);

    #[cfg(target_os = "windows")]
    collect_windows_candidates(&mut candidates);

    let devices = probe_device_candidates(candidates, fallback_label, |path| {
        std::fs::metadata(path).map(|metadata| metadata.is_dir())
    });

    if devices.is_empty() {
        debug!("No USB music devices detected");
    } else {
        info!(count = devices.len(), "USB music devices detected");
    }

    devices
}

/// Extract a human-readable name from a mount point path.
fn volume_name(path: &Path, fallback_label: &str) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(fallback_label)
        .to_string()
}

/// Sort and de-duplicate exact mount paths before probing each candidate once.
///
/// A failed probe invalidates only that candidate; another healthy device in
/// the same discovery snapshot must still be returned.
fn probe_device_candidates(
    mut candidates: Vec<PathBuf>,
    fallback_label: &str,
    mut probe: impl FnMut(&Path) -> std::io::Result<bool>,
) -> Vec<DeviceInfo> {
    candidates.sort_unstable();
    candidates.dedup();

    candidates
        .into_iter()
        .filter_map(|mount_point| match probe(&mount_point) {
            Ok(true) => {
                let name = volume_name(&mount_point, fallback_label);
                debug!(name = %name, path = %mount_point.display(), "Found mounted volume");
                Some(DeviceInfo { name, mount_point })
            }
            Ok(false) => None,
            Err(error) => {
                debug!(
                    path = %mount_point.display(),
                    %error,
                    "Skipping unreadable removable-volume candidate"
                );
                None
            }
        })
        .collect()
}

// ── Linux implementation ────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn collect_linux_candidates(candidates: &mut Vec<PathBuf>) {
    // Standard mount points for removable media. Resolve the login name from
    // USER (falling back to LOGNAME). If neither is set, bail out rather than
    // scanning the bare `/media` and `/run/media` roots — doing so would
    // enumerate *other* users' per-user mount directories and surface them as
    // bogus "devices" pointing at someone else's media root.
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    if user.is_empty() {
        debug!("USER/LOGNAME not set — skipping per-user removable media scan");
        return;
    }
    let mount_dirs = [format!("/media/{user}"), format!("/run/media/{user}")];

    for mount_dir in &mount_dirs {
        let Ok(entries) = std::fs::read_dir(mount_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            candidates.push(entry.path());
        }
    }
}

// ── macOS implementation ────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn collect_macos_candidates(candidates: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir("/Volumes") else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Skip the system volume (typically "Macintosh HD").
        if path == Path::new("/Volumes/Macintosh HD")
            || path == Path::new("/Volumes/Macintosh HD - Data")
        {
            continue;
        }

        candidates.push(path);
    }
}

// ── Windows implementation ──────────────────────────────────────────

#[cfg(target_os = "windows")]
fn collect_windows_candidates(candidates: &mut Vec<PathBuf>) {
    // Check drive letters A-Z for removable drives.
    // DRIVE_REMOVABLE = 2
    const DRIVE_REMOVABLE: u32 = 2;

    extern "system" {
        fn GetDriveTypeW(lp_root_path_name: *const u16) -> u32;
    }

    for letter in b'A'..=b'Z' {
        // Build the root path: "X:\\\0" as wide string.
        let root: Vec<u16> = format!("{}:\\", letter as char)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };

        if drive_type == DRIVE_REMOVABLE {
            candidates.push(PathBuf::from(format!("{}:\\", letter as char)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_name_with_name() {
        let path = Path::new("/media/user/WALKMAN");
        assert_eq!(volume_name(path, "USB Device"), "WALKMAN");
    }

    #[test]
    fn test_volume_name_root() {
        let path = Path::new("/");
        assert_eq!(volume_name(path, "Removable Media"), "Removable Media");
    }

    #[test]
    fn stale_error_and_non_directory_candidates_do_not_hide_healthy_devices() {
        let first = PathBuf::from("first-device");
        let non_directory = PathBuf::from("ordinary-file");
        let stale = PathBuf::from("stale-device");
        let second = PathBuf::from("second-device");

        let devices = probe_device_candidates(
            vec![
                stale.clone(),
                second.clone(),
                non_directory.clone(),
                first.clone(),
            ],
            "USB Device",
            |path| {
                if path == stale.as_path() {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "simulated stale mount",
                    ))
                } else {
                    Ok(path != non_directory.as_path())
                }
            },
        );

        let found: Vec<_> = devices
            .into_iter()
            .map(|device| device.mount_point)
            .collect();
        assert_eq!(found, vec![first, second]);
    }

    #[test]
    fn shuffled_duplicate_candidates_are_sorted_deduplicated_and_probed_once() {
        let first = PathBuf::from("alpha-device");
        let second = PathBuf::from("zeta-device");
        let mut probed = Vec::new();

        let devices = probe_device_candidates(
            vec![
                second.clone(),
                first.clone(),
                second.clone(),
                first.clone(),
                second.clone(),
            ],
            "USB Device",
            |path| {
                probed.push(path.to_path_buf());
                Ok(true)
            },
        );

        let found: Vec<_> = devices
            .into_iter()
            .map(|device| device.mount_point)
            .collect();
        assert_eq!(probed, vec![first.clone(), second.clone()]);
        assert_eq!(found, vec![first, second]);
    }
}
