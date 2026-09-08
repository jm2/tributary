//! Retained authority for filesystem access beneath an exact root.
//!
//! A marker string alone cannot prove that a configured path still names the
//! directory that was inspected. A [`RootAuthorityLease`] keeps both the root
//! directory and its marker open, then compares freshly opened objects against
//! those retained handles before a caller performs an authorized mutation.
//! [`MountedRootAuthority`] applies the same handle, filesystem-boundary, and
//! mount-generation checks to an ephemeral mounted root without requiring the
//! removable filesystem to contain an application marker.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

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

/// Live authority for one exact mounted root without an on-disk marker.
///
/// This is intentionally weaker than [`RootAuthorityLease`] as durable
/// identity evidence: the source lifecycle must replace its session epoch on
/// relocation, pre-unmount, or removal. While one session is live, retained
/// root and descendant handles prevent path replacement, symlink traversal,
/// or a nested filesystem from retargeting an admitted file.
pub struct MountedRootAuthority {
    token: Uuid,
    root: PathBuf,
    root_handle: RetainedObject,
    boundary: BoundaryIdentity,
    #[cfg(windows)]
    root_ancestors: Vec<RetainedObject>,
    mount_generation: Option<u64>,
}

/// A write-parent directory retained from staging through commit.
///
/// Publication must never consult the mutable path namespace at the moment
/// of the rename: a parent directory or mount root replaced between
/// validation and publish could otherwise redirect the write outside the
/// audited subtree. The write authority therefore retains this exact parent
/// directory object when a staged file is created and publishes through its
/// handle. Only the final parent object is retained; the short-lived
/// traversal guards are dropped so a live staged write never pins ancestor
/// renames or unmounts.
pub(super) struct RetainedWriteParent {
    lease_token: Uuid,
    directory: RetainedObject,
}

impl fmt::Debug for RetainedWriteParent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedWriteParent")
            .field("lease_token", &self.lease_token)
            .finish_non_exhaustive()
    }
}

impl RetainedWriteParent {
    /// Verify the retained parent still names the same directory object and
    /// still belongs to the same retained root authority.
    pub(super) fn validate_with(&self, authority: &MountedRootAuthority) -> io::Result<()> {
        validate_bound_token(authority, self.lease_token)?;
        self.directory.validate_live()?;
        authority.validate()
    }

    /// The retained parent directory handle. Publication is anchored to this
    /// exact object; it must never be replaced by a fresh path-based open.
    pub(super) fn handle(&self) -> &File {
        &self.directory.file
    }
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
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObjectIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObjectIdentity {
    volume: u64,
    file_id: WindowsFileId,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum WindowsFileId {
    Extended([u8; 16]),
    Legacy(u64),
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ObjectIdentity;

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
    /// Short-lived no-delete handle for the exact Windows mount-point
    /// namespace entry. Mounted authorities deliberately do not move this
    /// into their long-lived state, but validation and path-based descendant
    /// traversal retain the surrounding `OpenedRoot` until they finish.
    #[cfg(windows)]
    _namespace_guard: Option<File>,
}

struct OpenedDescendant {
    object: RetainedObject,
    parent_guards: Vec<RetainedObject>,
}

/// Internal common view of the two retained-root authority forms.
trait RootBinding {
    fn token(&self) -> Uuid;
    fn root(&self) -> &Path;
    fn root_handle(&self) -> &RetainedObject;
    fn boundary(&self) -> BoundaryIdentity;
    fn mount_generation(&self) -> Option<u64>;
    fn unmount_friendly_sharing(&self) -> bool;

    #[cfg(windows)]
    fn root_ancestors(&self) -> &[RetainedObject];

    fn validate_binding(&self) -> io::Result<()>;
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

impl fmt::Debug for MountedRootAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedRootAuthority")
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
    pub(crate) fn try_clone_file(&self) -> io::Result<File> {
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
        self.try_clone_for_root_consumption(lease)
    }

    /// Clone one exact mounted file while its retained root remains current.
    pub(super) fn try_clone_for_mounted_consumption(
        &self,
        authority: &MountedRootAuthority,
    ) -> io::Result<File> {
        self.try_clone_for_root_consumption(authority)
    }

    fn try_clone_for_root_consumption(&self, authority: &impl RootBinding) -> io::Result<File> {
        validate_bound_token(authority, self.lease_token)?;
        self.object.validate_live()?;
        validate_retained_objects(&self.parent_guards)?;
        authority.validate_binding()?;
        let file = self.object.file.try_clone()?;
        authority.validate_binding()?;
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

impl RootBinding for RootAuthorityLease {
    fn token(&self) -> Uuid {
        self.token
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn root_handle(&self) -> &RetainedObject {
        &self.root_handle
    }

    fn boundary(&self) -> BoundaryIdentity {
        self.boundary
    }

    fn mount_generation(&self) -> Option<u64> {
        self.mount_generation
    }

    fn unmount_friendly_sharing(&self) -> bool {
        false
    }

    #[cfg(windows)]
    fn root_ancestors(&self) -> &[RetainedObject] {
        &self.root_ancestors
    }

    fn validate_binding(&self) -> io::Result<()> {
        self.validate()
    }
}

impl RootBinding for MountedRootAuthority {
    fn token(&self) -> Uuid {
        self.token
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn root_handle(&self) -> &RetainedObject {
        &self.root_handle
    }

    fn boundary(&self) -> BoundaryIdentity {
        self.boundary
    }

    fn mount_generation(&self) -> Option<u64> {
        self.mount_generation
    }

    fn unmount_friendly_sharing(&self) -> bool {
        true
    }

    #[cfg(windows)]
    fn root_ancestors(&self) -> &[RetainedObject] {
        &self.root_ancestors
    }

    fn validate_binding(&self) -> io::Result<()> {
        self.validate()
    }
}

impl MountedRootAuthority {
    /// Open and retain the exact native mounted root currently at `root`.
    ///
    /// No marker is required or created. The caller's source lifecycle is
    /// responsible for replacing the owning session on relocation or removal.
    pub(crate) fn acquire(root: &Path) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(invalid_input(
                "mounted root authority requires an absolute native path",
            ));
        }

        let opened_root = open_configured_root(root, true, true)?;
        let boundary = boundary_identity(&opened_root.root.file)?;
        let mount_generation = root_mount_generation(&opened_root.root.file)?;
        let authority = Self {
            token: Uuid::new_v4(),
            root: root.to_path_buf(),
            root_handle: opened_root.root,
            boundary,
            #[cfg(windows)]
            root_ancestors: opened_root.ancestors,
            mount_generation,
        };
        authority.validate()?;
        Ok(authority)
    }

    /// Return the exact native mount path retained by this authority.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Open a real regular file using only normal components relative to the
    /// retained mounted root.
    pub(super) fn open_relative_regular_file(&self, relative: &Path) -> io::Result<BoundFile> {
        let components = strict_relative_components(relative)?;
        self.validate()?;
        let path = join_components(&self.root, &components);
        let opened =
            open_descendant_from_root(self, &path, &components, DescendantKind::RegularFile)?;
        self.validate()?;
        #[cfg(windows)]
        let OpenedDescendant {
            object,
            parent_guards: _,
        } = opened;
        #[cfg(not(windows))]
        let OpenedDescendant {
            object,
            parent_guards,
        } = opened;
        // Windows traversal guards deliberately deny delete sharing while the
        // absolute-path fallback opens the complete descendant chain. Once the
        // exact final file is retained, drop those short-lived namespace pins
        // so playback cannot block unmount/eject.
        #[cfg(windows)]
        let parent_guards = Vec::new();
        Ok(BoundFile {
            lease_token: self.token,
            path,
            object,
            parent_guards,
        })
    }

    /// Open a real directory using only normal components relative to the
    /// retained mounted root. Used by the write authority to bind a parent
    /// directory before staging a temporary file in it.
    pub(super) fn open_relative_directory(&self, relative: &Path) -> io::Result<BoundDirectory> {
        let components = strict_relative_components(relative)?;
        self.validate()?;
        let path = join_components(&self.root, &components);
        let opened =
            open_descendant_from_root(self, &path, &components, DescendantKind::Directory)?;
        self.validate()?;
        Ok(BoundDirectory {
            lease_token: self.token,
            path,
            object: opened.object,
            parent_guards: opened.parent_guards,
        })
    }

    /// Bind the exact retained root itself as a directory. Used by the write
    /// authority when a staged file is published directly beneath the root.
    pub(super) fn bind_root_directory(&self) -> io::Result<BoundDirectory> {
        self.validate()?;
        let file = self.root_handle.file.try_clone()?;
        ensure_boundary(self.boundary, &file)?;
        Ok(BoundDirectory {
            lease_token: self.token,
            path: self.root.clone(),
            object: RetainedObject::new(file)?,
            parent_guards: Vec::new(),
        })
    }

    /// Retain the write-parent directory for a staged write whose final path
    /// is `relative` beneath this root. An empty `relative` retains the root
    /// itself. Only the final directory object is retained: the traversal
    /// guards are dropped so a held staged write never pins ancestor
    /// renames or unmounts.
    pub(super) fn retain_write_parent(&self, relative: &Path) -> io::Result<RetainedWriteParent> {
        if relative.as_os_str().is_empty() {
            let bound = self.bind_root_directory()?;
            return Ok(RetainedWriteParent {
                lease_token: bound.lease_token,
                directory: bound.object,
            });
        }
        let bound = self.open_relative_directory(relative)?;
        Ok(RetainedWriteParent {
            lease_token: bound.lease_token,
            directory: bound.object,
        })
    }

    /// Rename one leaf to another leaf inside the retained write parent.
    ///
    /// On Unix the rename is issued relative to the retained parent handle,
    /// so a parent or mount replacement between validation and publish
    /// cannot redirect it: the rename lands in the exact audited directory
    /// object or fails. When `no_replace` is set the publication fails if
    /// the final leaf already exists; the platform-native no-replace rename
    /// is tried first, then a link-based publish, and finally an
    /// exclusive-reservation publish for filesystems that offer neither. On
    /// Windows the no-replace publish mirrors that cascade with safe path
    /// operations — a hard-link publish first, then the exclusive
    /// reservation — and the retained parent identity is revalidated
    /// immediately before and after.
    pub(super) fn rename_within_directory(
        &self,
        parent: &RetainedWriteParent,
        from_leaf: &OsStr,
        from_absolute: &Path,
        to_leaf: &OsStr,
        to_absolute: &Path,
        no_replace: bool,
    ) -> io::Result<()> {
        validate_leaf_name(from_leaf)?;
        validate_leaf_name(to_leaf)?;
        parent.validate_with(self)?;
        let outcome = if no_replace {
            rename_no_replace_within_parent(
                parent.handle(),
                from_leaf,
                from_absolute,
                to_leaf,
                to_absolute,
            )
        } else {
            rename_within_parent(
                parent.handle(),
                from_leaf,
                from_absolute,
                to_leaf,
                to_absolute,
            )
        };
        outcome?;
        parent.validate_with(self)
    }

    /// Remove the regular file at `relative` beneath the retained root.
    ///
    /// On Unix the removal is anchored to the retained root: the parent
    /// directory is walked no-follow from the retained root handle and the
    /// leaf is unlinked through its handle, so an intermediate symlink or
    /// replaced directory cannot redirect the removal. The final leaf must
    /// not be a directory; a symlink is removed as a link.
    pub(super) fn remove_regular_file_within(&self, relative: &Path) -> io::Result<()> {
        let components = strict_relative_components(relative)?;
        self.validate()?;
        #[cfg(unix)]
        {
            use rustix::fs::AtFlags;

            let leaf = components.last().expect("non-empty components").clone();
            let parent = self.retain_write_parent_directory(&components)?;
            let stat = match rustix::fs::statat(parent.handle(), &leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(error) => return Err(io::Error::from(error)),
            };
            if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to remove a directory through remove_relative_file",
                ));
            }
            rustix::fs::unlinkat(parent.handle(), &leaf, AtFlags::empty())
                .map_err(io::Error::from)?;
        }
        #[cfg(windows)]
        {
            let final_path = join_components(&self.root, &components);
            std::fs::remove_file(&final_path)?;
        }
        #[cfg(not(any(unix, windows)))]
        {
            return Err(unsupported_platform());
        }
        self.validate()
    }

    /// Remove the empty directory at `relative` beneath the retained root,
    /// anchored to the retained root exactly like
    /// [`Self::remove_regular_file_within`].
    pub(super) fn remove_directory_within(&self, relative: &Path) -> io::Result<()> {
        let components = strict_relative_components(relative)?;
        self.validate()?;
        #[cfg(unix)]
        {
            use rustix::fs::AtFlags;

            let leaf = components.last().expect("non-empty components").clone();
            let parent = self.retain_write_parent_directory(&components)?;
            let stat = match rustix::fs::statat(parent.handle(), &leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(error) => return Err(io::Error::from(error)),
            };
            if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to remove a non-directory through remove_relative_directory",
                ));
            }
            rustix::fs::unlinkat(parent.handle(), &leaf, AtFlags::REMOVEDIR)
                .map_err(io::Error::from)?;
        }
        #[cfg(windows)]
        {
            let final_path = join_components(&self.root, &components);
            std::fs::remove_dir(&final_path)?;
        }
        #[cfg(not(any(unix, windows)))]
        {
            return Err(unsupported_platform());
        }
        self.validate()
    }

    /// Create every component of `components` as a directory beneath the
    /// retained root, walking and creating each level no-follow from the
    /// retained root handle on Unix. An existing directory is tolerated
    /// only when it is a real directory, never a symlink.
    pub(super) fn create_directories_within(&self, components: &[OsString]) -> io::Result<()> {
        if components.is_empty() {
            return Err(invalid_input(
                "directory creation requires a path below the mounted root",
            ));
        }
        self.validate()?;
        #[cfg(unix)]
        {
            let mut current = self.root_handle.file.try_clone()?;
            for component in components {
                current = ensure_directory_component(&current, component)?;
                ensure_boundary(self.boundary, &current)?;
            }
        }
        #[cfg(windows)]
        {
            create_directory_tree_by_path(&self.root, components)?;
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = components;
            return Err(unsupported_platform());
        }
        self.validate()
    }

    /// Open the parent directory of the final component of `components` as a
    /// retained write parent. The root is used when only a leaf remains.
    fn retain_write_parent_directory(
        &self,
        components: &[OsString],
    ) -> io::Result<RetainedWriteParent> {
        let mut parent_relative = PathBuf::new();
        for component in &components[..components.len() - 1] {
            parent_relative.push(component);
        }
        self.retain_write_parent(&parent_relative)
    }

    /// Return the unique token identifying this exact authority instance.
    /// Other modules use this to reject evidence held by a different root.
    pub(super) fn token(&self) -> Uuid {
        self.token
    }

    /// Open a regular file beneath the root and run `body` with a cloned
    /// handle. The handle is revalidated before the call so a remount between
    /// the call site and `body` produces a fail-closed error rather than
    /// surfacing a stale file descriptor.
    ///
    /// The closure receives a `File` cloned from the retained handle; the
    /// caller is responsible for any I/O and error propagation through its
    /// own `Result` type. The bound is dropped when the closure returns, so
    /// the caller must clone the handle again if it needs to outlive the
    /// closure. The retained root and parent-chain handles are not exposed.
    pub(crate) fn with_relative_file<F, T>(&self, relative: &Path, body: F) -> io::Result<T>
    where
        F: FnOnce(File) -> io::Result<T>,
    {
        let bound = self.open_relative_regular_file(relative)?;
        let file = bound.try_clone_file()?;
        body(file)
    }

    /// Reopen the mount path and verify the exact retained root, filesystem
    /// boundary, ancestor chain, and platform mount generation.
    pub(crate) fn validate(&self) -> io::Result<()> {
        validate_root_binding(self)
    }

    /// Open and retain one exact accepted descendant as a mutation target.
    ///
    /// The descendant must be a real regular file named by strict mount-
    /// relative components. The retained mount authority, its ancestor chain,
    /// and the exact file handle stay bound for the target's entire lifetime;
    /// a commit section revalidates all of them immediately before the atomic
    /// replacement is allowed to proceed.
    pub(crate) fn open_mutation_target(
        self: &Arc<Self>,
        relative: &Path,
    ) -> io::Result<MountedMutationTarget> {
        let bound = self.open_relative_regular_file(relative)?;
        Ok(MountedMutationTarget {
            authority: Arc::clone(self),
            relative_path: bound
                .path
                .strip_prefix(&self.root)
                .map_err(|_| {
                    invalid_input("mutation target path must descend from its mounted root")
                })?
                .to_path_buf(),
            path: bound.path.clone(),
            file: Mutex::new(bound),
        })
    }
}

