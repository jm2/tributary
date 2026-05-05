//! USB mass-storage device detection.
//!
//! Detects mounted removable drives that may contain music files so
//! they can appear under "Devices" in the sidebar.
//!
//! # Platform support
//!
//! - **Linux**: scans `/media/$USER/` and `/run/media/$USER/` for
//!   removable volumes.
//! - **macOS**: scans `/Volumes/` for non-system volumes.
//! - **Windows**: enumerates drive letters and checks for removable
//!   drives via `GetDriveTypeW`.
//!
//! # Status
//!
//! Detection-only. Sync / transfer / browsing operations beyond a
//! one-shot library scan are not implemented (tracked in GitHub
//! issues #1 and #8).

pub mod usb;

/// Information about a detected portable device.
///
/// The fields are limited to what the UI consumes today (name +
/// mount point); add others back in if/when sync/transfer lands.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Human-readable device name (volume label or mount-point name).
    pub name: String,
    /// Filesystem mount point path.
    pub mount_point: std::path::PathBuf,
}
