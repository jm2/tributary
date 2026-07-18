//! Retained authority for filesystem mutations beneath a library root.
//!
//! A marker string alone cannot prove that a configured path still names the
//! directory that was inspected. A [`RootAuthorityLease`] keeps both the root
//! directory and its marker open, then compares freshly opened objects against
//! those retained handles before a caller performs an authorized mutation.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use uuid::Uuid;

const ROOT_IDENTITY_FILE: &str = ".tributary-root-id";
const ROOT_IDENTITY_PREFIX: &str = "marker:v1:";
const MAX_MARKER_BYTES: u64 = 128;

/// A live, fail-closed binding between an exact path and one library root.
///
/// The retained handles prevent their filesystem object identifiers from being
/// recycled while the lease exists. On Linux, the mount ID is read from the
/// directory handle as well, so a bind mount or remount cannot pass merely by
/// exposing the same device/inode pair.
pub(super) struct RootAuthorityLease {
    token: Uuid,
    root: PathBuf,
    expected_marker: String,
    root_handle: RetainedObject,
    marker_handle: RetainedObject,
    boundary: BoundaryIdentity,
    #[cfg(windows)]
    root_ancestors: Vec<RetainedObject>,
    mount_generation: Option<u64>,
}

/// A regular descendant opened through its retained library root.
pub(super) struct BoundFile {
    lease_token: Uuid,
    path: PathBuf,
    object: RetainedObject,
    parent_guards: Vec<RetainedObject>,
}

/// A descendant directory opened through its retained library root.
pub(super) struct BoundDirectory {
    lease_token: Uuid,
    path: PathBuf,
    object: RetainedObject,
    parent_guards: Vec<RetainedObject>,
}

/// Point-in-time evidence that one exact name was absent below a bound parent.
pub(super) struct AbsenceProof {
    lease_token: Uuid,
    path: PathBuf,
    missing_path: PathBuf,
    parent: BoundDirectory,
    leaf: OsString,
}

struct RetainedObject {
    file: File,
    identity: ObjectIdentity,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObjectIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObjectIdentity {
    volume: u64,
    file_id: WindowsFileId,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowsFileId {
    Extended([u8; 16]),
    Legacy(u64),
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObjectIdentity;

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoundaryIdentity(u64);

#[cfg(all(unix, not(target_os = "linux")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoundaryIdentity {
    device: u64,
    filesystem: u64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoundaryIdentity(u64);

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoundaryIdentity;

struct OpenedRoot {
    root: RetainedObject,
    #[cfg(windows)]
    ancestors: Vec<RetainedObject>,
}

struct OpenedDescendant {
    object: RetainedObject,
    parent_guards: Vec<RetainedObject>,
}

impl fmt::Debug for RootAuthorityLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RootAuthorityLease")
            .field("root", &self.root)
            .field("mount_generation", &self.mount_generation)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for BoundFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for BoundDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundDirectory")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for AbsenceProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AbsenceProof")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl RetainedObject {
    fn new(file: File) -> io::Result<Self> {
        let identity = object_identity(&file)?;
        Ok(Self { file, identity })
    }

    fn validate_live(&self) -> io::Result<()> {
        if object_identity(&self.file)? == self.identity {
            Ok(())
        } else {
            Err(authority_changed(
                "retained filesystem object identity is no longer stable",
            ))
        }
    }
}

impl BoundFile {
    /// Clone the already-authorized file handle for handle-based parsing.
    pub(super) fn try_clone_file(&self) -> io::Result<File> {
        self.object.validate_live()?;
        self.object.file.try_clone()
    }

    /// Clone the exact file retained at authorization time for media use.
    ///
    /// Unlike [`Self::validate`], this deliberately does not require the old
    /// pathname still to name the file. A committed library rename may move a
    /// track while an already-admitted playback or receiver range request is
    /// consuming it. The retained file object remains the authority in that
    /// case: it cannot turn into a replacement subsequently installed at the
    /// old path. The root, marker, retained parent chain, and lease token must
    /// all remain valid before another handle is issued.
    pub(super) fn try_clone_for_consumption(&self, lease: &RootAuthorityLease) -> io::Result<File> {
        lease.validate_bound_token(self.lease_token)?;
        self.object.validate_live()?;
        validate_retained_objects(&self.parent_guards)?;
        lease.validate()?;
        let file = self.object.file.try_clone()?;
        lease.validate()?;
        Ok(file)
    }

    /// Verify that the exact path still names this retained regular file.
    pub(super) fn validate(&self, lease: &RootAuthorityLease) -> io::Result<()> {
        lease.validate_bound_token(self.lease_token)?;
        self.object.validate_live()?;
        validate_retained_objects(&self.parent_guards)?;
        lease.validate()?;
        let current = lease.open_descendant(&self.path, DescendantKind::RegularFile)?;
        compare_object_chains(&self.parent_guards, &current.parent_guards)?;
        if current.object.identity != self.object.identity {
            return Err(authority_changed(
                "bound regular file no longer names the retained object",
            ));
        }
        lease.validate()
    }

    /// Return whether two bounds retain the same file under the same lease.
    pub(super) fn is_same_object_as(&self, other: &Self) -> bool {
        self.lease_token == other.lease_token && self.object.identity == other.object.identity
    }
}

impl BoundDirectory {
    /// Verify that the exact path still names this retained directory.
    pub(super) fn validate(&self, lease: &RootAuthorityLease) -> io::Result<()> {
        lease.validate_bound_token(self.lease_token)?;
        self.object.validate_live()?;
        validate_retained_objects(&self.parent_guards)?;
        lease.validate()?;
        let current = lease.open_descendant(&self.path, DescendantKind::Directory)?;
        compare_object_chains(&self.parent_guards, &current.parent_guards)?;
        if current.object.identity != self.object.identity {
            return Err(authority_changed(
                "bound directory no longer names the retained object",
            ));
        }
        lease.validate()
    }

    /// Return whether two bounds retain the same directory under one lease.
    pub(super) fn is_same_object_as(&self, other: &Self) -> bool {
        self.lease_token == other.lease_token && self.object.identity == other.object.identity
    }
}

impl AbsenceProof {
    /// Recheck the same missing name through its retained authoritative parent.
    pub(super) fn validate(&self, lease: &RootAuthorityLease) -> io::Result<()> {
        lease.validate_bound_token(self.lease_token)?;
        self.parent.validate(lease)?;
        validate_absent_at(&self.parent.object.file, &self.missing_path, &self.leaf)?;
        // The retained descriptor deliberately inspects the intended parent
        // even if its pathname is displaced. Revalidate that exact parent
        // afterward so absence from a renamed-away directory cannot authorize
        // deletion for a replacement now occupying the logical path.
        self.parent.validate(lease)?;
        lease.validate()
    }
}

#[derive(Clone, Copy)]
enum DescendantKind {
    RegularFile,
    Directory,
}

impl RootAuthorityLease {
    /// Open and retain the exact root and marker currently at `root`.
    ///
    /// The final root component and marker must be real filesystem entries,
    /// not symlinks or Windows reparse points. The root must be an absolute
    /// directory path, and `expected_marker` must be a canonical version-one
    /// Tributary marker identity. Any uncertainty is returned as an error.
    pub(super) fn acquire(root: &Path, expected_marker: &str) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(invalid_input(
                "library root authority requires an absolute configured path",
            ));
        }
        let parsed_marker = parse_root_marker(expected_marker)?;
        if parsed_marker != expected_marker {
            return Err(invalid_input(
                "library root authority marker must be canonical",
            ));
        }
        let expected_marker = parsed_marker;

        let opened_root = open_configured_root(root)?;
        let boundary = boundary_identity(&opened_root.root.file)?;
        let mount_generation = root_mount_generation(&opened_root.root.file)?;
        let marker_file = open_marker(root, &opened_root.root.file)?;
        ensure_boundary(boundary, &marker_file)?;
        validate_marker_file(&marker_file, &expected_marker)?;
        let marker_handle = RetainedObject::new(marker_file)?;

        let lease = Self {
            token: Uuid::new_v4(),
            root: root.to_path_buf(),
            expected_marker,
            root_handle: opened_root.root,
            marker_handle,
            boundary,
            #[cfg(windows)]
            root_ancestors: opened_root.ancestors,
            mount_generation,
        };

        // Windows cannot open a child relative to a directory handle through
        // the standard library. A second complete validation brackets the
        // path-based marker open there; it is also useful race hardening on
        // every other platform.
        lease.validate()?;
        Ok(lease)
    }

