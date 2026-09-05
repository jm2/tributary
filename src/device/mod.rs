//! Mounted portable-device discovery and transfer.
//!
//! GIO's native [`gtk::gio::VolumeMonitor`] supplies a cached snapshot of the
//! user-visible mounts selected by each platform backend. The UI owns that
//! monitor on the GTK main thread, publishes [`usb::mounted_devices`] snapshots,
//! and wires its mount-added, changed, pre-unmount, and removed signals for live
//! hotplug updates. Filesystem traversal remains separate background work.
//!
//! The [`transfer`] module adds a generic mounted-filesystem transfer planner
//! and executor that satisfies the P3.2 / GitHub issue #8 requirements:
//! retained write authority, capacity and conflict policy, atomic copy where
//! possible, progress reporting, cancellation, and rollback. It builds on the
//! same root-lease model used by the read paths in
//! [`crate::local::root_authority`] and
//! [`crate::local::write_authority`].

pub mod mtp;
pub mod transfer;
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
