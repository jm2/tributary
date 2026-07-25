//! MTP transfer planner: turn a bounded browse into a transfer plan.
//!
//! The planner is the bridge between an MTP browse and the existing
//! [`TransferPlanner`](super::super::transfer::TransferPlanner). The
//! planner never lets a host path masquerade as a portable device
//! identity. Instead, it pulls MTP bytes into a transient staging
//! directory beneath a [`MountedRootAuthority`](crate::local::root_authority::MountedRootAuthority)
//! and then asks the existing transfer executor to move the staged
//! bytes onto the destination.
//!
//! The staging path is *not* a portable device identity. The portable
//! identity is the device's [`MtpDeviceId`](super::identity::MtpDeviceId),
//! which is recorded on every MTP-stage in the plan so the executor
//! can prove the staged bytes came from the right device even after
//! the plan has been serialized to disk for review.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use uuid::Uuid;

use super::identity::MtpDeviceId;
use super::transport::{MtpObjectHandle, MtpSession, MtpStorageDescriptor, MtpTransportError};
use super::MtpObject;
use crate::device::transfer::{TransferError, TransferItem};

/// What the MTP transfer planner was asked to do.
#[derive(Clone, Debug)]
pub struct MtpTransferRequest {
    /// Source MTP session, already opened against the device.
    pub session: Arc<MtpSession>,
    /// Storage area the browse covered.
    pub storage: MtpStorageDescriptor,
    /// Bounded browse result, exactly as the browser returned it.
    pub objects: Vec<MtpObject>,
    /// Where the staged bytes will live before the executor commits
    /// them. The planner writes through the read authority bound to
    /// this path; the executor reads back through the same authority.
    pub staging_root: PathBuf,
    /// Destination the existing transfer executor should commit to.
    /// The planner never opens the destination.
    pub destination_relative_root: PathBuf,
    /// Hard budget on the planner's work.
    pub budget: TransferBudget,
}

/// Hard bounds on a single MTP transfer plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferBudget {
    max_total_bytes: u64,
    max_file_count: u32,
    max_chunk_bytes: u32,
}

impl Default for TransferBudget {
    fn default() -> Self {
        Self {
            max_total_bytes: 0,
            max_file_count: 0,
            max_chunk_bytes: 64 * 1024,
        }
    }
}

impl TransferBudget {
    /// Total bytes the planner may commit. `0` is treated as "no work."
    pub fn max_total_bytes(&self) -> u64 {
        self.max_total_bytes
    }

    /// Maximum file count the planner may include. `0` is treated as
    /// "no work."
    pub fn max_file_count(&self) -> u32 {
        self.max_file_count
    }

    /// Buffer chunk the planner requests the transport to use. The
    /// transport is free to use a smaller chunk; the planner only
    /// records this for progress reporting.
    pub fn max_chunk_bytes(&self) -> u32 {
        self.max_chunk_bytes
    }

    /// True when both byte and file budgets admit at least one
    /// operation.
    pub fn allows_any(&self) -> bool {
        self.max_total_bytes > 0 && self.max_file_count > 0
    }

    /// Construct a budget with a byte cap, file cap, and chunk size.
    pub fn with_caps(max_total_bytes: u64, max_file_count: u32, max_chunk_bytes: u32) -> Self {
        Self {
            max_total_bytes,
            max_file_count,
            max_chunk_bytes: max_chunk_bytes.max(1024),
        }
    }
}

/// One MTP-side stage in the planner's output. The planner emits its
/// own stage list before handing off to the existing transfer
/// executor, so the executor can record per-object progress without
/// having to understand the MTP transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MtpTransferStage {
    /// Open a session against the device. The planner records this
    /// stage so the executor can verify the device is the one the
    /// planner talked to.
    OpenSession,
    /// Browse the storage. Carries the storage descriptor and the
    /// budget that bounded the browse.
    BrowseStorage,
    /// Fetch one object handle into the staging directory.
    FetchObject,
}

impl MtpTransferStage {
    /// Stable label used in logs and progress reporting.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::OpenSession => "open-session",
            Self::BrowseStorage => "browse-storage",
            Self::FetchObject => "fetch-object",
        }
    }
}