    /// Return the exact configured path bound by this lease.
    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// Return the marker identity bound by this lease.
    pub(super) fn expected_marker(&self) -> &str {
        &self.expected_marker
    }

    /// Return the retained Linux mount ID, when the platform has one.
    pub(super) fn mount_generation(&self) -> Option<u64> {
        self.mount_generation
    }

    /// Open a real regular file through this retained authority.
    pub(super) fn open_regular_file(&self, path: &Path) -> io::Result<BoundFile> {
        self.validate()?;
        let opened = self.open_descendant(path, DescendantKind::RegularFile)?;
        self.validate()?;
        Ok(BoundFile {
            lease_token: self.token,
            path: path.to_path_buf(),
            object: opened.object,
            parent_guards: opened.parent_guards,
        })
    }

    /// Bind an exact real directory through this retained authority.
    pub(super) fn bind_directory(&self, path: &Path) -> io::Result<BoundDirectory> {
        self.validate()?;
        let opened = self.open_descendant(path, DescendantKind::Directory)?;
        self.validate()?;
        Ok(BoundDirectory {
            lease_token: self.token,
            path: path.to_path_buf(),
            object: opened.object,
            parent_guards: opened.parent_guards,
        })
    }

    /// Prove one exact descendant name absent through a retained parent.
    pub(super) fn prove_absent(&self, path: &Path) -> io::Result<AbsenceProof> {
        let components = descendant_components(&self.root, path, false)?;
        let mut parent_components = Vec::new();
        let mut parent = self.bind_directory(&self.root)?;

        for (index, component) in components.iter().enumerate() {
            let missing_path = join_components(&self.root, &{
                let mut candidate = parent_components.clone();
                candidate.push(component.clone());
                candidate
            });
            let is_leaf = index + 1 == components.len();
            if is_leaf {
                validate_absent_at(&parent.object.file, &missing_path, component)?;
                self.validate()?;
                return Ok(AbsenceProof {
                    lease_token: self.token,
                    path: path.to_path_buf(),
                    missing_path,
                    parent,
                    leaf: component.clone(),
                });
            }

            match self.bind_directory(&missing_path) {
                Ok(next_parent) => {
                    parent_components.push(component.clone());
                    parent = next_parent;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    validate_absent_at(&parent.object.file, &missing_path, component)?;
                    self.validate()?;
                    return Ok(AbsenceProof {
                        lease_token: self.token,
                        path: path.to_path_buf(),
                        missing_path,
                        parent,
                        leaf: component.clone(),
                    });
                }
                Err(error) => return Err(error),
            }
        }

        Err(invalid_input("absence proof requires a descendant name"))
    }