/// Typed retained authority to atomically replace one exact regular file
/// beneath a live mounted root.
///
/// A path alone cannot prove that the file a user selected is still the file
/// a later write would replace. A [`MountedMutationTarget`] retains the
/// mounted root authority and the exact accepted file object, serializes its
/// commit sections, and requires — immediately before each replacement — that
/// the mount, the retained ancestry, and the exact pathname still name the
/// file the authority admitted. Any uncertainty fails the commit closed and
/// leaves the filesystem untouched.
pub struct MountedMutationTarget {
    authority: Arc<MountedRootAuthority>,
    relative_path: PathBuf,
    path: PathBuf,
    file: Mutex<BoundFile>,
}

impl fmt::Debug for MountedMutationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedMutationTarget")
            .finish_non_exhaustive()
    }
}

impl MountedMutationTarget {
    /// Return the mount-relative pathname this target was admitted under.
    ///
    /// The native mount location is deliberately absent: only the mount-
    /// relative identity the source catalogue published may cross a boundary.
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    /// Return the exact native pathname an authorized replacement renames
    /// over. This is private machinery for the local tag writer and must not
    /// be logged or formatted.
    pub(crate) fn replacement_path(&self) -> &Path {
        &self.path
    }

    /// Return the admitted leaf name, relative to the retained parent
    /// directory. Private machinery for the retained-parent commit.
    fn relative_leaf(&self) -> io::Result<OsString> {
        self.relative_path
            .file_name()
            .map(|name| name.to_os_string())
            .ok_or_else(|| invalid_input("mutation target has no leaf file name"))
    }

    /// Revalidate the retained mount binding and exact file object.
    ///
    /// Every error is a loss of authority. This reopens the mount path and
    /// compares it against the retained root, boundary, ancestor chain, mount
    /// generation, and exact file identity; it never falls back to looser
    /// evidence such as canonical-path equality.
    pub(crate) fn validate(&self) -> io::Result<()> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("mutation target commit section is unavailable"))?;
        validate_mounted_bound(self.authority.as_ref(), &file)
    }

    /// Begin one serialized commit section over this exact target.
    ///
    /// The retained mount binding and exact file object are revalidated
    /// before the section starts and the section holds the target's commit
    /// lock until the guard is dropped, so overlapping commits observe either
    /// the pre-commit or re-anchored object — never a mixed one.
    pub(crate) fn begin_commit(&self) -> io::Result<MountedMutationCommit<'_>> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("mutation target commit section is unavailable"))?;
        validate_mounted_bound(self.authority.as_ref(), &file)?;
        Ok(MountedMutationCommit { target: self, file })
    }

    /// The retained directory that must contain the replacement — the exact
    /// parent directory handle plus the admitted leaf name — for read-only
    /// capability rehearsals.
    ///
    /// This is the same directory object the commit section resolves (the
    /// retained parent guard, or the retained root for a mount-top target),
    /// so a rehearsal run through it exercises the true directory even when
    /// an ancestor pathname was displaced after admission and an impostor
    /// directory now occupies the old name. A pathname-based rehearsal
    /// would instead probe inside whatever occupies the path now — outside
    /// the admitted mount — or reject a target the anchored writer could
    /// safely update.
    ///
    /// The handle is a clone; the target's retained evidence is untouched.
    #[cfg(unix)]
    pub(crate) fn retained_directory_handle(&self) -> io::Result<(File, OsString)> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("mutation target commit section is unavailable"))?;
        let parent = if let Some(guard) = file.parent_guards.last() {
            guard
        } else {
            self.authority.root_handle()
        };
        parent.validate_live()?;
        Ok((parent.file.try_clone()?, self.relative_leaf()?))
    }

    /// Point-in-time proof that the retained parent's leaf name still names
    /// the admitted object — the advisory preflight's twin of the commit
    /// section's leaf proof (see
    /// [`MountedMutationCommit::confirm_replacement_target`]).
    ///
    /// [`Self::validate`] deliberately checks only the retained object, the
    /// parent guards, and the root authority: a leaf renamed or replaced
    /// while its retained inode stays open keeps validating, and the swap is
    /// proven only inside the commit section. A preflight built on validate
    /// alone would therefore report a write-capable target whose every
    /// future commit is guaranteed to refuse. This check opens the leaf name
    /// through the retained parent directory — never an absolute pathname
    /// lookup, which a replaced parent could retarget — and compares exact
    /// identity against the admitted object.
    #[cfg(unix)]
    pub(crate) fn confirm_leaf_names_admitted_object(&self) -> io::Result<()> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("mutation target commit section is unavailable"))?;
        validate_mounted_bound(self.authority.as_ref(), &file)?;
        let leaf = self.relative_leaf()?;
        let parent = if let Some(guard) = file.parent_guards.last() {
            guard
        } else {
            self.authority.root_handle()
        };
        let current = open_unix_regular_at(&parent.file, &leaf)?;
        if object_identity(&current)? != file.object.identity {
            return Err(authority_changed(
                "mutation target no longer names the retained file",
            ));
        }
        // Close the check window on the authority side, mirroring the
        // double-validation discipline of every other retained-evidence
        // path.
        parent.validate_live()?;
        self.authority.validate()
    }

    /// Windows twin of [`Self::confirm_leaf_names_admitted_object`]:
    /// platforms without retained parent handles prove the pathname still
    /// names the admitted object through a fresh open and an exact identity
    /// comparison, exactly as the commit-side confirmation does.
    #[cfg(not(unix))]
    pub(crate) fn confirm_leaf_names_admitted_object(&self) -> io::Result<()> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("mutation target commit section is unavailable"))?;
        validate_mounted_bound(self.authority.as_ref(), &file)?;
        let current = File::open(&self.path)?;
        if object_identity(&current)? != file.object.identity {
            return Err(authority_changed(
                "mutation target no longer names the retained file",
            ));
        }
        self.authority.validate()
    }
}

/// One serialized commit section over a [`MountedMutationTarget`].
pub struct MountedMutationCommit<'a> {
    target: &'a MountedMutationTarget,
    file: MutexGuard<'a, BoundFile>,
}

/// The exact object a commit section published, together with its retained
/// handle.
///
/// The handle is kept open from the staged copy through the landing proof
/// into the re-anchor. An identity alone cannot distinguish the installed
/// object from a later stranger whose creation recycled the unlinked
/// object's identity — ext4 hands recently freed inodes to the next file
/// created in the same directory, so an unlink-plus-recreate swap can
/// produce a different object with the same `(device, inode)` pair. The
/// open handle pins the published object: on unix and Windows it exposes
/// the link count that proves the object was not unlinked, and on every
/// platform an object with an open handle still exists and holds its
/// identity, so no later creation can alias it.
struct InstalledReplacement {
    identity: ObjectIdentity,
    published: File,
}

impl InstalledReplacement {
    /// Prove the published object is still live and linked, returning the
    /// identity the re-anchor may condition on.
    ///
    /// A handle to an unlinked object reports zero links — on unix through
    /// `st_nlink`, and on Windows through `GetFileInformationByHandle`'s
    /// `nNumberOfLinks`, which drops to zero when the object's last name is
    /// removed under POSIX delete semantics even though the open handle
    /// keeps working and a plain `metadata()` read keeps succeeding. Without
    /// this check, a leaf swap whose stranger recycled the published
    /// object's identity would pass the re-anchor's identity comparison
    /// and anchor the target to a file the user never selected.
    ///
    /// On every platform the handle is still open here, and an object with
    /// an open handle still exists and holds its identity — deletion
    /// completes only at the last handle close — so no later creation can
    /// recycle the identity. A handle whose metadata can no longer be read
    /// has lost its object: fail closed rather than trust the identity.
    fn proven_identity(self) -> io::Result<ObjectIdentity> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            if self.published.metadata()?.nlink() == 0 {
                return Err(authority_changed(
                    "the proven replacement was removed before the target could re-anchor",
                ));
            }
        }
        #[cfg(windows)]
        {
            if windows_published_link_count(&self.published)? == 0 {
                return Err(authority_changed(
                    "the proven replacement was removed before the target could re-anchor",
                ));
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = self.published.metadata()?;
        }
        Ok(self.identity)
    }
}

