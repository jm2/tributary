//! USB mass storage device detection.
//!
//! Detects mounted removable drives that may contain music files.
//! Platform-specific implementations scan the appropriate mount points.

use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

use tracing::{debug, info};

use super::DeviceInfo;

/// Detect mounted USB mass storage devices that may contain music.
///
/// Returns a list of `DeviceInfo` for each detected removable volume.
/// This is a non-blocking scan of mount points — it does not perform
/// deep filesystem traversal.
pub fn detect_usb_devices() -> Vec<DeviceInfo> {
    let mut devices = Vec::new();

    #[cfg(target_os = "linux")]
    detect_linux(&mut devices);

    #[cfg(target_os = "macos")]
    detect_macos(&mut devices);

    #[cfg(target_os = "windows")]
    detect_windows(&mut devices);

    if devices.is_empty() {
        debug!("No USB music devices detected");
    } else {
        info!(count = devices.len(), "USB music devices detected");
    }

    devices
}

/// Extract a human-readable name from a mount point path.
fn volume_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("USB Device")
        .to_string()
}

// ── Linux implementation ────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn detect_linux(devices: &mut Vec<DeviceInfo>) {
    // Standard mount points for removable media.
    let user = std::env::var("USER").unwrap_or_default();
    let mount_dirs = [format!("/media/{user}"), format!("/run/media/{user}")];

    for mount_dir in &mount_dirs {
        let mount_path = Path::new(mount_dir);
        if !mount_path.is_dir() {
            continue;
        }

        let Ok(entries) = std::fs::read_dir(mount_path) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = volume_name(&path);
                debug!(name = %name, path = %path.display(), "Found mounted volume");
                devices.push(DeviceInfo {
                    name,
                    mount_point: path,
                });
            }
        }
    }
}

// ── macOS implementation ────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn detect_macos(devices: &mut Vec<DeviceInfo>) {
    let volumes = Path::new("/Volumes");
    if !volumes.is_dir() {
        return;
    }

    let Ok(entries) = std::fs::read_dir(volumes) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = volume_name(&path);

        // Skip the system volume (typically "Macintosh HD").
        if path == Path::new("/Volumes/Macintosh HD")
            || path == Path::new("/Volumes/Macintosh HD - Data")
        {
            continue;
        }

        debug!(name = %name, path = %path.display(), "Found mounted volume");
        devices.push(DeviceInfo {
            name,
            mount_point: path,
        });
    }
}

// ── Windows implementation ──────────────────────────────────────────

#[cfg(target_os = "windows")]
fn detect_windows(devices: &mut Vec<DeviceInfo>) {
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
            let path = PathBuf::from(format!("{}:\\", letter as char));
            if path.is_dir() {
                let name = volume_name(&path);
                debug!(
                    name = %name,
                    path = %path.display(),
                    "Found removable drive"
                );
                devices.push(DeviceInfo {
                    name,
                    mount_point: path,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_name_with_name() {
        let path = Path::new("/media/user/WALKMAN");
        assert_eq!(volume_name(path), "WALKMAN");
    }

    #[test]
    fn test_volume_name_root() {
        let path = Path::new("/");
        assert_eq!(volume_name(path), "USB Device");
    }

    #[test]
    fn test_detect_returns_vec() {
        // Should not panic on any platform.
        let devices = detect_usb_devices();
        // We can't assert specific devices exist, but the function
        // should return without error.
        assert!(devices.len() < 100); // sanity check
    }
}
