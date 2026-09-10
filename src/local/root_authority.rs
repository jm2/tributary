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

/// Platform identity of one published destination leaf, captured no-follow
/// by the write authority at publish time and carried opaquely by callers.
///
/// Rollback reversals compare this identity against whatever currently
/// occupies the leaf immediately before each reversal mutation: a mismatch
/// means a concurrent writer replaced the transfer's publication after it
/// committed, so reversing by pathname alone would destroy — or restore
/// over — a file the transfer does not own. Every such refusal is
/// fail-closed: the reversal is skipped and the caller reports a rollback
/// failure rather than touching the foreign object.
///
/// Device/inode (or volume/file-index) equality alone does not survive a
/// replacement that lands while the path is unlinked: filesystems may hand
/// the just-freed index straight back to the new occupant, which would
/// make the foreign object compare equal. Each identity therefore also
/// carries a creation-sensitive instant — the inode change time on Unix,
/// the creation timestamp on Windows — captured from the same object as
/// the index. A recreated object cannot inherit the recorded instant, so
/// a same-index replacement still compares unequal and is refused.
///
/// The fields are deliberately private: only this module captures and
/// compares identities; every other crate treats the value as an opaque
/// equality token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeafIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_secs: i64,
    #[cfg(unix)]
    change_nanos: i64,
    #[cfg(windows)]
    volume: u64,
    #[cfg(windows)]
    file_id: WindowsFileId,
    #[cfg(windows)]
    created: u64,
    #[cfg(not(any(unix, windows)))]
    _unsupported: (),
}

impl LeafIdentity {
    /// Whether both identities name the same underlying object, ignoring
    /// the creation-sensitive instant. Only the replace-publish machinery
    /// compares captures taken across its own atomic exchange, which
    /// legitimately updates the displaced object's change time between the
    /// bind capture and the post-swap verification capture; there the
    /// looser object check restores the intended coupling. Reversal
    /// verification compares full equality, where the instant is what
    /// defeats a same-path index reuse.
    #[cfg(any(unix, windows))]
    fn same_object(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode
        }
        #[cfg(windows)]
        {
            self.volume == other.volume && self.file_id == other.file_id
        }
    }
}

/// The outcome of an identity-checked reversal mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReversalOutcome {
    /// The mutation was performed.
    Reversed,
    /// The leaf was already absent, so nothing of the transfer's remained
    /// to reverse. Reported instead of a stale `NotFound` error.
    AlreadyAbsent,
    /// The leaf is occupied by a foreign object — its identity differs
    /// from the identity the transfer recorded at publish time — so the
    /// mutation was refused and nothing was changed. The caller must
    /// surface this as a rollback failure instead of deleting or
    /// restoring over a file the transfer does not own.
    RefusedForeignLeaf,
}

/// A publish whose rename already landed: the published-leaf identity bound
/// to the staged object before the winning rename, and the trailing
/// retained-parent revalidation.
///
/// The identity is captured from the staged leaf in the instant before the
/// rename — `rename` preserves the object — so it names the transfer's
/// publication even when a concurrent writer replaces the destination name
/// afterwards. It is `None` when the staged object's identity could not be
/// read before the rename; a reversal of such a record degrades to the
/// legacy path-only behavior.
///
/// The revalidation is REPORTED rather than enforced so a landed publish is
/// never surfaced as an unpublished failure: enforcing it would return an
/// ordinary I/O error for bytes already at the destination, and a caller
/// that reads such an error as "nothing was published" would strand the
/// published file — and, on an Overwrite commit, the bound backup of the
/// replaced original — with no rollback record. The caller folds a failed
/// revalidation into its own verified-publication error path (the write
/// authority's `CommitError::PublishVerification`).
pub(super) struct LandedPublish {
    /// No-follow identity of the published leaf: the refreshed capture
    /// when the post-publish proof verified the staged object, or the
    /// admitted staged capture when the proof failed (a vanishing leaf,
    /// an unreadable lookup, or a foreign interposition). Only a missing
    /// pre-rename capture records `None`, degrading the reversal to the
    /// legacy path-only behavior.
    pub(super) published_leaf: Option<LeafIdentity>,
    /// Result of the retained-parent revalidation that runs immediately
    /// after the rename and the identity capture.
    pub(super) post_validate: io::Result<()>,
}

/// The device number of an already-captured no-follow `stat` as the
/// identity's platform-neutral `u64` token. Linux reports `st_dev` as
/// `u64`; other Unix platforms report it signed, so the conversion is
/// explicit and maps out-of-range values to `0` rather than wrapping.
#[cfg(unix)]
fn leaf_device_number(stat: &rustix::fs::Stat) -> u64 {
    #[cfg(target_os = "linux")]
    {
        stat.st_dev
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        u64::try_from(stat.st_dev).unwrap_or(0)
    }
}

/// Widen an ABI-specific stat field to `i64` exactly. The concrete integer
/// type differs per Unix ABI (signed and unsigned, 32- and 64-bit), so the
/// conversion is kept generic: stat timestamps and nanosecond remainders
/// always fit `i64`, making the `0` fallback unreachable in practice.
#[cfg(unix)]
fn stat_field_i64<T>(value: T) -> i64
where
    i64: TryFrom<T>,
{
    i64::try_from(value).unwrap_or(0)
}

/// The inode-change instant of an already-captured no-follow `stat` as
/// (seconds, nanoseconds). This is the creation-sensitive half of
/// [`LeafIdentity`]: a recreated object cannot inherit the recorded
/// instant of the object it replaced, so same-path index reuse still
/// compares unequal. Every Unix ABI exposes the pair as `st_ctime` and
/// `st_ctime_nsec` (Apple's historical `st_ctimespec` spelling is a
/// structure, not a field of the unified `Stat` type), with ABI-specific
/// integer types that are widened explicitly.
#[cfg(unix)]
fn leaf_change_instant(stat: &rustix::fs::Stat) -> (i64, i64) {
    (
        stat_field_i64(stat.st_ctime),
        stat_field_i64(stat.st_ctime_nsec),
    )
}

/// Build a leaf identity from an already-captured no-follow `stat`. The
/// change instant is recorded only for non-directory leaves: a directory's
/// own change time legitimately moves while the transfer populates it (and
/// while the reversal empties it again), so pinning it would refuse every
/// legitimate directory reversal; a directory leaf is still protected by
/// its device/inode pair. A replaced non-directory leaf cannot inherit the
/// recorded change instant, so same-path index reuse still compares
/// unequal for files.
#[cfg(unix)]
fn leaf_identity_from_stat(stat: &rustix::fs::Stat) -> LeafIdentity {
    let is_directory =
        rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory;
    let (change_secs, change_nanos) = if is_directory {
        (0, 0)
    } else {
        leaf_change_instant(stat)
    };
    LeafIdentity {
        device: leaf_device_number(stat),
        inode: stat.st_ino,
        change_secs,
        change_nanos,
    }
}

/// Capture the no-follow identity of `leaf` inside the open `parent`
/// directory. `Ok(None)` means the leaf is absent; a capture that cannot
/// stat for any other reason is an error so callers never silently lose a
/// verification they asked for.
#[cfg(unix)]
fn leaf_identity_at(parent: &File, leaf: &OsStr) -> io::Result<Option<LeafIdentity>> {
    match rustix::fs::statat(parent, leaf, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(leaf_identity_from_stat(&stat))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(io::Error::from(error)),
    }
}

/// The outcome of the post-publish published-object proof.
///
/// The publish machinery binds the staged object's identity before the
/// rename and refreshes it after the rename lands, accepting the refresh
/// only when it names the same object the staged capture bound. The
/// refresh can fail to prove that: the leaf may have vanished, its
/// lookup may have errored, or a concurrent writer's replacement may
/// have interposed in the capture window. A failed proof must never
/// degrade the record to `None` — the legacy path-only reversal would
/// then clean the destination up by pathname alone, and whatever
/// replaced the publication would be destroyed unchecked.
#[cfg(unix)]
#[derive(Debug)]
enum PublishedIdentityProof {
    /// The post-publish lookup named the staged object: the refreshed
    /// identity is recorded, and a later same-name reversal compares
    /// exactly against the rename-updated change instant.
    Verified(LeafIdentity),
    /// The proof failed, but the staged capture still names what the
    /// transfer admits publishing: the admitted identity is recorded so
    /// a later reversal refuses a foreign replacement fail-closed
    /// instead of degrading to path-only cleanup.
    Admitted(LeafIdentity),
    /// No staged capture exists, so there is no admitted identity to
    /// preserve; the record degrades to the legacy path-only reversal.
    Uncaptured,
}

#[cfg(unix)]
impl PublishedIdentityProof {
    /// The identity to record on the landed publish.
    fn recorded(self) -> Option<LeafIdentity> {
        match self {
            Self::Verified(identity) | Self::Admitted(identity) => Some(identity),
            Self::Uncaptured => None,
        }
    }
}

/// Open a retained identity handle on `leaf` inside the open `parent`
/// directory: an `O_PATH|NOFOLLOW` descriptor that keeps the object alive
/// and re-statable across the publish rename. The post-publish proof uses
/// it to refresh the published object's identity EXACTLY — `fstat` names
/// the retained object itself, never whatever the name holds afterwards —
/// instead of inferring sameness from a device/index pair that an
/// immediate same-directory recreate can inherit (a freed index is the
/// first candidate the allocator hands back). `None` when the platform has
/// no path-less handle type or the open fails; the proof then degrades to
/// the legacy device/index comparison rather than changing behavior on
/// platforms the exact proof cannot serve.
#[cfg(unix)]
fn retained_leaf_handle(parent: &File, leaf: &OsStr) -> Option<File> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        rustix::fs::openat(
            parent,
            leaf,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .ok()
        .map(File::from)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (parent, leaf);
        None
    }
}

/// Refresh the identity of the object a retained handle holds. `fstat` is
/// permitted on `O_PATH` descriptors, so the handle needs neither read
/// access nor a re-lookup through the leaf's (possibly replaced) name; the
/// refresh reads the object's CURRENT metadata, which is what the rename
/// legitimately updated.
#[cfg(unix)]
fn retained_handle_identity(handle: &File) -> Option<LeafIdentity> {
    rustix::fs::fstat(handle)
        .ok()
        .map(|stat| leaf_identity_from_stat(&stat))
}

/// Refresh a pre-publish staged-object capture into the recorded published
/// identity. A rename legitimately updates the published object's change
/// instant, so the staged capture alone can never compare exactly
/// against a later same-name reversal verification; the post-publish lookup
/// here supplies the refreshed identity but is accepted ONLY when it names
/// the same object the staged capture bound — a concurrent writer's
/// replacement landing in the capture window is never recorded as the
/// publication (it would be matched and destroyed by a later rollback).
///
/// The acceptance proof is exact whenever a retained handle on the staged
/// object is available ([`retained_leaf_handle`], opened before the
/// rename): the refresh is accepted only when the destination's FULL
/// identity — device, index, and change instant — equals an `fstat` of the
/// retained object. A device/index-only comparison cannot distinguish the
/// staged object from a foreign replacement that inherited its recycled
/// index; the exact proof closes that hole. Both the `fstat` and the
/// destination lookup describe the same post-rename instant, so a landed
/// publish compares equal against its own retained object.
///
/// When the proof FAILS — the leaf vanished, the lookup errored, or a
/// foreign object interposed — the admitted staged identity is recorded
/// instead of `None`: the publication names the staged object regardless
/// of what happened after the rename, so a later reversal must verify
/// against that admitted identity and refuse a mismatched (foreign)
/// occupant fail-closed rather than degrade to path-only cleanup that
/// could destroy the replacement unchecked. Only a missing staged
/// capture — nothing admitted — degrades to the legacy path-only
/// reversal behavior.
#[cfg(unix)]
fn cross_checked_published_identity(
    parent: &File,
    leaf: &OsStr,
    staged: Option<LeafIdentity>,
    staged_object: Option<&File>,
) -> PublishedIdentityProof {
    let Some(staged) = staged else {
        return PublishedIdentityProof::Uncaptured;
    };
    if let Some(refreshed) = staged_object.and_then(retained_handle_identity) {
        return match leaf_identity_at(parent, leaf) {
            Ok(Some(current)) if current == refreshed => {
                PublishedIdentityProof::Verified(refreshed)
            }
            // A failed proof: preserve the admitted identity. An intact
            // publication whose lookup merely errored transiently may also
            // be refused by a later full-equality reversal — a safe
            // outcome, never a destructive one.
            Ok(_) | Err(_) => PublishedIdentityProof::Admitted(staged),
        };
    }
    // Degraded proof (no retained handle: a platform without `O_PATH`, or
    // the open failed): compare objects — device and index. A recreated
    // object that inherited the recycled index still passes here; that is
    // the hole the exact proof above closes, kept only where the exact
    // proof is unavailable.
    match leaf_identity_at(parent, leaf) {
        Ok(Some(current)) if current.same_object(&staged) => {
            PublishedIdentityProof::Verified(current)
        }
        // A failed proof: preserve the admitted identity. An intact
        // publication whose lookup merely errored transiently may also be
        // refused by a later full-equality reversal — a safe outcome, never
        // a destructive one.
        Ok(_) | Err(_) => PublishedIdentityProof::Admitted(staged),
    }
}