impl MountedMutationCommit<'_> {
    /// Clone the exact admitted file object as the mutation's read source.
    ///
    /// The clone is taken from the retained handle, never from the pathname,
    /// so the copied bytes are the bytes the authority admitted even if the
    /// pathname was disturbed while the section was preparing.
    ///
    /// The clone is rewound to offset zero before it is returned. A
    /// `try_clone` handle shares the retained handle's underlying file
    /// description, so it inherits the retained read cursor: a copy attempt
    /// that consumed the cursor — a failed flush, a refused commit, a batch
    /// retry — leaves it at end-of-file, and a clone taken from there would
    /// stage an empty or partial file instead of rereading the admitted
    /// audio. The cursor is a read cursor only; nothing reads the retained
    /// handle positionally, so rewinding the shared description is safe.
    pub(crate) fn source_file(&self) -> io::Result<File> {
        self.file.object.validate_live()?;
        let mut source = self.file.object.file.try_clone()?;
        source.seek(SeekFrom::Start(0))?;
        Ok(source)
    }

    /// Clone the retained parent directory handle and admitted leaf name
    /// that anchor the staged tag sibling.
    ///
    /// Private machinery for the local tag writer's anchored unix staging.
    /// The staged copy is created, written, flushed, and — on every failure
    /// path — cleaned up beneath this exact retained parent object, never by
    /// resolving the target pathname, so an ancestor displaced after this
    /// section's validation can neither strand the staging area inside an
    /// impostor directory nor leak the complete tagged copy of the admitted
    /// file across a symlink planted at an old name. The commit already
    /// refuses a staged leaf it cannot find beneath this parent, so
    /// anchoring staging here makes staging and install agree on one
    /// directory object for the whole section.
    #[cfg(unix)]
    pub(crate) fn retained_staging_anchor(&self) -> io::Result<(File, OsString)> {
        let parent = self.retained_parent();
        parent.validate_live()?;
        Ok((parent.file.try_clone()?, self.target.relative_leaf()?))
    }

    /// Prove the replacement target immediately before an atomic rename.
    ///
    /// The mounted root is revalidated against its retained identity, and the
    /// exact pathname must still name the retained object. A file that was
    /// renamed away, replaced, or removed refuses the replacement: the write
    /// is authorized for the file the user selected, not for whatever now
    /// occupies its old name. The proof opens the leaf name through the
    /// retained parent directory — never an absolute pathname lookup, which a
    /// replaced parent could retarget and which would follow a symlink
    /// planted at the target name.
    pub(crate) fn confirm_replacement_target(&self) -> io::Result<()> {
        validate_mounted_bound(self.target.authority.as_ref(), &self.file)?;
        let leaf = self.target.relative_leaf()?;
        #[cfg(unix)]
        {
            let parent = self.retained_parent();
            let current = open_unix_regular_at(&parent.file, &leaf)?;
            if object_identity(&current)? != self.file.object.identity {
                return Err(authority_changed(
                    "mutation target no longer names the retained file",
                ));
            }
            // Close the check window on the authority side before the caller
            // proceeds, mirroring the double-validation discipline of every
            // other retained-evidence path.
            parent.validate_live()?;
            self.target.authority.validate()
        }
        #[cfg(not(unix))]
        {
            // Platforms without retained parent handles (Windows) keep the
            // documented discipline: revalidate the mount, then prove the
            // pathname still names the admitted object through a fresh open
            // and an exact identity comparison.
            let _ = leaf;
            let current = File::open(&self.target.path)?;
            if object_identity(&current)? != self.file.object.identity {
                return Err(authority_changed(
                    "mutation target no longer names the retained file",
                ));
            }
            self.target.authority.validate()
        }
    }

    /// Confirm the replacement target and perform the atomic replacement with
    /// the staged copy at `staged`.
    ///
    /// The whole section — validation and rename together — first excludes
    /// every other Tributary-side committer for the same directory leaf (see
    /// [`with_leaf_commit_lock`]): the per-target commit lock serializes one
    /// [`MountedMutationTarget`] object, but two targets admitted at
    /// different times can name the same leaf, and their confirm-and-rename
    /// spans must never interleave.
    ///
    /// Confirmation then proves the mount, the retained ancestry, and the
    /// exact leaf identity (see [`Self::confirm_replacement_target`]). On
    /// platforms with retained parent handles the replacement itself is then
    /// performed relative to that retained parent directory on both sides, so
    /// no pathname resolution — root, ancestor, or leaf — can retarget the
    /// rename after the confirmation; the destination entry is displaced
    /// and re-proved before anything is installed over its name (see
    /// [`Self::replace_confirmed_staging`]), and the install itself is a
    /// no-replace rename that refuses a leaf recreated inside the vacancy.
    /// The staged copy must live inside
    /// the exact retained parent directory; anything else is a lost staging
    /// area and is refused rather than re-resolved by path. Any uncertainty
    /// fails closed and leaves both files untouched.
    ///
    /// After the replacement is proven landed, the section re-anchors the
    /// target's retained binding to the exact installed object — reopened
    /// through the retained parent and identity-checked against the proven
    /// landing, whose handle is still held so the proven object must still
    /// be live (see [`Self::reanchor_target_to_installed`]) — while the
    /// section's guard is still held. A pathname is never opened for the
    /// re-anchor outside this guard.
    ///
    /// `expected_staged_identity` carries the identity of the exact object
    /// the caller tagged, captured from its retained handle before the
    /// staging pathname is consulted again. When supplied, the commit
    /// verifies that the staged leaf still names that exact object before
    /// anything is displaced, so a stranger swapped over the staging name
    /// in the tag-to-commit window refuses the commit instead of being
    /// installed with the tagged identity. Callers without a retained
    /// staging handle (the documented path-based flows) pass `None` and
    /// keep their pathname-identity proof.
    pub(crate) fn commit_replacement(
        &mut self,
        staged: &Path,
        expected_staged_identity: Option<&ObjectIdentity>,
    ) -> io::Result<()> {
        #[cfg(unix)]
        let key = self.leaf_commit_key()?;
        #[cfg(not(unix))]
        let key = self.leaf_commit_key();
        with_leaf_commit_lock(key, || {
            self.confirm_replacement_target()?;
            #[cfg(test)]
            run_post_confirm_interpose(self);
            let installed = self.replace_confirmed_staging(staged, expected_staged_identity)?;
            #[cfg(test)]
            run_pre_reanchor_interpose(self);
            self.reanchor_target_to_installed(installed)
        })
    }

    /// Re-anchor the target's retained binding to the installed replacement.
    ///
    /// A successful replacement deliberately retires the object this section
    /// copied from; the pathname names the replacement now. The retained
    /// evidence must follow it, or the next commit section would revalidate
    /// against the retired object and refuse a legitimate follow-up write.
    ///
    /// The re-anchor happens inside the commit section — while this guard
    /// still holds the target's commit lock — so it can never interleave
    /// with another section, and it is conditioned on the exact object the
    /// landing proved, through two independent checks:
    ///
    /// * the published object's retained handle must still be linked — an
    ///   unlinked object reports zero links, and a leaf swap that unlinked
    ///   the replacement must refuse even when the stranger's creation
    ///   recycled the replacement's identity (filesystems such as ext4 hand
    ///   recently freed identities to the next created file); on platforms
    ///   without a link-count primitive, holding the handle open already
    ///   keeps the object alive and its identity unrecyclable;
    /// * the leaf is reopened while the published object's handle is still
    ///   held — the order matters, because only an open handle keeps the
    ///   published object alive and its identity unrecyclable — and must
    ///   carry the published object's exact identity. A live object's
    ///   identity is unique, so with the handle still open the match can
    ///   only name that same object. Anything else means a leaf swap landed
    ///   between the landing proof and this reopen; anchoring it would
    ///   authorize a later commit to overwrite a file the user never
    ///   selected.
    ///
    /// Either failure leaves the retained binding pointing at the retired
    /// pre-commit object, which makes every later revalidation fail closed:
    /// the target invalidates itself.
    ///
    /// The reopen goes through the same evidence the landing proof used —
    /// the retained parent directory where the section installed the
    /// replacement — never through the mount-relative pathname, which a
    /// parent displaced mid-commit would resolve to an impostor directory.
    fn reanchor_target_to_installed(&mut self, installed: InstalledReplacement) -> io::Result<()> {
        #[cfg(unix)]
        {
            let parent = self.retained_parent();
            let leaf = self.target.relative_leaf()?;
            let reopened = open_unix_regular_at(&parent.file, &leaf)?;
            let expected = installed.proven_identity()?;
            if object_identity(&reopened)? != expected {
                return Err(authority_changed(
                    "the replaced leaf changed again before the target could re-anchor",
                ));
            }
            let rebound = BoundFile {
                lease_token: self.target.authority.token,
                path: self.target.path.clone(),
                object: RetainedObject::new(reopened)?,
                // The reopened object's retained directory is this section's
                // retained parent, so pin that directory: later revalidations
                // keep proving the object through the exact directory it was
                // anchored from.
                parent_guards: vec![RetainedObject::new(parent.file.try_clone()?)?],
            };
            *self.file = rebound;
        }
        #[cfg(not(unix))]
        {
            // Platforms without retained parent handles keep their documented
            // discipline: the landing proof and this re-anchor both reopen the
            // admitted pathname, and the re-anchor accepts only the exact
            // identity the landing proved. The leaf is reopened while the
            // published handle is still held — the order matters, because
            // only an open handle keeps the published object alive and its
            // identity unrecyclable — so a deleted replacement holds its
            // identity against the stranger the pathname may name now.
            let reopened = File::open(&self.target.path)?;
            let expected = installed.proven_identity()?;
            if object_identity(&reopened)? != expected {
                return Err(authority_changed(
                    "the replaced leaf changed again before the target could re-anchor",
                ));
            }
            let rebound = BoundFile {
                lease_token: self.target.authority.token,
                path: self.target.path.clone(),
                object: RetainedObject::new(reopened)?,
                parent_guards: Vec::new(),
            };
            *self.file = rebound;
        }
        Ok(())
    }

    /// Identify the directory leaf this section would replace.
    ///
    /// On platforms with retained parent handles the leaf is identified by
    /// the retained parent's exact filesystem object plus the leaf name —
    /// never by a pathname spelling, which a displaced parent could retarget.
    #[cfg(unix)]
    fn leaf_commit_key(&self) -> io::Result<LeafCommitKey> {
        Ok(LeafCommitKey::Retained {
            parent: self.retained_parent().identity,
            leaf: self.target.relative_leaf()?,
        })
    }

    /// Windows keeps no retained parent handle; the admitted target's
    /// normalized absolute pathname is the leaf identity every target object
    /// for that leaf shares.
    ///
    /// Infallible by construction: the admitted path is already normalized,
    /// so unlike the unix form there is no resolution left to fail. A
    /// `Result` wrapper that can never hold an error fails the `-D warnings`
    /// clippy gate on Windows (`unnecessary_wraps`).
    #[cfg(windows)]
    fn leaf_commit_key(&self) -> LeafCommitKey {
        LeafCommitKey::AdmittedPath(self.target.path.clone())
    }

    /// Non-unix, non-Windows targets have no retained parent either; the
    /// admitted pathname identifies the leaf. Direct return for the same
    /// clippy reason as the Windows form above.
    #[cfg(not(any(unix, windows)))]
    fn leaf_commit_key(&self) -> LeafCommitKey {
        LeafCommitKey::AdmittedPath(self.target.path.clone())
    }

    /// Resolve the retained directory that must contain the replacement.
    #[cfg(unix)]
    fn retained_parent(&self) -> &RetainedObject {
        if let Some(parent) = self.file.parent_guards.last() {
            return parent;
        }
        // A target at the top of the mount has no retained ancestor chain;
        // the retained root itself is the parent.
        self.target.authority.root_handle()
    }

    /// Rename the staged copy over the confirmed target through the retained
    /// parent, conditioning the destination on its confirmed identity, then
    /// prove the replacement landed on that exact entry. Returns the exact
    /// identity of the object the section published, for the caller's
    /// re-anchor step.
    ///
    /// A plain rename over the leaf would silently overwrite whatever an
    /// external writer swapped into the name after the confirm step proved
    /// it. Instead the confirmed entry is first displaced under a unique
    /// quarantine sibling — a rename to a fresh name can never clobber
    /// anything — the displaced object is proved to be the exact admitted
    /// file, and the staged copy is installed with a no-replace rename that
    /// refuses an occupied destination (see [`Self::rename_noreplace_at`]),
    /// so even a leaf recreated inside the quarantine-to-install vacancy is
    /// preserved and refused rather than overwritten. If the displaced object
    /// is not the admitted file, the newcomer is returned byte-exact to its
    /// own name — through the same conditioned primitive — and the commit
    /// refuses with every object untouched.
    ///
    /// The staged object's evidence stays retained through the install and
    /// the landing proof, and the displaced admitted original is retired only
    /// after that proof succeeds: an external rename over the staging name in
    /// the capture-to-install window would otherwise install the stranger,
    /// destroy the admitted original inside the install, and only then fail
    /// the proof — the loss detected after it happened. Here a failed proof
    /// means the quarantined original is still intact; it is restored
    /// (conditioned) to its name and the stranger is displaced to a sibling.
    /// The published object's handle is returned with its identity so the
    /// re-anchor can prove the published object is still live and linked.
    #[cfg(unix)]
    fn replace_confirmed_staging(
        &self,
        staged: &Path,
        expected_staged_identity: Option<&ObjectIdentity>,
    ) -> io::Result<InstalledReplacement> {
        let parent = self.retained_parent();
        let leaf = self.target.relative_leaf()?;
        let staged_leaf = staged
            .file_name()
            .ok_or_else(|| invalid_input("staged tag replacement has no file name"))?;

        // The staged copy must be reachable through this exact retained
        // directory. Opening it here — rather than trusting the staging
        // pathname — proves the install below will publish the object this
        // section created, and refuses a parent that was disturbed enough to
        // strand the staging area elsewhere. The handle is retained through
        // the install and the landing proof as the evidence for the object
        // this section is publishing.
        let staged_file = open_unix_regular_at(&parent.file, staged_leaf)?;
        let staged_identity = object_identity(&staged_file)?;

        // The opened object must be the exact object the caller tagged.
        // The tagging half captured this identity from its retained handle
        // before dropping it, so a stranger swapped over the staging name
        // in the tag-to-commit window is detected here — before anything
        // is displaced — and the commit refuses with every file untouched.
        if let Some(expected) = expected_staged_identity {
            if staged_identity != *expected {
                return Err(authority_changed(
                    "the staged tag copy was disturbed before the commit",
                ));
            }
        }

        // Displace whatever occupies the leaf under a fresh quarantine name,
        // then prove the displaced entry is the exact object the confirm step
        // verified. Anything else means the leaf was swapped between confirm
        // and quarantine: the newcomer is put back — byte-exact, under its
        // own name, through the conditioned restore — and the commit refuses.
        let quarantine_leaf = Self::displace_leaf_under_fresh_quarantine_name(parent, &leaf)?;
        Self::refuse_unless_displaced_entry_is_confirmed(
            parent,
            &quarantine_leaf,
            &leaf,
            &self.file.object.identity,
        )?;

        // Install the staged copy into the vacant leaf name. The install
        // itself is conditioned: a no-replace rename refuses an occupied
        // destination atomically, so an external writer that recreates the
        // leaf inside the quarantine-to-install window is preserved and
        // refuses the commit exactly like every earlier disturbance — the
        // write is authorized for the file the authority admitted, not for
        // whatever now occupies the name.
        #[cfg(test)]
        run_pre_install_interpose(self);
        Self::install_staged_leaf_into_vacant_leaf(parent, staged_leaf, &leaf, &quarantine_leaf)?;
        if let Err(error) = Self::prove_replacement_landed(parent, &leaf, &staged_identity) {
            // The landing proof failed: an external writer swapped a stranger
            // over the staging name before the install, or over the leaf
            // after it. The admitted original is still intact under its
            // quarantine name — the install no longer retires it — so put it
            // back (conditioned) and refuse with both objects preserved.
            Self::recover_original_after_failed_landing_proof(parent, &leaf, &quarantine_leaf);
            return Err(error);
        }

        // The landing is proven; only now retire the admitted original under
        // its quarantine name. A removal failure here leaves it as a
        // quarantine sibling — debris, never destruction. The published
        // object's handle stays open and moves to the re-anchor as its
        // liveness evidence.
        let _ = rustix::fs::unlinkat(&parent.file, quarantine_leaf, rustix::fs::AtFlags::empty());
        Ok(InstalledReplacement {
            identity: staged_identity,
            published: staged_file,
        })
    }

    /// Recover the admitted original after a failed landing proof.
    ///
    /// Whatever the failed install left at the leaf is displaced under a
    /// fresh unique quarantine sibling — a rename to a fresh name cannot
    /// clobber anything — and the original is returned to its name through
    /// the conditioned restore. Best effort by design: if an external writer
    /// wins a recreate race during the recovery, the displaced objects stay
    /// under their quarantine names — debris, never destruction.
    #[cfg(unix)]
    fn recover_original_after_failed_landing_proof(
        parent: &RetainedObject,
        leaf: &OsStr,
        quarantine_leaf: &OsStr,
    ) {
        let _ = rustix::fs::renameat(&parent.file, leaf, &parent.file, quarantine_name(leaf));
        let _ = Self::restore_displaced_entry(parent, quarantine_leaf, leaf);
    }

    /// Prove the installed leaf names the exact object the staged copy held.
    ///
    /// The install renames the staged copy into the vacant leaf name, so the
    /// entry the name points at afterwards must carry the staged copy's exact
    /// identity. Anything else means the leaf was disturbed again after the
    /// conditioned install; the commit refuses rather than claim a
    /// replacement it cannot prove.
    #[cfg(unix)]
    fn prove_replacement_landed(
        parent: &RetainedObject,
        leaf: &OsStr,
        staged_identity: &ObjectIdentity,
    ) -> io::Result<()> {
        let replaced = open_unix_regular_at(&parent.file, leaf)?;
        if object_identity(&replaced)? != *staged_identity {
            return Err(authority_changed(
                "the tagged replacement did not land on the retained mutation target",
            ));
        }
        Ok(())
    }

    /// Displace whatever occupies `leaf` under a fresh unique quarantine
    /// sibling and return that name.
    ///
    /// The rename is atomic and its destination is a fresh unique name, so it
    /// cannot destroy anything: afterwards the confirmed object — or whatever
    /// replaced it since the confirm step — sits under the quarantine name
    /// and the leaf is vacant. A leaf that vanished after the confirm step
    /// refuses without touching anything.
    #[cfg(unix)]
    fn displace_leaf_under_fresh_quarantine_name(
        parent: &RetainedObject,
        leaf: &OsStr,
    ) -> io::Result<OsString> {
        let quarantine_leaf = quarantine_name(leaf);
        if let Err(error) = rustix::fs::renameat(&parent.file, leaf, &parent.file, &quarantine_leaf)
        {
            return Err(
                if io::Error::from(error).kind() == io::ErrorKind::NotFound {
                    // The leaf vanished after the confirm step. Nothing has
                    // been displaced; refuse without touching anything.
                    authority_changed("the confirmed mutation target no longer exists")
                } else {
                    io::Error::from(error)
                },
            );
        }
        Ok(quarantine_leaf)
    }

    /// Prove the displaced entry is the exact object the confirm step
    /// verified; restore a swapped newcomer — byte-exact, under its own
    /// name, through the conditioned restore — and refuse otherwise.
    #[cfg(unix)]
    fn refuse_unless_displaced_entry_is_confirmed(
        parent: &RetainedObject,
        quarantine_leaf: &OsStr,
        leaf: &OsStr,
        expected: &ObjectIdentity,
    ) -> io::Result<()> {
        let displaced_is_confirmed = open_unix_regular_at(&parent.file, quarantine_leaf)
            .and_then(|displaced| object_identity(&displaced))
            .is_ok_and(|identity| identity == *expected);
        if !displaced_is_confirmed {
            Self::restore_displaced_entry(parent, quarantine_leaf, leaf)?;
            return Err(authority_changed(
                "the confirmed mutation target was replaced before the commit",
            ));
        }
        Ok(())
    }

    /// Install the staged copy into the vacant leaf name through the
    /// conditioned no-replace rename.
    ///
    /// A no-replace rename refuses an occupied destination atomically, so an
    /// external writer that recreates the leaf inside the quarantine-to-
    /// install window is preserved under a fresh sibling and the commit
    /// refuses exactly like every earlier disturbance — the write is
    /// authorized for the file the authority admitted, not for whatever now
    /// occupies the name.
    ///
    /// The install deliberately does not retire the displaced original under
    /// its quarantine name: the landing proof has not run yet, so retiring
    /// here would destroy the admitted original before the section knows
    /// whether the installed object is the one it staged. The caller retires
    /// the quarantined original only after the landing proof succeeds.
    #[cfg(unix)]
    fn install_staged_leaf_into_vacant_leaf(
        parent: &RetainedObject,
        staged_leaf: &OsStr,
        leaf: &OsStr,
        quarantine_leaf: &OsStr,
    ) -> io::Result<()> {
        match rename_noreplace_at(parent, staged_leaf, leaf) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // A newcomer recreated the leaf after the quarantine proof.
                // Preserve it under a fresh unique sibling — a rename to a
                // fresh name cannot clobber anything — put the confirmed
                // original back, and refuse with both objects intact. If even
                // the conditioned restore loses a second recreate race, the
                // displaced objects stay under their quarantine names: debris,
                // never destruction.
                let _ =
                    rustix::fs::renameat(&parent.file, leaf, &parent.file, quarantine_name(leaf));
                let _ = Self::restore_displaced_entry(parent, quarantine_leaf, leaf);
                return Err(authority_changed(
                    "the mutation target leaf was recreated during the commit",
                ));
            }
            Err(error) => {
                // The install never landed and the leaf is still vacant.
                // Return the confirmed original to its name (best effort —
                // the caller sees the install error either way) and fail
                // closed rather than stranding it under the quarantine name.
                let _ = Self::restore_displaced_entry(parent, quarantine_leaf, leaf);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Platforms without retained parent handles replace through the staged
    /// and target pathnames, with the same conditional-destination discipline:
    /// displace the confirmed entry under a unique quarantine sibling, prove
    /// the displaced object is the exact admitted file, restore a swapped
    /// newcomer byte-exact to its own name, and only then install the staged
    /// copy into the vacancy through a no-replace rename, so a leaf recreated
    /// inside the install window is preserved and refuses the commit exactly
    /// like the unix form.
    ///
    /// The staged object's evidence is retained through the install, the
    /// landing is proved against it exactly like the unix form, and the
    /// displaced admitted original is retired only after that proof
    /// succeeds — mirroring the unix fix for the staged-entry swap window.
    /// The published object's handle is returned with its identity so the
    /// re-anchor can hold the published object open while it re-proves the
    /// leaf.
    ///
    /// Errors surfaced here are rebuilt path-free: the std payloads embed
    /// the native pathnames, which must never leak into logs (see
    /// `replacement_path`).
    #[cfg(not(unix))]
    fn replace_confirmed_staging(
        &self,
        staged: &Path,
        expected_staged_identity: Option<&ObjectIdentity>,
    ) -> io::Result<InstalledReplacement> {
        let leaf = self.target.relative_leaf()?;
        let quarantine_leaf = quarantine_name(&leaf);
        let Some(parent_dir) = self.target.path.parent() else {
            return Err(invalid_input(
                "mutation target has no parent directory to stage the replacement in",
            ));
        };
        let quarantine_path = parent_dir.join(&quarantine_leaf);

        // Retain the staged object's evidence through the install and the
        // landing proof, exactly like the unix form.
        let staged_file = std::fs::File::open(staged).map_err(|error| {
            io::Error::new(
                error.kind(),
                "the staged tag replacement could not be opened",
            )
        })?;
        let staged_identity = object_identity(&staged_file)?;

        // The opened object must be the exact object the caller tagged (the
        // anchored callers capture this identity from their retained handle
        // before the staging name is consulted again). Like every refusal
        // below, this happens before anything is displaced.
        if let Some(expected) = expected_staged_identity {
            if staged_identity != *expected {
                return Err(authority_changed(
                    "the staged tag copy was disturbed before the commit",
                ));
            }
        }

        // Probe the no-replace publish primitive before the first
        // displacement. On volumes without hard-link support (FAT/exFAT
        // removable media) every `rename_noreplace` below would fail, and a
        // post-quarantine failure of the conditioned restore would strand
        // the admitted original under its quarantine name with its leaf
        // vacant. Linking the staged copy to a fresh throwaway name is the
        // exact primitive and volume the install uses: if the probe fails,
        // the commit refuses while the admitted file still sits untouched
        // at its own name. The probe link is removed before the commit
        // proceeds; a failed removal leaves only a private debris sibling.
        let probe_leaf = quarantine_name(&leaf);
        if let Err(error) = std::fs::hard_link(staged, parent_dir.join(&probe_leaf)) {
            return Err(io::Error::new(
                error.kind(),
                "this filesystem cannot publish the conditioned no-replace \
                 replacement; the commit refused before displacing the \
                 admitted file",
            ));
        }
        let _ = std::fs::remove_file(parent_dir.join(&probe_leaf));

        // Displace the confirmed entry under the fresh quarantine name. A
        // rename to a name that did not exist a moment ago never replaces an
        // existing entry, so this cannot clobber anything.
        if let Err(error) = std::fs::rename(&self.target.path, &quarantine_path) {
            return Err(if error.kind() == io::ErrorKind::NotFound {
                authority_changed("the confirmed mutation target no longer exists")
            } else {
                io::Error::new(
                    error.kind(),
                    "the confirmed mutation target could not be displaced under \
                     its quarantine name",
                )
            });
        }

        // Prove the displaced entry is the exact admitted file; return a
        // swapped newcomer to its own name through the conditioned restore
        // and refuse otherwise.
        let displaced_is_confirmed = std::fs::File::open(&quarantine_path)
            .and_then(|displaced| object_identity(&displaced))
            .is_ok_and(|identity| identity == self.file.object.identity);
        if !displaced_is_confirmed {
            rename_noreplace(&quarantine_path, &self.target.path)?;
            return Err(authority_changed(
                "the confirmed mutation target was replaced before the commit",
            ));
        }

        // Install the staged copy into the vacant leaf name through a
        // no-replace rename, so a leaf recreated inside the quarantine-to-
        // install window is preserved and refuses the commit instead of
        // being overwritten.
        #[cfg(test)]
        run_pre_install_interpose(self);
        match rename_noreplace(staged, &self.target.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // A newcomer recreated the leaf after the quarantine proof.
                // Preserve it under a fresh unique sibling, put the confirmed
                // original back, and refuse with both objects intact. If even
                // the conditioned restore loses a second recreate race, the
                // displaced objects stay under their quarantine names: debris,
                // never destruction.
                let _ = std::fs::rename(&self.target.path, parent_dir.join(quarantine_name(&leaf)));
                let _ = rename_noreplace(&quarantine_path, &self.target.path);
                return Err(authority_changed(
                    "the mutation target leaf was recreated during the commit",
                ));
            }
            Err(error) => {
                // The install never landed and the leaf is still vacant.
                // Return the confirmed original to its name (best effort —
                // the caller sees the install error either way) and fail
                // closed rather than stranding it under the quarantine name.
                let _ = rename_noreplace(&quarantine_path, &self.target.path);
                return Err(error);
            }
        }

        // Prove the landing on the exact staged object before anything is
        // retired, mirroring the unix form. A failed proof means a stranger
        // was swapped over the staging name or the leaf; the admitted
        // original is still intact under its quarantine name — put it back
        // (conditioned) and refuse with both objects preserved.
        let replaced = std::fs::File::open(&self.target.path).map_err(|error| {
            io::Error::new(
                error.kind(),
                "the installed replacement could not be re-opened for its landing proof",
            )
        });
        let landed = match replaced {
            Ok(file) => object_identity(&file),
            Err(error) => Err(error),
        };
        if !landed.is_ok_and(|identity| identity == staged_identity) {
            let _ = std::fs::rename(&self.target.path, parent_dir.join(quarantine_name(&leaf)));
            let _ = rename_noreplace(&quarantine_path, &self.target.path);
            return Err(authority_changed(
                "the tagged replacement did not land on the retained mutation target",
            ));
        }

        // The landing is proven; only now retire the admitted original under
        // its quarantine name. A removal failure here leaves it as a
        // quarantine sibling — debris, never destruction. The published
        // object's handle stays open and moves to the re-anchor as its
        // liveness evidence.
        let _ = std::fs::remove_file(&quarantine_path);
        Ok(InstalledReplacement {
            identity: staged_identity,
            published: staged_file,
        })
    }

    /// Return one displaced entry to `leaf` through the retained parent
    /// without ever overwriting the name's current occupant.
    ///
    /// A refused replacement restores a displaced object to its own name
    /// while an external writer may be recreating entries in the same
    /// directory, so even the restore is conditioned: a plain rename here
    /// could destroy a newcomer exactly like the install it guards against.
    /// If the name is occupied, the occupant is displaced under a fresh
    /// unique sibling — a rename to a fresh name cannot clobber anything —
    /// and the restore is retried once; if a second recreate wins that
    /// nanosecond race, the displaced object stays under its quarantine name
    /// and the propagated error refuses the commit with both objects
    /// preserved.
    #[cfg(unix)]
    fn restore_displaced_entry(
        parent: &RetainedObject,
        from: &OsStr,
        leaf: &OsStr,
    ) -> io::Result<()> {
        match rename_noreplace_at(parent, from, leaf) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                rustix::fs::renameat(&parent.file, leaf, &parent.file, quarantine_name(leaf))
                    .map_err(io::Error::from)?;
                rename_noreplace_at(parent, from, leaf)
            }
            Err(error) => Err(error),
        }
    }
}

