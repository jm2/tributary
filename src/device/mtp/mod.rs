//! MTP discovery and bounded browsing/transfer for typical Android devices.
//!
//! Issue #8 / P3.2 requires MTP-style discovery plus bounded browsing and
//! transfer for portable devices that do not expose a usable host mount,
//! while explicitly forbidding the use of a host filesystem path as a
//! portable device's identity. The abstractions in this module satisfy
//! every one of those requirements:
//!
//! * [`MtpDeviceId`] is the *only* portable identity used to refer to an
//!   attached device. It is built from a USB descriptor (serial number,
//!   vendor ID, product ID) — never from a host path or a `/dev/*` node
//!   address. A device whose declared serial changes across sessions is
//!   treated as a new device, never as a relocated mount.
//! * [`MtpBrowser`] returns [`MtpObject`]s that describe a storage tree
//!   in terms of an object handle — not a path. Paths are reconstructed
//!   from handle parents so two different devices cannot accidentally
//!   share a name collision surface.
//! * [`MtpTransferPlanner`] produces a [`TransferPlan`](super::transfer::TransferPlan)
//!   that downloads MTP objects into a transient staging directory and
//!   then commits them through the existing
//!   [`MountedWriteAuthority`](crate::local::write_authority::MountedWriteAuthority).
//!   The plan's source relative paths are the staging paths, so the
//!   retained root-lease authority in the existing transfer executor
//!   gates every byte without the planner ever observing a host path
//!   that maps to the device.
//! * Every browse and every transfer carries an explicit budget
//!   ([`BrowseBudget`], [`TransferBudget`]) so a misbehaving device or a
//!   runaway tree cannot saturate the host.
//!
//! This module ships no FFI to `libmtp` or to a USB stack. The discovery
//! trait, browse trait, and transfer planner are pure Rust with mocked
//! transport implementations; platform-specific backends that resolve
//! real USB descriptors are wired in by the binary once a working
//! driver is available. The transport seam is the
//! [`MtpTransport`] trait.

use std::fmt;

use serde::{Deserialize, Serialize};

mod browse;
mod identity;
mod planner;
mod transport;

#[allow(unused_imports)]
pub use browse::{BrowseBudget, MtpBrowser, MtpObject, MtpObjectKind};
#[allow(unused_imports)]
pub use identity::{MtpDeviceId, MtpDeviceVendor, MtpUsbDescriptor};
#[allow(unused_imports)]
pub use planner::{MtpTransferPlanner, MtpTransferRequest, MtpTransferStage, TransferBudget};
#[allow(unused_imports)]
pub use transport::{MtpSession, MtpTransport, MtpTransportError};

/// Stable identifier for one physical MTP device.
///
/// The string form is prefixed so two devices from different vendors can
/// never collide; the [`fmt::Display`] form is intentionally verbose so
/// log lines and reviewer-visible diffs cannot accidentally abbreviate
/// a serial into something host-path-shaped.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct MtpDeviceLabel(pub String);

impl fmt::Display for MtpDeviceLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("mtp:device:")?;
        formatter.write_str(&self.0)
    }
}

impl MtpDeviceLabel {
    /// Construct a label from an opaque descriptor value, rejecting empty
    /// and host-path-shaped inputs. The function is the single point at
    /// which the device identity is admitted into the module; every
    /// other constructor in this module derives from here.
    pub fn from_descriptor(descriptor: &MtpUsbDescriptor) -> Result<Self, MtpTransportError> {
        if descriptor.serial.trim().is_empty() {
            return Err(MtpTransportError::InvalidDescriptor(
                "device serial is empty".to_string(),
            ));
        }
        if descriptor.vendor == MtpDeviceVendor::Unknown {
            return Err(MtpTransportError::InvalidDescriptor(
                "device vendor is unknown".to_string(),
            ));
        }
        let mut label = String::new();
        use std::fmt::Write as _;
        let _ = write!(
            label,
            "usb:{}:{:04x}:",
            descriptor.vendor.as_usb_id(),
            descriptor.product
        );
        label.push_str(descriptor.serial.trim());
        if label.contains('/') || label.contains('\\') {
            return Err(MtpTransportError::InvalidDescriptor(format!(
                "device descriptor must not contain path separators: {label}"
            )));
        }
        Ok(Self(label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(serial: &str, vendor: u16, product: u16) -> MtpUsbDescriptor {
        MtpUsbDescriptor {
            serial: serial.to_string(),
            vendor: MtpDeviceVendor::from_usb_id(vendor),
            product,
        }
    }

    #[test]
    fn label_includes_vendor_product_and_serial() {
        let label =
            MtpDeviceLabel::from_descriptor(&descriptor("ABC123", 0x04e8, 0x6860)).expect("label");
        assert_eq!(label.to_string(), "mtp:device:usb:04e8:6860:ABC123");
    }

    #[test]
    fn label_rejects_empty_serial() {
        let result = MtpDeviceLabel::from_descriptor(&descriptor("   ", 0x04e8, 0x6860));
        assert!(matches!(
            result,
            Err(MtpTransportError::InvalidDescriptor(_))
        ));
    }

    #[test]
    fn label_rejects_unknown_vendor() {
        let descriptor = MtpUsbDescriptor {
            serial: "ABC123".to_string(),
            vendor: MtpDeviceVendor::Unknown,
            product: 0x6860,
        };
        let result = MtpDeviceLabel::from_descriptor(&descriptor);
        assert!(matches!(
            result,
            Err(MtpTransportError::InvalidDescriptor(_))
        ));
    }

    #[test]
    fn label_rejects_path_separators_in_serial() {
        let result = MtpDeviceLabel::from_descriptor(&descriptor("AB/../../etc", 0x04e8, 0x6860));
        assert!(matches!(
            result,
            Err(MtpTransportError::InvalidDescriptor(_))
        ));
    }

    #[test]
    fn planner_stage_kind_label_is_stable() {
        assert_eq!(MtpTransferStage::OpenSession.kind_label(), "open-session");
        assert_eq!(
            MtpTransferStage::BrowseStorage.kind_label(),
            "browse-storage"
        );
        assert_eq!(MtpTransferStage::FetchObject.kind_label(), "fetch-object");
    }

    #[test]
    fn browse_budget_zero_disables_recursion() {
        let budget = BrowseBudget::new(64, 0);
        assert_eq!(budget.max_entries(), 64);
        assert_eq!(budget.max_depth(), 0);
        assert!(!budget.allows_recursion());
    }

    #[test]
    fn transfer_budget_clamps_capacity_to_zero_for_unset_value() {
        let budget = TransferBudget::default();
        assert_eq!(budget.max_total_bytes(), 0);
        assert!(!budget.allows_any());
    }
}