/// Capture the no-follow identity of the entry at `path`. `Ok(None)` means
/// the path is absent; anything else is an error.
///
/// The identity is read through an attributes-only handle instead of
/// directory metadata: the `Metadata` volume/index extensions are
/// nightly-only (`windows_by_handle`), while `GetFileInformationByHandle`
/// is stable through `windows-sys` and, with `FILE_FLAG_BACKUP_SEMANTICS`,
/// covers directory leaves as well. The handle also yields the creation
/// timestamp, the creation-sensitive half of [`LeafIdentity`] that keeps a
/// same-index replacement from comparing equal.
#[cfg(windows)]
fn leaf_identity_at_path(path: &Path) -> io::Result<Option<LeafIdentity>> {
    use std::mem::MaybeUninit;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: `wide` is a NUL-terminated UTF-16 path valid for the call;
    // every other argument is null or a constant. The attributes-only,
    // fully shared access cannot disturb a concurrent writer, and
    // `FILE_FLAG_BACKUP_SEMANTICS` admits directory leaves.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `handle` is live and `info` is a correctly sized, aligned
    // output buffer that the API fully initializes on success.
    let filled = unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) };
    // SAFETY: `handle` was created above and is closed exactly once on
    // every path.
    unsafe { CloseHandle(handle) };
    if filled == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call above initialized the complete structure.
    let info = unsafe { info.assume_init() };
    Ok(Some(leaf_identity_from_handle_info(&info)))
}