/// Rename `from` to `to` through the retained parent, refusing an existing
/// destination.
///
/// `renameat2(RENAME_NOREPLACE)` — `renameatx_np(RENAME_EXCL)` on macOS —
/// makes the refusal atomic: no separate existence check can be interleaved
/// between the decision and the rename, which is exactly the property the
/// conditioned install and restores need.
///
/// A kernel or filesystem with no flag support (`ENOSYS` from a pre-10.12
/// macOS, `EINVAL` from a filesystem that never implemented the flag) fails
/// the call closed: the only primitive left would be the plain overwriting
/// rename, and silently degrading to it would destroy a leaf recreated
/// inside the vacancy and report success — precisely the clobber every
/// other conditioning in this section exists to prevent. The commit refuses
/// instead, leaving both files untouched.
#[cfg(unix)]
fn rename_noreplace_at(parent: &RetainedObject, from: &OsStr, to: &OsStr) -> io::Result<()> {
    match rustix::fs::renameat_with(
        &parent.file,
        from,
        &parent.file,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(error) => {
            let error = io::Error::from(error);
            if matches!(
                error.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
            ) {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this filesystem does not support the atomic no-replace rename \
                     the conditioned replacement requires; the commit refused \
                     without touching either file",
                ))
            } else {
                Err(error)
            }
        }
    }
}

/// Rename `from` to `to`, refusing an existing destination.
///
/// Windows has no no-replace rename primitive in the standard library, and
/// reaching `MoveFileExW` directly would put raw FFI `unsafe` into the
/// authority's publish path. `std::fs::hard_link` provides the same
/// conditioned refusal: it fails atomically with `AlreadyExists` when the
/// destination exists — the same refusal `RENAME_NOREPLACE` provides on
/// Unix — and because a hard link names the very same file object, the
/// published destination carries the source's exact identity, which the
/// replacement proof re-verifies after the install. The source name is then
/// retired best effort: a failure there only strands a second name for the
/// already-published object (debris, never destruction).
///
/// There is deliberately no fallback for filesystems without hard-link
/// support (FAT/exFAT removable media): the only primitive left would be
/// the plain overwriting rename, and silently degrading to it would destroy
/// a leaf recreated inside the vacancy — the exact clobber this section
/// refuses to commit. Such a filesystem fails the commit closed instead,
/// with an error that names the platform limitation. The OS payload is
/// dropped when rebuilding that error: it embeds the native pathnames,
/// which must never leak into logs (see `replacement_path`).
#[cfg(windows)]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            let _ = std::fs::remove_file(from);
            Ok(())
        }
        // The destination exists: the conditioned refusal itself. Propagated
        // unchanged so the install and restore callers keep their
        // AlreadyExists contract.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
        // No hard links available (or any other failure): the conditioned
        // no-replace publish cannot happen, so nothing happens — fail closed
        // with a path-free error.
        Err(error) => Err(io::Error::new(
            error.kind(),
            "this filesystem does not support the atomic no-replace publish \
             the conditioned replacement requires; the commit refused \
             without touching either file",
        )),
    }
}

/// Platforms with neither retained parent handles nor a no-replace rename
/// primitive have no way to publish a replacement without first refusing an
/// occupied destination, and a plain overwriting rename would destroy a leaf
/// recreated inside the vacancy. They fail every conditioned publish closed
/// rather than silently degrade to that rename.
#[cfg(not(any(unix, windows)))]
fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform does not provide the atomic no-replace rename \
         the conditioned replacement requires; the commit refused \
         without touching either file",
    ))
}

/// The key that identifies one replaceable directory leaf across every
/// mutation target that resolves to it.
///
/// On platforms with retained parent handles the key is the retained parent's
/// exact filesystem object plus the leaf name, so a pathname spelling can
/// never widen or split the exclusion. Platforms without retained parents
/// fall back to the admitted absolute pathname.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum LeafCommitKey {
    #[cfg(unix)]
    Retained {
        parent: ObjectIdentity,
        leaf: OsString,
    },
    #[cfg(not(unix))]
    AdmittedPath(PathBuf),
}

/// The process-wide registry of per-leaf replacement exclusions.
static LEAF_COMMIT_LOCKS: OnceLock<Mutex<HashMap<LeafCommitKey, Arc<Mutex<()>>>>> = OnceLock::new();

/// Run one replacement section while holding the process-wide exclusion for
/// one directory leaf.
///
/// The per-target commit lock serializes commit sections through one
/// [`MountedMutationTarget`] object, but nothing about that object is unique
/// to the leaf it names: two targets admitted at different times can name the
/// same directory entry, and each carries only its own admitted object. Their
/// confirm-and-rename spans must never interleave, or the later confirm would
/// prove a leaf that the earlier rename then overwrites with a different
/// object. The exclusion is therefore scoped to the leaf itself — identified
/// by retained parent object plus leaf name — and shared by every target for
/// that leaf.
///
/// The registry entry is removed when the section's lock is released and no
/// other committer holds or waits for that leaf, so the registry cannot grow
/// without bound. The strong-count re-check happens under the registry lock,
/// which every committer must hold to clone the entry, so a removal never
/// strands a waiter on an orphaned lock.
fn with_leaf_commit_lock(
    key: LeafCommitKey,
    run: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let registry = LEAF_COMMIT_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let entry = {
        let mut table = registry
            .lock()
            .map_err(|_| io::Error::other("leaf commit registry is unavailable"))?;
        Arc::clone(table.entry(key.clone()).or_default())
    };
    let result = {
        let _leaf_lock = entry
            .lock()
            .map_err(|_| io::Error::other("leaf commit section is unavailable"))?;
        run()
    };
    // Two strong references — the registry table and this section's own
    // clone — mean no other committer holds or waits for this leaf, so the
    // entry can be retired. The re-check under the registry lock, which
    // every committer must hold to clone the entry, closes the window
    // against a concurrent clone between the two checks: a removal never
    // strands a waiter on an orphaned lock, and a waiter that clones first
    // raises the count past two so the entry stays.
    if Arc::strong_count(&entry) == 2 {
        if let Ok(mut table) = registry.lock() {
            if let Some(existing) = table.get(&key) {
                if Arc::strong_count(existing) == 2 && Arc::ptr_eq(existing, &entry) {
                    table.remove(&key);
                }
            }
        }
    }
    result
}

