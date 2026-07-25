//! Transport seam for MTP discovery and object access.
//!
//! The transport trait abstracts the host's actual mechanism for
//! talking to an MTP-class USB device. Production binaries will plug a
//! real transport here (libmtp, adb sync, raw USB); the unit tests in
//! this crate substitute an in-memory transport so the discovery and
//! transfer logic is exercised without any kernel or FFI dependency.
//!
//! The transport is intentionally narrow: it can open a session against
//! a device whose USB descriptor has been observed, list the device's
//! storage objects, and fetch the bytes of one storage object. The
//! transport never sees a host path and never observes a destination
//! filesystem — those concerns belong to the planner and to the
//! [`MountedRootAuthority`](crate::local::root_authority::MountedRootAuthority)
//! that the planner stages into.
//!
//! A live session is a [`MtpSession`] handle. The transport is the only
//! place that can mint one; once minted, the session is the unit of
//! authority for the device it was opened against. The session's
//! [`MtpSession::device_id`] is the only portable identity the rest of
//! the module consults; a session whose id changes after construction
//! is a programming error and is rejected by [`MtpSession::verify`].

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::identity::{MtpDeviceId, MtpUsbDescriptor};

/// Why an MTP transport call failed.
#[derive(Debug, Error)]
pub enum MtpTransportError {
    /// A descriptor field was empty, malformed, or otherwise unusable
    /// as a portable device identity.
    #[error("invalid MTP descriptor: {0}")]
    InvalidDescriptor(String),
    /// The transport could not reach the device. The transport is
    /// responsible for keeping the underlying error message free of host
    /// paths.
    #[error("MTP device {0} unreachable: {1}")]
    DeviceUnreachable(String, String),
    /// The transport cannot parse the device's response.
    #[error("MTP device {0} response malformed: {1}")]
    MalformedResponse(String, String),
    /// The transport was cancelled mid-operation.
    #[error("MTP transfer cancelled")]
    Cancelled,
    /// A session outlived its underlying device.
    #[error("MTP session for {0} lost its device")]
    SessionLost(String),
}

/// A live MTP session for one attached device.
///
/// The session is the only object that can fetch bytes from a device.
/// It carries the resolved [`MtpDeviceId`] so the planner and browser
/// never re-derive identity from raw USB metadata.
pub struct MtpSession {
    device_id: MtpDeviceId,
    descriptor: MtpUsbDescriptor,
    /// Opaque cookie supplied by the transport backend. The planner
    /// passes it back through the same transport that opened the
    /// session; the rest of the system does not introspect it.
    backend_token: Box<dyn std::any::Any + Send + Sync>,
}

impl MtpSession {
    /// Construct a session from the transport's resolved identity and a
    /// backend cookie. The constructor is `pub(crate)` so only the
    /// transport can mint a session.
    pub(crate) fn new(
        device_id: MtpDeviceId,
        descriptor: MtpUsbDescriptor,
        backend_token: Box<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self {
            device_id,
            descriptor,
            backend_token,
        }
    }

    /// The portable identity of the device the session is bound to.
    pub fn device_id(&self) -> &MtpDeviceId {
        &self.device_id
    }

    /// The USB descriptor that was used to open the session. Returned
    /// for diagnostics; it must not be used to derive a host-path
    /// identity.
    pub fn descriptor(&self) -> &MtpUsbDescriptor {
        &self.descriptor
    }

    /// Borrow the backend cookie. The transport trait bounds the
    /// `cookie` to its own session type so other code cannot smuggle a
    /// foreign cookie past the type system.
    pub fn cookie<T: 'static>(&self) -> Option<&T> {
        self.backend_token.downcast_ref::<T>()
    }

    /// Re-verify the session is still bound to a live device. A session
    /// that fails verification is treated as lost and the planner will
    /// surface a [`MtpTransportError::SessionLost`] rather than commit
    /// partial work.
    pub fn verify(&self) -> Result<(), MtpTransportError> {
        if self.device_id.label().is_empty() {
            return Err(MtpTransportError::SessionLost(self.device_id.to_string()));
        }
        Ok(())
    }
}