/// Build the no-follow [`LeafIdentity`] of an open object from an already
/// captured `BY_HANDLE_FILE_INFORMATION`. The creation timestamp is the
/// creation-sensitive half of the identity that keeps a same-index
/// replacement from comparing equal.
#[cfg(windows)]
fn leaf_identity_from_handle_info(
    info: &windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
) -> LeafIdentity {
    LeafIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file_id: WindowsFileId::Legacy(
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ),
        created: (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(info.ftCreationTime.dwLowDateTime),
    }
}

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

    /// Rename one leaf to another leaf inside the retained write parent,
    /// enforcing the trailing retained-parent revalidation.
    ///
    /// On Unix the rename is issued relative to the retained parent handle,
    /// so a parent or mount replacement between validation and publish
    /// cannot redirect it: the rename lands in the exact audited directory
    /// object or fails. When `no_replace` is set the publication fails if
    /// the final leaf already exists; the platform-native no-replace rename
    /// is tried first, then a link-based publish, and filesystems offering
    /// neither primitive fail closed with `Unsupported` — see
    /// [`Self::rename_no_replace_within`]. On Windows the no-replace publish
    /// mirrors that cascade with safe path operations — a hard-link publish,
    /// then the same fail-closed refusal — and the retained parent identity
    /// is revalidated immediately before and after.
    ///
    /// On success the no-follow identity of the published leaf is bound to
    /// the staged object immediately BEFORE the rename: `rename` preserves
    /// the object it moves, so the identity the staged leaf had in the
    /// instant before the publish is exactly the identity the destination
    /// leaf has in the instant after — even if a concurrent writer replaces
    /// the destination name later. A post-rename pathname lookup would
    /// instead report whatever replaced the publication in that window (or
    /// `None` for a leaf it watched vanish), so reversal would refuse or
    /// destroy the wrong object. Capture is best-effort: an identity that
    /// cannot be read before the rename reports `Ok(None)` and the reversal
    /// of that record degrades to the legacy path-only behavior.
    ///
    /// The trailing revalidation is ENFORCED here: callers for which a
    /// landed rename must never be mistaken for a failure (the forward
    /// publish) use [`Self::publish_within_directory`] instead and handle
    /// the reported revalidation themselves.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn rename_within_directory(
        &self,
        parent: &RetainedWriteParent,
        from_leaf: &OsStr,
        from_absolute: &Path,
        to_leaf: &OsStr,
        to_absolute: &Path,
        no_replace: bool,
    ) -> io::Result<Option<LeafIdentity>> {
        let landed = self.publish_within_directory(
            parent,
            from_leaf,
            from_absolute,
            to_leaf,
            to_absolute,
            no_replace,
        )?;
        landed.post_validate?;
        Ok(landed.published_leaf)
    }

    /// Publish one leaf onto another inside the retained write parent,
    /// reporting — not enforcing — the trailing retained-parent
    /// revalidation.
    ///
    /// The rename body is [`Self::rename_within_directory`]'s; the
    /// difference is the failure discipline after the rename has landed. A
    /// revalidation failure at that point is a verification failure about a
    /// publication that already happened: enforcing it here would surface
    /// an ordinary I/O error for bytes that are already at the destination,
    /// and a caller that treated that error as "nothing was published"
    /// would strand the published file — and, on an Overwrite commit, the
    /// bound backup of the replaced original — with no rollback record.
    /// The caller therefore receives [`LandedPublish`] and folds a failed
    /// revalidation into its own verified-publication error path.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn publish_within_directory(
        &self,
        parent: &RetainedWriteParent,
        from_leaf: &OsStr,
        from_absolute: &Path,
        to_leaf: &OsStr,
        to_absolute: &Path,
        no_replace: bool,
    ) -> io::Result<LandedPublish> {
        validate_leaf_name(from_leaf)?;
        validate_leaf_name(to_leaf)?;
        parent.validate_with(self)?;
        // Bind the published-leaf identity to the staged object before the
        // rename (see the doc comment above): the rename preserves the
        // object, so this capture names the publication no matter what a
        // concurrent writer does to the destination name afterwards.
        #[cfg(unix)]
        let staged_identity = leaf_identity_at(parent.handle(), from_leaf).ok().flatten();
        #[cfg(unix)]
        let staged_object = retained_leaf_handle(parent.handle(), from_leaf);
        #[cfg(windows)]
        let staged_identity = leaf_identity_at_path(from_absolute).ok().flatten();
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
        // Unix: a rename legitimately updates the published object's change
        // instant, so the staged capture alone can never compare exactly
        // against a later same-name reversal verification. Refresh it with
        // a post-publish lookup — accepted only when it names the staged
        // object, so a concurrent writer's replacement in the capture
        // window is never recorded as the publication. A failed proof — a
        // vanishing leaf, an unreadable capture, or an interposed foreign
        // object — records the ADMITTED staged identity rather than
        // degrading to `None`: a `None` record would reverse by pathname
        // alone and could destroy the replacement unchecked.
        //
        // Windows: the identity's creation-sensitive fields are immutable
        // across a rename, so the staged capture itself is the stable
        // published identity.
        #[cfg(unix)]
        let published_leaf = cross_checked_published_identity(
            parent.handle(),
            to_leaf,
            staged_identity,
            staged_object.as_ref(),
        )
        .recorded();
        #[cfg(windows)]
        let published_leaf = staged_identity;
        let post_validate = parent.validate_with(self);
        Ok(LandedPublish {
            published_leaf,
            post_validate,
        })
    }

    /// Replace the leaf at `to_leaf` with `from_leaf` inside the retained
    /// write parent, binding a backup of the replaced occupant first.
    ///
    /// This is the Overwrite publish. The occupant — whatever name
    /// `to_leaf` resolves to at the moment of replacement — is bound to
    /// `backup_leaf` so the caller can restore it on rollback, and the
    /// returned pair reports what happened:
    ///
    /// * `Ok((true, identity))` — `to_leaf` was occupied by the exact
    ///   object the backup names; the backup holds the replaced bytes and
    ///   `identity` is the no-follow identity of the published leaf.
    /// * `Ok((false, identity))` — `to_leaf` was absent; the staged leaf
    ///   took the name through the no-replace publish, so a concurrent
    ///   creation is never silently replaced-and-deleted. A creation
    ///   racing the publish re-enters the bind loop and is backed up
    ///   instead.
    ///
    /// Either way the published leaf's identity is bound to the staged
    /// object immediately before the winning publish (see
    /// [`Self::rename_within_directory`]); it is `None` when the staged
    /// object's identity could not be read in that instant.
    ///
    /// A directory occupant is refused with a typed `InvalidInput` error —
    /// a file publish never replaces a directory. The retained parent is
    /// revalidated immediately before the publish loop and immediately
    /// after it; the trailing revalidation is REPORTED on the returned
    /// [`LandedPublish`] rather than enforced, so a replace that already
    /// landed is never surfaced as an unpublished I/O failure — the caller
    /// folds it into its verified-publication error path.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn replace_within_directory(
        &self,
        parent: &RetainedWriteParent,
        from_leaf: &OsStr,
        from_absolute: &Path,
        to_leaf: &OsStr,
        to_absolute: &Path,
        backup_leaf: &OsStr,
        backup_absolute: &Path,
    ) -> io::Result<(bool, LandedPublish)> {
        validate_leaf_name(from_leaf)?;
        validate_leaf_name(to_leaf)?;
        validate_leaf_name(backup_leaf)?;
        parent.validate_with(self)?;
        let (replaced, published_leaf) = replace_publish_loop(
            parent.handle(),
            from_leaf,
            from_absolute,
            to_leaf,
            to_absolute,
            backup_leaf,
            backup_absolute,
        )?;
        let post_validate = parent.validate_with(self);
        Ok((
            replaced,
            LandedPublish {
                published_leaf,
                post_validate,
            },
        ))
    }

    /// Remove the regular file at `relative` beneath the retained root.
    ///
    /// Every platform walks the intermediate components no-follow from the
    /// retained root and retains the leaf's parent directory for the
    /// duration of the removal: Unix through parent-directory handles, and
    /// Windows through reparse-refusing directory opens that pin each walk
    /// level, so an intermediate symlink or replaced directory cannot
    /// redirect the removal outside the retained root. The final leaf is
    /// typed no-follow: it must not be a directory (a typed `InvalidInput`
    /// refusal, not a platform permission error); a symlink is removed as a
    /// link.
    pub(super) fn remove_regular_file_within(&self, relative: &Path) -> io::Result<()> {
        self.remove_regular_file_within_verified(relative, None)
            .map(|_| ())
    }

    /// Remove the regular file at `relative` beneath the retained root,
    /// verifying against the publish-time leaf identity when one is
    /// supplied. See [`ReversalOutcome`] for the outcome semantics: a
    /// destination that no longer names the transfer's publication is
    /// reported as [`ReversalOutcome::RefusedForeignLeaf`] and left
    /// untouched.
    pub(super) fn remove_regular_file_within_verified(
        &self,
        relative: &Path,
        expected: Option<LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        let components = strict_relative_components(relative)?;
        self.validate()?;
        let parent = self.retain_write_parent_directory(&components)?;
        let outcome = Self::remove_regular_leaf_entry(self, &parent, &components, expected)?;
        drop(parent);
        self.validate()?;
        Ok(outcome)
    }

    /// Remove the final component of `components` through the retained
    /// parent, refusing a directory leaf with the typed `InvalidInput`
    /// error. When `expected` is supplied and the leaf is occupied by a
    /// different object, the removal is refused and
    /// [`ReversalOutcome::RefusedForeignLeaf`] is reported without touching
    /// the foreign object. Per-platform leaf-removal body of
    /// [`Self::remove_regular_file_within`].
    fn remove_regular_leaf_entry(
        authority: &Self,
        parent: &RetainedWriteParent,
        components: &[OsString],
        expected: Option<LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        #[cfg(unix)]
        {
            let _ = authority;
            remove_regular_leaf_entry_unix(
                parent.handle(),
                components.last().expect("non-empty components"),
                expected,
            )
        }
        #[cfg(windows)]
        {
            remove_regular_leaf_entry_windows(authority, parent, components, expected)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (authority, parent, components, expected);
            Err(unsupported_platform())
        }
    }

    /// Remove the empty directory at `relative` beneath the retained root,
    /// anchored to the retained root exactly like
    /// [`Self::remove_regular_file_within`]. The final leaf must be a real
    /// directory; a symlink or junction leaf is refused with a typed
    /// `InvalidInput` error instead of being unlinked through its target.
    pub(super) fn remove_directory_within(&self, relative: &Path) -> io::Result<()> {
        self.remove_directory_within_verified(relative, None)
            .map(|_| ())
    }

    /// Remove the empty directory at `relative` beneath the retained root,
    /// verifying against the creation-time leaf identity when one is
    /// supplied. See [`ReversalOutcome`] for the outcome semantics.
    pub(super) fn remove_directory_within_verified(
        &self,
        relative: &Path,
        expected: Option<LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        let components = strict_relative_components(relative)?;
        self.validate()?;
        let parent = self.retain_write_parent_directory(&components)?;
        let outcome = Self::remove_directory_leaf_entry(self, &parent, &components, expected)?;
        drop(parent);
        self.validate()?;
        Ok(outcome)
    }

    /// Restore a previously saved backup over its destination slot within
    /// the same parent directory, verifying against the published-leaf
    /// identity when one is supplied. See
    /// [`MountedWriteAuthority::restore_relative_file_verified`](crate::local::write_authority::MountedWriteAuthority::restore_relative_file_verified)
    /// for the full contract.
    pub(super) fn restore_relative_file_verified(
        &self,
        backup_relative: &Path,
        destination_relative: &Path,
        expected: Option<&LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        let backup_components = strict_relative_components(backup_relative)?;
        let destination_components = strict_relative_components(destination_relative)?;
        if parent_components_of(&backup_components) != parent_components_of(&destination_components)
        {
            return Err(invalid_input(
                "backup and destination must share a parent directory",
            ));
        }
        let parent_components = parent_components_of(&destination_components);
        self.validate()?;
        let parent = self.retain_write_parent(&parent_components)?;
        let backup_leaf = backup_components.last().expect("non-empty").clone();
        let destination_leaf = destination_components.last().expect("non-empty").clone();
        // Identity gate on the destination slot before anything is moved:
        // a slot occupied by a different object than the recorded
        // publication belongs to a concurrent writer and must not be
        // replaced by the backup.
        if destination_slot_is_foreign(
            &self.root,
            &parent,
            &destination_leaf,
            &destination_components,
            expected,
        )? {
            return Ok(ReversalOutcome::RefusedForeignLeaf);
        }
        #[cfg(unix)]
        {
            restore_backup_by_exchange_unix(
                parent.handle(),
                backup_leaf.as_os_str(),
                destination_leaf.as_os_str(),
                self.root.join(backup_relative).as_path(),
                self.root.join(destination_relative).as_path(),
                expected,
            )
        }
        #[cfg(windows)]
        {
            self.rename_within_directory(
                &parent,
                backup_leaf.as_os_str(),
                self.root.join(backup_relative).as_path(),
                destination_leaf.as_os_str(),
                self.root.join(destination_relative).as_path(),
                false,
            )?;
            Ok(ReversalOutcome::Reversed)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (
                backup_leaf,
                destination_leaf,
                backup_relative,
                destination_relative,
            );
            Err(unsupported_platform())
        }
    }

    /// Best-effort no-follow identity of the leaf at `relative` beneath the
    /// retained root. `None` when the leaf is absent, the path is not a
    /// strict relative path, or the identity cannot be captured: callers
    /// record `None` and the eventual reversal degrades to the legacy
    /// path-only behavior rather than failing an operation that has not
    /// even been attempted yet.
    pub(super) fn relative_leaf_identity(&self, relative: &Path) -> Option<LeafIdentity> {
        let components = strict_relative_components(relative).ok()?;
        if components.is_empty() {
            return None;
        }
        #[cfg(unix)]
        {
            let parent_components = parent_components_of(&components);
            let parent = self.retain_write_parent(&parent_components).ok()?;
            let leaf = components.last().expect("non-empty").clone();
            leaf_identity_at(parent.handle(), &leaf).ok().flatten()
        }
        #[cfg(windows)]
        {
            leaf_identity_at_path(&join_components(&self.root, &components))
                .ok()
                .flatten()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = components;
            None
        }
    }

    /// Remove the final component of `components` through the retained
    /// parent as an empty directory, refusing a non-directory leaf with the
    /// typed `InvalidInput` error. When `expected` is supplied and the leaf
    /// is occupied by a different object, the removal is refused and
    /// [`ReversalOutcome::RefusedForeignLeaf`] is reported without touching
    /// the foreign object. Per-platform leaf-removal body of
    /// [`Self::remove_directory_within`].
    fn remove_directory_leaf_entry(
        authority: &Self,
        parent: &RetainedWriteParent,
        components: &[OsString],
        expected: Option<LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        #[cfg(unix)]
        {
            let _ = authority;
            remove_directory_leaf_entry_unix(
                parent.handle(),
                components.last().expect("non-empty components"),
                expected,
            )
        }
        #[cfg(windows)]
        {
            remove_directory_leaf_entry_windows(authority, parent, components, expected)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (authority, parent, components, expected);
            Err(unsupported_platform())
        }
    }

    /// Create every component of `components` as a directory beneath the
    /// retained root, walking and creating each level no-follow from the
    /// retained root handle on Unix. An existing directory is tolerated
    /// only when it is a real directory, never a symlink.
    pub(super) fn create_directories_within(
        &self,
        components: &[OsString],
    ) -> io::Result<Vec<usize>> {
        if components.is_empty() {
            return Err(invalid_input(
                "directory creation requires a path below the mounted root",
            ));
        }
        self.validate()?;
        #[cfg(unix)]
        {
            let mut current = self.root_handle.file.try_clone()?;
            let mut created = Vec::new();
            for (index, component) in components.iter().enumerate() {
                let (opened, created_component) = ensure_directory_component(&current, component)?;
                if created_component {
                    created.push(index);
                }
                current = opened;
                ensure_boundary(self.boundary, &current)?;
            }
            self.validate()?;
            Ok(created)
        }
        #[cfg(windows)]
        {
            let created = create_directory_tree_by_path(&self.root, components)?;
            self.validate()?;
            Ok(created)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = components;
            Err(unsupported_platform())
        }
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

/// The parent prefix of a component list, as a relative path. The root
/// itself (an empty path) for a single-component list; `retain_write_parent`
/// binds the root directory for that case.
fn parent_components_of(components: &[OsString]) -> PathBuf {
    let mut path = PathBuf::new();
    for component in &components[..components.len().saturating_sub(1)] {
        path.push(component);
    }
    path
}

/// Read the no-follow identity of a destination slot and report whether it
/// is held by a foreign object: `Ok(true)` means the slot's current
/// occupant and `expected` are both known and differ, so the occupant
/// belongs to a concurrent writer and the caller must not touch it. When
/// either side is unknown the gate degrades to the legacy path-only
/// behavior and the slot is reported not foreign.
// Every parameter feeds exactly one platform's identity probe: unix reads
// the leaf through the retained parent handle, Windows through the joined
// path, so each cfg combination leaves the other platform's parameters
// unused. A platform-conditional allow cannot cover both directions.
#[allow(unused_variables)]
fn destination_slot_is_foreign(
    root: &Path,
    parent: &RetainedWriteParent,
    destination_leaf: &OsStr,
    destination_components: &[OsString],
    expected: Option<&LeafIdentity>,
) -> io::Result<bool> {
    #[cfg(unix)]
    let current = leaf_identity_at(parent.handle(), destination_leaf)?;
    #[cfg(windows)]
    let current = leaf_identity_at_path(&join_components(root, destination_components))?;
    #[cfg(not(any(unix, windows)))]
    let current: Option<LeafIdentity> = None;
    Ok(matches!(
        (expected, current),
        (Some(expected), Some(current)) if expected != &current
    ))
}

/// What kind of leaf a reversal quarantine removes: the typing performed on
/// the quarantined object before it is removed.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum QuarantinedLeafKind {
    /// The object must be a regular file or symlink; a directory is refused.
    RegularFile,
    /// The object must be a directory, removed with `AT_REMOVEDIR`.
    EmptyDirectory,
}