/// A fresh hidden sibling name for the quarantine step of a conditional
/// replacement. The uuid makes the name unpredictable and collision-free,
/// and the original leaf contributes a bounded prefix so a long leaf cannot
/// exceed the filesystem's name limit once the suffix is appended.
fn quarantine_name(leaf: &OsStr) -> OsString {
    let lossy = leaf.to_string_lossy();
    let mut prefix = String::new();
    for character in lossy.chars() {
        if prefix.len() + character.len_utf8() > 96 {
            break;
        }
        prefix.push(character);
    }
    format!(".{}.tributary-replaced-{}", prefix, Uuid::new_v4().simple()).into()
}

/// Test-only seam: run the registered post-confirm interposition, if any.
///
/// The conditional replacement's refusal logic is only reachable when the
/// leaf is swapped inside the confirm-to-replace window — microseconds wide
/// in production. A regression test registers a closure here that performs
/// that swap deterministically between the confirm step and the replace
/// step. Never compiled outside `cargo test`.
#[cfg(test)]
fn run_post_confirm_interpose(commit: &MountedMutationCommit<'_>) {
    if let Some(interpose) = POST_CONFIRM_INTERPOSE.lock().unwrap().as_ref() {
        interpose(commit);
    }
}

#[cfg(test)]
type PostConfirmInterpose = dyn Fn(&MountedMutationCommit<'_>) + Send + Sync;

#[cfg(test)]
static POST_CONFIRM_INTERPOSE: Mutex<Option<Box<PostConfirmInterpose>>> = Mutex::new(None);

/// Serialize tests that use the post-confirm interposition seam.
#[cfg(test)]
fn with_post_confirm_interpose(interpose: Box<PostConfirmInterpose>, run: impl FnOnce()) {
    let _serial = POST_CONFIRM_INTERPOSE_SERIAL.lock().unwrap();
    *POST_CONFIRM_INTERPOSE.lock().unwrap() = Some(interpose);
    run();
    *POST_CONFIRM_INTERPOSE.lock().unwrap() = None;
}

#[cfg(test)]
static POST_CONFIRM_INTERPOSE_SERIAL: Mutex<()> = Mutex::new(());

/// Test-only seam: run the registered pre-install interposition, if any.
///
/// The install-window refusal is only reachable when the leaf is recreated
/// between the quarantine proof and the install rename — nanoseconds wide in
/// production. A regression test registers a closure here that recreates the
/// leaf deterministically in that vacancy. Never compiled outside
/// `cargo test`.
#[cfg(test)]
fn run_pre_install_interpose(commit: &MountedMutationCommit<'_>) {
    if let Some(interpose) = PRE_INSTALL_INTERPOSE.lock().unwrap().as_ref() {
        interpose(commit);
    }
}

#[cfg(test)]
type PreInstallInterpose = dyn Fn(&MountedMutationCommit<'_>) + Send + Sync;

#[cfg(test)]
static PRE_INSTALL_INTERPOSE: Mutex<Option<Box<PreInstallInterpose>>> = Mutex::new(None);

/// Serialize tests that use the pre-install interposition seam.
#[cfg(test)]
fn with_pre_install_interpose(interpose: Box<PreInstallInterpose>, run: impl FnOnce()) {
    let _serial = PRE_INSTALL_INTERPOSE_SERIAL.lock().unwrap();
    *PRE_INSTALL_INTERPOSE.lock().unwrap() = Some(interpose);
    run();
    *PRE_INSTALL_INTERPOSE.lock().unwrap() = None;
}

#[cfg(test)]
static PRE_INSTALL_INTERPOSE_SERIAL: Mutex<()> = Mutex::new(());

/// Test-only seam: run the registered pre-re-anchor interposition, if any.
///
/// The re-anchor refusal is only reachable when the leaf is swapped between
/// the landing proof and the re-anchor's authority reopen — nanoseconds wide
/// in production. A regression test registers a closure here that swaps the
/// leaf deterministically in that window. Never compiled outside `cargo
/// test`.
#[cfg(test)]
fn run_pre_reanchor_interpose(commit: &MountedMutationCommit<'_>) {
    if let Some(interpose) = PRE_REANCHOR_INTERPOSE.lock().unwrap().as_ref() {
        interpose(commit);
    }
}

#[cfg(test)]
type PreReanchorInterpose = dyn Fn(&MountedMutationCommit<'_>) + Send + Sync;

#[cfg(test)]
static PRE_REANCHOR_INTERPOSE: Mutex<Option<Box<PreReanchorInterpose>>> = Mutex::new(None);

/// Serialize tests that use the pre-re-anchor interposition seam.
#[cfg(test)]
fn with_pre_reanchor_interpose(interpose: Box<PreReanchorInterpose>, run: impl FnOnce()) {
    let _serial = PRE_REANCHOR_INTERPOSE_SERIAL.lock().unwrap();
    *PRE_REANCHOR_INTERPOSE.lock().unwrap() = Some(interpose);
    run();
    *PRE_REANCHOR_INTERPOSE.lock().unwrap() = None;
}

#[cfg(test)]
static PRE_REANCHOR_INTERPOSE_SERIAL: Mutex<()> = Mutex::new(());

/// Verify a mounted bound against its retained mount authority.
fn validate_mounted_bound(authority: &MountedRootAuthority, bound: &BoundFile) -> io::Result<()> {
    validate_bound_token(authority, bound.lease_token)?;
    bound.object.validate_live()?;
    validate_retained_objects(&bound.parent_guards)?;
    authority.validate()
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

        let opened_root = open_configured_root(root, false, false)?;
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

        let current_root = open_configured_root(&self.root, false, false)?;
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

        let after_marker = open_configured_root(&self.root, false, false)?;
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

fn validate_bound_token(authority: &impl RootBinding, token: Uuid) -> io::Result<()> {
    if token == authority.token() {
        Ok(())
    } else {
        Err(authority_changed(
            "bound filesystem evidence belongs to a different root authority",
        ))
    }
}

fn validate_opened_root_binding(
    authority: &impl RootBinding,
    current: &OpenedRoot,
) -> io::Result<()> {
    if current.root.identity != authority.root_handle().identity {
        return Err(authority_changed(
            "mounted root path no longer names the retained directory",
        ));
    }
    if root_mount_generation(&current.root.file)? != authority.mount_generation() {
        return Err(authority_changed(
            "mounted root path no longer belongs to the retained mount",
        ));
    }
    if boundary_identity(&current.root.file)? != authority.boundary() {
        return Err(authority_changed(
            "mounted root filesystem boundary changed",
        ));
    }
    #[cfg(windows)]
    compare_object_chains(authority.root_ancestors(), &current.ancestors)?;
    Ok(())
}

fn validate_root_binding(authority: &impl RootBinding) -> io::Result<()> {
    authority.root_handle().validate_live()?;
    #[cfg(windows)]
    validate_retained_objects(authority.root_ancestors())?;

    let current = open_configured_root(
        authority.root(),
        authority.unmount_friendly_sharing(),
        authority.unmount_friendly_sharing(),
    )?;
    validate_opened_root_binding(authority, &current)?;
    authority.root_handle().validate_live()?;

    // A second reopen catches a path replacement racing the first comparison.
    let after = open_configured_root(
        authority.root(),
        authority.unmount_friendly_sharing(),
        authority.unmount_friendly_sharing(),
    )?;
    validate_opened_root_binding(authority, &after)?;
    authority.root_handle().validate_live()
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

fn strict_relative_components(relative: &Path) -> io::Result<Vec<OsString>> {
    if relative.is_absolute() {
        return Err(invalid_input("mounted descendant path must be relative"));
    }
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(value) => components.push(value.to_os_string()),
            _ => {
                return Err(invalid_input(
                    "mounted descendant path contains a non-normal component",
                ))
            }
        }
    }
    if components.is_empty() {
        return Err(invalid_input(
            "mounted file authority requires a path below the root",
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

/// Verify one leaf name is a single normal path component: non-empty and
/// free of separators and NUL. Handles passed to handle-relative operations
/// must never be able to traverse out of the retained parent directory.
fn validate_leaf_name(leaf: &OsStr) -> io::Result<()> {
    let bytes = leaf.as_encoded_bytes();
    if bytes.is_empty() {
        return Err(invalid_input("leaf name is empty"));
    }
    if bytes.contains(&0) {
        return Err(invalid_input("leaf name contains NUL"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        if leaf.as_bytes().contains(&b'/') {
            return Err(invalid_input("leaf name contains a path separator"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        // `as` casts are not valid in match patterns, so the wide-char
        // separators are declared as constants and matched by name.
        const BACKSLASH: u16 = b'\\' as u16;
        const FORWARD_SLASH: u16 = b'/' as u16;
        const COLON: u16 = b':' as u16;

        for unit in leaf.encode_wide() {
            if matches!(unit, BACKSLASH | FORWARD_SLASH | COLON) {
                return Err(invalid_input("leaf name contains a path separator"));
            }
        }
    }
    Ok(())
}

/// Create one path component as a directory inside `current` (or verify the
/// existing entry is a real directory), then open it no-follow for the next
/// walk level. Unix only; used by
/// [`MountedRootAuthority::create_directories_within`].
#[cfg(unix)]
fn ensure_directory_component(current: &File, component: &OsString) -> io::Result<File> {
    use rustix::fs::{AtFlags, Mode, OFlags};

    match rustix::fs::mkdirat(current, component, Mode::from_bits_truncate(0o777)) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => {
            let stat = rustix::fs::statat(current, component, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "intermediate path is not a directory",
                ));
            }
        }
        Err(error) => return Err(io::Error::from(error)),
    }
    let opened = rustix::fs::openat(
        current,
        component,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(opened))
}

/// Rename `from_leaf` to `to_leaf` inside the parent directory `parent`,
/// replacing any existing `to_leaf`. The rename is anchored to the retained
/// parent handle on Unix; Windows uses the absolute paths after the caller
/// revalidated the retained parent.
#[cfg(unix)]
fn rename_within_parent(
    parent: &File,
    from_leaf: &OsStr,
    _from_absolute: &Path,
    to_leaf: &OsStr,
    _to_absolute: &Path,
) -> io::Result<()> {
    rustix::fs::renameat(parent, from_leaf, parent, to_leaf).map_err(io::Error::from)
}

/// Windows replace-rename: `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`
/// through the absolute paths. The retained parent was revalidated by the
/// caller immediately before, and is revalidated again after.
#[cfg(windows)]
fn rename_within_parent(
    _parent: &File,
    _from_leaf: &OsStr,
    from_absolute: &Path,
    _to_leaf: &OsStr,
    to_absolute: &Path,
) -> io::Result<()> {
    std::fs::rename(from_absolute, to_absolute)
}

#[cfg(not(any(unix, windows)))]
fn rename_within_parent(
    _parent: &File,
    _from_leaf: &OsStr,
    _from_absolute: &Path,
    _to_leaf: &OsStr,
    _to_absolute: &Path,
) -> io::Result<()> {
    Err(unsupported_platform())
}

/// No-replace rename of `from_leaf` to `to_leaf` inside the retained parent.
///
/// The platform-native no-replace rename is tried first: `renameat_with`
/// with `RENAME_NOREPLACE`, backed by `renameat2` on Linux/Android and the
/// flagged renamer (`renameatx_np`) on Apple platforms. Filesystems that do
/// not implement the flag — FAT and exFAT USB mounts return `EINVAL`/
/// `ENOSYS`, for example — fall back to a link-based publish, which is
/// atomic and fails with `EEXIST` on a collision; the staged leaf is then
/// unlinked. Filesystems without hard links (also common on FAT) fall back
/// to an exclusive-reservation publish: the destination leaf is created
/// exclusively as a private placeholder and the staged leaf is renamed over
/// it, so a collision fails definitively and no pre-existing or concurrently
/// created file can ever be replaced. A collision detected by any strategy
/// is a definitive failure that leaves the destination untouched.
#[cfg(unix)]
fn rename_no_replace_within_parent(
    parent: &File,
    from_leaf: &OsStr,
    from_absolute: &Path,
    to_leaf: &OsStr,
    to_absolute: &Path,
) -> io::Result<()> {
    use rustix::fs::{linkat, renameat_with, unlinkat, AtFlags, RenameFlags};

    match renameat_with(parent, from_leaf, parent, to_leaf, RenameFlags::NOREPLACE) {
        // The data is published under the final name and the staged leaf is
        // gone; there is nothing left for the fallbacks below to do.
        Ok(()) => return Ok(()),
        // Filesystems and kernels without renameat2 flag support fall back
        // to the link-based publish below.
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::OPNOTSUPP) => {
        }
        Err(other) => return Err(io::Error::from(other)),
    }

    match linkat(parent, from_leaf, parent, to_leaf, AtFlags::empty()) {
        Ok(()) => {
            // The data is published under the final name. Unlinking the
            // staged leaf is best-effort: a failure leaves a hidden
            // temporary behind rather than lying about the publish.
            let _ = unlinkat(parent, from_leaf, AtFlags::empty());
            return Ok(());
        }
        Err(rustix::io::Errno::EXIST) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination appeared before the no-replace publish",
            ));
        }
        Err(
            rustix::io::Errno::PERM
            | rustix::io::Errno::OPNOTSUPP
            | rustix::io::Errno::NOSYS
            | rustix::io::Errno::XDEV
            | rustix::io::Errno::MLINK,
        ) => {}
        Err(error) => return Err(io::Error::from(error)),
    }

    publish_by_exclusive_reservation(parent, from_leaf, from_absolute, to_leaf, to_absolute)
}

/// Final no-replace strategy for filesystems offering neither rename flags
/// nor hard links.
///
/// The destination leaf is created exclusively (`O_CREAT | O_EXCL`) as a
/// private placeholder, then the staged leaf is renamed over it. Any other
/// creator loses the exclusive create and learns the name is taken, so the
/// only bytes the replace can ever discard are the placeholder's own —
/// unlike an existence probe bracketing a plain rename, which could replace
/// a file created inside the probe-to-rename window. A failed replace
/// unlinks the placeholder best-effort so the name is released cleanly; it
/// held only bytes this call created. Both steps stay anchored to the
/// retained parent handle.
#[cfg(unix)]
fn publish_by_exclusive_reservation(
    parent: &File,
    from_leaf: &OsStr,
    from_absolute: &Path,
    to_leaf: &OsStr,
    to_absolute: &Path,
) -> io::Result<()> {
    use rustix::fs::{unlinkat, AtFlags, Mode, OFlags};

    let reserved = rustix::fs::openat(
        parent,
        to_leaf,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NOCTTY,
        Mode::from_bits_truncate(0o600),
    );
    match reserved {
        // The name is reserved. Close the placeholder immediately: the
        // publish replaces the directory entry, not this open descriptor.
        Ok(reserved) => drop(reserved),
        Err(rustix::io::Errno::EXIST) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination appeared before the no-replace publish",
            ));
        }
        Err(error) => return Err(io::Error::from(error)),
    }
    if let Err(error) = rename_within_parent(parent, from_leaf, from_absolute, to_leaf, to_absolute)
    {
        // Release the reserved name so a retry sees a clean directory; the
        // placeholder never held caller data.
        let _ = unlinkat(parent, to_leaf, AtFlags::empty());
        return Err(error);
    }
    Ok(())
}

/// Windows no-replace publish, mirroring the Unix strategy cascade with
/// safe `std` operations. A hard-link publish is tried first: creating the
/// link fails when the final leaf exists, so the publish is atomic and a
/// collision is a definitive failure. Filesystems without hard-link support
/// — FAT and exFAT USB mounts — fall back to the exclusive-reservation
/// publish.
#[cfg(windows)]
fn rename_no_replace_within_parent(
    _parent: &File,
    _from_leaf: &OsStr,
    from_absolute: &Path,
    _to_leaf: &OsStr,
    to_absolute: &Path,
) -> io::Result<()> {
    match std::fs::hard_link(from_absolute, to_absolute) {
        Ok(()) => {
            // The data is published under the final name. Removing the
            // staged leaf is best-effort: a failure leaves a hidden
            // temporary behind rather than lying about the publish.
            let _ = std::fs::remove_file(from_absolute);
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination appeared before the no-replace publish",
            ));
        }
        // No hard-link support on this filesystem: fall through to the
        // exclusive-reservation publish below.
        Err(_) => {}
    }
    publish_no_replace_by_reservation(from_absolute, to_absolute)
}

