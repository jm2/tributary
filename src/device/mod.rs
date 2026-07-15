//! Mounted portable-device discovery.
//!
//! GIO's native [`gtk::gio::VolumeMonitor`] supplies a cached snapshot of the
//! user-visible mounts selected by each platform backend. The UI owns that
//! monitor on the GTK main thread, publishes [`usb::mounted_devices`] snapshots,
//! and wires its mount-added, changed, pre-unmount, and removed signals for live
//! hotplug updates. Filesystem traversal remains separate background work.
//!
//! This layer currently supports browsing mounted filesystems. Device sync,
//! transfer, and mounting an unmounted volume remain outside its scope (tracked
//! in GitHub issues #1 and #8).

pub mod usb;

/// Information about one mounted, browseable device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Best available logical source key from cached platform mount metadata.
    ///
    /// A filesystem UUID is preferred when available. It identifies a logical
    /// filesystem rather than guaranteed unique physical hardware, so a cloned
    /// filesystem can intentionally collide with its source. Fallback device
    /// paths and root URIs can change across a replug or relocation.
    pub source_key: String,
    /// Human-readable name supplied by the platform mount backend.
    pub name: String,
    /// Native filesystem path used by the background audio scanner.
    pub mount_point: std::path::PathBuf,
}