/// Allocate a unique, private tombstone leaf for reversal quarantine. The
/// name is created `O_EXCL` so it can never collide with a concurrent
/// writer's file, and no other process ever learns it: after the quarantine
/// exchange, the verify-then-remove on the tombstone name is race-free.
#[cfg(unix)]
fn create_reversal_tombstone(parent: &File) -> io::Result<OsString> {
    use rustix::fs::{Mode, OFlags};

    for _ in 0..8 {
        let mut name = OsString::from(".tributary-reversal-");
        name.push(Uuid::new_v4().to_string());
        name.push(".tmp");
        match rustix::fs::openat(
            parent,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_bits_truncate(0o600),
        ) {
            Ok(descriptor) => {
                drop(File::from(descriptor));
                return Ok(name);
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(io::Error::from(error)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a private reversal tombstone name",
    ))
}

/// Atomically exchange the names `leaf` and `tombstone` within `parent`.
/// Used both to quarantine a leaf ahead of its reversal and to restore the
/// original naming when a reversal must be refused or has failed.
#[cfg(unix)]
fn exchange_leaf_names(parent: &File, leaf: &OsStr, tombstone: &OsStr) -> io::Result<()> {
    use rustix::fs::RenameFlags;

    rustix::fs::renameat_with(parent, leaf, parent, tombstone, RenameFlags::EXCHANGE)
        .map_err(io::Error::from)
}

/// Unix body of the backup restoration: put the backup back over its
/// destination slot without an uncoupled check-then-replace window. An
/// absent slot takes the backup through a no-replace rename, so a
/// concurrent creation is refused — never destroyed. An occupied slot is
/// first verified exactly against the recorded publication identity (the
/// publication has not been renamed by us, so the creation-sensitive fields
/// compare), then exchanged atomically with the backup: the slot receives
/// the backup's object while the captured prior occupant — the transfer's
/// publication, or a concurrent writer's interposition that raced past the
/// exact verification — is preserved at the private backup name until an
/// object-level (device and inode) coupling check confirms it is the
/// verified publication and discards it. A filesystem without an exchange
/// primitive fails closed rather than replacing unverified.
#[cfg(unix)]
fn restore_backup_by_exchange_unix(
    parent: &File,
    backup_leaf: &OsStr,
    destination_leaf: &OsStr,
    backup_absolute: &Path,
    destination_absolute: &Path,
    expected: Option<&LeafIdentity>,
) -> io::Result<ReversalOutcome> {
    use rustix::fs::{statat, unlinkat, AtFlags, RenameFlags};

    match rename_no_replace_within_parent(
        parent,
        backup_leaf,
        backup_absolute,
        destination_leaf,
        destination_absolute,
    ) {
        // The slot was empty: the backup took the name atomically.
        Ok(()) => return Ok(ReversalOutcome::Reversed),
        // The slot is occupied: verify-then-exchange below.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // Verify the occupied slot BEFORE anything moves. The recorded
    // publication has not been renamed since its capture, so the
    // creation-sensitive fields compare exactly here: a slot naming a
    // different object — a concurrent writer's interposition, or a
    // same-index replacement — is refused without a single rename.
    match statat(parent, destination_leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let found = leaf_identity_from_stat(&stat);
            if expected.is_some_and(|expected| expected != &found) {
                return Ok(ReversalOutcome::RefusedForeignLeaf);
            }
        }
        // The slot emptied between the no-replace refusal and this stat:
        // re-run the atomic no-replace publish rather than exchange with a
        // name that may vanish again.
        Err(rustix::io::Errno::NOENT) => {
            return rename_no_replace_within_parent(
                parent,
                backup_leaf,
                backup_absolute,
                destination_leaf,
                destination_absolute,
            )
            .map(|()| ReversalOutcome::Reversed)
        }
        Err(error) => return Err(io::Error::from(error)),
    }
    match rustix::fs::renameat_with(
        parent,
        backup_leaf,
        parent,
        destination_leaf,
        RenameFlags::EXCHANGE,
    ) {
        Ok(()) => {}
        // No exchange primitive: fail closed. The exchange never landed, so
        // the destination still names the publication and the backup still
        // names the original occupant — nothing was displaced.
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::OPNOTSUPP) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem offers no atomic exchange primitive; refusing a backup restore \
                 that could destroy a concurrent writer's interposition unverified",
            ));
        }
        Err(error) => return Err(io::Error::from(error)),
    }
    // The destination now names the backup's object; the backup name holds
    // the captured prior occupant. The exchange legitimately updated the
    // captured object's change instant, so the coupling check compares
    // objects — device and inode — not change instants: whatever now sits
    // at the private backup name must be the object the slot was verified
    // against, or the exchange captured an interposer and must undo.
    match statat(parent, backup_leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let found = leaf_identity_from_stat(&stat);
            let coupled = expected.is_none_or(|expected| found.same_object(expected));
            if !coupled {
                // An interposed writer's object was captured: exchange it
                // back and refuse. The exchange preserves both sides, so
                // refusing destroys nothing.
                rustix::fs::renameat_with(
                    parent,
                    backup_leaf,
                    parent,
                    destination_leaf,
                    RenameFlags::EXCHANGE,
                )
                .map_err(io::Error::from)?;
                return Ok(ReversalOutcome::RefusedForeignLeaf);
            }
            // The captured occupant is exactly the publication being
            // reversed — or no identity was recorded and the slot is the
            // transfer's own by contract. Discard the redundant link.
            unlinkat(parent, backup_leaf, AtFlags::empty()).map_err(io::Error::from)?;
            Ok(ReversalOutcome::Reversed)
        }
        Err(verification) => {
            // The captured occupant cannot be verified: put it back rather
            // than remove an unverifiable object.
            let _ = rustix::fs::renameat_with(
                parent,
                backup_leaf,
                parent,
                destination_leaf,
                RenameFlags::EXCHANGE,
            );
            Err(io::Error::from(verification))
        }
    }
}

/// Unix body shared by the identity-coupled reversals. Two verification
/// phases bracket the quarantine so the removal can never destroy an object
/// the transfer does not own:
///
/// 1. The leaf is verified EXACTLY against the recorded identity while it
///    still sits at its published name — nothing has renamed it since the
///    capture, so the creation-sensitive fields (which guard against
///    same-index inode reuse) compare strictly. A foreign occupant is
///    refused here without a single rename.
/// 2. The verified leaf is quarantined with an atomic exchange against a
///    private tombstone name no concurrent writer can know, re-verified at
///    the tombstone with the object-level check — the exchange
///    legitimately updated the object's change instant, so the coupling
///    compares device and inode, not change instants — and only then
///    removed. An interposition that raced between the phases is captured
///    by the exchange, exchanged back, and refused: captured, never
///    destroyed.
///
/// The historical check-then-unlink deleted whatever bore the leaf name at
/// unlink time, so a concurrent writer's replacement landing between the
/// two syscalls was destroyed while the reversal still reported an
/// identity-verified outcome; a filesystem without an exchange primitive
/// fails closed instead of racing.
#[cfg(unix)]
fn quarantine_and_remove_leaf_unix(
    parent: &File,
    leaf: &OsStr,
    expected: Option<LeafIdentity>,
    kind: QuarantinedLeafKind,
    operation: &str,
) -> io::Result<ReversalOutcome> {
    use rustix::fs::{statat, unlinkat, AtFlags};

    // Phase 1: exact verification at the still-published name.
    let verified = match statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let found = leaf_identity_from_stat(&stat);
            if expected.is_some_and(|expected| expected != found) {
                // A concurrent writer's replacement — or a same-index
                // reuse of the recorded inode — occupies the leaf: refuse
                // without touching it.
                return Ok(ReversalOutcome::RefusedForeignLeaf);
            }
            let is_directory = rustix::fs::FileType::from_raw_mode(stat.st_mode)
                == rustix::fs::FileType::Directory;
            match kind {
                QuarantinedLeafKind::RegularFile if is_directory => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to remove a directory through remove_relative_file",
                    ));
                }
                QuarantinedLeafKind::EmptyDirectory if !is_directory => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to remove a non-directory through remove_relative_directory",
                    ));
                }
                _ => {}
            }
            found
        }
        Err(rustix::io::Errno::NOENT) => return Ok(ReversalOutcome::AlreadyAbsent),
        Err(error) => return Err(io::Error::from(error)),
    };

    // Phase 2: quarantine by atomic exchange against a private tombstone.
    let tombstone = create_reversal_tombstone(parent)?;
    let release_tombstone_file =
        |parent: &File| -> io::Result<()> { exchange_leaf_names(parent, leaf, &tombstone) };
    match rustix::fs::renameat_with(
        parent,
        leaf,
        parent,
        &tombstone,
        rustix::fs::RenameFlags::EXCHANGE,
    ) {
        Ok(()) => {}
        // The leaf vanished after the exact verification: nothing of the
        // transfer's remained to reverse.
        Err(rustix::io::Errno::NOENT) => {
            let _ = unlinkat(parent, &tombstone, AtFlags::empty());
            return Ok(ReversalOutcome::AlreadyAbsent);
        }
        // No exchange primitive: fail closed rather than racing a
        // concurrent writer with an uncoupled check-then-unlink.
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::OPNOTSUPP) => {
            let _ = unlinkat(parent, &tombstone, AtFlags::empty());
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem offers no atomic exchange primitive; refusing a reversal that \
                 could remove a concurrent writer's replacement unverified",
            ));
        }
        Err(error) => {
            let _ = unlinkat(parent, &tombstone, AtFlags::empty());
            return Err(io::Error::from(error));
        }
    }
    // The exchange landed: `leaf` now names our empty tombstone file and
    // `tombstone` names exactly the object the leaf named at the exchange
    // instant. Capture the parked tombstone file's identity now, while the
    // exchange has just placed it: the public leaf's final cleanup must
    // remove exactly that object and nothing a concurrent writer may race
    // into the leaf name while the verified object is being removed.
    // Coupling check through the private tombstone name: the
    // object must be the one phase 1 verified (device and inode — the
    // exchange updated its change instant), otherwise an interposer was
    // captured and is exchanged back before refusing.
    let parked_tombstone = leaf_identity_at(parent, leaf).ok().flatten();
    let removal_flags = match kind {
        QuarantinedLeafKind::RegularFile => AtFlags::empty(),
        QuarantinedLeafKind::EmptyDirectory => AtFlags::REMOVEDIR,
    };
    match statat(parent, &tombstone, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let quarantined = leaf_identity_from_stat(&stat);
            if !quarantined.same_object(&verified) {
                if let Err(restore) = release_tombstone_file(parent) {
                    return Err(io::Error::new(
                        restore.kind(),
                        format!(
                            "{operation} refused a foreign replacement: {restore}; the \
                             replacement could not be restored to its leaf name and remains \
                             at a private .tributary-reversal-* tombstone"
                        ),
                    ));
                }
                return Ok(ReversalOutcome::RefusedForeignLeaf);
            }
            if let Err(error) = unlinkat(parent, &tombstone, removal_flags).map_err(io::Error::from)
            {
                // The verified object could not be removed. Restore it to
                // the leaf name so no private-name litter holds live data,
                // then surface the failure.
                if let Err(restore) = release_tombstone_file(parent) {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "{operation} failed: {error}; the quarantined object could not be \
                             restored to its leaf name ({restore}) and remains at a private \
                             .tributary-reversal-* tombstone"
                        ),
                    ));
                }
                return Err(error);
            }
            // The verified object is removed. `leaf` still names the
            // private empty tombstone file the exchange parked there —
            // unless a concurrent writer raced a replacement into the
            // public name, in which case an unconditional unlink would
            // destroy that writer's object. Release the leaf through the
            // race-conditioned helper: only the parked tombstone file is
            // ever removed.
            release_parked_reversal_leaf(parent, leaf, parked_tombstone, &tombstone)?;
            Ok(ReversalOutcome::Reversed)
        }
        Err(verification) => {
            // The quarantined object cannot be verified: put it back rather
            // than remove an unverifiable object.
            let verification = io::Error::from(verification);
            if let Err(restore) = release_tombstone_file(parent) {
                return Err(io::Error::new(
                    verification.kind(),
                    format!(
                        "{operation} failed before removal: {verification}; the quarantined \
                         object could not be restored ({restore}) and remains at a private \
                         .tributary-reversal-* tombstone"
                    ),
                ));
            }
            Err(verification)
        }
    }
}

/// Release the private tombstone file the quarantine exchange parked at
/// the public `leaf` name, after the verified object itself has been
/// removed through the now-vacant private `tombstone` name.
///
/// The cleanup is conditioned on the object actually parked there: a
/// concurrent writer that raced a replacement into the public leaf name
/// owns the name now, and an unconditional unlink would destroy that
/// writer's object. The leaf's occupant is therefore moved — by an
/// atomic rename, never a check-then-unlink — to the vacant private
/// tombstone name, where the capture can be verified race-free against
/// `parked`, the identity captured for the parked tombstone file right
/// after the exchange. Only the verified parked tombstone file is
/// removed; anything else is renamed back to the public name untouched
/// (the restore is itself no-replace, so a fresh concurrent creation at
/// the leaf is left in place and the parked file stays at the private
/// name rather than displacing it).
///
/// `parked` is `None` when the parked file's identity could not be
/// captured; verification cannot admit an unknown object, so the
/// occupant is renamed back in that case as well — the public name keeps
/// whatever it holds and nothing is destroyed.
#[cfg(unix)]
fn release_parked_reversal_leaf(
    parent: &File,
    leaf: &OsStr,
    parked: Option<LeafIdentity>,
    tombstone: &OsStr,
) -> io::Result<()> {
    use rustix::fs::{statat, unlinkat, AtFlags, RenameFlags};

    // Atomically vacate the public leaf into the private tombstone name.
    // `Err(NOENT)` means the leaf is already absent — nothing of the
    // transfer's is parked there and there is nothing to release.
    match rustix::fs::renameat(parent, leaf, parent, tombstone) {
        Ok(()) => {}
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(io::Error::from(error)),
    }
    // Race-free verify-then-remove on the private name.
    let parked_object = match statat(parent, tombstone, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Some(leaf_identity_from_stat(&stat)),
        Err(_) => None,
    };
    if parked_object.is_some_and(|object| parked.is_some_and(|parked| object.same_object(&parked)))
    {
        unlinkat(parent, tombstone, AtFlags::empty()).map_err(io::Error::from)
    } else {
        // A concurrent writer's object (or an unverifiable one) was
        // parked: move it back to the public name untouched. A fresh
        // concurrent creation at the leaf makes the restore fail with
        // `AlreadyExists` — the writer's object stays and the parked
        // object remains at the private tombstone name, which the error
        // below documents.
        match rustix::fs::renameat_with(parent, tombstone, parent, leaf, RenameFlags::NOREPLACE) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::EXIST) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the reversal leaf was recreated by a concurrent writer during cleanup; \
                 the parked object was left at a private .tributary-reversal-* tombstone \
                 and the writer's object was left untouched",
            )),
            Err(error) => Err(io::Error::from(error)),
        }
    }
}