/// Final no-replace strategy for filesystems offering no hard links.
///
/// The destination leaf is created exclusively as a private placeholder,
/// then the staged leaf is renamed over it. Any other exclusive creator
/// loses and learns the name is taken, so the only bytes the replace can
/// ever discard are the placeholder's own — unlike an existence probe
/// bracketing a plain rename, which could replace a file created inside the
/// probe-to-rename window. A failed replace removes the placeholder
/// best-effort so the name is released cleanly; it held only bytes this
/// call created.
#[cfg(windows)]
fn publish_no_replace_by_reservation(from_absolute: &Path, to_absolute: &Path) -> io::Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to_absolute)
    {
        // The name is reserved. Close the placeholder immediately: the
        // publish replaces the directory entry, not this handle.
        Ok(reserved) => drop(reserved),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination appeared before the no-replace publish",
            ));
        }
        Err(error) => return Err(error),
    }
    if let Err(error) = std::fs::rename(from_absolute, to_absolute) {
        // Release the reserved name so a retry sees a clean directory; the
        // placeholder never held caller data.
        let _ = std::fs::remove_file(to_absolute);
        return Err(error);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn rename_no_replace_within_parent(
    _parent: &File,
    _from_leaf: &OsStr,
    _from_absolute: &Path,
    _to_leaf: &OsStr,
    _to_absolute: &Path,
) -> io::Result<()> {
    Err(unsupported_platform())
}

/// Windows fallback for [`MountedRootAuthority::create_directories_within`]:
/// path-based per-component creation with a reparse-free is-directory check,
/// mirroring the historical `create_directory_atomic` behavior.
#[cfg(windows)]
fn create_directory_tree_by_path(root: &Path, components: &[OsString]) -> io::Result<()> {
    let mut path = root.to_path_buf();
    for component in components {
        path.push(component);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&path)?;
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "intermediate path is not a directory",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn object_identity(file: &File) -> io::Result<ObjectIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(ObjectIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
pub fn object_identity(file: &File) -> io::Result<ObjectIdentity> {
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

/// Read the link count of an open file through its handle on Windows.
///
/// `GetFileInformationByHandle` reports the same liveness signal unix
/// exposes through `st_nlink`: when a file's last name is removed under
/// POSIX delete semantics, the reported count drops to zero while the open
/// handle keeps working — and a plain `metadata()` read keeps succeeding,
/// so the link count, not the read's success, is the deleted-object proof.
/// The call reads the object through the handle itself, never through a
/// name, so a removed or replaced directory entry cannot influence it. A
/// failed read means liveness cannot be proven and the caller fails closed.
#[cfg(windows)]
fn windows_published_link_count(file: &File) -> io::Result<u32> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let handle = file.as_raw_handle() as HANDLE;
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `handle` is live and `info` is a correctly sized, aligned
    // output buffer that is fully initialized on success.
    let result = unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call above initialized the complete structure.
    let info = unsafe { info.assume_init() };
    Ok(info.nNumberOfLinks)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn object_identity(_file: &File) -> io::Result<ObjectIdentity> {
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
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    use rustix::fs::{Mode, OFlags};

    // On Linux, `O_NOFOLLOW` protects the final path component, but a trailing
    // slash or `/.` makes the preceding symlink an intermediate component and
    // therefore follows it. Remove only those semantically redundant suffixes
    // before the no-follow open, preserving native non-UTF-8 bytes and `/`.
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    loop {
        while bytes.len() > 1 && bytes.last() == Some(&b'/') {
            bytes.pop();
        }
        if bytes.ends_with(b"/.") {
            bytes.truncate(bytes.len() - 2);
            if bytes.is_empty() {
                bytes.push(b'/');
            }
            continue;
        }
        break;
    }
    let no_follow_path = PathBuf::from(OsString::from_vec(bytes));
    let descriptor = rustix::fs::open(
        &no_follow_path,
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
fn open_unix_regular_at(parent: &File, name: &OsStr) -> io::Result<File> {
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
fn is_windows_volume_mount_point(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;

    let mut mount_point: Vec<u16> = path.as_os_str().encode_wide().collect();
    if !mount_point
        .last()
        .is_some_and(|unit| *unit == u16::from(b'\\') || *unit == u16::from(b'/'))
    {
        mount_point.push(u16::from(b'\\'));
    }
    mount_point.push(0);
    // A volume GUID path is far below this fixed bound. Failure, truncation,
    // and every non-volume reparse all fail closed as `false`.
    let mut volume_name = [0_u16; 256];
    // SAFETY: both pointers refer to writable/readable NUL-terminated buffers
    // for the complete duration of the call, and the size is in u16 elements.
    unsafe {
        GetVolumeNameForVolumeMountPointW(
            mount_point.as_ptr(),
            volume_name.as_mut_ptr(),
            volume_name.len() as u32,
        ) != 0
    }
}

#[cfg(windows)]
struct OpenedWindowsDirectory {
    target: File,
    namespace_guard: Option<File>,
}

#[cfg(windows)]
fn open_windows_directory(
    path: &Path,
    unmount_friendly_sharing: bool,
    follow_final_mount_target: bool,
) -> io::Result<OpenedWindowsDirectory> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    // If the GIO-supplied mounted root is a reparse point, pin that exact
    // namespace entry without delete sharing while proving it is an actual
    // volume mount and opening its target. Merely omitting
    // `FILE_FLAG_OPEN_REPARSE_POINT` would also follow arbitrary symlinks,
    // junctions, and cloud-provider reparses.
    let namespace_guard = if follow_final_mount_target {
        let guard = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = guard.metadata()?;
        // Rust's `is_symlink_dir` also classifies volume-mount name-surrogate
        // reparses as symlinks, so the Windows volume API is the discriminator
        // here: ordinary directory symlinks and junctions fail this probe.
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && !is_windows_volume_mount_point(path)
        {
            return Err(invalid_marker(
                "mounted root reparse point is not a volume mount",
            ));
        }
        Some(guard)
    } else {
        None
    };

    // Marker-backed roots omit `FILE_SHARE_DELETE`: those handles pin the
    // namespace through an authorized SQLite mutation. Ephemeral mounted
    // roots opt in so a live browse/playback authority does not unnecessarily
    // block rename, unmount, or eject; identity revalidation remains the
    // authority boundary there.
    let share_mode = FILE_SHARE_READ
        | FILE_SHARE_WRITE
        | if unmount_friendly_sharing {
            FILE_SHARE_DELETE
        } else {
            0
        };
    let file = OpenOptions::new()
        .read(true)
        .share_mode(share_mode)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS
                | if follow_final_mount_target {
                    0
                } else {
                    FILE_FLAG_OPEN_REPARSE_POINT
                },
        )
        .open(path)?;
    let metadata = file.metadata()?;
    if !follow_final_mount_target && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(invalid_marker("library root must not be a reparse point"));
    }
    if !metadata.is_dir() {
        return Err(invalid_marker("library root is not a directory"));
    }
    Ok(OpenedWindowsDirectory {
        target: file,
        namespace_guard,
    })
}

#[cfg(windows)]
fn open_windows_regular(
    path: &Path,
    share_writes: bool,
    unmount_friendly_sharing: bool,
) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    // Marker-backed files keep delete sharing disabled for the same namespace
    // pinning reason as directory handles. Mounted files allow write/delete
    // sharing for unmount friendliness; retained object identity prevents a
    // renamed or replaced path from retargeting an admitted capability.
    let share_mode = FILE_SHARE_READ
        | if share_writes { FILE_SHARE_WRITE } else { 0 }
        | if unmount_friendly_sharing {
            FILE_SHARE_WRITE | FILE_SHARE_DELETE
        } else {
            0
        };
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
fn open_configured_root(
    path: &Path,
    _unmount_friendly_sharing: bool,
    _follow_final_mount_target: bool,
) -> io::Result<OpenedRoot> {
    // Configured aliases may contain an ancestor symlink (notably `/var` on
    // macOS). The final component itself is never followed, and every
    // descendant operation below is anchored to the retained directory fd.
    Ok(OpenedRoot {
        root: RetainedObject::new(open_unix_directory_path(path)?)?,
    })
}

#[cfg(windows)]
fn open_configured_root(
    path: &Path,
    unmount_friendly_sharing: bool,
    follow_final_mount_target: bool,
) -> io::Result<OpenedRoot> {
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

    let prefix_count = prefixes.len();
    let mut handles = Vec::with_capacity(prefix_count);
    let mut namespace_guard = None;
    for (index, prefix) in prefixes.into_iter().enumerate() {
        // A Windows directory volume mount point is itself a reparse point.
        // GIO supplies the mounted root, so mounted authority follows only
        // that final target and binds its exact volume/file identity. Every
        // ancestor and every later descendant remains no-follow.
        let follow_final_mount_target = follow_final_mount_target && index + 1 == prefix_count;
        let opened =
            open_windows_directory(&prefix, unmount_friendly_sharing, follow_final_mount_target)?;
        if opened.namespace_guard.is_some() {
            debug_assert!(follow_final_mount_target);
            namespace_guard = opened.namespace_guard;
        }
        handles.push(RetainedObject::new(opened.target)?);
    }
    let root = handles
        .pop()
        .ok_or_else(|| invalid_input("configured library root has no absolute component"))?;
    Ok(OpenedRoot {
        root,
        ancestors: handles,
        _namespace_guard: namespace_guard,
    })
}

#[cfg(not(any(unix, windows)))]
fn open_configured_root(
    _path: &Path,
    _unmount_friendly_sharing: bool,
    _follow_final_mount_target: bool,
) -> io::Result<OpenedRoot> {
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
    open_windows_regular(&root.join(ROOT_IDENTITY_FILE), true, false)
}

#[cfg(not(any(unix, windows)))]
fn open_marker(_root: &Path, _root_file: &File) -> io::Result<File> {
    Err(unsupported_platform())
}

#[cfg(unix)]
fn open_descendant_from_root(
    authority: &impl RootBinding,
    _path: &Path,
    components: &[OsString],
    kind: DescendantKind,
) -> io::Result<OpenedDescendant> {
    if components.is_empty() {
        let file = authority.root_handle().file.try_clone()?;
        ensure_boundary(authority.boundary(), &file)?;
        return Ok(OpenedDescendant {
            object: RetainedObject::new(file)?,
            parent_guards: Vec::new(),
        });
    }

    let mut parent = authority.root_handle().file.try_clone()?;
    let mut parent_guards = Vec::with_capacity(components.len().saturating_sub(1));
    for (index, component) in components.iter().enumerate() {
        let is_last = index + 1 == components.len();
        let file = if is_last && matches!(kind, DescendantKind::RegularFile) {
            open_unix_regular_at(&parent, component)?
        } else {
            open_unix_directory_at(&parent, component)?
        };
        ensure_boundary(authority.boundary(), &file)?;
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
    authority: &impl RootBinding,
    _path: &Path,
    components: &[OsString],
    kind: DescendantKind,
) -> io::Result<OpenedDescendant> {
    if components.is_empty() {
        let file = authority.root_handle().file.try_clone()?;
        ensure_boundary(authority.boundary(), &file)?;
        return Ok(OpenedDescendant {
            object: RetainedObject::new(file)?,
            parent_guards: Vec::new(),
        });
    }

    // The standard library cannot open Windows descendants relative to a
    // retained directory handle. Mounted authority normally shares delete so
    // it does not block eject, but that would let an intermediate directory be
    // replaced by a same-volume junction between path-based component opens.
    // Temporarily reopen and pin the exact root/ancestor namespace without
    // delete sharing, then retain similarly strict directory guards until the
    // final no-follow regular-file handle has been opened.
    let _mounted_traversal_root = if authority.unmount_friendly_sharing() {
        let current = open_configured_root(authority.root(), false, true)?;
        validate_opened_root_binding(authority, &current)?;
        Some(current)
    } else {
        None
    };

    let mut current_path = authority.root().to_path_buf();
    let mut parent_guards = Vec::with_capacity(components.len().saturating_sub(1));
    for (index, component) in components.iter().enumerate() {
        current_path.push(component);
        let is_last = index + 1 == components.len();
        let file = if is_last && matches!(kind, DescendantKind::RegularFile) {
            open_windows_regular(&current_path, false, authority.unmount_friendly_sharing())?
        } else {
            open_windows_directory(&current_path, false, false)?.target
        };
        ensure_boundary(authority.boundary(), &file)?;
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
    _authority: &impl RootBinding,
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

    /// The exclusive-reservation publish is the final no-replace strategy
    /// for filesystems offering neither rename flags nor hard links. It must
    /// refuse an existing destination without touching it, and publish by
    /// consuming the staged leaf when the destination is free.
    #[cfg(unix)]
    #[test]
    fn exclusive_reservation_publish_refuses_existing_destination() {
        let directory = TestDirectory::new("reservation-publish");
        let parent = fs::File::open(directory.path()).expect("open parent handle");
        fs::write(directory.path().join(".stage-tmp"), b"payload").expect("write staged leaf");
        fs::write(directory.path().join("final.flac"), b"original")
            .expect("write existing destination");

        let error = publish_by_exclusive_reservation(
            &parent,
            OsStr::new(".stage-tmp"),
            directory.path().join(".stage-tmp").as_path(),
            OsStr::new("final.flac"),
            directory.path().join("final.flac").as_path(),
        )
        .expect_err("existing destination must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(directory.path().join("final.flac")).expect("read destination"),
            b"original",
            "the pre-existing destination must be untouched"
        );
        assert_eq!(
            fs::read(directory.path().join(".stage-tmp")).expect("read staged leaf"),
            b"payload",
            "the staged leaf must be untouched on a refused publish"
        );

        publish_by_exclusive_reservation(
            &parent,
            OsStr::new(".stage-tmp"),
            directory.path().join(".stage-tmp").as_path(),
            OsStr::new("published.flac"),
            directory.path().join("published.flac").as_path(),
        )
        .expect("publish to a free destination");
        assert_eq!(
            fs::read(directory.path().join("published.flac")).expect("read published"),
            b"payload",
            "publish must move the staged bytes under the final name"
        );
        assert!(
            !directory.path().join(".stage-tmp").exists(),
            "publish must consume the staged leaf"
        );
    }

    #[test]
    fn mounted_authority_needs_no_marker_and_opens_only_relative_files() {
        let directory = TestDirectory::new("mounted-valid");
        let album = directory.path().join("album");
        fs::create_dir(&album).expect("create album");
        let song = album.join("song.flac");
        fs::write(&song, b"mounted audio").expect("write song");

        let authority =
            MountedRootAuthority::acquire(directory.path()).expect("acquire mounted authority");
        assert_eq!(authority.root(), directory.path());
        authority.validate().expect("validate mounted authority");
        let bound = authority
            .open_relative_regular_file(Path::new("album/song.flac"))
            .expect("open relative mounted file");
        let mut file = bound
            .try_clone_for_mounted_consumption(&authority)
            .expect("clone mounted file");
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).expect("read mounted file");
        assert_eq!(contents, b"mounted audio");

        assert!(authority.open_relative_regular_file(&song).is_err());
        assert!(authority
            .open_relative_regular_file(Path::new("../outside.flac"))
            .is_err());
        assert!(authority
            .open_relative_regular_file(Path::new("album/../song.flac"))
            .is_err());
        assert!(authority.open_relative_regular_file(Path::new("")).is_err());
    }

    #[test]
    fn mounted_bounds_cannot_cross_authority_instances() {
        let first = TestDirectory::new("mounted-first");
        fs::write(first.path().join("song.flac"), b"first").expect("write first song");
        let first_authority =
            MountedRootAuthority::acquire(first.path()).expect("first mounted authority");
        let bound = first_authority
            .open_relative_regular_file(Path::new("song.flac"))
            .expect("bind first song");

        let second = TestDirectory::new("mounted-second");
        fs::write(second.path().join("song.flac"), b"second").expect("write second song");
        let second_authority =
            MountedRootAuthority::acquire(second.path()).expect("second mounted authority");

        assert!(bound
            .try_clone_for_mounted_consumption(&second_authority)
            .is_err());
    }

    #[test]
    fn mutation_target_retains_the_exact_file_through_a_commit_section() {
        let directory = TestDirectory::new("mutation-commit");
        let album = directory.path().join("album");
        fs::create_dir(&album).expect("create album");
        fs::write(album.join("song.flac"), b"mounted audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("album/song.flac"))
            .expect("open mutation target");
        assert_eq!(target.relative_path(), Path::new("album/song.flac"));
        target.validate().expect("validate retained target");

        // The commit section reads from the retained handle — never a path
        // lookup — and proves the pathname still names the admitted object.
        let commit = target.begin_commit().expect("begin commit section");
        let mut source = commit.source_file().expect("clone retained source");
        let mut contents = Vec::new();
        source.read_to_end(&mut contents).expect("read source");
        assert_eq!(contents, b"mounted audio");
        commit
            .confirm_replacement_target()
            .expect("confirm untouched replacement target");
    }

    #[test]
    fn mutation_target_refuses_replacement_after_the_path_names_another_file() {
        let directory = TestDirectory::new("mutation-swap");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        // Swap the pathname between selection and commit, as an outside
        // writer could. The retained object stays live and still reads its
        // admitted bytes, but the pathname no longer names it — so the
        // commit must refuse rather than replace whatever took its place.
        let displaced = directory.path().join("displaced.flac");
        fs::rename(&song, &displaced).expect("displace the admitted file");
        fs::write(&song, b"replacement audio").expect("install different bytes");

        target.validate().expect("retained object is still live");
        let commit = target.begin_commit().expect("commit section opens");
        let mut source = commit.source_file().expect("clone retained source");
        let mut contents = Vec::new();
        source.read_to_end(&mut contents).expect("read source");
        assert_eq!(contents, b"original audio");
        commit
            .confirm_replacement_target()
            .expect_err("the pathname no longer names the admitted file");
    }

    #[test]
    fn commit_replacement_reanchors_the_target_to_the_installed_object() {
        let directory = TestDirectory::new("mutation-reanchor-success");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        // The staged copy sits beside the target, as the tag writer stages it.
        let staged = directory.path().join(".song.tributary-tag-tmp.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");

        // A successful replacement retires the object the section copied
        // from; the section re-anchors the retained evidence to the exact
        // installed object — identity-checked against the proven landing,
        // inside the commit guard — so a follow-up commit is authorized for
        // the file that exists now, without ever reopening the leaf by path
        // outside the section.
        // The commit section holds the target's commit lock until its guard
        // is dropped, so the first section must end before the follow-up
        // section begins — a nested begin_commit on the same target would
        // self-deadlock on the non-reentrant Mutex.
        {
            let mut commit = target.begin_commit().expect("begin commit section");
            commit
                .commit_replacement(&staged, None)
                .expect("the replacement commits and re-anchors the target");
            assert!(
                !staged.exists(),
                "a successful install consumes the staging name"
            );
        }

        let follow_up = target.begin_commit().expect("begin the follow-up section");
        follow_up
            .confirm_replacement_target()
            .expect("the re-anchored object is the admitted one");
        let mut source = follow_up.source_file().expect("clone re-anchored source");
        let mut contents = Vec::new();
        source.read_to_end(&mut contents).expect("read source");
        assert_eq!(
            contents, b"tagged audio",
            "the re-anchored evidence must read the installed replacement"
        );
    }

    /// The re-anchor must stay conditional on the proven landing: an external
    /// writer that swaps the leaf between the landing proof and the
    /// re-anchor's authority reopen must not be adopted as the retained
    /// evidence — a later commit anchored to that stranger would overwrite a
    /// file the user never selected. The section must refuse, and the target
    /// must invalidate itself: its binding still names the retired pre-commit
    /// object, so every later revalidation fails closed.
    #[test]
    fn commit_replacement_refuses_a_leaf_swapped_after_the_landing_proof_and_invalidates_the_target(
    ) {
        let directory = TestDirectory::new("mutation-reanchor-swap");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        // The staged copy sits beside the target, as the tag writer stages it.
        let staged = directory.path().join(".song.tributary-tag-tmp.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");
        let watched_leaf = song.clone();

        with_pre_reanchor_interpose(
            Box::new(move |commit| {
                if commit.target.path != watched_leaf {
                    return;
                }
                // The landing proof has already passed on the staged copy.
                // Swap the leaf now — unlink plus recreate, a different
                // object — exactly where a stranger would be adopted by an
                // unconstrained post-commit rebind.
                let path = commit.target.path.clone();
                fs::remove_file(&path).expect("unlink the installed replacement");
                fs::write(&path, b"stranger audio").expect("install a stranger at the leaf");
            }),
            || {
                let mut commit = target.begin_commit().expect("begin commit section");
                commit
                    .commit_replacement(&staged, None)
                    .expect_err("a leaf swapped before the re-anchor must refuse the commit");
            },
        );

        // The stranger keeps the name: the refused re-anchor must not touch
        // the leaf, and the retired original must not be resurrected over it.
        assert_eq!(
            fs::read(&song).expect("read the leaf back"),
            b"stranger audio",
            "the refused re-anchor must leave the leaf exactly as the external writer left it"
        );
        // The landing was proven, so the section retired the quarantined
        // original and consumed the staged copy before the swap.
        assert!(
            !staged.exists(),
            "the landing preceded the swap, so the staged copy was consumed"
        );
        assert_no_quarantine_sibling_remains(&directory);

        // The target invalidated itself: the binding still names the retired
        // pre-commit object, so the follow-up section must refuse.
        let follow_up = target.begin_commit().expect("begin the follow-up section");
        follow_up
            .confirm_replacement_target()
            .expect_err("the target must fail closed after a refused re-anchor");
    }

    /// An identity match alone cannot prove the leaf is the installed
    /// replacement: once the section unlinks a file, a stranger created in
    /// the same directory can receive the recycled identity — ext4 hands
    /// recently freed inodes to the next created file — so an identity-only
    /// re-anchor would adopt a file the user never selected. The section
    /// therefore holds the published object's handle open and refuses when
    /// it reports zero links: the replacement was removed after its landing
    /// was proven, whatever the leaf's identity says now. The target must
    /// invalidate itself.
    #[test]
    fn commit_replacement_refuses_when_the_installed_replacement_was_unlinked_before_the_re_anchor()
    {
        let directory = TestDirectory::new("mutation-reanchor-unlinked");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        // The staged copy sits beside the target, as the tag writer stages it.
        let staged = directory.path().join(".song.tributary-tag-tmp.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");
        let watched_leaf = song.clone();

        with_pre_reanchor_interpose(
            Box::new(move |commit| {
                if commit.target.path != watched_leaf {
                    return;
                }
                // The landing proof has already passed on the installed
                // replacement. Unlink it and recreate the leaf with the
                // replacement's own bytes — the hostile case where a
                // stranger's creation can recycle the replacement's identity
                // and an identity-only re-anchor would adopt it.
                let path = commit.target.path.clone();
                fs::remove_file(&path).expect("unlink the installed replacement");
                fs::write(&path, b"tagged audio")
                    .expect("recreate the leaf over the unlinked replacement");
            }),
            || {
                let mut commit = target.begin_commit().expect("begin commit section");
                let error = commit
                    .commit_replacement(&staged, None)
                    .expect_err("the re-anchor must refuse a removed replacement");
                assert!(
                    error
                        .to_string()
                        .contains("removed before the target could re-anchor"),
                    "the refusal must come from the published object's liveness proof, not a \
                     later check: {error}"
                );
            },
        );

        // The recreated leaf keeps the name: the refused re-anchor must not
        // touch it, and the retired original must not be resurrected over it.
        assert_eq!(
            fs::read(&song).expect("read the leaf back"),
            b"tagged audio",
            "the refused re-anchor must leave the leaf exactly as the external writer left it"
        );
        // The landing was proven, so the section retired the quarantined
        // original and consumed the staged copy before the unlink.
        assert!(
            !staged.exists(),
            "the landing preceded the unlink, so the staged copy was consumed"
        );
        assert_no_quarantine_sibling_remains(&directory);

        // The target invalidated itself: the binding still names the retired
        // pre-commit object, so the follow-up section must refuse even though
        // the leaf now holds the replacement's exact bytes.
        let follow_up = target.begin_commit().expect("begin the follow-up section");
        follow_up
            .confirm_replacement_target()
            .expect_err("the target must fail closed after the replacement was unlinked");
    }

    /// The replacement must be performed relative to the retained parent
    /// directory, never by resolving the target pathname: a parent displaced
    /// between selection and commit must not redirect the write into
    /// whatever now occupies the old name.
    #[cfg(unix)]
    #[test]
    fn mutation_target_replacement_lands_through_the_retained_parent_after_parent_displacement() {
        let directory = TestDirectory::new("mutation-parent-displace");
        let album = directory.path().join("album");
        fs::create_dir(&album).expect("create album");
        let song = album.join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("album/song.flac"))
            .expect("open mutation target");

        // Displace the retained parent and install an impostor directory at
        // its old pathname, as an outside writer could between selection and
        // commit. A path-based rename would land the replacement on the
        // impostor; the retained parent cannot.
        let displaced_album = directory.path().join("displaced-album");
        fs::rename(&album, &displaced_album).expect("displace retained parent");
        fs::create_dir(&album).expect("install impostor parent");
        fs::write(album.join("song.flac"), b"impostor audio").expect("install impostor file");

        // Stage inside the retained parent — the directory object the
        // authority still holds — not beside the now-impostor pathname.
        let staged = displaced_album.join(".tributary-tag-staged.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");

        let mut commit = target.begin_commit().expect("begin commit section");
        commit
            .commit_replacement(&staged, None)
            .expect("commit through the retained parent");

        assert_eq!(
            fs::read(displaced_album.join("song.flac")).expect("read replaced file"),
            b"tagged audio",
            "the replacement must land beside the admitted file in the retained directory"
        );
        assert_eq!(
            fs::read(album.join("song.flac")).expect("read impostor file"),
            b"impostor audio",
            "the impostor directory must never receive the write"
        );
    }

    /// A symlink installed at the target name must never be followed by the
    /// commit-time identity proof: the proof opens the leaf through the
    /// retained parent with no-follow semantics, so a displaced-and-linked
    /// name refuses the replacement instead of re-finding the retained
    /// identity behind the link.
    #[cfg(unix)]
    #[test]
    fn mutation_target_confirm_refuses_a_symlink_installed_at_the_target_name() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("mutation-symlink-name");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        let displaced = directory.path().join("displaced.flac");
        fs::rename(&song, &displaced).expect("displace the admitted file");
        symlink(&displaced, &song).expect("install a symlink at the target name");

        let commit = target.begin_commit().expect("begin commit section");
        commit
            .confirm_replacement_target()
            .expect_err("a symlink at the target name must never be followed");
        assert_eq!(
            fs::read(&displaced).expect("read displaced file"),
            b"original audio",
            "the retained original must stay byte-for-byte intact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mounted_authority_rejects_symlink_escape_and_root_replacement() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("mounted-symlink");
        let outside = TestDirectory::new("mounted-outside");
        fs::write(outside.path().join("outside.flac"), b"outside").expect("write outside file");
        symlink(outside.path(), directory.path().join("escape")).expect("create escape symlink");
        let authority =
            MountedRootAuthority::acquire(directory.path()).expect("acquire mounted authority");
        assert!(authority
            .open_relative_regular_file(Path::new("escape/outside.flac"))
            .is_err());

        let replacement = TestDirectory::new("mounted-replacement");
        let displaced = directory.path().with_extension("displaced");
        fs::rename(directory.path(), &displaced).expect("displace mounted root");
        fs::rename(replacement.path(), directory.path()).expect("install replacement root");
        assert!(authority.validate().is_err());

        drop(authority);
        fs::rename(directory.path(), replacement.path()).expect("restore replacement root");
        fs::rename(&displaced, directory.path()).expect("restore mounted root");
    }

    #[cfg(windows)]
    #[test]
    fn windows_mounted_file_handles_allow_namespace_retirement() {
        let directory = TestDirectory::new("windows-mounted-sharing");
        let song = directory.path().join("song.flac");
        let moved = directory.path().join("moved.flac");
        fs::write(&song, b"audio").expect("write song");
        let authority =
            MountedRootAuthority::acquire(directory.path()).expect("acquire mounted authority");
        let bound = authority
            .open_relative_regular_file(Path::new("song.flac"))
            .expect("bind mounted song");

        fs::rename(&song, &moved).expect("mounted sharing permits file rename");
        let mut retained = bound
            .try_clone_for_mounted_consumption(&authority)
            .expect("clone renamed exact file");
        let mut contents = Vec::new();
        retained
            .read_to_end(&mut contents)
            .expect("read renamed file");
        assert_eq!(contents, b"audio");
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

        // `O_NOFOLLOW` alone follows the alias when a slash or dot is appended
        // because the alias is no longer the kernel's final path component.
        // Authority normalizes only those redundant suffixes before opening.
        let trailing_slash = PathBuf::from(format!("{}/", final_alias.display()));
        let trailing_dot = final_alias.join(".");
        assert!(RootAuthorityLease::acquire(&trailing_slash, MARKER).is_err());
        assert!(RootAuthorityLease::acquire(&trailing_dot, MARKER).is_err());
        assert!(MountedRootAuthority::acquire(&trailing_slash).is_err());
        assert!(MountedRootAuthority::acquire(&trailing_dot).is_err());
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

    /// Assert that a refused commit left no quarantine sibling behind: the
    /// caller's staging name must already be cleaned up so only replacement
    /// debris could match.
    fn assert_no_quarantine_sibling_remains(directory: &TestDirectory) {
        let leftovers: Vec<PathBuf> = fs::read_dir(directory.path())
            .expect("list the directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains("tributary-replaced"))
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "a refused commit must restore the displaced leaf and leave no quarantine sibling: {leftovers:?}"
        );
    }

    /// Assert the refused commit returned the confirmed original to its own
    /// name, byte-exact.
    fn assert_confirmed_original_restored_to_its_name(leaf: &Path) {
        assert_eq!(
            fs::read(leaf).expect("read the leaf back"),
            b"original audio",
            "the refused commit must restore the confirmed original to its own name"
        );
    }

    /// Assert the recreated newcomer survived the refused install —
    /// displaced under exactly one fresh quarantine sibling, byte-for-byte
    /// intact.
    fn assert_recreated_newcomer_displaced_under_one_fresh_sibling(
        directory: &TestDirectory,
        expected: &[u8],
    ) {
        let siblings: Vec<PathBuf> = fs::read_dir(directory.path())
            .expect("list the directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains("tributary-replaced"))
            })
            .collect();
        assert_eq!(
            siblings.len(),
            1,
            "exactly the displaced stranger may remain, under one fresh sibling: {siblings:?}"
        );
        assert_eq!(
            fs::read(&siblings[0]).expect("read the displaced object"),
            expected,
            "the preserved object must be byte-for-byte intact"
        );
    }

    /// The replacement must be conditional on the confirmed leaf identity.
    /// An external writer that swaps the leaf after the confirm step has
    /// proven it — precisely what a plain rename over the name would
    /// silently overwrite — must be detected, restored byte-exact to its
    /// own name, and the commit refused with every pre-existing object
    /// untouched and no quarantine sibling left behind.
    #[test]
    fn commit_replacement_refuses_a_leaf_swapped_after_confirm_and_restores_the_newcomer() {
        let directory = TestDirectory::new("mutation-post-confirm-swap");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        // The staged copy sits beside the target, as the tag writer stages it.
        let staged = directory.path().join(".song.tributary-tag-tmp.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");
        let displaced_original = directory.path().join("displaced.flac");
        let displaced_for_closure = displaced_original.clone();
        // The seam is process-global and the harness runs tests in parallel,
        // so the closure must act only on this test's own leaf: a concurrent
        // test's commit section must pass through the seam untouched.
        let watched_leaf = song.clone();

        with_post_confirm_interpose(
            Box::new(move |commit| {
                if commit.target.path != watched_leaf {
                    return;
                }
                // Swap the leaf after the confirm step proved it: the
                // admitted object is moved aside and a different object
                // takes the name, as an external writer would mid-commit.
                let path = commit.target.path.clone();
                fs::rename(&path, &displaced_for_closure).expect("displace the confirmed leaf");
                fs::write(&path, b"newcomer audio").expect("install a newcomer at the leaf");
            }),
            || {
                let mut commit = target.begin_commit().expect("begin commit section");
                commit
                    .commit_replacement(&staged, None)
                    .expect_err("a leaf swapped after confirm must refuse the commit");
            },
        );

        // The newcomer keeps its name and exact bytes: the refused write
        // must not replace whatever took the name.
        assert_eq!(
            fs::read(&song).expect("read the leaf back"),
            b"newcomer audio",
            "the refused write must not replace whatever took the name"
        );
        // The displaced admitted object is untouched where the external
        // writer moved it.
        assert_eq!(
            fs::read(&displaced_original).expect("read the displaced original"),
            b"original audio",
            "the admitted object must be byte-for-byte untouched"
        );
        // A refused commit does not consume the staged copy; the caller
        // cleans it up. Remove it here, then require that the refusal left
        // no quarantine sibling behind.
        assert!(
            staged.exists(),
            "the staged copy must survive a refused commit for the caller to clean up"
        );
        fs::remove_file(&staged).expect("remove the staged copy");
        assert_no_quarantine_sibling_remains(&directory);
    }

    /// The replacement must stay conditional on the destination remaining
    /// vacant through the quarantine-to-install window: an external writer
    /// that recreates the leaf after the confirmed original was displaced —
    /// exactly what a plain overwriting install rename would silently
    /// destroy — must be preserved under a fresh sibling and must refuse the
    /// commit with the confirmed original restored byte-exact to its own
    /// name.
    #[test]
    fn commit_replacement_refuses_a_leaf_recreated_in_the_install_window_and_preserves_the_newcomer(
    ) {
        let directory = TestDirectory::new("mutation-install-window-recreate");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        // The staged copy sits beside the target, as the tag writer stages it.
        let staged = directory.path().join(".song.tributary-tag-tmp.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");
        let watched_leaf = song.clone();

        with_pre_install_interpose(
            Box::new(move |commit| {
                if commit.target.path != watched_leaf {
                    return;
                }
                // The quarantine step displaced the confirmed original and
                // proved it; the leaf is vacant. Recreate it, as an external
                // writer would inside the install window.
                fs::write(&commit.target.path, b"newcomer audio")
                    .expect("recreate the leaf in the install window");
            }),
            || {
                let mut commit = target.begin_commit().expect("begin commit section");
                commit
                    .commit_replacement(&staged, None)
                    .expect_err("a leaf recreated in the install window must refuse the commit");
            },
        );

        // The confirmed original is back under its own name, byte-exact, and
        // the newcomer survives — displaced under a fresh quarantine sibling,
        // never destroyed by the refused install.
        assert_confirmed_original_restored_to_its_name(&song);
        assert_recreated_newcomer_displaced_under_one_fresh_sibling(&directory, b"newcomer audio");

        // A refused commit does not consume the staged copy; the caller
        // cleans it up.
        assert!(
            staged.exists(),
            "the staged copy must survive a refused commit for the caller to clean up"
        );
        fs::remove_file(&staged).expect("remove the staged copy");
    }

    /// The staged object's evidence must stay retained through the install,
    /// and the admitted original must be retired only after the landing
    /// proof succeeds. An external rename over the staging name inside the
    /// capture-to-install window makes the install publish a stranger; the
    /// commit must detect that at the landing proof, restore the admitted
    /// original byte-exact to its own name, and refuse — not destroy the
    /// original inside the install and only then discover the loss.
    #[cfg(unix)]
    #[test]
    fn commit_replacement_refuses_a_staging_name_swapped_before_the_install_and_restores_the_original(
    ) {
        let directory = TestDirectory::new("mutation-staging-swap");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        // The staged copy sits beside the target, as the tag writer stages it.
        let staged = directory.path().join(".song.tributary-tag-tmp.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");
        let stolen = directory.path().join("stolen.flac");
        let stolen_for_closure = stolen.clone();
        let watched_staged = staged.clone();
        let watched_leaf = song.clone();

        with_pre_install_interpose(
            Box::new(move |commit| {
                if commit.target.path != watched_leaf {
                    return;
                }
                // The quarantine step displaced the confirmed original and
                // proved it, and the staged identity has already been
                // captured. Swap the staging name now — exactly the window
                // where a stranger would be installed and the original
                // destroyed before the landing proof could run.
                fs::rename(&watched_staged, &stolen_for_closure)
                    .expect("move the staged copy aside");
                fs::write(&watched_staged, b"stranger audio")
                    .expect("install a stranger at the staging name");
            }),
            || {
                let mut commit = target.begin_commit().expect("begin commit section");
                commit
                    .commit_replacement(&staged, None)
                    .expect_err("a stranger swapped over the staging name must refuse the commit");
            },
        );

        // The admitted original is back under its own name, byte-exact: the
        // refused install must never destroy the displaced original.
        assert_confirmed_original_restored_to_its_name(&song);
        // The true staged copy survives where the external writer moved it.
        assert_eq!(
            fs::read(&stolen).expect("read the moved staged copy"),
            b"tagged audio",
            "the true staged copy must survive the refused commit untouched"
        );
        fs::remove_file(&stolen).expect("remove the moved staged copy");
        // The stranger the install briefly published survives displaced
        // under exactly one fresh quarantine sibling — debris, never
        // destruction.
        assert_recreated_newcomer_displaced_under_one_fresh_sibling(&directory, b"stranger audio");
    }

    /// The tagged staging object's identity is conditioned into the commit:
    /// when a stranger replaces the staging name after the tagged handle's
    /// identity was captured — the tag-to-commit window — the commit refuses
    /// before anything is displaced, and both the admitted original and the
    /// stranger's victims stay untouched.
    #[test]
    fn commit_replacement_refuses_when_the_opened_staging_object_is_not_the_expected_identity() {
        let directory = TestDirectory::new("mutation-staged-identity-mismatch");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        // The tagged copy sits beside the target; its identity is captured
        // from the handle, exactly as the anchored tag writer captures it.
        let staged = directory.path().join(".song.tributary-tag-tmp.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");
        let expected = object_identity(&fs::File::open(&staged).expect("open the staged copy"))
            .expect("identity of the tagged staging object");

        // A stranger takes over the staging name after the capture.
        let moved = directory.path().join("moved.flac");
        fs::rename(&staged, &moved).expect("move the true staged copy aside");
        fs::write(&staged, b"stranger audio").expect("plant a stranger at the staging name");

        let mut commit = target.begin_commit().expect("begin commit section");
        let error = commit
            .commit_replacement(&staged, Some(&expected))
            .expect_err("a disturbed staging object must refuse the commit");
        assert_eq!(
            error.kind(),
            io::ErrorKind::PermissionDenied,
            "the staged-object conditioning must refuse like every other disturbance"
        );

        // The refusal happened before the displacement: the admitted
        // original is untouched at its own name, and the stranger still sits
        // at the staging name it stole.
        assert_eq!(
            fs::read(&song).expect("read the leaf back"),
            b"original audio",
            "a staged-object refusal must leave the admitted original untouched"
        );
        assert_eq!(
            fs::read(&staged).expect("read the stranger back"),
            b"stranger audio",
            "a staged-object refusal must leave the staging name alone"
        );
        assert_eq!(
            fs::read(&moved).expect("read the true staged copy back"),
            b"tagged audio",
            "the true staged copy must survive the refusal untouched"
        );
    }

    /// Supplying the exact tagged identity does not disturb the happy path:
    /// the commit installs the staged copy that matches it.
    #[test]
    fn commit_replacement_accepts_a_staged_object_matching_the_expected_identity() {
        let directory = TestDirectory::new("mutation-staged-identity-match");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        let staged = directory.path().join(".song.tributary-tag-tmp.flac");
        fs::write(&staged, b"tagged audio").expect("stage the replacement");
        let expected = object_identity(&fs::File::open(&staged).expect("open the staged copy"))
            .expect("identity of the tagged staging object");

        let mut commit = target.begin_commit().expect("begin commit section");
        commit
            .commit_replacement(&staged, Some(&expected))
            .expect("the matching staged copy commits");
        assert_eq!(
            fs::read(&song).expect("read the leaf back"),
            b"tagged audio",
            "the conditioned commit must install the exact staged object"
        );
        assert!(
            !staged.exists(),
            "a successful install consumes the staging name"
        );
    }

    /// A cloned read source shares the retained handle's underlying file
    /// description, so a copy attempt that consumed the cursor must not leave
    /// the next retry staging bytes from end-of-file: every `source_file()`
    /// clone is returned rewound to offset zero.
    #[test]
    fn source_file_starts_at_zero_after_a_prior_copy_consumed_the_cursor() {
        let directory = TestDirectory::new("mutation-source-cursor");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let target = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open mutation target");

        let commit = target.begin_commit().expect("begin commit section");
        let mut first = commit.source_file().expect("clone the retained source");
        let mut consumed = Vec::new();
        std::io::Read::read_to_end(&mut first, &mut consumed).expect("consume the first copy");
        assert_eq!(consumed, b"original audio");
        drop(first);

        // The shared cursor now sits at end-of-file; the next clone must
        // still read the full admitted bytes.
        let mut second = commit.source_file().expect("re-clone the retained source");
        let mut retried = Vec::new();
        std::io::Read::read_to_end(&mut second, &mut retried).expect("read the retry copy");
        assert_eq!(
            retried, b"original audio",
            "a retry must stage the full admitted bytes, not a cursor-at-EOF tail"
        );
    }

    /// Two targets admitted for the same leaf at different times must
    /// serialize on the leaf itself, not on each other's object: the first
    /// replacement retires the object the second target admitted, and the
    /// second commit must then refuse closed — without deadlocking the
    /// leaf exclusion both sections share.
    #[test]
    fn two_targets_for_one_leaf_serialize_and_the_displaced_one_refuses() {
        let directory = TestDirectory::new("mutation-two-targets-one-leaf");
        let song = directory.path().join("song.flac");
        fs::write(&song, b"original audio").expect("write song");

        let authority =
            Arc::new(MountedRootAuthority::acquire(directory.path()).expect("acquire authority"));
        let first = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open the first target");
        let second = authority
            .open_mutation_target(Path::new("song.flac"))
            .expect("open the second target");

        let staged = directory.path().join(".staged-one.flac");
        fs::write(&staged, b"tagged audio").expect("stage the first replacement");
        {
            let mut commit = first
                .begin_commit()
                .expect("begin the first commit section");
            commit
                .commit_replacement(&staged, None)
                .expect("the first replacement commits");
        }
        assert!(
            !staged.exists(),
            "a successful install consumes the staging name"
        );

        // The second target's admitted object was retired by the first
        // replacement; its commit must refuse and leave the first
        // replacement installed and exact.
        let staged_second = directory.path().join(".staged-two.flac");
        fs::write(&staged_second, b"second audio").expect("stage the second replacement");
        {
            let mut commit = second
                .begin_commit()
                .expect("begin the second commit section");
            commit
                .commit_replacement(&staged_second, None)
                .expect_err("the displaced target must refuse the commit");
        }
        assert_eq!(
            fs::read(&song).expect("read the leaf back"),
            b"tagged audio",
            "the refused second commit must not replace the first replacement"
        );
        fs::remove_file(&staged_second)
            .expect("a refused commit leaves its staging name to the caller");
    }
}
