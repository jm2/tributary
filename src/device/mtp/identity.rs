//! Device-identity policy for MTP discovery.
//!
//! MTP device identity is built exclusively from the device's USB
//! descriptor. A host path or a `/dev/bus/usb/*` node is never a portable
//! device identity: those addresses are kernel-side allocations that can
//! shift across replugs, hubs, and reboots, and they identify a USB
//! socket, not the device attached to it.
//!
//! [`MtpUsbDescriptor`] captures the fields the kernel exposes for an
//! MTP-class device. [`MtpDeviceId`] is the resolved, validated device
//! identity the rest of the system uses. [`MtpDeviceVendor`] is a typed
//! enumeration of well-known MTP vendors with a free-form fallback for
//! unknown ones; the variant is carried in the device id so reviewers
//! can grep for which vendor the system is talking to.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::MtpTransportError;

/// A USB descriptor reported by the kernel for an attached MTP device.
///
/// The descriptor is the only input the rest of the module accepts as
/// evidence of "this is a portable device." Every other source of
/// identity (host path, bus address, port number) is rejected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MtpUsbDescriptor {
    /// USB iSerial descriptor value. Trimmed; whitespace-only is
    /// rejected at the label-construction boundary.
    pub serial: String,
    /// USB idVendor. Resolved against [`MtpDeviceVendor`] so callers do
    /// not have to repeat magic numbers.
    pub vendor: MtpDeviceVendor,
    /// USB idProduct. The numeric product ID is retained verbatim
    /// because product IDs are not centrally allocated.
    pub product: u16,
}

impl MtpUsbDescriptor {
    /// Construct a descriptor from raw USB IDs and a serial, rejecting
    /// empty serials. The vendor id must resolve to a known vendor; an
    /// id of `0x0000` is never a valid MTP vendor.
    pub fn new(
        vendor_id: u16,
        product: u16,
        serial: impl Into<String>,
    ) -> Result<Self, MtpTransportError> {
        let vendor = MtpDeviceVendor::from_usb_id(vendor_id);
        if matches!(vendor, MtpDeviceVendor::Unknown) {
            return Err(MtpTransportError::InvalidDescriptor(format!(
                "vendor id {vendor_id:#06x} is not a known MTP vendor"
            )));
        }
        let serial = serial.into();
        if serial.trim().is_empty() {
            return Err(MtpTransportError::InvalidDescriptor(
                "device serial is empty".to_string(),
            ));
        }
        Ok(Self {
            serial,
            vendor,
            product,
        })
    }
}

/// A typed enumeration of USB vendor IDs known to ship MTP-class
/// devices. Unknown IDs are kept as a separate variant so reviewer
/// diffs can surface a "this vendor is not in the table" condition
/// rather than silently aliasing it to a placeholder.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum MtpDeviceVendor {
    /// Samsung Electronics (`0x04e8`).
    Samsung,
    /// Google Inc. (`0x18d1`).
    Google,
    /// LG Electronics (`0x1004`).
    Lg,
    /// Sony Corporation (`0x054c`).
    Sony,
    /// HTC Corporation (`0x0bb4`).
    Htc,
    /// Huawei Technologies (`0x12d1`).
    Huawei,
    /// OnePlus Technology (`0x2a70`).
    OnePlus,
    /// Xiaomi Inc. (`0x2717`).
    Xiaomi,
    /// Motorola Mobility (`0x22b8`).
    Motorola,
    /// Any vendor the system has not been taught about yet. The id is
    /// retained in the variant so logs can show which vendor was seen.
    Other(u16),
    /// Vendor id `0x0000`. A descriptor that resolved here is not an
    /// MTP-class device and must not be admitted as one.
    Unknown,
}

impl MtpDeviceVendor {
    /// Resolve a USB vendor id to a typed variant.
    pub fn from_usb_id(id: u16) -> Self {
        match id {
            0x04e8 => Self::Samsung,
            0x18d1 => Self::Google,
            0x1004 => Self::Lg,
            0x054c => Self::Sony,
            0x0bb4 => Self::Htc,
            0x12d1 => Self::Huawei,
            0x2a70 => Self::OnePlus,
            0x2717 => Self::Xiaomi,
            0x22b8 => Self::Motorola,
            0x0000 => Self::Unknown,
            other => Self::Other(other),
        }
    }

    /// Render the vendor as the four-digit USB id string used inside
    /// the portable device label. Lower nibble is zero-padded.
    pub fn as_usb_id(&self) -> String {
        match self {
            Self::Samsung => "04e8".to_string(),
            Self::Google => "18d1".to_string(),
            Self::Lg => "1004".to_string(),
            Self::Sony => "054c".to_string(),
            Self::Htc => "0bb4".to_string(),
            Self::Huawei => "12d1".to_string(),
            Self::OnePlus => "2a70".to_string(),
            Self::Xiaomi => "2717".to_string(),
            Self::Motorola => "22b8".to_string(),
            Self::Other(id) => format!("{id:04x}"),
            Self::Unknown => "0000".to_string(),
        }
    }
}

impl fmt::Display for MtpDeviceVendor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Samsung => formatter.write_str("Samsung"),
            Self::Google => formatter.write_str("Google"),
            Self::Lg => formatter.write_str("LG"),
            Self::Sony => formatter.write_str("Sony"),
            Self::Htc => formatter.write_str("HTC"),
            Self::Huawei => formatter.write_str("Huawei"),
            Self::OnePlus => formatter.write_str("OnePlus"),
            Self::Xiaomi => formatter.write_str("Xiaomi"),
            Self::Motorola => formatter.write_str("Motorola"),
            Self::Other(id) => write!(formatter, "Unknown(0x{id:04x})"),
            Self::Unknown => formatter.write_str("Unknown"),
        }
    }
}