/// Unix body of the regular-file reversal: quarantine the leaf with an
/// atomic exchange against a private tombstone (see
/// [`quarantine_and_remove_leaf_unix`]), gate the quarantined object on the
/// publish-time identity, refuse a directory leaf with the typed
/// `InvalidInput` error, then remove the verified object. A symlink leaf is
/// removed as a link.
#[cfg(unix)]
fn remove_regular_leaf_entry_unix(
    parent: &File,
    leaf: &OsStr,
    expected: Option<LeafIdentity>,
) -> io::Result<ReversalOutcome> {
    quarantine_and_remove_leaf_unix(
        parent,
        leaf,
        expected,
        QuarantinedLeafKind::RegularFile,
        "remove_relative_file",
    )
}

/// What kind of leaf the Windows object-conditioned removal must find: the
/// typing performed on the captured object before it is deleted.
#[cfg(windows)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum WindowsRemovalKind {
    /// The object must not be a real directory; a directory leaf is
    /// refused. A reparse-point leaf is removed as itself (the link),
    /// never through its target.
    RegularFile,
    /// The object must be a real (non-reparse) directory.
    EmptyDirectory,
}

/// Remove the leaf at `path` by object, not by pathname.
///
/// The leaf is opened ONCE with delete access — no reparse following, so a
/// symlink or junction leaf is opened as itself — and the identity of the
/// open object is compared against `expected` before anything is deleted:
/// a mismatch reports [`ReversalOutcome::RefusedForeignLeaf`] without
/// touching the foreign object. The deletion itself is then requested
/// through the open handle (POSIX-semantics deletion when the filesystem
/// offers it, delete-on-close otherwise), so it is bound to the object the
/// identity was verified against. A concurrent writer that replaces the
/// PATH after the handle is opened can never make this removal destroy the
/// replacement: the handle keeps naming the verified object, and the
/// writer's object at the path is untouched by the close.
///
/// `Ok(AlreadyAbsent)` reports the leaf was already absent. A filesystem
/// that offers no handle-conditioned delete primitive fails closed with
/// `Unsupported` rather than falling back to an unverified check-then-
/// `remove_file` race.
#[cfg(windows)]
fn remove_leaf_by_object(
    path: &Path,
    expected: Option<LeafIdentity>,
    kind: WindowsRemovalKind,
) -> io::Result<ReversalOutcome> {
    use std::mem::MaybeUninit;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, DELETE,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: `wide` is a NUL-terminated UTF-16 path valid for the call;
    // every other argument is null or a constant. `OPEN_REPARSE_POINT` keeps
    // a symlink or junction leaf from resolving to its target, and
    // `FILE_FLAG_BACKUP_SEMANTICS` admits directory leaves.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DELETE | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(ReversalOutcome::AlreadyAbsent)
        } else {
            Err(error)
        };
    }
    let outcome = (|| -> io::Result<ReversalOutcome> {
        let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: `handle` is live and `info` is a correctly sized, aligned
        // output buffer that the API fully initializes on success.
        let filled = unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) };
        if filled == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful call above initialized the complete structure.
        let info = unsafe { info.assume_init() };
        let found = leaf_identity_from_handle_info(&info);
        if expected.is_some_and(|expected| expected != found) {
            return Ok(ReversalOutcome::RefusedForeignLeaf);
        }
        let is_reparse = info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let is_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        match kind {
            WindowsRemovalKind::RegularFile if is_directory && !is_reparse => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to remove a directory through remove_relative_file",
                ));
            }
            WindowsRemovalKind::EmptyDirectory if !is_directory || is_reparse => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to remove a non-directory through remove_relative_directory",
                ));
            }
            _ => {}
        }
        request_object_deletion(handle)?;
        Ok(ReversalOutcome::Reversed)
    })();
    // SAFETY: `handle` was created above and is closed exactly once on
    // every path. A requested deletion commits at this close.
    unsafe { CloseHandle(handle) };
    // A filesystem without POSIX-semantics deletion reports some failures
    // (a non-empty directory, a pinned file) only at close time, where they
    // are not surfaced as an error. A read-only existence check confirms
    // the verified object actually went away; it can never destroy
    // anything, and a replacement that raced in after the close is left
    // untouched and reported as a failure rather than assumed reversed.
    match outcome {
        Ok(ReversalOutcome::Reversed) => match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ReversalOutcome::Reversed),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::other(
                "the verified object could not be deleted by handle; the leaf was left \
                 in place",
            )),
        },
        other => other,
    }
}

/// Mark the open object for deletion through its handle: POSIX semantics
/// when the filesystem supports them (the name disappears at close even
/// with concurrent handles on the same object), delete-on-close otherwise.
/// Both bind the deletion to the object the handle names, so the public
/// path can never be re-resolved onto a different object between the
/// identity verification and the removal.
#[cfg(windows)]
fn request_object_deletion(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<()> {
    use std::mem::size_of;

    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, FileDispositionInfoEx, SetFileInformationByHandle,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO,
        FILE_DISPOSITION_INFO_EX,
    };

    let request_ex = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: `handle` is a live file handle and the buffer is a correctly
    // initialized structure of exactly the class' expected size.
    let requested = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfoEx,
            core::ptr::from_ref(&request_ex).cast::<core::ffi::c_void>(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if requested != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // Pre-1709 Windows and filesystems without POSIX-semantics deletion
    // reject the extended class. Basic delete-on-close still binds the
    // deletion to the verified object.
    let unsupported = matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_INVALID_FUNCTION as i32
                || code == ERROR_INVALID_PARAMETER as i32
                || code == ERROR_NOT_SUPPORTED as i32
    );
    if !unsupported {
        return Err(error);
    }
    let request_basic = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `handle` is a live file handle and the buffer is a correctly
    // initialized structure of exactly the class' expected size.
    let requested_basic = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfo,
            core::ptr::from_ref(&request_basic).cast::<core::ffi::c_void>(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if requested_basic != 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

/// Windows body of the regular-file reversal. The leaf is removed by
/// object through a delete-access handle (see [`remove_leaf_by_object`]):
/// the recorded identity is verified against the open object before the
/// deletion is requested, a directory leaf is refused with the typed
/// `InvalidInput` error, and a symlink or junction leaf is removed as
/// itself, never through its target. The retained parent is revalidated
/// immediately before the removal.
#[cfg(windows)]
fn remove_regular_leaf_entry_windows(
    authority: &MountedRootAuthority,
    parent: &RetainedWriteParent,
    components: &[OsString],
    expected: Option<LeafIdentity>,
) -> io::Result<ReversalOutcome> {
    parent.validate_with(authority)?;
    let final_path = join_components(&authority.root, components);
    remove_leaf_by_object(&final_path, expected, WindowsRemovalKind::RegularFile)
}

/// Unix body of the empty-directory reversal: quarantine the leaf with an
/// atomic exchange against a private tombstone (see
/// [`quarantine_and_remove_leaf_unix`]), gate the quarantined object on the
/// creation-time identity, refuse a non-directory leaf with the typed
/// `InvalidInput` error, then remove the verified empty directory.
#[cfg(unix)]
fn remove_directory_leaf_entry_unix(
    parent: &File,
    leaf: &OsStr,
    expected: Option<LeafIdentity>,
) -> io::Result<ReversalOutcome> {
    quarantine_and_remove_leaf_unix(
        parent,
        leaf,
        expected,
        QuarantinedLeafKind::EmptyDirectory,
        "remove_relative_directory",
    )
}

/// Windows body of the empty-directory reversal: same identity gate and
/// typing as the regular-file reversal, refusing a non-directory leaf and
/// removing the verified empty directory by object through a
/// delete-access handle (see [`remove_leaf_by_object`]).
#[cfg(windows)]
fn remove_directory_leaf_entry_windows(
    authority: &MountedRootAuthority,
    parent: &RetainedWriteParent,
    components: &[OsString],
    expected: Option<LeafIdentity>,
) -> io::Result<ReversalOutcome> {
    parent.validate_with(authority)?;
    let final_path = join_components(&authority.root, components);
    remove_leaf_by_object(&final_path, expected, WindowsRemovalKind::EmptyDirectory)
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
///
/// Reports whether this invocation actually created the component: a
/// component that already existed (and was adopted) is reported as not
/// created, so callers can record ownership of exactly what they created.
#[cfg(unix)]
fn ensure_directory_component(current: &File, component: &OsString) -> io::Result<(File, bool)> {
    use rustix::fs::{AtFlags, Mode, OFlags};

    let created = match rustix::fs::mkdirat(current, component, Mode::from_bits_truncate(0o777)) {
        Ok(()) => true,
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
            false
        }
        Err(error) => return Err(io::Error::from(error)),
    };
    let opened = rustix::fs::openat(
        current,
        component,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok((File::from(opened), created))
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
/// with `RENAME_NOREPLACE`, which rustix backs with `renameat2` on
/// Linux/Android and with the weak-linked `renameatx_np` flagged renamer on
/// Apple platforms (macOS 10.12 and later; the macOS (aarch64) CI run at
/// the rework head passed through this path). Filesystems that do not
/// implement the flag — FAT and exFAT USB mounts return `EINVAL`/`ENOSYS`,
/// for example — fall back to a link-based publish, which is atomic and
/// fails with `EEXIST` on a collision; the staged leaf is then unlinked.
/// Filesystems offering neither primitive fail closed with `Unsupported`
/// and the staged file is left for the caller's normal discard path: the
/// historical exclusive-reservation fallback reserved the destination name
/// against creation but not against a later unlink, so another writer could
/// remove the placeholder, create the leaf, and the replacing rename would
/// have destroyed that writer's file — with the failure-path unlink then
/// deleting it outright.
#[cfg(unix)]
fn rename_no_replace_within_parent(
    parent: &File,
    from_leaf: &OsStr,
    _from_absolute: &Path,
    to_leaf: &OsStr,
    _to_absolute: &Path,
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

    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem offers neither no-replace rename nor hard links; refusing an unsafe publish",
    ))
}

/// Windows no-replace publish, mirroring the Unix strategy cascade with
/// safe `std` operations. A hard-link publish is tried first: creating the
/// link fails when the final leaf exists, so the publish is atomic and a
/// collision is a definitive failure. Filesystems without hard-link support
/// — FAT and exFAT USB mounts — fail closed with `Unsupported` and the
/// staged file is left for the caller's normal discard path, mirroring the
/// Unix reservation-fallback removal: an exclusive placeholder cannot
/// reserve a name against a later unlink, so the replacing rename could
/// have destroyed another writer's file.
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
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination appeared before the no-replace publish",
        )),
        // No hard-link support on this filesystem: fail closed rather than
        // publishing through an unbacked replace.
        Err(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem offers neither no-replace rename nor hard links; refusing an unsafe publish",
        )),
    }
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

/// How many times the replace publish may re-bind a changing occupant
/// before it gives up as a definitive collision. A bound of a few rounds
/// absorbs an active concurrent writer without spinning.
const REPLACE_BIND_ATTEMPTS: usize = 4;