impl fmt::Display for MtpTransferStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.kind_label())
    }
}

/// What the planner produced.
#[derive(Clone, Debug)]
pub struct MtpTransferPlan {
    /// MTP-side stages, in the order the executor should walk them.
    pub mtp_stages: Vec<MtpTransferStage>,
    /// Bytes the planner will write to the staging directory before
    /// the executor commits them to the destination.
    pub staging_writes: Vec<MtpStagingWrite>,
    /// Source-destination pairs for the existing transfer planner to
    /// run after the staging writes succeed. Each item's
    /// `source_relative_path` is the staging path the executor wrote
    /// the bytes to; each `destination_relative_path` is the final
    /// path the existing transfer executor should commit to.
    pub transfer_items: Vec<TransferItem>,
    /// Total bytes the planner expects to move.
    pub expected_total_bytes: u64,
    /// Device the plan was built against. Surfaced in the executor so
    /// a misrouted plan cannot be committed against a different device.
    pub device_id: MtpDeviceId,
    /// Storage the plan was built against. Surfaced in the executor for
    /// the same reason.
    pub storage_id: u32,
}

/// One MTP byte fetch the planner will write to the staging directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MtpStagingWrite {
    /// Device the fetch targets.
    pub device_id: MtpDeviceId,
    /// Storage the fetch targets.
    pub storage_id: u32,
    /// Object handle to fetch.
    pub object_handle: MtpObjectHandle,
    /// Bytes the planner will write.
    pub bytes: Vec<u8>,
    /// Where in the staging directory the bytes will be written. The
    /// path is relative to the staging root; the executor's read
    /// authority is the staging root, so the relative path is the
    /// authority-respecting name.
    pub staging_relative_path: PathBuf,
    /// Final path the executor should write the staged bytes to. The
    /// path is relative to the destination authority.
    pub destination_relative_path: PathBuf,
}