/// The portable identity used to address one attached MTP device.
///
/// Two descriptors that resolve to the same [`MtpDeviceId`] refer to
/// the same physical device. Two descriptors that differ in any field
/// resolve to two different devices, even if the kernel attached both
/// at the same USB socket, because the kernel is free to reuse the
/// address for an unrelated device on the next plug event.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct MtpDeviceId {
    label: String,
    vendor: MtpDeviceVendor,
    product: u16,
}

impl MtpDeviceId {
    /// Resolve a USB descriptor into a portable device id.
    pub fn from_descriptor(descriptor: &MtpUsbDescriptor) -> Result<Self, MtpTransportError> {
        let trimmed = descriptor.serial.trim();
        if trimmed.is_empty() {
            return Err(MtpTransportError::InvalidDescriptor(
                "device serial is empty".to_string(),
            ));
        }
        let label = super::MtpDeviceLabel::from_descriptor(descriptor)?;
        Ok(Self {
            label: label.0,
            vendor: descriptor.vendor,
            product: descriptor.product,
        })
    }

    /// The resolved label, identical to the
    /// [`MtpDeviceLabel`](super::MtpDeviceLabel) derived from the same
    /// descriptor.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The vendor that produced the device. Surfaced in logs and UI so
    /// reviewers can verify the right vendor table is in use.
    pub fn vendor(&self) -> MtpDeviceVendor {
        self.vendor
    }

    /// The USB product ID. Returned alongside the vendor so a future
    /// per-vendor config (e.g. PTP quirks) can dispatch on it.
    pub fn product(&self) -> u16 {
        self.product
    }
}

impl fmt::Display for MtpDeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("mtp:id:")?;
        formatter.write_str(&self.label)
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
    fn from_usb_id_resolves_known_vendors() {
        assert_eq!(
            MtpDeviceVendor::from_usb_id(0x04e8),
            MtpDeviceVendor::Samsung
        );
        assert_eq!(
            MtpDeviceVendor::from_usb_id(0x18d1),
            MtpDeviceVendor::Google
        );
        assert_eq!(
            MtpDeviceVendor::from_usb_id(0x2a70),
            MtpDeviceVendor::OnePlus
        );
        assert_eq!(
            MtpDeviceVendor::from_usb_id(0x0000),
            MtpDeviceVendor::Unknown
        );
        assert!(matches!(
            MtpDeviceVendor::from_usb_id(0x1234),
            MtpDeviceVendor::Other(0x1234)
        ));
    }

    #[test]
    fn as_usb_id_round_trips_known_vendors() {
        for vendor in [
            MtpDeviceVendor::Samsung,
            MtpDeviceVendor::Google,
            MtpDeviceVendor::Lg,
            MtpDeviceVendor::Sony,
            MtpDeviceVendor::Htc,
            MtpDeviceVendor::Huawei,
            MtpDeviceVendor::OnePlus,
            MtpDeviceVendor::Xiaomi,
            MtpDeviceVendor::Motorola,
            MtpDeviceVendor::Other(0xbeef),
        ] {
            let id = vendor.as_usb_id();
            assert_eq!(
                MtpDeviceVendor::from_usb_id(u16::from_str_radix(&id, 16).unwrap()),
                vendor
            );
        }
    }

    #[test]
    fn vendor_display_does_not_leak_host_path() {
        for vendor in [
            MtpDeviceVendor::Samsung,
            MtpDeviceVendor::Google,
            MtpDeviceVendor::Other(0x1234),
            MtpDeviceVendor::Unknown,
        ] {
            let rendered = vendor.to_string();
            assert!(!rendered.contains('/'));
            assert!(!rendered.contains(".."));
        }
    }

    #[test]
    fn descriptor_rejects_zero_vendor() {
        let result = MtpUsbDescriptor::new(0x0000, 0x6860, "ABC123");
        assert!(matches!(
            result,
            Err(MtpTransportError::InvalidDescriptor(_))
        ));
    }

    #[test]
    fn descriptor_rejects_empty_serial() {
        let result = MtpUsbDescriptor::new(0x04e8, 0x6860, "   ");
        assert!(matches!(
            result,
            Err(MtpTransportError::InvalidDescriptor(_))
        ));
    }

    #[test]
    fn descriptor_accepts_known_vendor_with_serial() {
        let descriptor = MtpUsbDescriptor::new(0x04e8, 0x6860, "ABC123").expect("descriptor");
        assert_eq!(descriptor.serial, "ABC123");
        assert_eq!(descriptor.vendor, MtpDeviceVendor::Samsung);
        assert_eq!(descriptor.product, 0x6860);
    }

    #[test]
    fn device_id_label_matches_underlying_label() {
        let descriptor = descriptor("ABC123", 0x04e8, 0x6860);
        let id = MtpDeviceId::from_descriptor(&descriptor).expect("id");
        let label = super::super::MtpDeviceLabel::from_descriptor(&descriptor).expect("label");
        assert_eq!(id.label(), label.0);
        assert_eq!(id.to_string(), format!("mtp:id:{}", label.0));
    }

    #[test]
    fn device_id_rejects_empty_serial() {
        let descriptor = descriptor(" ", 0x04e8, 0x6860);
        let result = MtpDeviceId::from_descriptor(&descriptor);
        assert!(matches!(
            result,
            Err(MtpTransportError::InvalidDescriptor(_))
        ));
    }
}