/// Rename the staged file over the destination after the occupant that was
/// destroyed by the replace has been bound to `backup_leaf`. A failed
/// rename releases the backup so the parent is not polluted with a hidden
/// copy. Publication body shared by the no-atomic-swap fallbacks of
/// [`replace_publish_loop`].
#[cfg(unix)]
fn rename_over_bound_backup(
    parent: &File,
    from_leaf: &OsStr,
    to_leaf: &OsStr,
    backup_leaf: &OsStr,
) -> io::Result<()> {
    use rustix::fs::{renameat, unlinkat, AtFlags};

    if let Err(error) = renameat(parent, from_leaf, parent, to_leaf) {
        let _ = unlinkat(parent, backup_leaf, AtFlags::empty());
        return Err(io::Error::from(error));
    }
    Ok(())
}

/// The Overwrite publish loop (Unix): bind the current occupant to the
/// backup leaf, then replace it through an identity-verified atomic swap;
/// when the name is absent, publish through the no-replace cascade and
/// report a fresh publish. Returns whether an occupant was replaced (and
/// therefore backed up) plus the no-follow identity of the published leaf.
///
/// The replace is coupled to the backup by [`rustix::fs::RenameFlags::EXCHANGE`]
/// (`RENAME_SWAP` on Apple platforms): the atomic swap simultaneously
/// publishes the staged data at `to_leaf` and moves the displaced occupant
/// to the staged leaf — a private name no concurrent writer can reach —
/// where its identity is compared against the identity bound in the backup.
/// A mismatch means a writer interposed a new occupant between the bind and
/// the swap: that writer's file is intact at the private staged name and is
/// renamed back to `to_leaf`, the stale backup is released, and the loop
/// re-binds. There is therefore no window in which the publish destroys an
/// occupant its backup does not name. A filesystem without `EXCHANGE` (no
/// `renameat2`/`renamex_np` support) offers no primitive that couples the
/// backup to the destroy, so the overwrite fails closed there — a
/// verify-then-rename fallback would leave a window in which a concurrent
/// replacement is destroyed while the backup names the previous occupant.
#[cfg(unix)]
fn replace_publish_loop(
    parent: &File,
    from_leaf: &OsStr,
    from_absolute: &Path,
    to_leaf: &OsStr,
    to_absolute: &Path,
    backup_leaf: &OsStr,
    _backup_absolute: &Path,
) -> io::Result<(bool, Option<LeafIdentity>)> {
    for _ in 0..REPLACE_BIND_ATTEMPTS {
        if let Some(result) = replace_publish_attempt(
            parent,
            from_leaf,
            from_absolute,
            to_leaf,
            to_absolute,
            backup_leaf,
        )? {
            return Ok(result);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "destination kept changing through the replace publish",
    ))
}

/// One bind-and-replace attempt of the Unix Overwrite publish loop. An
/// occupant that keeps changing under the bind reports `Ok(None)` so the
/// caller re-binds within the retry bound; a definitive outcome (published
/// or failed) is returned directly.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn replace_publish_attempt(
    parent: &File,
    from_leaf: &OsStr,
    from_absolute: &Path,
    to_leaf: &OsStr,
    to_absolute: &Path,
    backup_leaf: &OsStr,
) -> io::Result<Option<(bool, Option<LeafIdentity>)>> {
    use rustix::fs::{statat, AtFlags};

    // Observe the occupant first so the absent case publishes no-replace
    // and a concurrent creation is never silently replaced-and-deleted.
    match statat(parent, to_leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {}
        Err(rustix::io::Errno::NOENT) => {
            // Bind the published-leaf identity to the staged object before
            // the rename (the rename preserves the object), so the reported
            // identity names the transfer's publication even if a
            // concurrent writer replaces the destination name afterwards.
            let staged = leaf_identity_at(parent, from_leaf).ok().flatten();
            let staged_object = retained_leaf_handle(parent, from_leaf);
            return match rename_no_replace_within_parent(
                parent,
                from_leaf,
                from_absolute,
                to_leaf,
                to_absolute,
            ) {
                Ok(()) => Ok(Some((
                    false,
                    cross_checked_published_identity(
                        parent,
                        to_leaf,
                        staged,
                        staged_object.as_ref(),
                    )
                    .recorded(),
                ))),
                // The creation won the race — loop back and bind it.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(None),
                Err(error) => Err(error),
            };
        }
        Err(error) => return Err(io::Error::from(error)),
    }

    // Bind the current occupant to the backup leaf, then replace it through
    // the strongest atomic primitive the platform offers.
    let Some(bound_identity) = bind_occupant_backup(parent, to_leaf, backup_leaf)? else {
        // The occupant vanished (or was replaced) while it was being bound:
        // the stale backup was released and the caller re-binds.
        return Ok(None);
    };
    swap_and_verify_replace(
        parent,
        from_leaf,
        from_absolute,
        to_leaf,
        to_absolute,
        backup_leaf,
        &bound_identity,
    )
}

/// Bind the current occupant of `to_leaf` to `backup_leaf`: a hard link
/// where the filesystem supports them, else a verified commit-time copy.
/// `Ok(None)` — the occupant vanished (or was replaced) while it was being
/// bound; the stale backup was released and the caller must re-observe.
#[cfg(unix)]
fn bind_occupant_backup(
    parent: &File,
    to_leaf: &OsStr,
    backup_leaf: &OsStr,
) -> io::Result<Option<LeafIdentity>> {
    use rustix::fs::{linkat, AtFlags};

    // `linkat` never follows the old path unless `AT_SYMLINK_FOLLOW` is
    // supplied — passing `SYMLINK_NOFOLLOW` here would be an unknown flag
    // bit and fail with `EINVAL` — so a symlink occupant is bound as
    // itself.
    match linkat(parent, to_leaf, parent, backup_leaf, AtFlags::empty()) {
        Ok(()) => match leaf_identity_at(parent, to_leaf) {
            Ok(Some(identity)) => Ok(Some(identity)),
            // The occupant was deleted after our bind: the backup is now
            // the only link to a file its owner destroyed. Release it —
            // restoring a deliberately deleted file would resurrect bytes
            // the destination no longer wants — and re-observe.
            Ok(None) => {
                release_occupant_backup(parent, backup_leaf);
                Ok(None)
            }
            Err(error) => {
                release_occupant_backup(parent, backup_leaf);
                Err(error)
            }
        },
        // Absent at bind time: loop back and publish no-replace.
        Err(rustix::io::Errno::NOENT) => Ok(None),
        // Hard links unavailable: copy-bind the occupant at commit time.
        // The copy is verified against the occupant's identity before it
        // is reported bound.
        Err(
            rustix::io::Errno::PERM
            | rustix::io::Errno::OPNOTSUPP
            | rustix::io::Errno::NOSYS
            | rustix::io::Errno::XDEV
            | rustix::io::Errno::MLINK,
        ) => match copy_bind_occupant_backup(parent, to_leaf, backup_leaf)? {
            OccupantBackup::Bound(identity) => Ok(Some(identity)),
            // The occupant vanished (or was replaced) while it was being
            // copied: discard the stale copy and re-bind from the top.
            OccupantBackup::Vanished => Ok(None),
        },
        Err(error) => Err(io::Error::from(error)),
    }
}

/// Best-effort release of a bound backup. Every path that abandons the bind
/// must unlink the hidden sibling or the parent accumulates hidden litter.
#[cfg(unix)]
fn release_occupant_backup(parent: &File, backup_leaf: &OsStr) {
    use rustix::fs::{unlinkat, AtFlags};

    let _ = unlinkat(parent, backup_leaf, AtFlags::empty());
}

/// Replace the bound occupant: the identity-verified atomic swap. `Ok(None)`
/// — the attempt observed a changing occupant; the stale state was undone
/// and the caller must re-bind.
///
/// There is no non-atomic fallback: a filesystem without an exchange
/// primitive has no way to couple the backup to the destroy, and a
/// verify-then-rename sequence leaves a window in which a concurrent
/// writer's replacement is destroyed while the backup still names the
/// previous occupant. Such filesystems fail closed with a typed
/// `Unsupported` error instead of racing a concurrent writer.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn swap_and_verify_replace(
    parent: &File,
    from_leaf: &OsStr,
    _from_absolute: &Path,
    to_leaf: &OsStr,
    _to_absolute: &Path,
    backup_leaf: &OsStr,
    bound_identity: &LeafIdentity,
) -> io::Result<Option<(bool, Option<LeafIdentity>)>> {
    use rustix::fs::{renameat_with, RenameFlags};

    // Bind the published-leaf identity to the staged object before the
    // exchange: the exchange preserves the object, so the post-swap refresh
    // (accepted only when it still names this object) is the identity of
    // the transfer's publication even if a concurrent writer replaces the
    // destination name later. The refresh is required because the exchange
    // itself updates the object's change instant — the raw staged capture
    // could never compare exactly in a later reversal.
    let staged = leaf_identity_at(parent, from_leaf).ok().flatten();
    let staged_object = retained_leaf_handle(parent, from_leaf);
    match renameat_with(parent, from_leaf, parent, to_leaf, RenameFlags::EXCHANGE) {
        Ok(()) => {
            let published =
                cross_checked_published_identity(parent, to_leaf, staged, staged_object.as_ref())
                    .recorded();
            verify_atomic_swap(
                parent,
                from_leaf,
                to_leaf,
                backup_leaf,
                bound_identity,
                published,
            )
        }
        // Filesystem (or kernel) without atomic-swap support: fail closed.
        // The swap never happened, so nothing was displaced and the backup —
        // which names a live occupant — must be released.
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::OPNOTSUPP) => {
            release_occupant_backup(parent, backup_leaf);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "destination filesystem offers no atomic exchange primitive; refusing an \
                 overwrite replace that could destroy a concurrent writer's file unbacked",
            ))
        }
        Err(error) => {
            // The swap never happened: nothing was displaced, so the
            // backup — which names a live occupant — must be released or
            // the parent accumulates hidden litter.
            release_occupant_backup(parent, backup_leaf);
            Err(io::Error::from(error))
        }
    }
}

/// The exchange landed: verify the displaced object at the private staged
/// leaf against the bind-time identity. A mismatch means a writer
/// interposed a new occupant between the bind and the swap: that writer's
/// file is intact at the private staged name and is renamed back to
/// `to_leaf`, the stale backup is released, and the caller re-binds.
/// `Ok(None)` reports that interposition.
///
/// A failed restore never releases the backup and never unlinks the
/// displaced object: the destination still names the transfer's published
/// bytes, the backup still names the bind-time occupant, and the displaced
/// object is preserved at the private staged leaf. The failure carries the
/// [`DisplacedOccupantFailure`] marker so the caller can record the
/// publication for rollback and shield the staged leaf from cleanup.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn verify_atomic_swap(
    parent: &File,
    from_leaf: &OsStr,
    to_leaf: &OsStr,
    backup_leaf: &OsStr,
    bound_identity: &LeafIdentity,
    published_leaf: Option<LeafIdentity>,
) -> io::Result<Option<(bool, Option<LeafIdentity>)>> {
    use rustix::fs::{renameat, unlinkat, AtFlags};

    match leaf_identity_at(parent, from_leaf) {
        // Same object check, not full equality: the exchange itself updates
        // the displaced object's change time, so the post-swap capture
        // cannot equal the bind-time capture on the instant fields.
        Ok(Some(displaced)) if displaced.same_object(bound_identity) => {
            // The swap displaced exactly the object the backup names. The
            // displaced bytes live on in the backup; drop the now-redundant
            // link at the staged name. The published identity is the one
            // bound to the staged object before the exchange.
            let _ = unlinkat(parent, from_leaf, AtFlags::empty());
            Ok(Some((true, published_leaf)))
        }
        Ok(_) => {
            // An interposed writer's object was displaced: restore it to
            // the destination before reporting the interposition. The
            // restore replaces the transfer's published bytes at
            // `to_leaf` — nothing published remains, so this is an
            // ordinary retryable interposition.
            if let Err(error) = renameat(parent, from_leaf, parent, to_leaf) {
                return Err(displaced_occupant_failure(
                    published_leaf,
                    io::Error::from(error),
                    None,
                ));
            }
            release_occupant_backup(parent, backup_leaf);
            Ok(None)
        }
        // The displaced leaf cannot be read back: fail closed with the
        // destination restored to the swap-instant state (the displaced
        // object returns to `to_leaf`) rather than leaving an unverifiable
        // publication. The backup is released only once the restoration is
        // confirmed; if the restore itself fails, the backup and the
        // displaced object are kept and the failure carries the
        // displaced-occupant marker.
        Err(verification) => match renameat(parent, from_leaf, parent, to_leaf) {
            Ok(()) => {
                release_occupant_backup(parent, backup_leaf);
                Err(verification)
            }
            Err(restore) => Err(displaced_occupant_failure(
                published_leaf,
                verification,
                Some(io::Error::from(restore)),
            )),
        },
    }
}