    fn validate_bound_token(&self, token: Uuid) -> io::Result<()> {
        if token == self.token {
            Ok(())
        } else {
            Err(authority_changed(
                "bound filesystem evidence belongs to a different root lease",
            ))
        }
    }

    fn open_descendant(&self, path: &Path, kind: DescendantKind) -> io::Result<OpenedDescendant> {
        let components =
            descendant_components(&self.root, path, matches!(kind, DescendantKind::Directory))?;
        open_descendant_from_root(self, path, &components, kind)
    }

    /// Verify that the configured path and marker still name retained objects.
    ///
    /// Callers must treat every error as loss of authority. This method never
    /// falls back to canonical-path or marker-content equality when a handle
    /// comparison or Linux mount probe fails.
    pub(super) fn validate(&self) -> io::Result<()> {
        self.root_handle.validate_live()?;
        self.marker_handle.validate_live()?;
        ensure_boundary(self.boundary, &self.marker_handle.file)?;
        #[cfg(windows)]
        validate_retained_objects(&self.root_ancestors)?;

        let current_root = open_configured_root(&self.root)?;
        let current_mount_generation = root_mount_generation(&current_root.root.file)?;
        if current_root.root.identity != self.root_handle.identity {
            return Err(authority_changed(
                "configured library root no longer names the retained directory",
            ));
        }
        if current_mount_generation != self.mount_generation {
            return Err(authority_changed(
                "configured library root no longer belongs to the retained mount",
            ));
        }
        if boundary_identity(&current_root.root.file)? != self.boundary {
            return Err(authority_changed(
                "configured library root filesystem boundary changed",
            ));
        }
        #[cfg(windows)]
        compare_object_chains(&self.root_ancestors, &current_root.ancestors)?;

        let current_marker_file = open_marker(&self.root, &current_root.root.file)?;
        ensure_boundary(self.boundary, &current_marker_file)?;
        validate_marker_file(&current_marker_file, &self.expected_marker)?;
        let current_marker = RetainedObject::new(current_marker_file)?;
        if current_marker.identity != self.marker_handle.identity {
            return Err(authority_changed(
                "library root marker no longer names the retained marker file",
            ));
        }

        let after_marker = open_configured_root(&self.root)?;
        let after_marker_mount = root_mount_generation(&after_marker.root.file)?;
        if after_marker.root.identity != self.root_handle.identity
            || after_marker_mount != self.mount_generation
            || boundary_identity(&after_marker.root.file)? != self.boundary
        {
            return Err(authority_changed(
                "configured library root changed while its marker was validated",
            ));
        }
        #[cfg(windows)]
        compare_object_chains(&self.root_ancestors, &after_marker.ancestors)?;

        Ok(())
    }
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_marker(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn authority_changed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn unsupported_platform() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "retained library-root authority is unsupported on this platform",
    )
}

fn descendant_components(root: &Path, path: &Path, allow_root: bool) -> io::Result<Vec<OsString>> {
    if !path.is_absolute() {
        return Err(invalid_input("bound descendant path must be absolute"));
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| invalid_input("bound descendant path is outside the retained library root"))?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(value) => components.push(value.to_os_string()),
            _ => {
                return Err(invalid_input(
                    "bound descendant path contains a non-normal component",
                ))
            }
        }
    }
    if components.is_empty() && !allow_root {
        return Err(invalid_input(
            "bound operation requires a path below the library root",
        ));
    }
    Ok(components)
}

fn join_components(root: &Path, components: &[OsString]) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in components {
        path.push(component);
    }
    path
}

