//! Tests for the MTP transfer planner.

use super::*;
use crate::device::mtp::transport::test_transport::InMemoryMtpTransport;
use crate::device::mtp::transport::MtpTransport;
use crate::device::mtp::MtpUsbDescriptor;
use std::fs;
use std::path::Path;
use std::sync::Arc;

fn descriptor() -> MtpUsbDescriptor {
    MtpUsbDescriptor::new(0x04e8, 0x6860, "ABC123").expect("descriptor")
}

fn unique_root(label: &str) -> PathBuf {
    tempfile::Builder::new()
        .prefix(&format!("tributary-mtp-{label}-"))
        .tempdir()
        .expect("create temp root")
        .keep()
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

#[test]
fn plan_rejects_parent_dir_in_destination_root() {
    let transport = InMemoryMtpTransport::single_device(descriptor());
    let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
    let staging = unique_root("dotdot");
    let request = MtpTransferRequest {
        session,
        storage: storage_descriptor(),
        objects: vec![object(1, None, "song.flac", 5)],
        staging_root: staging.clone(),
        destination_relative_root: PathBuf::from("../outside"),
        budget: TransferBudget::with_caps(1024, 1, 1024),
    };
    let result = MtpTransferPlanner::new().plan(&request);
    assert!(matches!(
        result,
        Err(MtpPlanError::AbsoluteDestinationPath { .. })
    ));
    cleanup(&staging);
}

#[test]
fn plan_rejects_missing_staging_root() {
    let transport = InMemoryMtpTransport::single_device(descriptor());
    let session = Arc::new(transport.open_session(&descriptor()).expect("session"));
    let staging = unique_root("missing");
    cleanup(&staging); // ensure the directory does not exist
    let request = MtpTransferRequest {
        session,
        storage: storage_descriptor(),
        objects: vec![object(1, None, "song.flac", 5)],
        staging_root: staging,
        destination_relative_root: PathBuf::from("Music"),
        budget: TransferBudget::with_caps(1024, 1, 1024),
    };
    let result = MtpTransferPlanner::new().plan(&request);
    assert!(matches!(
        result,
        Err(MtpPlanError::StagingRootMissing { .. })
    ));
}