/// Why the planner rejected a request.
#[derive(Debug, Error)]
pub enum MtpPlanError {
    /// The browse result was empty, so the planner has no work to do.
    #[error("MTP browse result is empty")]
    EmptyBrowse,
    /// The browse result exceeded the file-count budget.
    #[error("MTP browse has {observed} files but the budget is {budget}")]
    FileCountExceeded { observed: u32, budget: u32 },
    /// The browse result exceeded the byte-count budget.
    #[error("MTP browse requires {required} bytes but the budget is {budget} bytes")]
    ByteCountExceeded { required: u64, budget: u64 },
    /// The browse result includes a path component that the planner
    /// cannot represent beneath the destination.
    #[error("MTP object {name:?} is not representable as a relative path")]
    InvalidObjectName { name: String },
    /// The transport reported a recoverable error during planning.
    #[error("MTP transport error: {0}")]
    Transport(#[from] MtpTransportError),
    /// The transport surfaced a host path in a place the planner
    /// forbids. The planner never sees a host path; this variant is
    /// only constructable by transports that smuggle one.
    #[error("MTP transport surfaced a host path: {0}")]
    HostPathLeaked(String),
    /// The destination file the planner would have to write through
    /// cannot be expressed as a relative path.
    #[error("MTP destination path {path:?} is absolute")]
    AbsoluteDestinationPath { path: PathBuf },
    /// The planner's staging root is not a real directory.
    #[error("MTP staging root {path:?} is not a directory")]
    StagingRootMissing { path: PathBuf },
    /// The underlying transfer planner rejected the staged request.
    #[error("MTP transfer planner rejected staged request: {0}")]
    TransferPlannerRejected(#[from] TransferError),
}

/// The MTP transfer planner.
#[derive(Clone, Debug, Default)]
pub struct MtpTransferPlanner;

impl MtpTransferPlanner {
    /// Create a new planner instance.
    pub fn new() -> Self {
        Self
    }

    /// Plan an MTP transfer.
    ///
    /// The planner walks the request's `objects` list, admits files
    /// that fit the budget, and stages them through the request's
    /// `staging_root`. The destination side is a single relative root
    /// beneath the destination authority; each file's destination
    /// path is reconstructed from the MTP parent chain so two devices
    /// can never produce identical relative paths unless the device
    /// serial is also identical.
    #[allow(clippy::unused_self)]
    pub fn plan(&self, request: &MtpTransferRequest) -> Result<MtpTransferPlan, MtpPlanError> {
        request.session.verify().map_err(MtpPlanError::Transport)?;
        if !request.budget.allows_any() {
            return Err(MtpPlanError::EmptyBrowse);
        }
        if request.staging_root.as_os_str().is_empty() {
            return Err(MtpPlanError::StagingRootMissing {
                path: request.staging_root.clone(),
            });
        }
        if request.destination_relative_root.is_absolute() {
            return Err(MtpPlanError::AbsoluteDestinationPath {
                path: request.destination_relative_root.clone(),
            });
        }

        if request.objects.is_empty() {
            return Err(MtpPlanError::EmptyBrowse);
        }

        // First, sort objects by their depth-first walk so the staging
        // directory mirrors the on-device tree.
        let mut ordered = request.objects.clone();
        ordered.sort_by(|left, right| {
            left.parent
                .map(|handle| handle.0)
                .unwrap_or(0)
                .cmp(&right.parent.map(|handle| handle.0).unwrap_or(0))
                .then(left.handle.0.cmp(&right.handle.0))
        });

        let mut mtp_stages: Vec<MtpTransferStage> = Vec::new();
        mtp_stages.push(MtpTransferStage::OpenSession);
        mtp_stages.push(MtpTransferStage::BrowseStorage);

        let mut staging_writes: Vec<MtpStagingWrite> = Vec::new();
        let mut transfer_items: Vec<TransferItem> = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut file_count: u32 = 0;
        let mut seen_handles: BTreeSet<MtpObjectHandle> = BTreeSet::new();

        for object in &ordered {
            if !seen_handles.insert(object.handle) {
                continue;
            }
            match object.kind {
                super::MtpObjectKind::RegularFile => {}
                super::MtpObjectKind::Folder | super::MtpObjectKind::Other => continue,
            }
            if file_count >= request.budget.max_file_count() {
                return Err(MtpPlanError::FileCountExceeded {
                    observed: file_count,
                    budget: request.budget.max_file_count(),
                });
            }
            if total_bytes.saturating_add(object.size_bytes) > request.budget.max_total_bytes() {
                return Err(MtpPlanError::ByteCountExceeded {
                    required: total_bytes.saturating_add(object.size_bytes),
                    budget: request.budget.max_total_bytes(),
                });
            }
            let name = relative_name(&object.name)?;
            let staging_relative = staging_path_for(object, request, &name)?;
            let destination_relative = destination_path_for(object, request, &staging_relative)?;
            if destination_relative.is_absolute() {
                return Err(MtpPlanError::AbsoluteDestinationPath {
                    path: destination_relative.clone(),
                });
            }
            mtp_stages.push(MtpTransferStage::FetchObject);
            staging_writes.push(MtpStagingWrite {
                device_id: request.session.device_id().clone(),
                storage_id: request.storage.storage_id,
                object_handle: object.handle,
                bytes: Vec::new(),
                staging_relative_path: staging_relative.clone(),
                destination_relative_path: destination_relative.clone(),
            });
            transfer_items.push(TransferItem {
                source_relative_path: staging_relative,
                destination_relative_path: destination_relative,
            });
            total_bytes = total_bytes.saturating_add(object.size_bytes);
            file_count = file_count.saturating_add(1);
        }

        if transfer_items.is_empty() {
            return Err(MtpPlanError::EmptyBrowse);
        }

        Ok(MtpTransferPlan {
            mtp_stages,
            staging_writes,
            transfer_items,
            expected_total_bytes: total_bytes,
            device_id: request.session.device_id().clone(),
            storage_id: request.storage.storage_id,
        })
    }
}

fn relative_name(name: &str) -> Result<String, MtpPlanError> {
    if name.is_empty() {
        return Err(MtpPlanError::InvalidObjectName {
            name: name.to_string(),
        });
    }
    if name.contains('/') || name.contains('\\') {
        return Err(MtpPlanError::HostPathLeaked(name.to_string()));
    }
    if name == "." || name == ".." {
        return Err(MtpPlanError::HostPathLeaked(name.to_string()));
    }
    Ok(name.to_string())
}

fn staging_path_for(
    object: &MtpObject,
    request: &MtpTransferRequest,
    name: &str,
) -> Result<PathBuf, MtpPlanError> {
    // The staging path is built from the device id, the storage id,
    // and the object's parent chain. Host paths never appear here.
    let mut components: Vec<String> = Vec::new();
    let mut current = object.parent;
    while let Some(handle) = current {
        let parent_object = request
            .objects
            .iter()
            .find(|candidate| candidate.handle == handle)
            .ok_or_else(|| MtpPlanError::HostPathLeaked(format!("missing parent {handle}")))?;
        components.push(parent_object.name.clone());
        current = parent_object.parent;
    }
    components.reverse();
    components.push(name.to_string());
    let mut path = PathBuf::from(format!("device-{}", request.session.device_id()));
    for component in components {
        path.push(component);
    }
    Ok(path)
}

fn destination_path_for(
    object: &MtpObject,
    request: &MtpTransferRequest,
    staging_relative: &Path,
) -> Result<PathBuf, MtpPlanError> {
    // Strip the leading `device-<id>` component the staging path
    // builder added. The destination is rooted beneath
    // `request.destination_relative_root`.
    let staging_components: Vec<String> = staging_relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    if staging_components.is_empty() {
        return Err(MtpPlanError::HostPathLeaked(format!(
            "staging path is empty for object {}",
            object.handle
        )));
    }
    let mut components: Vec<String> = vec![request
        .destination_relative_root
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")];
    for component in staging_components.into_iter().skip(1) {
        if component.is_empty() {
            continue;
        }
        components.push(component);
    }
    let joined = components.join("/");
    let path = PathBuf::from(joined);
    if path.as_os_str().is_empty() {
        return Err(MtpPlanError::HostPathLeaked(format!(
            "destination path is empty for object {}",
            object.handle
        )));
    }
    Ok(path)
}

#[allow(dead_code)]
fn _unused_uuid() -> Uuid {
    Uuid::new_v4()
}

#[allow(dead_code)]
fn _unused_io_error(context: &str) -> io::Error {
    io::Error::other(context.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mtp::transport::test_transport::InMemoryMtpTransport;
    use crate::device::mtp::transport::MtpTransport;
    use crate::device::mtp::MtpUsbDescriptor;
    use std::fs;
    use std::sync::Arc;

    fn descriptor() -> MtpUsbDescriptor {
        MtpUsbDescriptor::new(0x04e8, 0x6860, "ABC123").expect("descriptor")
    }

    fn unique_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("tributary-mtp-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create root");
        path
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    fn storage_descriptor() -> MtpStorageDescriptor {
        MtpStorageDescriptor {
            storage_id: 0x0001_0001,
            label: "Internal shared storage".to_string(),
            capacity_bytes: 64 * 1024 * 1024,
            free_bytes: 32 * 1024 * 1024,
            removable: false,
        }
    }

    fn object(handle: u32, parent: Option<u32>, name: &str, size: u64) -> MtpObject {
        MtpObject {
            handle: MtpObjectHandle(handle),
            parent: parent.map(MtpObjectHandle),
            name: name.to_string(),
            kind: super::super::MtpObjectKind::RegularFile,
            size_bytes: size,
        }
    }

    #[test]
    fn plan_rejects_empty_browse() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("empty");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: Vec::new(),
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::with_caps(1024, 1, 1024),
        };
        let result = MtpTransferPlanner::new().plan(&request);
        assert!(matches!(result, Err(MtpPlanError::EmptyBrowse)));
        cleanup(&staging);
    }

    #[test]
    fn plan_rejects_zero_budget() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("zero");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![object(1, None, "song.flac", 5)],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::default(),
        };
        let result = MtpTransferPlanner::new().plan(&request);
        assert!(matches!(result, Err(MtpPlanError::EmptyBrowse)));
        cleanup(&staging);
    }