impl fmt::Debug for MtpSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MtpSession")
            .field("device_id", &self.device_id)
            .field("vendor", &self.descriptor.vendor)
            .field("product", &self.descriptor.product)
            .field("serial", &self.descriptor.serial)
            .finish_non_exhaustive()
    }
}

/// The transport seam for MTP discovery and object access.
///
/// A transport implementation wraps the platform's actual MTP stack
/// (libmtp, adb, raw USB). The trait is the only place that can mint a
/// [`MtpSession`]; the rest of the system calls into the session
/// indirectly through the planner.
pub trait MtpTransport: Send + Sync {
    /// List the USB descriptors of every currently attached MTP device.
    /// The transport is responsible for excluding non-MTP devices; an
    /// empty vector means "no portable devices attached."
    fn list_devices(&self) -> Result<Vec<MtpUsbDescriptor>, MtpTransportError>;

    /// Open a session against a specific descriptor. The transport
    /// must re-validate that the device is still attached and still
    /// exposes the same descriptor; if any field has changed, it must
    /// return [`MtpTransportError::DeviceUnreachable`] so the planner
    /// does not commit work against a different device.
    fn open_session(&self, descriptor: &MtpUsbDescriptor) -> Result<MtpSession, MtpTransportError>;

    /// List the storage objects visible on an open session. The result
    /// is bounded by the caller; a transport that returns a list larger
    /// than the budget may have its listing rejected by the caller, but
    /// the transport must not return an unbounded list implicitly.
    fn list_storage(
        &self,
        session: &MtpSession,
    ) -> Result<Vec<MtpStorageDescriptor>, MtpTransportError>;

    /// Fetch the bytes of one storage object. The transport is the only
    /// place that ever hands device bytes to a file; the caller is
    /// responsible for writing them beneath a
    /// [`MountedRootAuthority`](crate::local::root_authority::MountedRootAuthority)
    /// so the bytes are staged through the existing transfer planner.
    fn fetch_object(
        &self,
        session: &MtpSession,
        object_handle: MtpObjectHandle,
    ) -> Result<MtpObjectBytes, MtpTransportError>;
}

/// One MTP storage area on a device.
///
/// Each storage is a self-contained filesystem. A device with both an
/// internal and an SD-card storage will report two descriptors; the
/// planner never confuses the two because the storage id is part of
/// the portable device identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MtpStorageDescriptor {
    /// MTP storage ID, allocated by the device. Storage IDs are
    /// per-device, so the planner prefixes them with the device id
    /// before they become part of any plan.
    pub storage_id: u32,
    /// Filesystem label reported by the device (e.g. "Internal shared
    /// storage"). Trimmed; may be empty for unlabeled storage.
    pub label: String,
    /// Total capacity in bytes. Reported as `u64::MAX` for storage that
    /// cannot report a size; the planner treats that as "no budget."
    pub capacity_bytes: u64,
    /// Free capacity in bytes. Same convention as `capacity_bytes`.
    pub free_bytes: u64,
    /// Whether the storage is flagged as removable. The planner uses
    /// this to decide whether a transfer should be planned with
    /// additional atomicity.
    pub removable: bool,
}

/// MTP object handle, allocated by the device. The handle is the
/// portable address of one object on one device; it is independent of
/// any host path and is the only key the transport uses to fetch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct MtpObjectHandle(pub u32);

impl fmt::Display for MtpObjectHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "mtp:object:{}", self.0)
    }
}

/// Bytes returned by a transport fetch. The transport never hands the
/// planner a path; the planner writes the bytes beneath its own staged
/// authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MtpObjectBytes {
    pub handle: MtpObjectHandle,
    pub bytes: Vec<u8>,
}