#[cfg(unix)]
fn object_identity(file: &File) -> io::Result<ObjectIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(ObjectIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn object_identity(file: &File) -> io::Result<ObjectIdentity> {
    use std::mem::{size_of, MaybeUninit};
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO,
    };

    let handle = file.as_raw_handle() as HANDLE;
    let mut info = MaybeUninit::<FILE_ID_INFO>::zeroed();
    // SAFETY: `file` owns a live Windows handle, `info` is aligned for
    // `FILE_ID_INFO`, and the buffer length exactly matches that type. The API
    // initializes the whole structure before returning success.
    let result = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call above initialized the complete structure.
    let info = unsafe { info.assume_init() };
    if !info.FileId.Identifier.iter().all(|byte| *byte == 0) {
        if info.VolumeSerialNumber == 0 {
            return Err(invalid_marker(
                "filesystem did not provide a durable volume identity",
            ));
        }
        return Ok(ObjectIdentity {
            volume: info.VolumeSerialNumber,
            file_id: WindowsFileId::Extended(info.FileId.Identifier),
        });
    }

    // Microsoft specifies an all-zero FILE_ID_128 for filesystems without
    // 128-bit IDs. Their legacy 64-bit index remains the documented unique ID
    // on those filesystems; ReFS supplies the extended ID above, so it never
    // takes this fallback whose index is not unique on ReFS.
    let mut legacy = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `handle` is live and `legacy` is a correctly sized, aligned
    // output buffer that is fully initialized on success.
    let result = unsafe { GetFileInformationByHandle(handle, legacy.as_mut_ptr()) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call above initialized the complete structure.
    let legacy = unsafe { legacy.assume_init() };
    let legacy_id = (u64::from(legacy.nFileIndexHigh) << 32) | u64::from(legacy.nFileIndexLow);
    if legacy.dwVolumeSerialNumber == 0 || legacy_id == 0 {
        return Err(invalid_marker(
            "filesystem did not provide a durable file identity",
        ));
    }
    Ok(ObjectIdentity {
        volume: u64::from(legacy.dwVolumeSerialNumber),
        file_id: WindowsFileId::Legacy(legacy_id),
    })
}

#[cfg(not(any(unix, windows)))]
fn object_identity(_file: &File) -> io::Result<ObjectIdentity> {
    Err(unsupported_platform())
}

#[cfg(target_os = "linux")]
fn boundary_identity(file: &File) -> io::Result<BoundaryIdentity> {
    root_mount_generation(file)?
        .map(BoundaryIdentity)
        .ok_or_else(|| invalid_marker("Linux root handle has no mount identity"))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn boundary_identity(file: &File) -> io::Result<BoundaryIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let filesystem = rustix::fs::fstatvfs(file).map_err(io::Error::from)?;
    Ok(BoundaryIdentity {
        device: metadata.dev(),
        filesystem: filesystem.f_fsid,
    })
}

#[cfg(windows)]
fn boundary_identity(file: &File) -> io::Result<BoundaryIdentity> {
    object_identity(file).map(|identity| BoundaryIdentity(identity.volume))
}

#[cfg(not(any(unix, windows)))]
fn boundary_identity(_file: &File) -> io::Result<BoundaryIdentity> {
    Err(unsupported_platform())
}

fn ensure_boundary(expected: BoundaryIdentity, file: &File) -> io::Result<()> {
    if boundary_identity(file)? == expected {
        Ok(())
    } else {
        Err(authority_changed(
            "bound descendant crosses a nested mount or filesystem boundary",
        ))
    }
}

fn parse_root_marker(contents: &str) -> io::Result<String> {
    let value = contents.strip_suffix('\n').unwrap_or(contents);
    if value.is_empty() || value.contains(char::is_whitespace) {
        return Err(invalid_marker("library root marker has invalid whitespace"));
    }
    let Some(uuid) = value.strip_prefix(ROOT_IDENTITY_PREFIX) else {
        return Err(invalid_marker(
            "library root marker has an unsupported format",
        ));
    };
    let uuid = Uuid::parse_str(uuid).map_err(|error| {
        invalid_marker(format!("library root marker has an invalid UUID: {error}"))
    })?;
    Ok(format!("{ROOT_IDENTITY_PREFIX}{uuid}"))
}

fn validate_marker_file(file: &File, expected_marker: &str) -> io::Result<()> {
    let metadata = file.metadata()?;
    validate_marker_metadata(&metadata)?;
    if metadata.len() > MAX_MARKER_BYTES {
        return Err(invalid_marker("library root marker exceeds 128 bytes"));
    }

    // The descriptor is newly opened for each validation, so its offset is
    // private to this read. The independent take limit closes a concurrent
    // growth race after the metadata length check.
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_MARKER_BYTES + 1).read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_MARKER_BYTES {
        return Err(invalid_marker("library root marker exceeds 128 bytes"));
    }
    let contents = std::str::from_utf8(&contents)
        .map_err(|error| invalid_marker(format!("library root marker is not UTF-8: {error}")))?;
    if parse_root_marker(contents)? != expected_marker {
        return Err(authority_changed(
            "library root marker does not match the retained authority",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_marker_metadata(metadata: &std::fs::Metadata) -> io::Result<()> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(invalid_marker("library root marker is not a regular file"))
    }
}

#[cfg(windows)]
fn validate_marker_metadata(metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_marker(
            "library root marker must not be a reparse point",
        ));
    }
    if metadata.is_file() {
        Ok(())
    } else {
        Err(invalid_marker("library root marker is not a regular file"))
    }
}

#[cfg(not(any(unix, windows)))]
fn validate_marker_metadata(_metadata: &std::fs::Metadata) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "retained library-root authority is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn open_unix_directory_path(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(descriptor))
}

#[cfg(unix)]
fn open_unix_directory_at(parent: &File, name: &OsString) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(descriptor))
}

#[cfg(unix)]
fn open_unix_regular_at(parent: &File, name: &OsString) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let file = File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(invalid_marker("bound descendant is not a regular file"));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_windows_directory(path: &Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    // `FILE_SHARE_DELETE` is intentionally omitted. These are namespace
    // guards, not ordinary read handles: allowing delete sharing would let a
    // retained root, ancestor, or bound directory be renamed or unlinked
    // between final authority validation and the SQLite commit it authorizes.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_marker("library root must not be a reparse point"));
    }
    if !metadata.is_dir() {
        return Err(invalid_marker("library root is not a directory"));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_windows_regular(path: &Path, share_writes: bool) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    // Keep delete sharing disabled for the same reason as directory handles:
    // the marker and bound files must pin their namespace entries through the
    // database commit. `share_writes` is permitted only where an in-place
    // content update can be detected by marker/evidence revalidation.
    let share_mode = FILE_SHARE_READ | if share_writes { FILE_SHARE_WRITE } else { 0 };
    let file = OpenOptions::new()
        .read(true)
        .share_mode(share_mode)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_marker(
            "bound regular file must not be a reparse point",
        ));
    }
    if !metadata.is_file() {
        return Err(invalid_marker("bound descendant is not a regular file"));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_configured_root(path: &Path) -> io::Result<OpenedRoot> {
    // Configured aliases may contain an ancestor symlink (notably `/var` on
    // macOS). The final component itself is never followed, and every
    // descendant operation below is anchored to the retained directory fd.
    Ok(OpenedRoot {
        root: RetainedObject::new(open_unix_directory_path(path)?)?,
    })
}

#[cfg(windows)]
fn open_configured_root(path: &Path) -> io::Result<OpenedRoot> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(invalid_input(
            "configured library root contains a non-normal component",
        ));
    }
    let mut prefixes: Vec<PathBuf> = path
        .ancestors()
        .filter(|ancestor| ancestor.is_absolute())
        .map(Path::to_path_buf)
        .collect();
    prefixes.reverse();
    if prefixes.last().map(PathBuf::as_path) != Some(path) {
        return Err(invalid_input(
            "configured library root could not be decomposed safely",
        ));
    }

    let mut handles = Vec::with_capacity(prefixes.len());
    for prefix in prefixes {
        handles.push(RetainedObject::new(open_windows_directory(&prefix)?)?);
    }
    let root = handles
        .pop()
        .ok_or_else(|| invalid_input("configured library root has no absolute component"))?;
    Ok(OpenedRoot {
        root,
        ancestors: handles,
    })
}