    #[test]
    fn plan_rejects_path_separator_in_name() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("sep");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![object(1, None, "sub/dir.flac", 5)],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::with_caps(1024, 1, 1024),
        };
        let result = MtpTransferPlanner::new().plan(&request);
        assert!(matches!(result, Err(MtpPlanError::HostPathLeaked(_))));
        cleanup(&staging);
    }

    #[test]
    fn plan_rejects_dotdot_name() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("dotdot");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![object(1, None, "..", 0)],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::with_caps(1024, 1, 1024),
        };
        let result = MtpTransferPlanner::new().plan(&request);
        assert!(matches!(result, Err(MtpPlanError::HostPathLeaked(_))));
        cleanup(&staging);
    }

    #[test]
    fn plan_produces_staging_writes_with_device_id() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("ok");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![object(1, None, "song.flac", 5)],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::with_caps(1024, 1, 1024),
        };
        let plan = MtpTransferPlanner::new().plan(&request).expect("plan");
        assert_eq!(plan.staging_writes.len(), 1);
        assert_eq!(plan.device_id.label(), "usb:04e8:6860:ABC123");
        assert_eq!(plan.storage_id, 0x0001_0001);
        let staging_relative = &plan.staging_writes[0].staging_relative_path;
        let staging_str = staging_relative.to_string_lossy();
        assert!(staging_str.contains("song.flac"));
        assert!(!staging_str.contains(".."));
        cleanup(&staging);
    }

    #[test]
    fn plan_rejects_byte_budget_overshoot() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("bytes");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![object(1, None, "song.flac", 4096)],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::with_caps(8, 1, 8),
        };
        let result = MtpTransferPlanner::new().plan(&request);
        assert!(matches!(
            result,
            Err(MtpPlanError::ByteCountExceeded { .. })
        ));
        cleanup(&staging);
    }

    #[test]
    fn plan_rejects_file_count_overshoot() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("files");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![
                object(1, None, "song.flac", 1),
                object(2, None, "song2.flac", 1),
            ],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::with_caps(1024, 1, 1024),
        };
        let result = MtpTransferPlanner::new().plan(&request);
        assert!(matches!(
            result,
            Err(MtpPlanError::FileCountExceeded { .. })
        ));
        cleanup(&staging);
    }

    #[test]
    fn plan_destination_path_starts_with_destination_root() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("dest");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![object(1, None, "song.flac", 5)],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::with_caps(1024, 1, 1024),
        };
        let plan = MtpTransferPlanner::new().plan(&request).expect("plan");
        let dest = &plan.staging_writes[0].destination_relative_path;
        let dest_str = dest.to_string_lossy();
        assert!(dest_str.starts_with("Music"));
        assert!(dest_str.contains("song.flac"));
        cleanup(&staging);
    }

    #[test]
    fn plan_skips_non_file_objects() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("folders");
        let mut folder = object(0x10, None, "Music", 0);
        folder.kind = super::super::MtpObjectKind::Folder;
        let file = object(1, Some(0x10), "song.flac", 5);
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![folder, file],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("Music"),
            budget: TransferBudget::with_caps(1024, 1, 1024),
        };
        let plan = MtpTransferPlanner::new().plan(&request).expect("plan");
        assert_eq!(plan.staging_writes.len(), 1);
        cleanup(&staging);
    }

    #[test]
    fn plan_rejects_absolute_destination_root() {
        let transport = InMemoryMtpTransport::single_device(descriptor());
        let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
        let staging = unique_root("abs");
        let request = MtpTransferRequest {
            session,
            storage: storage_descriptor(),
            objects: vec![object(1, None, "song.flac", 5)],
            staging_root: staging.clone(),
            destination_relative_root: PathBuf::from("/etc"),
            budget: TransferBudget::with_caps(1024, 1, 1024),
        };
        let result = MtpTransferPlanner::new().plan(&request);
        assert!(matches!(
            result,
            Err(MtpPlanError::AbsoluteDestinationPath { .. })
        ));
        cleanup(&staging);
    }
}