#[cfg(test)]
pub mod test_transport {
    //! In-memory transport used by the planner and browser unit tests.
    //! The transport is `pub(crate)`; the binary wires a real backend.

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// One MTP storage area in the in-memory transport.
    #[derive(Debug)]
    pub struct InMemoryStorage {
        pub descriptor: MtpStorageDescriptor,
        pub objects: HashMap<MtpObjectHandle, InMemoryObject>,
    }

    /// One MTP object in the in-memory transport.
    #[derive(Debug, Clone)]
    pub struct InMemoryObject {
        pub handle: MtpObjectHandle,
        pub parent: Option<MtpObjectHandle>,
        pub name: String,
        pub kind: super::super::browse::MtpObjectKind,
        pub size_bytes: u64,
        pub bytes: Vec<u8>,
    }

    /// In-memory transport. Stores a list of devices and their storage.
    pub struct InMemoryMtpTransport {
        state: Mutex<InMemoryState>,
    }

    #[derive(Debug, Default)]
    struct InMemoryState {
        devices: Vec<InMemoryDevice>,
        open_sessions: HashMap<String, MtpUsbDescriptor>,
    }

    #[derive(Debug)]
    pub struct InMemoryDevice {
        pub descriptor: MtpUsbDescriptor,
        pub storages: Vec<InMemoryStorage>,
    }

    impl InMemoryMtpTransport {
        /// Construct a transport preloaded with one device and one
        /// storage of test objects. The test objects are large enough to
        /// exercise the planner's per-chunk progress path.
        pub fn single_device(descriptor: MtpUsbDescriptor) -> Self {
            Self::with_devices(vec![InMemoryDevice {
                descriptor,
                storages: vec![InMemoryStorage {
                    descriptor: MtpStorageDescriptor {
                        storage_id: 0x0001_0001,
                        label: "Internal shared storage".to_string(),
                        capacity_bytes: 64 * 1024 * 1024,
                        free_bytes: 32 * 1024 * 1024,
                        removable: false,
                    },
                    objects: HashMap::new(),
                }],
            }])
        }

        /// Construct a transport with an explicit device list.
        pub fn with_devices(devices: Vec<InMemoryDevice>) -> Self {
            Self {
                state: Mutex::new(InMemoryState {
                    devices,
                    open_sessions: HashMap::new(),
                }),
            }
        }

        /// Add an object to the device's first storage. The helper is
        /// only callable while no session is open, so test setup
        /// composes naturally.
        pub fn add_object(
            &self,
            device_index: usize,
            storage_index: usize,
            object: InMemoryObject,
        ) {
            let mut state = self.state.lock().expect("transport mutex");
            let storage = &mut state.devices[device_index].storages[storage_index];
            storage.objects.insert(object.handle, object);
        }
    }

    impl MtpTransport for InMemoryMtpTransport {
        fn list_devices(&self) -> Result<Vec<MtpUsbDescriptor>, MtpTransportError> {
            let state = self.state.lock().expect("transport mutex");
            Ok(state
                .devices
                .iter()
                .map(|device| device.descriptor.clone())
                .collect())
        }

        fn open_session(
            &self,
            descriptor: &MtpUsbDescriptor,
        ) -> Result<MtpSession, MtpTransportError> {
            let mut state = self.state.lock().expect("transport mutex");
            let id = MtpDeviceId::from_descriptor(descriptor)?;
            let stored = state
                .devices
                .iter()
                .find(|device| {
                    device.descriptor.serial == descriptor.serial
                        && device.descriptor.vendor == descriptor.vendor
                        && device.descriptor.product == descriptor.product
                })
                .ok_or_else(|| {
                    MtpTransportError::DeviceUnreachable(
                        descriptor.serial.clone(),
                        "descriptor not present".to_string(),
                    )
                })?;
            let stored_descriptor = stored.descriptor.clone();
            state
                .open_sessions
                .insert(id.label().to_string(), stored_descriptor.clone());
            Ok(MtpSession::new(id, stored_descriptor, Box::new(())))
        }