/// Error payload for a replace publish whose atomic exchange landed but
/// whose post-swap verification could not restore the destination: the
/// transfer's bytes ARE published at the destination, the bind-time backup
/// is retained for restoration, and the displaced object survives at the
/// private staged leaf — which the caller must shield from cleanup.
/// Surfaced through an [`io::Error`] payload so it travels every `?` on the
/// publish path; the write authority's commit maps it to a
/// verified-publication failure carrying the outcome.
#[cfg(unix)]
pub(crate) struct DisplacedOccupantFailure {
    /// Identity of the published leaf, bound to the staged object before
    /// the exchange.
    pub(crate) published_leaf: Option<LeafIdentity>,
}

#[cfg(unix)]
impl fmt::Debug for DisplacedOccupantFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DisplacedOccupantFailure")
            .field("published_leaf_captured", &self.published_leaf.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl fmt::Display for DisplacedOccupantFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the atomic exchange published the staged bytes but the displaced occupant \
             could not be restored to the destination; the bind-time backup is retained \
             and the displaced object is preserved at the private staged leaf"
        )
    }
}

#[cfg(unix)]
impl std::error::Error for DisplacedOccupantFailure {}

/// Build the marker-carrying error for a displaced occupant the publish
/// machinery could not restore. `cause` is the primary failure; `detail`
/// is a secondary failure woven into the message.
#[cfg(unix)]
fn displaced_occupant_failure(
    published_leaf: Option<LeafIdentity>,
    cause: io::Error,
    detail: Option<io::Error>,
) -> io::Error {
    let failure = RestoreFailure {
        payload: DisplacedOccupantFailure { published_leaf },
        cause,
        detail,
    };
    io::Error::other(failure)
}

/// Wrapper pairing the marker payload with its causing error so the
/// surfaced message names both the state and the cause. The write
/// authority's commit path downcasts to this wrapper to recover the
/// published-leaf identity of a landed-but-unrestorable exchange.
#[cfg(unix)]
pub(crate) struct RestoreFailure {
    pub(crate) payload: DisplacedOccupantFailure,
    cause: io::Error,
    detail: Option<io::Error>,
}

#[cfg(unix)]
impl fmt::Debug for RestoreFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestoreFailure")
            .field("payload", &self.payload)
            .field("detail", &self.detail.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl fmt::Display for RestoreFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(
                formatter,
                "{}: {} (restore also failed: {})",
                self.payload, self.cause, detail
            ),
            None => write!(formatter, "{}: {}", self.payload, self.cause),
        }
    }
}

#[cfg(unix)]
impl std::error::Error for RestoreFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Outcome of a commit-time copy-bind attempt.
#[cfg(unix)]
enum OccupantBackup {
    /// The backup leaf now names a verified copy of the occupant; the
    /// payload is the occupant's captured identity.
    Bound(LeafIdentity),
    /// The occupant vanished or changed under the copy; the backup was
    /// discarded and the caller must re-bind.
    Vanished,
}

/// Refuse to copy-bind an occupant that cannot be backed up safely: a
/// directory must never be replaced by a file, and a symlink occupant
/// cannot be opened no-follow, so without hard links there is no way to
/// bind the link itself — fail closed rather than replacing it unbacked.
#[cfg(unix)]
fn classify_copy_bind_occupant(st_mode: rustix::fs::RawMode) -> io::Result<()> {
    use rustix::fs::FileType;

    if FileType::from_raw_mode(st_mode) == FileType::Directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to replace a directory with a file",
        ));
    }
    if FileType::from_raw_mode(st_mode) == FileType::Symlink {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot bind a symlink occupant for backup on a filesystem without hard links",
        ));
    }
    Ok(())
}

/// Open the backup leaf exclusively through the retained parent handle: a
/// staged 0600 write-only file that must not already exist, so a backup
/// sibling planted by a concurrent writer fails the bind instead of being
/// overwritten.
#[cfg(unix)]
fn open_exclusive_backup_leaf(parent: &File, backup_leaf: &OsStr) -> io::Result<File> {
    use rustix::fs::{openat, Mode, OFlags};

    match openat(
        parent,
        backup_leaf,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NOCTTY,
        Mode::from_bits_truncate(0o600),
    ) {
        Ok(backup) => Ok(File::from(backup)),
        Err(rustix::io::Errno::EXIST) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "backup sibling appeared before the copy-bind",
        )),
        Err(error) => Err(io::Error::from(error)),
    }
}

/// Verify the occupant of `to_leaf` still names the exact object that was
/// copied: a replacement or a deletion discards the copy and the caller
/// re-binds from the top.
#[cfg(unix)]
fn verify_occupant_identity(
    parent: &File,
    to_leaf: &OsStr,
    before: &rustix::fs::Stat,
) -> io::Result<bool> {
    use rustix::fs::{statat, AtFlags};

    match statat(parent, to_leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(after) => Ok(after.st_dev == before.st_dev && after.st_ino == before.st_ino),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(io::Error::from(error)),
    }
}

/// Copy the current occupant of `to_leaf` into `backup_leaf` through the
/// retained parent handle, used when the filesystem offers no hard links
/// for an atomic bind. The copy is verified against the occupant's identity
/// after the fact: a vanished or replaced occupant discards the copy and
/// reports [`OccupantBackup::Vanished`] so the caller re-binds, and a bound
/// copy reports the occupant's captured no-follow identity so the publish
/// can verify the replace displaces exactly the copied object. A directory
/// occupant is refused with the typed `InvalidInput` error.
#[cfg(unix)]
fn copy_bind_occupant_backup(
    parent: &File,
    to_leaf: &OsStr,
    backup_leaf: &OsStr,
) -> io::Result<OccupantBackup> {
    use rustix::fs::{openat, statat, unlinkat, AtFlags, Mode, OFlags};

    let before = match statat(parent, to_leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(OccupantBackup::Vanished),
        Err(error) => return Err(io::Error::from(error)),
    };
    let before_identity = leaf_identity_from_stat(&before);
    classify_copy_bind_occupant(before.st_mode)?;
    let occupant = openat(
        parent,
        to_leaf,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let mut occupant_file = File::from(occupant);
    let mut backup_file = open_exclusive_backup_leaf(parent, backup_leaf)?;
    let copied = std::io::copy(&mut occupant_file, &mut backup_file);
    let synced = backup_file.sync_all();
    drop(backup_file);
    drop(occupant_file);
    // A failed or unverified copy leaves the backup leaf behind: remove it
    // so the parent is not polluted with a hidden partial copy.
    let bound = match copied.and(synced) {
        // The bind is only trustworthy if the name still holds the exact
        // occupant that was copied. Anything else — a replacement, or a
        // deletion — discards the copy and re-binds from the top.
        Ok(()) => match verify_occupant_identity(parent, to_leaf, &before) {
            Ok(bound) => bound,
            Err(error) => {
                let _ = unlinkat(parent, backup_leaf, AtFlags::empty());
                return Err(error);
            }
        },
        Err(error) => {
            let _ = unlinkat(parent, backup_leaf, AtFlags::empty());
            return Err(error);
        }
    };
    if bound {
        Ok(OccupantBackup::Bound(before_identity))
    } else {
        let _ = unlinkat(parent, backup_leaf, AtFlags::empty());
        Ok(OccupantBackup::Vanished)
    }
}

/// Bind the current occupant of `to_absolute` to `backup_absolute` with a
/// hard link, then rename the staged file over it — but only after the
/// destination is re-verified to still name the exact object the backup
/// binds. `Ok(Some(true))` means the occupant was replaced and backed up;
/// `Ok(None)` means the occupant vanished or was replaced after the bind —
/// the stale backup was released and the caller must re-bind from the top.
///
/// Windows exposes no atomic swap primitive in user mode, so the coupling
/// between the backup and the replace cannot be a single syscall here the
/// way `RENAME_EXCHANGE` makes it on Unix. The verification immediately
/// before the rename shrinks the interposition window to the rename
/// itself, and any interposition is detected on the next loop iteration's
/// re-bind instead of destroying the writer's file unbacked.
#[cfg(windows)]
fn hard_link_bind_and_replace(
    from_absolute: &Path,
    to_absolute: &Path,
    backup_absolute: &Path,
) -> io::Result<Option<bool>> {
    match std::fs::hard_link(to_absolute, backup_absolute) {
        Ok(()) => {}
        // The occupant vanished between the typing and the bind.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        // No hard-link support (or an unbindable occupant): fail
        // closed rather than replacing unbacked.
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cannot bind the destination occupant for backup; refusing an unbacked replace",
            ))
        }
    }
    // Identity-coupled replace: the backup binds one exact object (a hard
    // link names the same volume and file id); proceed only while the
    // destination still resolves to that same object.
    let bound = leaf_identity_at_path(backup_absolute)?;
    let current = leaf_identity_at_path(to_absolute)?;
    match (bound, current) {
        (Some(bound), Some(current)) if bound == current => {}
        // The occupant was replaced (a stale backup naming the earlier
        // object) or deleted (the backup is now a dead object's last
        // link): release it — restoring a deliberately deleted file would
        // resurrect bytes the destination no longer wants — and re-bind.
        _ => {
            let _ = std::fs::remove_file(backup_absolute);
            return Ok(None);
        }
    }
    if let Err(error) = std::fs::rename(from_absolute, to_absolute) {
        // The publish failed; release our backup so the parent is
        // not polluted with a hidden copy.
        let _ = std::fs::remove_file(backup_absolute);
        return Err(error);
    }
    Ok(Some(true))
}

/// Windows Overwrite publish loop. See [`replace_publish_loop`] for the
/// contract; the operations are absolute-path based after the caller
/// revalidated and pinned the retained parent, mirroring the established
/// Windows publish discipline. The occupant bind is identity-verified
/// immediately before the replace (see
/// [`hard_link_bind_and_replace`]); a verification mismatch releases the
/// stale backup and re-binds within the retry bound instead of replacing
/// an object the backup does not name.
#[cfg(windows)]
fn replace_publish_loop(
    parent: &File,
    from_leaf: &OsStr,
    from_absolute: &Path,
    to_leaf: &OsStr,
    to_absolute: &Path,
    _backup_leaf: &OsStr,
    backup_absolute: &Path,
) -> io::Result<(bool, Option<LeafIdentity>)> {
    for _ in 0..REPLACE_BIND_ATTEMPTS {
        // Type the occupant no-follow first: a directory is refused with
        // the typed error, and a present non-directory is bound by hard
        // link before the replace.
        match std::fs::symlink_metadata(to_absolute) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to replace a directory with a file",
                ));
            }
            Ok(_) => {
                if let Some(replaced) =
                    hard_link_bind_and_replace(from_absolute, to_absolute, backup_absolute)?
                {
                    let published = leaf_identity_at_path(to_absolute).ok().flatten();
                    return Ok((replaced, published));
                }
                // The occupant vanished or was replaced between the typing
                // and the verified bind; loop back and re-type before the
                // next bind attempt.
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match rename_no_replace_within_parent(
                    parent,
                    from_leaf,
                    from_absolute,
                    to_leaf,
                    to_absolute,
                ) {
                    Ok(()) => {
                        let published = leaf_identity_at_path(to_absolute).ok().flatten();
                        return Ok((false, published));
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "destination kept changing through the replace publish",
    ))
}