#[cfg(not(any(unix, windows)))]
fn open_configured_root(_path: &Path) -> io::Result<OpenedRoot> {
    Err(unsupported_platform())
}

#[cfg(unix)]
fn open_marker(_root: &Path, root_file: &File) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::openat(
        root_file,
        ROOT_IDENTITY_FILE,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(descriptor))
}

#[cfg(windows)]
fn open_marker(root: &Path, _root_file: &File) -> io::Result<File> {
    open_windows_regular(&root.join(ROOT_IDENTITY_FILE), true)
}

#[cfg(not(any(unix, windows)))]
fn open_marker(_root: &Path, _root_file: &File) -> io::Result<File> {
    Err(unsupported_platform())
}

#[cfg(unix)]
fn open_descendant_from_root(
    lease: &RootAuthorityLease,
    _path: &Path,
    components: &[OsString],
    kind: DescendantKind,
) -> io::Result<OpenedDescendant> {
    if components.is_empty() {
        let file = lease.root_handle.file.try_clone()?;
        ensure_boundary(lease.boundary, &file)?;
        return Ok(OpenedDescendant {
            object: RetainedObject::new(file)?,
            parent_guards: Vec::new(),
        });
    }

    let mut parent = lease.root_handle.file.try_clone()?;
    let mut parent_guards = Vec::with_capacity(components.len().saturating_sub(1));
    for (index, component) in components.iter().enumerate() {
        let is_last = index + 1 == components.len();
        let file = if is_last && matches!(kind, DescendantKind::RegularFile) {
            open_unix_regular_at(&parent, component)?
        } else {
            open_unix_directory_at(&parent, component)?
        };
        ensure_boundary(lease.boundary, &file)?;
        if is_last {
            return Ok(OpenedDescendant {
                object: RetainedObject::new(file)?,
                parent_guards,
            });
        }
        let guard = RetainedObject::new(file)?;
        parent = guard.file.try_clone()?;
        parent_guards.push(guard);
    }
    Err(invalid_input("bound descendant has no final component"))
}

#[cfg(windows)]
fn open_descendant_from_root(
    lease: &RootAuthorityLease,
    _path: &Path,
    components: &[OsString],
    kind: DescendantKind,
) -> io::Result<OpenedDescendant> {
    if components.is_empty() {
        let file = lease.root_handle.file.try_clone()?;
        ensure_boundary(lease.boundary, &file)?;
        return Ok(OpenedDescendant {
            object: RetainedObject::new(file)?,
            parent_guards: Vec::new(),
        });
    }

    let mut current_path = lease.root.clone();
    let mut parent_guards = Vec::with_capacity(components.len().saturating_sub(1));
    for (index, component) in components.iter().enumerate() {
        current_path.push(component);
        let is_last = index + 1 == components.len();
        let file = if is_last && matches!(kind, DescendantKind::RegularFile) {
            open_windows_regular(&current_path, false)?
        } else {
            open_windows_directory(&current_path)?
        };
        ensure_boundary(lease.boundary, &file)?;
        let object = RetainedObject::new(file)?;
        if is_last {
            return Ok(OpenedDescendant {
                object,
                parent_guards,
            });
        }
        parent_guards.push(object);
    }
    Err(invalid_input("bound descendant has no final component"))
}

#[cfg(not(any(unix, windows)))]
fn open_descendant_from_root(
    _lease: &RootAuthorityLease,
    _path: &Path,
    _components: &[OsString],
    _kind: DescendantKind,
) -> io::Result<OpenedDescendant> {
    Err(unsupported_platform())
}

#[cfg(unix)]
fn validate_absent_at(parent: &File, _path: &Path, leaf: &OsString) -> io::Result<()> {
    use rustix::fs::AtFlags;

    match rustix::fs::statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(io::Error::from(error)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "bound descendant path is present",
        )),
    }
}

#[cfg(windows)]
fn validate_absent_at(_parent: &File, path: &Path, _leaf: &OsString) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "bound descendant path is present",
        )),
    }
}

#[cfg(not(any(unix, windows)))]
fn validate_absent_at(_parent: &File, _path: &Path, _leaf: &OsString) -> io::Result<()> {
    Err(unsupported_platform())
}

fn validate_retained_objects(guards: &[RetainedObject]) -> io::Result<()> {
    for guard in guards {
        guard.validate_live()?;
    }
    Ok(())
}

fn compare_object_chains(
    expected: &[RetainedObject],
    current: &[RetainedObject],
) -> io::Result<()> {
    if expected.len() == current.len()
        && expected
            .iter()
            .zip(current)
            .all(|(left, right)| left.identity == right.identity)
    {
        Ok(())
    } else {
        Err(authority_changed("bound filesystem ancestor chain changed"))
    }
}