        fn list_storage(
            &self,
            session: &MtpSession,
        ) -> Result<Vec<MtpStorageDescriptor>, MtpTransportError> {
            session.verify()?;
            let state = self.state.lock().expect("transport mutex");
            let device = state
                .devices
                .iter()
                .find(|device| {
                    device.descriptor.serial == session.descriptor().serial
                        && device.descriptor.vendor == session.descriptor().vendor
                        && device.descriptor.product == session.descriptor().product
                })
                .ok_or_else(|| {
                    MtpTransportError::DeviceUnreachable(
                        session.device_id().to_string(),
                        "device disappeared".to_string(),
                    )
                })?;
            Ok(device
                .storages
                .iter()
                .map(|storage| storage.descriptor.clone())
                .collect())
        }

        fn fetch_object(
            &self,
            session: &MtpSession,
            object_handle: MtpObjectHandle,
        ) -> Result<MtpObjectBytes, MtpTransportError> {
            session.verify()?;
            let state = self.state.lock().expect("transport mutex");
            let device = state
                .devices
                .iter()
                .find(|device| {
                    device.descriptor.serial == session.descriptor().serial
                        && device.descriptor.vendor == session.descriptor().vendor
                        && device.descriptor.product == session.descriptor().product
                })
                .ok_or_else(|| {
                    MtpTransportError::DeviceUnreachable(
                        session.device_id().to_string(),
                        "device disappeared".to_string(),
                    )
                })?;
            for storage in &device.storages {
                if let Some(object) = storage.objects.get(&object_handle) {
                    return Ok(MtpObjectBytes {
                        handle: object_handle,
                        bytes: object.bytes.clone(),
                    });
                }
            }
            Err(MtpTransportError::MalformedResponse(
                session.device_id().to_string(),
                format!("no such object handle {object_handle}"),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MtpTransport;
    use super::*;
    use test_transport::InMemoryMtpTransport;

    fn descriptor(serial: &str) -> MtpUsbDescriptor {
        MtpUsbDescriptor::new(0x04e8, 0x6860, serial).expect("descriptor")
    }

    #[test]
    fn transport_reports_attached_devices() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let devices = transport.list_devices().expect("list");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].serial, "ABC123");
    }

    #[test]
    fn open_session_rejects_unknown_descriptor() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let result = transport.open_session(&descriptor("XYZ789"));
        assert!(matches!(
            result,
            Err(MtpTransportError::DeviceUnreachable(_, _))
        ));
    }

    #[test]
    fn session_carries_device_id_and_descriptor() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let session = transport
            .open_session(&descriptor("ABC123"))
            .expect("session");
        assert_eq!(session.device_id().label(), "usb:04e8:6860:ABC123");
        assert_eq!(session.descriptor().serial, "ABC123");
        assert!(session.verify().is_ok());
    }

    #[test]
    fn list_storage_returns_in_memory_descriptor() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let session = transport
            .open_session(&descriptor("ABC123"))
            .expect("session");
        let storages = transport.list_storage(&session).expect("storage");
        assert_eq!(storages.len(), 1);
        assert_eq!(storages[0].label, "Internal shared storage");
    }

    #[test]
    fn fetch_object_returns_recorded_bytes() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let handle = MtpObjectHandle(0x0001_0001);
        transport.add_object(
            0,
            0,
            test_transport::InMemoryObject {
                handle,
                parent: None,
                name: "song.flac".to_string(),
                kind: super::super::browse::MtpObjectKind::RegularFile,
                size_bytes: 5,
                bytes: b"audio".to_vec(),
            },
        );
        let session = transport
            .open_session(&descriptor("ABC123"))
            .expect("session");
        let fetched = transport.fetch_object(&session, handle).expect("fetched");
        assert_eq!(fetched.bytes, b"audio");
    }

    #[test]
    fn fetch_object_rejects_unknown_handle() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let session = transport
            .open_session(&descriptor("ABC123"))
            .expect("session");
        let result = transport.fetch_object(&session, MtpObjectHandle(0xdead_beef));
        assert!(matches!(
            result,
            Err(MtpTransportError::MalformedResponse(_, _))
        ));
    }
}