/// Windows fallback for [`MountedRootAuthority::create_directories_within`]:
/// path-based per-component creation with a reparse-free is-directory check,
/// mirroring the historical `create_directory_atomic` behavior.
#[cfg(windows)]
fn create_directory_tree_by_path(root: &Path, components: &[OsString]) -> io::Result<Vec<usize>> {
    let mut path = root.to_path_buf();
    let mut created = Vec::new();
    for (index, component) in components.iter().enumerate() {
        path.push(component);
        match std::fs::create_dir(&path) {
            Ok(()) => created.push(index),
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
    Ok(created)
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

    /// Interposition regressions for the post-publish published-object
    /// proof: a failed proof must record the admitted staged identity, so
    /// a later reversal refuses a foreign replacement fail-closed instead
    /// of degrading to unchecked path-only cleanup.
    #[cfg(unix)]
    mod published_identity_proof {
        use super::*;

        fn open_parent_dir(path: &Path) -> File {
            let opened = rustix::fs::openat(
                rustix::fs::CWD,
                path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .expect("open parent directory handle");
            File::from(opened)
        }

        fn staged_capture(parent: &File, leaf: &OsStr) -> LeafIdentity {
            leaf_identity_at(parent, leaf)
                .expect("stat staged leaf")
                .expect("staged leaf exists")
        }

        // The exact-object proof requires a path-less retained handle
        // (O_PATH), which only Linux-family kernels provide; elsewhere the
        // proof legitimately degrades and [`degraded_proof_still_verifies_a_genuine_refresh`]
        // covers the fallback.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        #[test]
        fn verified_proof_records_the_refreshed_identity() {
            let directory = TestDirectory::new("proof-verified");
            fs::write(directory.path().join("staged.tmp"), b"published bytes")
                .expect("write staged leaf");
            let parent = open_parent_dir(directory.path());
            let staged = staged_capture(&parent, OsStr::new("staged.tmp"));
            let staged_object = retained_leaf_handle(&parent, OsStr::new("staged.tmp"))
                .expect("retain the staged object");

            // The publish renames the staged object onto the destination:
            // same object, updated change instant.
            fs::rename(
                directory.path().join("staged.tmp"),
                directory.path().join("song.flac"),
            )
            .expect("publish rename");

            match cross_checked_published_identity(
                &parent,
                OsStr::new("song.flac"),
                Some(staged),
                Some(&staged_object),
            ) {
                PublishedIdentityProof::Verified(recorded) => {
                    let current = staged_capture(&parent, OsStr::new("song.flac"));
                    assert_eq!(
                        recorded, current,
                        "the verified record must be the post-publish capture"
                    );
                    assert!(
                        recorded.same_object(&staged),
                        "the verified record must name the staged object"
                    );
                }
                other => panic!("expected a verified proof, got {other:?}"),
            }
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        #[test]
        fn foreign_interposition_preserves_the_admitted_identity() {
            let directory = TestDirectory::new("proof-foreign");
            fs::write(directory.path().join("song.flac"), b"published bytes")
                .expect("write published leaf");
            let parent = open_parent_dir(directory.path());
            let staged = staged_capture(&parent, OsStr::new("song.flac"));
            let staged_object = retained_leaf_handle(&parent, OsStr::new("song.flac"))
                .expect("retain the staged object");

            // A concurrent writer replaces the publication with a NEW
            // object in the capture window. The unlink-then-recreate
            // pattern deliberately invites the allocator to hand the freed
            // index straight back (observed on the aarch64 CI runners): the
            // proof must not care either way, because it compares the FULL
            // identity of the retained object — device, index, and change
            // instant — and the replacement carries its own later instant
            // even when it inherits the index.
            fs::remove_file(directory.path().join("song.flac")).expect("racer unlink");
            fs::write(directory.path().join("song.flac"), b"racer bytes").expect("racer replace");

            match cross_checked_published_identity(
                &parent,
                OsStr::new("song.flac"),
                Some(staged),
                Some(&staged_object),
            ) {
                PublishedIdentityProof::Admitted(recorded) => {
                    assert_eq!(
                        recorded, staged,
                        "the admitted record must be the staged capture, never the foreign object"
                    );
                }
                other => panic!(
                    "a foreign interposition must admit the staged identity, never degrade: \
                     {other:?}"
                ),
            }
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        #[test]
        fn vanishing_leaf_preserves_the_admitted_identity() {
            let directory = TestDirectory::new("proof-vanished");
            fs::write(directory.path().join("song.flac"), b"published bytes")
                .expect("write published leaf");
            let parent = open_parent_dir(directory.path());
            let staged = staged_capture(&parent, OsStr::new("song.flac"));
            let staged_object = retained_leaf_handle(&parent, OsStr::new("song.flac"))
                .expect("retain the staged object");

            fs::remove_file(directory.path().join("song.flac")).expect("racer removal");

            match cross_checked_published_identity(
                &parent,
                OsStr::new("song.flac"),
                Some(staged),
                Some(&staged_object),
            ) {
                PublishedIdentityProof::Admitted(recorded) => {
                    assert_eq!(recorded, staged);
                }
                other => panic!(
                    "a vanishing leaf must admit the staged identity, never degrade: {other:?}"
                ),
            }
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        #[test]
        fn missing_staged_capture_degrades_to_uncaptured() {
            let directory = TestDirectory::new("proof-uncaptured");
            fs::write(directory.path().join("song.flac"), b"published bytes")
                .expect("write published leaf");
            let parent = open_parent_dir(directory.path());
            let staged_object = retained_leaf_handle(&parent, OsStr::new("song.flac"))
                .expect("retain the published object");

            // A retained handle without a staged capture proves nothing —
            // only the pre-rename capture is the admission — so the record
            // still degrades to the legacy path-only reversal.
            match cross_checked_published_identity(
                &parent,
                OsStr::new("song.flac"),
                None,
                Some(&staged_object),
            ) {
                PublishedIdentityProof::Uncaptured => {}
                other => panic!("a missing staged capture must degrade: {other:?}"),
            }
            assert!(PublishedIdentityProof::Uncaptured.recorded().is_none());
        }

        #[test]
        fn degraded_proof_still_verifies_a_genuine_refresh() {
            let directory = TestDirectory::new("proof-degraded");
            fs::write(directory.path().join("staged.tmp"), b"published bytes")
                .expect("write staged leaf");
            let parent = open_parent_dir(directory.path());
            let staged = staged_capture(&parent, OsStr::new("staged.tmp"));

            fs::rename(
                directory.path().join("staged.tmp"),
                directory.path().join("song.flac"),
            )
            .expect("publish rename");

            // Platforms without a path-less handle type (and failed
            // handle opens) fall back to the legacy device/index
            // comparison; it must keep verifying a genuine rename refresh.
            match cross_checked_published_identity(
                &parent,
                OsStr::new("song.flac"),
                Some(staged),
                None,
            ) {
                PublishedIdentityProof::Verified(recorded) => {
                    let current = staged_capture(&parent, OsStr::new("song.flac"));
                    assert_eq!(
                        recorded, current,
                        "the verified record must be the post-publish capture"
                    );
                }
                other => panic!("expected a verified degraded proof, got {other:?}"),
            }
        }
    }

    /// Interposition regressions for the parked-tombstone release: the
    /// public leaf's cleanup must remove only the parked tombstone file —
    /// never a concurrent writer's replacement that raced into the name.
    #[cfg(unix)]
    mod parked_leaf_release {
        use super::*;

        const PARKED: &str = ".tributary-reversal-parked-a.tmp";
        const SCRATCH: &str = ".tributary-reversal-scratch-b.tmp";

        fn open_parent_dir(path: &Path) -> File {
            let opened = rustix::fs::openat(
                rustix::fs::CWD,
                path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .expect("open parent directory handle");
            File::from(opened)
        }

        #[test]
        fn release_removes_exactly_the_parked_object() {
            let directory = TestDirectory::new("parked-clean");
            fs::write(directory.path().join(PARKED), b"").expect("park the tombstone file");
            let parent = open_parent_dir(directory.path());
            let parked = leaf_identity_at(&parent, OsStr::new(PARKED))
                .expect("stat parked file")
                .expect("parked file exists");

            release_parked_reversal_leaf(
                &parent,
                OsStr::new(PARKED),
                Some(parked),
                OsStr::new(SCRATCH),
            )
            .expect("release the parked tombstone file");

            assert!(
                !directory.path().join(PARKED).exists(),
                "the parked tombstone file must be removed from the public name"
            );
            assert!(
                !directory.path().join(SCRATCH).exists(),
                "the private scratch name must be vacant afterwards"
            );
            assert_eq!(
                fs::read_dir(directory.path())
                    .expect("read directory")
                    .count(),
                0,
                "no litter may survive the release"
            );
        }

        #[test]
        fn release_preserves_a_foreign_replacement_of_the_leaf() {
            let directory = TestDirectory::new("parked-foreign");
            // The parked tombstone file exists only as an identity here:
            // the racer replaced the public name before the cleanup ran.
            let parked_root = TestDirectory::new("parked-foreign-source");
            fs::write(parked_root.path().join("source.tmp"), b"parked").expect("write source");
            let parked_parent = open_parent_dir(parked_root.path());
            let parked = leaf_identity_at(&parked_parent, OsStr::new("source.tmp"))
                .expect("stat parked stand-in")
                .expect("parked stand-in exists");
            fs::write(directory.path().join(PARKED), b"racer bytes").expect("racer replaces leaf");
            let parent = open_parent_dir(directory.path());

            release_parked_reversal_leaf(
                &parent,
                OsStr::new(PARKED),
                Some(parked),
                OsStr::new(SCRATCH),
            )
            .expect("the release must succeed without destroying the foreign object");

            assert_eq!(
                fs::read(directory.path().join(PARKED)).expect("read the racer's object"),
                b"racer bytes",
                "the concurrent writer's replacement must survive the release untouched"
            );
            assert!(
                !directory.path().join(SCRATCH).exists(),
                "the foreign object must be restored off the private name"
            );
        }

        #[test]
        fn release_tolerates_an_absent_leaf() {
            let directory = TestDirectory::new("parked-absent");
            let parent = open_parent_dir(directory.path());
            let parked_root = TestDirectory::new("parked-absent-source");
            fs::write(parked_root.path().join("source.tmp"), b"parked").expect("write source");
            let parked_parent = open_parent_dir(parked_root.path());
            let parked = leaf_identity_at(&parked_parent, OsStr::new("source.tmp"))
                .expect("stat parked stand-in")
                .expect("parked stand-in exists");

            release_parked_reversal_leaf(
                &parent,
                OsStr::new(PARKED),
                Some(parked),
                OsStr::new(SCRATCH),
            )
            .expect("an absent leaf is nothing to release");

            assert_eq!(
                fs::read_dir(directory.path())
                    .expect("read directory")
                    .count(),
                0,
                "an absent leaf release must not create anything"
            );
        }

        #[test]
        fn release_without_a_captured_parked_identity_destroys_nothing() {
            let directory = TestDirectory::new("parked-uncaptured");
            fs::write(directory.path().join(PARKED), b"unverified").expect("occupy the leaf");
            let parent = open_parent_dir(directory.path());

            release_parked_reversal_leaf(&parent, OsStr::new(PARKED), None, OsStr::new(SCRATCH))
                .expect("an unverifiable occupant must be restored, not removed");

            assert_eq!(
                fs::read(directory.path().join(PARKED)).expect("read the occupant"),
                b"unverified",
                "an unverifiable occupant must stay at the public name"
            );
            assert!(
                !directory.path().join(SCRATCH).exists(),
                "the private scratch name must be vacant afterwards"
            );
        }
    }
}