#[cfg(target_os = "linux")]
fn root_mount_generation(root_file: &File) -> io::Result<Option<u64>> {
    use std::os::fd::AsRawFd;

    use rustix::fs::{AtFlags, StatxFlags};

    if let Ok(stat) = rustix::fs::statx(
        root_file,
        "",
        AtFlags::EMPTY_PATH | AtFlags::NO_AUTOMOUNT,
        StatxFlags::MNT_ID,
    ) {
        if stat.stx_mask & StatxFlags::MNT_ID.bits() != 0 {
            return Ok(Some(stat.stx_mnt_id));
        }
    }

    // `/proc/self/fdinfo` has exposed the mount ID associated with an open
    // descriptor since Linux 3.8. It preserves handle-based semantics on
    // kernels or sandboxes where STATX_MNT_ID is unavailable.
    let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", root_file.as_raw_fd()))?;
    parse_fdinfo_mount_generation(&fdinfo).map(Some)
}

#[cfg(target_os = "linux")]
fn parse_fdinfo_mount_generation(contents: &str) -> io::Result<u64> {
    let mut values = contents.lines().filter_map(|line| {
        line.strip_prefix("mnt_id:")
            .map(str::trim)
            .map(str::parse::<u64>)
    });
    let value = values
        .next()
        .ok_or_else(|| invalid_marker("descriptor information has no mount ID"))?
        .map_err(|error| invalid_marker(format!("descriptor mount ID is invalid: {error}")))?;
    if values.next().is_some() {
        return Err(invalid_marker(
            "descriptor information has multiple mount IDs",
        ));
    }
    Ok(value)
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn root_mount_generation(_root_file: &File) -> io::Result<Option<u64>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const MARKER: &str = "marker:v1:12345678-1234-5678-9234-567812345678";
    const OTHER_MARKER: &str = "marker:v1:87654321-4321-8765-a321-876543218765";

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "tributary-root-authority-{label}-{}",
                Uuid::new_v4()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_marker(&self, marker: &str) {
            fs::write(self.0.join(ROOT_IDENTITY_FILE), format!("{marker}\n"))
                .expect("write root marker");
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lease_retains_and_validates_exact_root_authority() {
        let directory = TestDirectory::new("valid");
        directory.write_marker(MARKER);

        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");

        assert_eq!(lease.root(), directory.path());
        assert_eq!(lease.expected_marker(), MARKER);
        #[cfg(target_os = "linux")]
        assert!(lease.mount_generation().is_some());
        lease.validate().expect("validate lease");
    }

    #[test]
    fn wrong_or_missing_marker_fails_closed() {
        let missing = TestDirectory::new("missing-marker");
        assert!(RootAuthorityLease::acquire(missing.path(), MARKER).is_err());

        let wrong = TestDirectory::new("wrong-marker");
        wrong.write_marker(OTHER_MARKER);
        assert!(RootAuthorityLease::acquire(wrong.path(), MARKER).is_err());
    }

    #[test]
    fn malformed_expected_marker_fails_closed() {
        let directory = TestDirectory::new("malformed-expected");
        directory.write_marker(MARKER);

        assert!(RootAuthorityLease::acquire(directory.path(), "not-a-marker").is_err());
        assert!(RootAuthorityLease::acquire(directory.path(), &format!("{MARKER}\n")).is_err());
    }

    #[test]
    fn in_place_marker_change_invalidates_lease() {
        let directory = TestDirectory::new("changed-marker");
        directory.write_marker(MARKER);
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");

        directory.write_marker(OTHER_MARKER);

        assert!(lease.validate().is_err());
    }

    #[test]
    fn marker_replacement_is_blocked_or_invalidates_the_lease() {
        let directory = TestDirectory::new("replaced-marker");
        directory.write_marker(MARKER);
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");

        if fs::remove_file(directory.path().join(ROOT_IDENTITY_FILE)).is_err() {
            // Some platforms deny replacement while the retained handle is
            // open. That is an equally strong pin: the configured authority
            // cannot be swapped during the mutation.
            lease
                .validate()
                .expect("blocked replacement keeps authority");
            return;
        }
        directory.write_marker(MARKER);

        assert!(lease.validate().is_err());
    }

    #[test]
    fn root_replacement_is_blocked_or_invalidates_the_lease() {
        let directory = TestDirectory::new("replaced-root");
        directory.write_marker(MARKER);
        let replacement = TestDirectory::new("replacement-root");
        replacement.write_marker(MARKER);
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");
        let displaced = directory.path().with_extension("displaced");

        if fs::rename(directory.path(), &displaced).is_err() {
            lease
                .validate()
                .expect("blocked replacement keeps authority");
            return;
        }
        fs::rename(replacement.path(), directory.path()).expect("install replacement root");

        assert!(lease.validate().is_err());

        drop(lease);
        fs::rename(directory.path(), replacement.path()).expect("restore replacement path");
        fs::rename(&displaced, directory.path()).expect("restore retained root path");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_and_marker_fail_closed() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("symlink-root-target");
        directory.write_marker(MARKER);
        let root_link = directory.path().with_extension("root-link");
        symlink(directory.path(), &root_link).expect("create root symlink");
        assert!(RootAuthorityLease::acquire(&root_link, MARKER).is_err());
        fs::remove_file(&root_link).expect("remove root symlink");

        fs::remove_file(directory.path().join(ROOT_IDENTITY_FILE)).expect("remove marker");
        let marker_target = directory.path().join("marker-target");
        fs::write(&marker_target, format!("{MARKER}\n")).expect("write marker target");
        symlink(&marker_target, directory.path().join(ROOT_IDENTITY_FILE))
            .expect("create marker symlink");
        assert!(RootAuthorityLease::acquire(directory.path(), MARKER).is_err());
    }

    #[test]
    fn relative_root_and_non_directory_fail_closed() {
        assert!(RootAuthorityLease::acquire(Path::new("relative-root"), MARKER).is_err());

        let directory = TestDirectory::new("root-file");
        let file = directory.path().join("not-a-directory");
        fs::write(&file, b"not a directory").expect("write root file");
        assert!(RootAuthorityLease::acquire(&file, MARKER).is_err());
    }

    #[test]
    fn bound_file_and_directory_validate_through_retained_root() {
        let directory = TestDirectory::new("bound-descendants");
        directory.write_marker(MARKER);
        let album = directory.path().join("album");
        fs::create_dir(&album).expect("create album");
        let song = album.join("song.flac");
        fs::write(&song, b"audio bytes").expect("write song");
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");

        let bound_album = lease.bind_directory(&album).expect("bind album");
        let bound_song = lease.open_regular_file(&song).expect("bind song");
        let mut cloned = bound_song.try_clone_file().expect("clone bound song");
        let mut contents = Vec::new();
        cloned.read_to_end(&mut contents).expect("read bound song");

        assert_eq!(contents, b"audio bytes");
        bound_album.validate(&lease).expect("validate album");
        bound_song.validate(&lease).expect("validate song");
        assert!(bound_album.is_same_object_as(&bound_album));
        assert!(bound_song.is_same_object_as(&bound_song));
    }

    #[test]
    fn bounds_cannot_be_validated_by_another_lease() {
        let first = TestDirectory::new("bound-first-lease");
        first.write_marker(MARKER);
        let song = first.path().join("song.flac");
        fs::write(&song, b"audio").expect("write song");
        let first_lease = RootAuthorityLease::acquire(first.path(), MARKER).expect("first lease");
        let bound = first_lease
            .open_regular_file(&song)
            .expect("bind first song");

        let second = TestDirectory::new("bound-second-lease");
        second.write_marker(MARKER);
        let second_lease =
            RootAuthorityLease::acquire(second.path(), MARKER).expect("second lease");

        assert!(bound.validate(&second_lease).is_err());
    }

    #[test]
    fn absence_proof_tracks_leaf_and_missing_ancestor() {
        let directory = TestDirectory::new("absence");
        directory.write_marker(MARKER);
        let album = directory.path().join("album");
        fs::create_dir(&album).expect("create album");
        let missing_song = album.join("missing.flac");
        let missing_subtree_song = directory.path().join("gone-album").join("missing.flac");
        let replace_album = directory.path().join("replace-album");
        fs::create_dir(&replace_album).expect("create replaceable album");
        let replaced_parent_song = replace_album.join("missing.flac");
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");

        let leaf = lease
            .prove_absent(&missing_song)
            .expect("prove missing leaf");
        leaf.validate(&lease).expect("validate missing leaf");
        let subtree = lease
            .prove_absent(&missing_subtree_song)
            .expect("prove missing ancestor");
        subtree.validate(&lease).expect("validate missing ancestor");
        let replaced_parent = lease
            .prove_absent(&replaced_parent_song)
            .expect("prove absence beneath replaceable parent");

        fs::write(&missing_song, b"appeared").expect("create missing leaf");
        fs::create_dir(directory.path().join("gone-album")).expect("create missing ancestor");
        let displaced_album = directory.path().join("displaced-album");
        if let Err(error) = fs::rename(&replace_album, &displaced_album) {
            #[cfg(windows)]
            if error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(32) {
                assert!(leaf.validate(&lease).is_err());
                assert!(subtree.validate(&lease).is_err());
                replaced_parent
                    .validate(&lease)
                    .expect("Windows retained parent prevents replacement");
                assert!(lease.prove_absent(&missing_song).is_err());
                return;
            }
            panic!("displace absence-proof parent: {error}");
        }
        fs::create_dir(&replace_album).expect("replace absence-proof parent");
        assert!(leaf.validate(&lease).is_err());
        assert!(subtree.validate(&lease).is_err());
        assert!(replaced_parent.validate(&lease).is_err());
        assert!(lease.prove_absent(&missing_song).is_err());
    }

    #[test]
    fn escape_and_non_directory_components_fail_closed() {
        let directory = TestDirectory::new("escape");
        directory.write_marker(MARKER);
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");
        let outside = directory
            .path()
            .parent()
            .expect("test parent")
            .join("outside.flac");
        let escaping = directory.path().join("..").join("outside.flac");
        let ordinary_file = directory.path().join("ordinary");
        fs::write(&ordinary_file, b"file").expect("write ordinary file");

        assert!(lease.open_regular_file(&outside).is_err());
        assert!(lease.open_regular_file(&escaping).is_err());
        assert!(lease
            .open_regular_file(&ordinary_file.join("child.flac"))
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_descendant_components_fail_closed() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("descendant-symlink");
        directory.write_marker(MARKER);
        let real_album = directory.path().join("real-album");
        fs::create_dir(&real_album).expect("create real album");
        let song = real_album.join("song.flac");
        fs::write(&song, b"audio").expect("write song");
        let linked_album = directory.path().join("linked-album");
        symlink(&real_album, &linked_album).expect("link album");
        let linked_file = directory.path().join("linked.flac");
        symlink(&song, &linked_file).expect("link song");
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");

        assert!(lease
            .open_regular_file(&linked_album.join("song.flac"))
            .is_err());
        assert!(lease.open_regular_file(&linked_file).is_err());
        assert!(lease.bind_directory(&linked_album).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn configured_ancestor_symlink_is_allowed_but_final_symlink_is_not() {
        use std::os::unix::fs::symlink;

        let container = TestDirectory::new("configured-alias");
        let real_parent = container.path().join("real-parent");
        let real_root = real_parent.join("library");
        fs::create_dir(&real_parent).expect("create real parent");
        fs::create_dir(&real_root).expect("create real root");
        fs::write(real_root.join(ROOT_IDENTITY_FILE), format!("{MARKER}\n")).expect("write marker");
        let alias = container.path().join("alias");
        symlink(&real_parent, &alias).expect("create parent alias");
        let aliased_root = alias.join("library");

        RootAuthorityLease::acquire(&aliased_root, MARKER).expect("acquire through parent alias");
        let final_alias = container.path().join("final-alias");
        symlink(&real_root, &final_alias).expect("create final alias");
        assert!(RootAuthorityLease::acquire(&final_alias, MARKER).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_bound_descendants_invalidates_them() {
        let directory = TestDirectory::new("replace-bound");
        directory.write_marker(MARKER);
        let album = directory.path().join("album");
        fs::create_dir(&album).expect("create album");
        let song = album.join("song.flac");
        fs::write(&song, b"original").expect("write song");
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");
        let bound_album = lease.bind_directory(&album).expect("bind album");
        let bound_song = lease.open_regular_file(&song).expect("bind song");

        let old_song = album.join("old-song.flac");
        fs::rename(&song, &old_song).expect("move retained song");
        fs::write(&song, b"replacement").expect("replace song");
        assert!(bound_song.validate(&lease).is_err());

        let old_album = directory.path().join("old-album");
        fs::rename(&album, &old_album).expect("move retained album");
        fs::create_dir(&album).expect("replace album");
        assert!(bound_album.validate(&lease).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_parent_is_rejected_even_when_the_final_file_is_hard_linked() {
        let directory = TestDirectory::new("replace-bound-parent");
        directory.write_marker(MARKER);
        let album = directory.path().join("album");
        fs::create_dir(&album).expect("create album");
        let song = album.join("song.flac");
        fs::write(&song, b"original").expect("write song");
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");
        let bound_song = lease.open_regular_file(&song).expect("bind song");

        let displaced_album = directory.path().join("displaced-album");
        fs::rename(&album, &displaced_album).expect("displace bound parent");
        fs::create_dir(&album).expect("replace bound parent");
        fs::hard_link(displaced_album.join("song.flac"), &song)
            .expect("link the same final object through replacement parent");

        assert!(bound_song.validate(&lease).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_lease_pins_root_and_marker_namespace_until_drop() {
        let directory = TestDirectory::new("windows-lease-namespace-pin");
        directory.write_marker(MARKER);
        let marker = directory.path().join(ROOT_IDENTITY_FILE);
        let displaced_marker = directory.path().join("displaced-marker");
        let displaced_root = directory.path().with_extension("displaced-root");
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");

        assert!(fs::remove_file(&marker).is_err());
        assert!(fs::rename(directory.path(), &displaced_root).is_err());

        drop(lease);
        fs::rename(&marker, &displaced_marker).expect("rename marker after lease drop");
        fs::rename(&displaced_marker, &marker).expect("restore marker");
        fs::rename(directory.path(), &displaced_root).expect("rename root after lease drop");
        fs::rename(&displaced_root, directory.path()).expect("restore root");
    }

    #[cfg(windows)]
    #[test]
    fn windows_bounds_pin_file_and_directory_namespace_until_drop() {
        let directory = TestDirectory::new("windows-bound-namespace-pin");
        directory.write_marker(MARKER);
        let album = directory.path().join("album");
        fs::create_dir(&album).expect("create album");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"audio").expect("write song");
        let moved_song = directory.path().join("moved.flac");
        let moved_album = directory.path().join("moved-album");
        let lease = RootAuthorityLease::acquire(directory.path(), MARKER).expect("acquire lease");

        let bound_song = lease.open_regular_file(&song).expect("bind song");
        assert!(fs::remove_file(&song).is_err());
        drop(bound_song);
        fs::rename(&song, &moved_song).expect("rename song after bound handle drop");
        fs::rename(&moved_song, &song).expect("restore song");

        let bound_album = lease.bind_directory(&album).expect("bind album");
        assert!(fs::rename(&album, &moved_album).is_err());
        drop(bound_album);
        fs::rename(&album, &moved_album).expect("rename album after bound handle drop");
        fs::rename(&moved_album, &album).expect("restore album");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nested_mount_boundary_is_rejected_by_handle_identity() {
        let directory = TestDirectory::new("boundary");
        let root = open_unix_directory_path(directory.path()).expect("open root");
        let proc_root = open_unix_directory_path(Path::new("/proc")).expect("open proc");
        let boundary = boundary_identity(&root).expect("root boundary");

        assert!(ensure_boundary(boundary, &root).is_ok());
        assert!(ensure_boundary(boundary, &proc_root).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fdinfo_mount_generation_parser_is_strict() {
        assert_eq!(
            parse_fdinfo_mount_generation("pos:\t0\nflags:\t0100000\nmnt_id:\t42\n")
                .expect("parse mount generation"),
            42
        );
        assert!(parse_fdinfo_mount_generation("pos:\t0\n").is_err());
        assert!(parse_fdinfo_mount_generation("mnt_id:\tnot-a-number\n").is_err());
        assert!(parse_fdinfo_mount_generation("mnt_id:\t1\nmnt_id:\t2\n").is_err());
    }
}
