//! USB device detection and management scaffolding.
//!
//! Provides trait definitions for portable music device interaction and
//! basic detection of mounted USB drives with music files.
//!
//! # Architecture
//!
//! The `Device` trait abstracts over different portable device types
//! (USB mass storage, MTP, iPod, etc.).  The initial implementation
//! targets USB mass storage — removable drives that appear as mounted
//! filesystems with standard audio files.
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
//! This module is scaffolding for GitHub issues #1 and #8.  The trait
//! definitions and detection logic are complete; sync/transfer
//! operations are not yet implemented.

pub mod transfer;
pub mod usb;

/// Information about a detected portable device.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DeviceInfo {
    /// Human-readable device name (e.g. "WALKMAN", "iPod", volume label).
    pub name: String,
    /// Filesystem mount point path.
    pub mount_point: std::path::PathBuf,
    /// Total capacity in bytes, if known.
    pub capacity_bytes: Option<u64>,
    /// Free space in bytes, if known.
    pub free_bytes: Option<u64>,
    /// Device type classification.
    pub device_type: DeviceType,
}

/// Classification of portable device types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DeviceType {
    /// USB mass storage device (appears as a mounted filesystem).
    UsbMassStorage,
    /// MTP device (Android phones, some DAPs).
    Mtp,
    /// Apple iPod (classic, nano, shuffle).
    IPod,
}

/// Trait for portable music device interaction.
///
/// All methods are designed to be called from the GTK main thread.
/// Operations that perform I/O should be dispatched to background
/// threads internally.
#[allow(dead_code)]
pub trait Device {
    /// Human-readable display name for the sidebar.
    fn name(&self) -> &str;

    /// Filesystem mount point.
    fn mount_point(&self) -> &std::path::Path;

    /// Device type classification.
    fn device_type(&self) -> DeviceType;

    /// Whether the device is currently connected and mounted.
    fn is_connected(&self) -> bool;

    /// Total storage capacity in bytes, if known.
    fn capacity_bytes(&self) -> Option<u64>;

    /// Available free space in bytes, if known.
    fn free_bytes(&self) -> Option<u64>;

    /// Scan the device for music files and return their paths.
    ///
    /// Returns paths relative to the mount point.
    fn scan_music_files(&self) -> Vec<std::path::PathBuf>;
}
