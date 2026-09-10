//! Audio tag writer — wraps `lofty` to safely write metadata to audio files.
//!
//! Supports MP3 (ID3v2), M4A/AAC (MP4 atoms), OGG Vorbis, and FLAC.
//! All writes go through [`write_tags`] which validates the edit, copies the
//! file to an exclusively created sibling temp path, tags the copy, flushes it,
//! and atomically renames it over the original.  The caller is responsible for
//! triggering a library re-scan of the modified file afterwards.
//!
//! # Guarantees
//!
//! - **Nothing is written unless the whole edit is valid.** A malformed number
//!   is rejected before the file is opened, so a bad Year cannot leave the file
//!   rewritten-but-unchanged while the UI reports success.
//! - **Cleanup is attempted on every failure path.** The sibling is owned by
//!   an RAII guard that calls `remove_file` after errors including a failed
//!   rename. Cleanup itself remains fallible (and cannot run after process
//!   termination), so scans and the watcher also exclude its exact reserved
//!   filename shape.
//! - **The temp path is unguessable and exclusively created** (`O_EXCL` via
//!   `create_new`), so two concurrent saves to the same file cannot collide and
//!   the copy cannot be redirected through a pre-planted symlink. For a
//!   retained-authority write the staging never resolves a pathname at all:
//!   the sibling is created, written, flushed, and cleaned up beneath the
//!   retained parent handle, so no displaced ancestor or planted symlink can
//!   observe or receive any part of the copy.
//! - **A Windows copy is never exposed through an inherited DACL.** Its handle
//!   denies competing opens from creation until the source DACL and protection
//!   state are installed; a fresh exclusive write handle must then pass that
//!   DACL before the first audio byte is copied.
//! - **The replacement is durable**: the tagged copy is `fsync`ed before the
//!   rename, so a crash cannot leave a truncated file in place of the original.

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::file::{TaggedFile, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagExt, TagItem};
use uuid::Uuid;

use super::root_authority::{MountedMutationCommit, MountedMutationTarget, ObjectIdentity};
// Only the unix anchored staging flow captures a staged object identity;
// the Windows and fallback authorities prove staging identity from the
// pathname-side commit, so the helper import must not gate their builds.
#[cfg(unix)]
use super::root_authority::object_identity;

/// Reserved filename prefix for the private sibling used by atomic tag writes.
const TAG_WRITE_TEMP_PREFIX: &str = ".tributary-tag-";

/// Formats whose tags Tributary can rewrite safely.
const WRITABLE_EXTENSIONS: &[&str] = &["mp3", "m4a", "aac", "ogg", "flac"];

/// Why a file cannot currently enter the tag-write path.
///
/// This is deliberately a small, path-free category for UI decisions. The
/// capability probe is only a point-in-time preflight; [`write_tags`] still
/// treats every filesystem operation as fallible because a mount, portal
/// grant, or ACL can change immediately afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagWritePreflightError {
    /// The filename does not identify a format whose tags we can rewrite.
    UnsupportedFormat,
    /// The path is not an existing regular file (symlinks are not rewritten).
    NotRegularFile,
    /// The file cannot be read or its directory cannot host and remove the
    /// private sibling required for an atomic replacement.
    Unavailable,
}

/// Return whether `path` has the exact shape emitted for a tag-write sibling.
///
/// The bounded ASCII name is `.tributary-tag-<canonical UUID>.<format>`. Exact
/// recognition minimizes the reserved namespace while allowing the scanner
/// and watcher to keep an in-progress full-size copy out of the library.
pub fn is_tag_write_temp_file(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let Some(suffix) = name
        .as_encoded_bytes()
        .strip_prefix(TAG_WRITE_TEMP_PREFIX.as_bytes())
    else {
        return false;
    };
    if suffix.len() < 38 || suffix.get(36) != Some(&b'.') {
        return false;
    }

    let Ok(id) = std::str::from_utf8(&suffix[..36]) else {
        return false;
    };
    let Ok(extension) = std::str::from_utf8(&suffix[37..]) else {
        return false;
    };

    WRITABLE_EXTENSIONS.contains(&extension)
        && Uuid::parse_str(id).is_ok_and(|uuid| uuid.hyphenated().to_string() == id)
}

/// Fields that can be edited in the properties dialog.
///
/// Each field is `Option<String>` — `None` means "don't change this field",
/// `Some(value)` means "set to this value" (empty string clears the field).
#[derive(Debug, Clone, Default)]
pub struct TagEdits {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub year: Option<String>,
    pub track_number: Option<String>,
    pub disc_number: Option<String>,
    pub comment: Option<String>,
}

impl TagEdits {
    /// Returns `true` if no fields have been changed.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.artist.is_none()
            && self.album.is_none()
            && self.album_artist.is_none()
            && self.genre.is_none()
            && self.composer.is_none()
            && self.year.is_none()
            && self.track_number.is_none()
            && self.disc_number.is_none()
            && self.comment.is_none()
    }

    /// Check every numeric field before any file is touched.
    ///
    /// Call this from the UI to reject a bad edit while the user can still fix
    /// it. [`write_tags`] calls it too, so a caller that forgets cannot corrupt
    /// intent — but by then the only recourse is an error dialog.
    pub fn validate(&self) -> Result<()> {
        parse_tag_number("Year", self.year.as_deref())?;
        parse_tag_number("Track #", self.track_number.as_deref())?;
        parse_tag_number("Disc #", self.disc_number.as_deref())?;
        Ok(())
    }
}

/// What the user asked us to do with one numeric tag field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberEdit {
    /// The field was not edited.
    Unchanged,
    /// The field was emptied, meaning "remove this tag".
    Clear,
    /// The field holds a value to write.
    Set(u32),
}

/// Interpret one numeric tag field, rejecting anything that is not a number.
///
/// The previous code silently discarded unparseable input, so typing `2026a`
/// into Year rewrote the file, changed nothing, and reported success.
fn parse_tag_number(field: &str, raw: Option<&str>) -> Result<NumberEdit> {
    let Some(raw) = raw else {
        return Ok(NumberEdit::Unchanged);
    };
    if raw.is_empty() {
        return Ok(NumberEdit::Clear);
    }
    let value = raw
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("{field} must be a whole number, but \"{raw}\" is not one"))?;
    Ok(NumberEdit::Set(value))
}

/// A borrowed DACL plus the LocalAlloc-owned descriptor that contains it.
///
/// `GetSecurityInfo` returns the DACL as a pointer into `descriptor`; only the
/// descriptor is freed, and it must outlive every use of the DACL pointer.
#[cfg(target_os = "windows")]
struct WindowsDacl {
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    dacl: *mut windows_sys::Win32::Security::ACL,
    protected: bool,
}

#[cfg(target_os = "windows")]
impl WindowsDacl {
    /// Snapshot a file handle's DACL and inheritance-protection state.
    fn read_from(file: &std::fs::File) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorControl, DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        };

        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut dacl,
                std::ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32));
        }
        if descriptor.is_null() {
            return Err(std::io::Error::other(
                "GetSecurityInfo returned a null security descriptor",
            ));
        }

        // Construct the owner immediately so a later validation error still
        // releases the LocalAlloc allocation returned by GetSecurityInfo.
        let mut snapshot = Self {
            descriptor,
            dacl,
            protected: false,
        };
        let mut control = 0;
        let mut revision = 0;
        if unsafe {
            GetSecurityDescriptorControl(snapshot.descriptor, &raw mut control, &raw mut revision)
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        snapshot.protected = control & SE_DACL_PROTECTED != 0;
        Ok(snapshot)
    }

    /// Install this exact DACL on an exclusively held destination handle.
    fn apply_to(&self, file: &std::fs::File) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::Security::Authorization::{SetSecurityInfo, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        };

        let protection = if self.protected {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | protection,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.dacl,
                std::ptr::null(),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32))
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsDacl {
    fn drop(&mut self) {
        // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
        // The DACL pointer aliases it and is deliberately not freed separately.
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.descriptor);
        }
    }
}

/// A temp file that attempts to delete itself unless it is explicitly persisted.
///
/// The previous implementation cleaned up only when *tagging* failed; a failed
/// `rename` escaped through `?` and orphaned the temp file next to the user's
/// music forever.
struct TempFile {
    path: PathBuf,
    persisted: bool,
    /// Unix authority-based staging: the retained parent directory the
    /// staged sibling is anchored at. Set only when `path` holds the bare
    /// leaf name of a sibling created beneath a retained parent handle; the
    /// drop cleanup then unlinks that leaf through the handle — never
    /// through a pathname — so the cleanup cannot mutate a displaced
    /// impostor directory.
    #[cfg(unix)]
    anchored_parent: Option<File>,
}

/// Randomized staged-sibling leaf name, preserving the final extension.
///
/// `lofty` infers the format from the final extension, and the retained
/// commit machinery addresses the staged copy by this exact leaf, so
/// preserve only that extension: including the whole source name could
/// overflow a filesystem's component-length limit for an otherwise valid
/// long filename.
fn staged_sibling_name(target_leaf: &OsStr) -> std::ffi::OsString {
    let extension = Path::new(target_leaf)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let mut candidate_name =
        std::ffi::OsString::from(format!("{TAG_WRITE_TEMP_PREFIX}{}", Uuid::new_v4()));
    if let Some(extension) = extension.as_deref() {
        candidate_name.push(".");
        candidate_name.push(extension);
    }
    candidate_name
}

impl TempFile {
    /// Create an empty, exclusively owned temp file beside `target`.
    ///
    /// A sibling keeps the later `rename` on one filesystem (a cross-device
    /// rename fails with `EXDEV`). The name is randomized and created with
    /// `create_new` (`O_EXCL`) so it cannot collide with a concurrent save or
    /// follow a symlink an attacker planted at a predictable path.
    ///
    /// Error contexts name the target by `target_label`, never by `target`:
    /// an authority-based caller's target is a native mount path that must
    /// not leak into logs (see `replacement_path`).
    fn create_beside(target: &Path, target_label: &str) -> Result<(Self, std::fs::File)> {
        let directory = target.parent().unwrap_or_else(|| Path::new("."));

        for _ in 0..8 {
            let candidate = directory.join(staged_sibling_name(target.as_os_str()));

            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // A full source copy may exist here before final permissions
                // are applied. Never let the process umask expose it to group
                // or other users in a shared/searchable directory.
                options.mode(0o600);
            }
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::fs::OpenOptionsExt;

                use windows_sys::Win32::Foundation::GENERIC_WRITE;
                use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

                // Set the source DACL through this handle before copying any
                // bytes. Denying every share mode from the instant of creation
                // prevents another process from retaining an inherited-ACL
                // read handle across that transition.
                options.access_mode(GENERIC_WRITE | WRITE_DAC).share_mode(0);
            }

            match options.open(&candidate) {
                Ok(file) => {
                    return Ok((
                        Self {
                            path: candidate,
                            persisted: false,
                            #[cfg(unix)]
                            anchored_parent: None,
                        },
                        file,
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to create a temp file beside {target_label}")
                    })
                }
            }
        }

        anyhow::bail!("Failed to create a unique temp file beside {target_label}")
    }

    /// Create an empty, exclusively owned temp file beside the admitted leaf
    /// *through the retained parent handle* — the unix authority-based twin
    /// of [`TempFile::create_beside`].
    ///
    /// The sibling is created with `openat(O_RDWR|O_CREAT|O_EXCL|O_NOFOLLOW)`
    /// beneath the retained parent directory, so creation can neither
    /// traverse a displaced ancestor nor follow a symlink planted at the
    /// candidate name, and the copy starts from that handle without a
    /// reopening window. The temp's `path` records only the bare leaf name:
    /// every consumer — tagging, flush, commit, and the drop cleanup —
    /// addresses the staged leaf through the retained parent, never through
    /// a pathname. Error contexts name the target by `target_label`, never
    /// by any path.
    #[cfg(unix)]
    fn create_beside_retained(
        parent: &File,
        leaf: &OsStr,
        target_label: &str,
    ) -> Result<(Self, std::fs::File)> {
        for _ in 0..8 {
            let candidate_name = staged_sibling_name(leaf);
            match create_retained_sibling_exclusive(parent, &candidate_name) {
                Ok(staged) => {
                    let cleanup_parent = parent.try_clone().with_context(|| {
                        format!("Failed to retain the staging parent of {target_label}")
                    })?;
                    return Ok((
                        Self {
                            path: PathBuf::from(candidate_name),
                            persisted: false,
                            anchored_parent: Some(cleanup_parent),
                        },
                        staged,
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to create a temp file beside {target_label}")
                    })
                }
            }
        }

        anyhow::bail!("Failed to create a unique temp file beside {target_label}")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Reopen an already protected Windows sibling for the actual write.
    ///
    /// Closing the creation handle first makes access pass through the DACL we
    /// just installed. The new handle remains exclusive while content is
    /// copied, so a source DACL that denies a later write fails while the
    /// sibling is still empty rather than after a full copy has been made.
    #[cfg(target_os = "windows")]
    fn reopen_exclusive_for_tagging(&self) -> std::io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;

        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::DELETE;

        let mut options = std::fs::OpenOptions::new();
        // Lofty subsequently needs both read and write access. Proving both on
        // the empty sibling also covers owner-relative ACE behavior after the
        // source DACL is installed on this newly owned file. DELETE covers the
        // access MoveFileExW needs when the sibling becomes the replacement.
        options
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(0);
        options.open(&self.path)
    }

    /// Atomically move the temp file onto `target`, disarming the cleanup.
    ///
    /// On failure `self` is dropped, so the temp file is still removed.
    fn persist_to(&mut self, target: &Path) -> Result<()> {
        std::fs::rename(&self.path, target).with_context(|| {
            format!(
                "Failed to atomically replace {} with the tagged copy",
                target.display()
            )
        })?;
        self.persisted = true;
        Ok(())
    }

    /// Disarm the drop cleanup after an external authority renamed this temp
    /// file into place. The staging name no longer exists, and a failed
    /// best-effort cleanup of a name that was never persisted must not
    /// mislead the guard.
    fn disarm_cleanup(&mut self) {
        self.persisted = true;
    }

    /// Remove a probe sibling and disarm the best-effort drop cleanup.
    ///
    /// A capability check is successful only when cleanup succeeds. Returning
    /// success while leaving a private sibling behind would make merely
    /// opening Properties mutate the library indefinitely.
    fn remove(mut self) -> Result<()> {
        std::fs::remove_file(&self.path).with_context(|| {
            format!(
                "Failed to remove the tag-write probe beside {}",
                self.path.display()
            )
        })?;
        self.persisted = true;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.persisted {
            #[cfg(unix)]
            if let Some(parent) = &self.anchored_parent {
                // Anchored staging: unlink the staged leaf through the
                // retained parent. `unlinkat` never follows a symlink at the
                // leaf and resolves nothing above it, so the cleanup removes
                // exactly the entry this section created and can never
                // mutate a directory the authority did not admit — the
                // pre-anchoring cleanup walked the absolute pathname and
                // removed its stranded sibling from whatever directory an
                // external writer had installed at the old ancestor name.
                let _ = rustix::fs::unlinkat(parent, &self.path, rustix::fs::AtFlags::empty());
                return;
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Returns `true` if the file extension is a format we can write tags to.
///
/// This says nothing about the current filesystem capability. Call
/// [`preflight_tag_write`] before presenting a file as editable.
pub fn supports_tag_writes(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| WRITABLE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Validate the per-file portion of tag-write preflight without mutating the
/// containing directory.
///
/// Batch callers should validate every distinct track, run
/// [`preflight_tag_write_target_access`] for every track, then call
/// [`preflight_tag_write_directory`] once per distinct parent directory.
pub fn validate_tag_write_target(path: &Path) -> Result<(), TagWritePreflightError> {
    if !supports_tag_writes(path) {
        return Err(TagWritePreflightError::UnsupportedFormat);
    }

    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| TagWritePreflightError::Unavailable)?;
    if !metadata.file_type().is_file() {
        return Err(TagWritePreflightError::NotRegularFile);
    }
    #[cfg(target_os = "windows")]
    if metadata.permissions().readonly() {
        return Err(TagWritePreflightError::Unavailable);
    }

    // The real write copies from the source before replacing it. Prove that
    // the same read is currently possible before touching the directory.
    std::fs::File::open(path).map_err(|_| TagWritePreflightError::Unavailable)?;
    Ok(())
}

/// Check file-specific access needed by the atomic tag writer.
///
/// On Windows, two files in one directory may have different DACLs. Apply this
/// exact file's DACL to an empty, exclusively held sibling, close the
/// privileged creation handle, and require a fresh ordinary read/write open
/// through the installed DACL before removing the sibling. No content is ever
/// copied into this probe. Other platforms need only the non-mutating target
/// validation above because permissions are applied after tagging.
///
/// This performs blocking filesystem I/O and must not run on the GTK thread.
pub fn preflight_tag_write_target_access(path: &Path) -> Result<(), TagWritePreflightError> {
    validate_tag_write_target(path)?;

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;

        use windows_sys::Win32::Foundation::GENERIC_READ;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let mut source_options = std::fs::OpenOptions::new();
        source_options
            .access_mode(GENERIC_READ | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        let source = source_options
            .open(path)
            .map_err(|_| TagWritePreflightError::Unavailable)?;
        let source_dacl =
            WindowsDacl::read_from(&source).map_err(|_| TagWritePreflightError::Unavailable)?;
        drop(source);

        let (probe, creation_file) = TempFile::create_beside(path, &path.to_string_lossy())
            .map_err(|_| TagWritePreflightError::Unavailable)?;
        let dacl_result = source_dacl.apply_to(&creation_file);
        drop(creation_file);
        dacl_result.map_err(|_| TagWritePreflightError::Unavailable)?;

        let tagging_file = probe
            .reopen_exclusive_for_tagging()
            .map_err(|_| TagWritePreflightError::Unavailable)?;
        drop(tagging_file);
        probe
            .remove()
            .map_err(|_| TagWritePreflightError::Unavailable)?;
    }

    Ok(())
}

/// Check the parent-directory mechanics needed by the atomic tag writer.
///
/// This exclusively creates, flushes, replaces, and removes two empty private
/// siblings beside the track. Batch callers may run the file-specific access
/// check above for every track, then run this once per distinct parent. That is
/// the only reliable cross-platform way to observe directory capability:
/// permission bits alone do not describe Flatpak read-only bind mounts, portal
/// grants, ACLs, FUSE filesystems, or Windows access rules.
///
/// This performs blocking filesystem I/O and must not run on the GTK thread.
pub fn preflight_tag_write_directory(path: &Path) -> Result<(), TagWritePreflightError> {
    validate_tag_write_target(path)?;

    // Rehearse the complete metadata-only shape of the atomic replacement:
    // create two exclusive siblings, flush them, replace an existing sibling,
    // and require explicit cleanup. The user's audio file is never modified.
    let (mut replacement, replacement_file) =
        TempFile::create_beside(path, &path.to_string_lossy())
            .map_err(|_| TagWritePreflightError::Unavailable)?;
    let replacement_result = replacement_file.sync_all();
    drop(replacement_file);
    replacement_result.map_err(|_| TagWritePreflightError::Unavailable)?;

    let (destination, destination_file) = TempFile::create_beside(path, &path.to_string_lossy())
        .map_err(|_| TagWritePreflightError::Unavailable)?;
    let destination_result = destination_file.sync_all();
    drop(destination_file);
    destination_result.map_err(|_| TagWritePreflightError::Unavailable)?;

    replacement
        .persist_to(destination.path())
        .map_err(|_| TagWritePreflightError::Unavailable)?;
    destination
        .remove()
        .map_err(|_| TagWritePreflightError::Unavailable)
}

/// Check whether the complete atomic tag writer can currently operate.
pub fn preflight_tag_write(path: &Path) -> Result<(), TagWritePreflightError> {
    preflight_tag_write_target_access(path)?;
    preflight_tag_write_directory(path)
}

/// Rehearse the anchored replacement's directory mechanics through the
/// retained parent handle — the anchored twin of
/// [`preflight_tag_write_directory`].
///
/// The rehearsal exclusively creates two private siblings beneath the
/// retained parent, flushes them, replaces one with the other through the
/// retained directory, and unlinks the result: the same create, replace,
/// and remove shapes the anchored writer performs, addressed entirely
/// through the directory handle. No absolute pathname is resolved, so an
/// ancestor displaced after the authority's validation can neither take
/// the probe outside the admitted mount directory nor reject a rehearsal
/// the anchored writer could safely perform.
///
/// `leaf` is the admitted file's leaf name; it only seeds the randomized
/// probe names' extension. Error contexts name the target by
/// `target_label`, never by any path. On any failure the dropped probe
/// siblings clean themselves up through the retained parent.
///
/// This performs blocking filesystem I/O and must not run on the GTK
/// thread.
#[cfg(unix)]
pub fn preflight_tag_write_directory_retained(
    parent: &std::fs::File,
    leaf: &OsStr,
    target_label: &str,
) -> Result<(), TagWritePreflightError> {
    let (mut replacement, replacement_file) =
        TempFile::create_beside_retained(parent, leaf, target_label)
            .map_err(|_| TagWritePreflightError::Unavailable)?;
    replacement_file
        .sync_all()
        .map_err(|_| TagWritePreflightError::Unavailable)?;
    drop(replacement_file);

    let (mut destination, destination_file) =
        TempFile::create_beside_retained(parent, leaf, target_label)
            .map_err(|_| TagWritePreflightError::Unavailable)?;
    destination_file
        .sync_all()
        .map_err(|_| TagWritePreflightError::Unavailable)?;
    drop(destination_file);

    // Replace the destination sibling with the replacement sibling — the
    // rename shape the anchored commit's install performs.
    rustix::fs::renameat(
        parent,
        replacement.path().as_os_str(),
        parent,
        destination.path().as_os_str(),
    )
    .map_err(|_| TagWritePreflightError::Unavailable)?;
    replacement.disarm_cleanup();

    // A capability check is successful only when cleanup succeeds: the
    // probe sibling is removed through the retained parent and the drop
    // guard disarmed.
    rustix::fs::unlinkat(
        parent,
        destination.path().as_os_str(),
        rustix::fs::AtFlags::empty(),
    )
    .map_err(|_| TagWritePreflightError::Unavailable)?;
    destination.disarm_cleanup();
    Ok(())
}

/// Write tag edits to an audio file.
///
/// Only fields that are `Some(...)` in `edits` are modified.
/// This is a blocking operation — call from a background thread.
pub fn write_tags(path: &Path, edits: &TagEdits) -> Result<()> {
    if edits.is_empty() {
        return Ok(());
    }

    // Reject the whole edit before opening anything. A file must never be
    // rewritten for an edit we are going to silently discard.
    edits.validate()?;

    if let Err(failure) = validate_tag_write_target(path) {
        match failure {
            TagWritePreflightError::UnsupportedFormat => anyhow::bail!(
                "Unsupported format for tag writing: {}",
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("unknown")
            ),
            TagWritePreflightError::NotRegularFile => {
                anyhow::bail!("The selected tag-write target is not a regular file")
            }
            TagWritePreflightError::Unavailable => {
                anyhow::bail!("The selected tag-write target is unavailable or read-only")
            }
        }
    }

    // The local pathname is user-visible, so naming it in error contexts is
    // fine here — unlike the authority-based writer below.
    let target_label = path.to_string_lossy();
    let source = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {} for tag writing", path.display()))?;
    atomic_tag_replacement(source, path, &target_label, edits, |temp| {
        temp.persist_to(path)
    })
}

/// Write tag edits to one exact file beneath a retained mounted authority.
///
/// This is the removable-media form of [`write_tags`]. The read source is the
/// retained exact file object — never a pathname lookup — and the atomic
/// replacement is confirmed and performed by the retained authority itself:
/// the mounted root, the retained ancestry, and the exact pathname must still
/// name the file the authority admitted, revalidated immediately before the
/// rename, and the rename runs relative to the retained parent directory so
/// no pathname resolution can retarget it. Every failure leaves the target
/// untouched. This is a blocking operation — call from a background thread.
pub fn write_tags_with_mutation_target(
    target: &MountedMutationTarget,
    edits: &TagEdits,
) -> Result<()> {
    if edits.is_empty() {
        return Ok(());
    }

    // Reject the whole edit before touching the authority. A file must never
    // be rewritten for an edit we are going to silently discard.
    edits.validate()?;

    if !supports_tag_writes(target.replacement_path()) {
        anyhow::bail!(
            "Unsupported format for tag writing: {}",
            target
                .replacement_path()
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("unknown")
        );
    }

    // Open the serialized commit section first: it revalidates the mount,
    // the retained ancestry, and the exact retained file before any byte is
    // read, and holds the target's commit lock through the replacement.
    let mut commit = target.begin_commit().map_err(|error| {
        anyhow::anyhow!("The retained mutation authority is no longer valid: {error}")
    })?;
    let source = commit
        .source_file()
        .with_context(|| "Failed to read the exact retained mutation target".to_string())?;
    write_tag_edits_for_commit(&mut commit, source, target, edits)
}

/// Perform the staged tag replacement for an open commit section.
///
/// The replacement path is a native mount path whose contract forbids
/// logging or formatting it, so every error context names the target by a
/// redacted label instead of the path — the removable-media error branch
/// surfaces the whole chain to the user.
#[cfg(unix)]
#[cfg_attr(
    not(test),
    allow(unused_variables) // only the test-only staging seam reads the target
)]
fn write_tag_edits_for_commit(
    commit: &mut MountedMutationCommit<'_>,
    source: File,
    target: &MountedMutationTarget,
    edits: &TagEdits,
) -> Result<()> {
    // Anchor the staging at the retained parent before anything is staged:
    // the pinned parent identity makes staging and the later install agree
    // on one directory object, and no step below ever resolves an absolute
    // pathname.
    let (staging_parent, staged_leaf) = commit.retained_staging_anchor().map_err(|error| {
        anyhow::Error::new(error).context("The retained mutation authority lost the staging parent")
    })?;
    #[cfg(test)]
    run_pre_staging_interpose(target);
    anchored_atomic_tag_replacement(
        source,
        &staging_parent,
        &staged_leaf,
        "the retained mutation target",
        edits,
        |temp, expected_staged| {
            finish_committed_tag_replacement(commit, temp, Some(expected_staged))
        },
    )
}

/// Platforms without retained parent handles keep the documented path-based
/// staging discipline: the commit re-proves the admitted identity through
/// the pathname before anything is installed, and a staging area stranded
/// by a displaced ancestor is refused and cleaned up by pathname.
#[cfg(not(unix))]
fn write_tag_edits_for_commit(
    commit: &mut MountedMutationCommit<'_>,
    source: File,
    target: &MountedMutationTarget,
    edits: &TagEdits,
) -> Result<()> {
    let replacement_path = target.replacement_path().to_path_buf();
    atomic_tag_replacement(
        source,
        &replacement_path,
        "the retained mutation target",
        edits,
        |temp| finish_committed_tag_replacement(commit, temp, None),
    )
}

/// Rename the staged copy into place through the retained authority and
/// disarm the staging cleanup once the authority owns the name.
///
/// The replacement and the re-anchor of the retained evidence to the
/// installed object both happen inside the commit section, while its guard
/// still holds the target's commit lock — the re-anchor is
/// identity-conditioned on the proven landing, so no pathname is ever
/// opened outside the guard and no leaf swap between the landing and the
/// re-anchor can be silently adopted.
///
/// `expected_staged_identity` is the identity captured from the tagged
/// staging handle before the commit reopens the staging leaf: the commit
/// verifies the leaf still names that exact object before anything is
/// displaced. Path-based staging flows have no retained handle and pass
/// `None`.
fn finish_committed_tag_replacement(
    commit: &mut MountedMutationCommit<'_>,
    temp: &mut TempFile,
    expected_staged_identity: Option<&ObjectIdentity>,
) -> Result<()> {
    commit
        .commit_replacement(temp.path(), expected_staged_identity)
        .map_err(|error| {
            anyhow::Error::new(error)
                .context("The retained mutation authority refused the tagged replacement")
        })?;
    // The retained authority renamed the staged copy into place; the
    // staging name no longer exists for the drop guard to remove.
    temp.disarm_cleanup();
    Ok(())
}

/// Stage and tag the admitted file's replacement with every staged access
/// anchored at the retained parent handle (unix).
///
/// [`TempFile::create_beside_retained`] creates the exclusively-created
/// sibling beneath the retained parent; the admitted bytes are copied into
/// that handle; tagging reads and saves through the same handle; and the
/// flush, permission carry-over, and — on every failure path — the drop
/// cleanup address the staged leaf through the retained parent as well.
/// [`finish_committed_tag_replacement`] then consumes the staged copy by
/// leaf name through the retained parent. No step resolves an absolute
/// pathname, so an ancestor displaced after the section's validation can
/// neither strand the staging area in an impostor directory nor leak the
/// complete tagged copy of the admitted file across a symlink planted at
/// an old name.
#[cfg(unix)]
fn anchored_atomic_tag_replacement(
    mut source: File,
    staging_parent: &File,
    staged_leaf: &OsStr,
    target_label: &str,
    edits: &TagEdits,
    commit_replacement: impl FnOnce(&mut TempFile, &ObjectIdentity) -> Result<()>,
) -> Result<()> {
    let (mut temp, mut staged) =
        TempFile::create_beside_retained(staging_parent, staged_leaf, target_label)?;
    let copy_result = copy_source_into_destination(&mut source, &mut staged, target_label);
    // Capture the replacement's Unix permissions from the exact source
    // object while the handle is still open — never from a lookup at any
    // pathname, which an external writer can retarget (same discipline as
    // the path-based flow).
    let retained_permissions = source
        .metadata()
        .ok()
        .map(|metadata| metadata.permissions());
    drop(source);
    copy_result?;

    write_tags_to_retained(&mut staged, target_label, edits)?;
    flush_and_prepare_tagged_copy_retained(&staged, target_label, retained_permissions)?;
    // Capture the tagged staging object's identity from its retained
    // handle before the handle is dropped. The commit reopens the staging
    // leaf by name and verifies it still names this exact object before
    // anything is displaced, so a stranger swapped over the staging name
    // in the drop-to-commit window refuses the commit instead of being
    // installed as if it were the tagged copy.
    let staged_identity = object_identity(&staged).with_context(|| {
        format!("The tagged staging copy of {target_label} could not be identified for the commit")
    })?;
    drop(staged);
    commit_replacement(&mut temp, &staged_identity)?;

    tracing::debug!("Tags written successfully");
    Ok(())
}

/// Rewind the staged copy and parse it through its retained handle, guessing
/// the format from the content — the anchored twin of the path-based read
/// half of [`write_tags_to`], never touching a pathname.
#[cfg(unix)]
fn read_tagged_file_retained(staged: &mut File, target_label: &str) -> Result<TaggedFile> {
    use std::io::Seek;

    staged
        .rewind()
        .with_context(|| format!("Failed to read tags from {target_label}"))?;
    lofty::probe::Probe::new(&mut *staged)
        .guess_file_type()
        .map_err(anyhow::Error::from)
        .with_context(|| format!("Failed to read tags from {target_label}"))?
        .read()
        .with_context(|| format!("Failed to read tags from {target_label}"))
}

/// Probe, edit, and save the staged copy through its retained handle — the
/// anchored twin of [`write_tags_to`], never touching a pathname.
///
/// lofty's own `save_to_path` is *open a fresh read/write handle, then*
/// `save_to`, so driving the same `save_to` through the anchored sibling
/// handle at position zero is behavior-identical to the path-based flow;
/// the format is guessed from the content instead of the staged name, which
/// the name's random prefix would not identify anyway.
#[cfg(unix)]
fn write_tags_to_retained(staged: &mut File, target_label: &str, edits: &TagEdits) -> Result<()> {
    use std::io::Seek;

    let mut tagged_file = read_tagged_file_retained(staged, target_label)?;

    let tag = ensure_primary_tag(&mut tagged_file, target_label)?;
    apply_tag_edits(tag, edits)?;

    staged
        .rewind()
        .with_context(|| format!("Failed to write tags to {target_label}"))?;
    tag.save_to(staged, WriteOptions::default())
        .with_context(|| format!("Failed to write tags to {target_label}"))?;
    Ok(())
}

/// Flush the tagged copy and carry over the source permissions, addressing
/// the staged leaf through its retained handle — the anchored twin of
/// [`flush_and_prepare_tagged_copy`]. The flush happens *before* the
/// permission copy, exactly like the path-based flow: a read-only file
/// cannot be flushed.
#[cfg(unix)]
fn flush_and_prepare_tagged_copy_retained(
    staged: &File,
    target_label: &str,
    retained_permissions: Option<std::fs::Permissions>,
) -> Result<()> {
    staged
        .sync_all()
        .with_context(|| format!("Failed to flush the tagged copy of {target_label}"))?;

    // Best-effort, exactly like the path-based flow: a capture failure
    // skips the carry-over rather than failing the write.
    if let Some(permissions) = retained_permissions {
        let _ = staged.set_permissions(permissions);
    }

    Ok(())
}

/// `openat(parent, name, O_RDWR|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC, 0o600)`
/// — the creation twin of the retained-parent opens the commit machinery
/// already performs. Resolution starts at the retained parent handle, the
/// final component never follows a symlink, the exclusive create refuses an
/// occupied name, and the private mode keeps a full source copy unexposed
/// before final permissions are applied.
#[cfg(unix)]
fn create_retained_sibling_exclusive(
    parent: &File,
    name: &OsStr,
) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::openat(
        parent,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)?;
    Ok(std::fs::File::from(descriptor))
}

/// Copy `source` to an exclusively created sibling of `target_path`, tag the
/// copy, flush it, and hand it to `commit_replacement` for the atomic
/// replacement of `target_path`.
///
/// This is the path-based staging flow: the user-visible pathname writer
/// uses it directly, and platforms without retained parent handles keep it
/// as their documented authority-based staging discipline (their commit
/// re-proves the admitted identity through the pathname before anything is
/// installed). Unix authority-based callers stage through the retained
/// parent instead — see [`anchored_atomic_tag_replacement`].
///
/// `commit_replacement` runs after the flush and permission carry-over and
/// owns the entire final gate: authority-checked callers prove the exact
/// target identity there and perform the rename through retained handles.
/// Path-based callers have no retained identity to compare and simply rename
/// the staged copy into place; the rename itself is their point-in-time
/// replacement.
///
/// Error contexts never format `target_path`: an authority-based caller's
/// target is a native mount path that must not leak into logs, so contexts
/// name the target by `target_label` instead. Path-based callers pass the
/// user-visible pathname's own text.
fn atomic_tag_replacement(
    source: File,
    target_path: &Path,
    target_label: &str,
    edits: &TagEdits,
    commit_replacement: impl FnOnce(&mut TempFile) -> Result<()>,
) -> Result<()> {
    let mut source = source;
    #[cfg(target_os = "windows")]
    let source_dacl = WindowsDacl::read_from(&source)
        .with_context(|| format!("Failed to read the Windows DACL of {target_label}"))?;

    let (mut temp, destination) = TempFile::create_beside(target_path, target_label)?;
    #[cfg(target_os = "windows")]
    let mut destination = open_tag_copy_destination(source_dacl, destination, &temp, target_label)?;
    #[cfg(not(target_os = "windows"))]
    let mut destination = destination;
    let copy_result = copy_source_into_destination(&mut source, &mut destination, target_label);
    // Capture the replacement's Unix permissions from the exact source
    // object while the handle is still open. A target_path lookup here would
    // read whatever occupies the name at commit time — not necessarily the
    // file whose bytes were just copied — so an external writer that
    // temporarily swaps the pathname to a permissive file could launder those
    // permissions onto the committed replacement.
    #[cfg(unix)]
    let retained_permissions = source
        .metadata()
        .ok()
        .map(|metadata| metadata.permissions());
    drop(source);
    drop(destination);
    copy_result?;

    write_tags_to(temp.path(), target_label, edits)?;
    #[cfg(unix)]
    flush_and_prepare_tagged_copy(&temp, target_label, retained_permissions)?;
    #[cfg(not(unix))]
    flush_and_prepare_tagged_copy(&temp, target_label)?;
    commit_replacement(&mut temp)?;

    tracing::debug!("Tags written successfully");
    Ok(())
}

/// Protect the tagged copy with the source file's original DACL and reopen it
/// exclusively for tagging.
///
/// Windows attribute inheritance applies at creation: the complete DACL must
/// be installed before the first copied byte, and installing it can strip the
/// creation handle's write access, so tagging reopens a fresh exclusive
/// handle afterwards.
#[cfg(target_os = "windows")]
fn open_tag_copy_destination(
    source_dacl: WindowsDacl,
    destination: File,
    temp: &TempFile,
    target_label: &str,
) -> Result<File> {
    let security_result = source_dacl.apply_to(&destination).with_context(|| {
        format!(
            "Failed to protect the tagged copy of {target_label} with its original Windows DACL",
        )
    });
    drop(destination);
    security_result?;
    temp.reopen_exclusive_for_tagging().with_context(|| {
        format!(
            "The original Windows DACL of {target_label} does not permit writing the tagged copy",
        )
    })
}

/// Copy the source bytes into the tagged copy, reporting but not raising a
/// copy failure so the caller can release both handles before surfacing it.
fn copy_source_into_destination(
    source: &mut File,
    destination: &mut File,
    target_label: &str,
) -> Result<()> {
    std::io::copy(source, destination)
        .map(|_| ())
        .with_context(|| format!("Failed to copy {target_label} for tag writing"))
}

/// Flush the tagged copy and carry over the source file's Unix permissions
/// before the caller's final replacement gate runs.
///
/// The flush happens *before* the permission copy: replacing a read-only file
/// would otherwise make the temp read-only too, and a read-only file cannot
/// be flushed. Windows installs the complete DACL before the first copied
/// byte; its std Permissions value represents only the DOS read-only
/// attribute, so the permission carry-over is Unix-only.
///
/// `retained_permissions` is captured from the source file handle — the exact
/// object whose bytes were copied — never from a lookup at the target's name,
/// which an external writer can retarget between admission and commit. Error
/// contexts name the target by `target_label`, never by its native pathname.
fn flush_and_prepare_tagged_copy(
    temp: &TempFile,
    target_label: &str,
    #[cfg(unix)] retained_permissions: Option<std::fs::Permissions>,
) -> Result<()> {
    // Flush the tagged copy before it becomes the user's file. Without this a
    // crash between rename and writeback can leave a truncated file where the
    // original used to be.
    flush_to_disk(temp.path())
        .with_context(|| format!("Failed to flush the tagged copy of {target_label}"))?;

    // Best-effort: match the Unix permissions of the exact file object the
    // bytes were copied from. A capture failure skips the carry-over rather
    // than failing the write; the replacement never invents permissions it
    // could not prove.
    #[cfg(unix)]
    if let Some(permissions) = retained_permissions {
        let _ = std::fs::set_permissions(temp.path(), permissions);
    }

    Ok(())
}

/// Flush a file's contents to disk.
///
/// The handle must be opened for **writing**. Windows implements `sync_all` as
/// `FlushFileBuffers`, which requires `GENERIC_WRITE`, so syncing through a
/// read-only handle fails with access-denied and would break every tag write on
/// that platform.
fn flush_to_disk(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()
}

/// Apply `edits` to the tags of the file at `temp_path` in-place.
///
/// Error contexts name the target by `target_label`, never by `temp_path`:
/// the staged copy is created beside the target, so its path points inside
/// the target's own directory — a native mount directory for
/// authority-based callers — and formatting it would leak exactly the
/// location the redacted label exists to protect.
fn write_tags_to(temp_path: &Path, target_label: &str, edits: &TagEdits) -> Result<()> {
    let mut tagged_file = lofty::read_from_path(temp_path)
        .with_context(|| format!("Failed to read tags from {target_label}"))?;

    let tag = ensure_primary_tag(&mut tagged_file, target_label)?;
    apply_tag_edits(tag, edits)?;

    // Save back to the temp file.
    tag.save_to_path(temp_path, WriteOptions::default())
        .with_context(|| format!("Failed to write tags to {target_label}"))?;

    Ok(())
}

/// Get or create the primary tag for this file type. Files with no
/// existing primary tag (e.g. a stripped MP3, or a FLAC without a Vorbis
/// comment block) need a fresh tag of the file's primary type so new
/// metadata can be authored on them — primary_tag_mut() alone never
/// creates one.
///
/// Errors name the target by `target_label`, never by any path.
fn ensure_primary_tag<'a>(
    tagged_file: &'a mut TaggedFile,
    target_label: &str,
) -> Result<&'a mut Tag> {
    if tagged_file.primary_tag_mut().is_none() {
        let tag_type = tagged_file.primary_tag_type();
        tagged_file.insert_tag(Tag::new(tag_type));
    }

    tagged_file.primary_tag_mut().ok_or_else(|| {
        anyhow::anyhow!("No primary tag found and cannot create one for {target_label}")
    })
}

/// Apply every requested edit to `tag` — only touch fields that are Some —
/// in the order the edits were declared, so field application order stays
/// stable across refactors.
fn apply_tag_edits(tag: &mut Tag, edits: &TagEdits) -> Result<()> {
    apply_core_text_edits(tag, edits);
    apply_contributor_and_genre_edits(tag, edits);
    apply_number_edits(tag, edits)?;
    apply_comment_edit(tag, edits);
    Ok(())
}

fn apply_core_text_edits(tag: &mut Tag, edits: &TagEdits) {
    if let Some(ref title) = edits.title {
        if title.is_empty() {
            tag.remove_title();
        } else {
            tag.set_title(title.clone());
        }
    }

    if let Some(ref artist) = edits.artist {
        if artist.is_empty() {
            tag.remove_artist();
        } else {
            tag.set_artist(artist.clone());
        }
    }

    if let Some(ref album) = edits.album {
        if album.is_empty() {
            tag.remove_album();
        } else {
            tag.set_album(album.clone());
        }
    }
}

fn apply_contributor_and_genre_edits(tag: &mut Tag, edits: &TagEdits) {
    // The album-artist edit was previously declared and counted toward
    // `is_empty()`, but never applied — the file was rewritten and the field
    // silently ignored.
    if let Some(ref album_artist) = edits.album_artist {
        if album_artist.is_empty() {
            tag.remove_key(ItemKey::AlbumArtist);
        } else {
            tag.insert(TagItem::new(
                ItemKey::AlbumArtist,
                ItemValue::Text(album_artist.clone()),
            ));
        }
    }

    if let Some(ref genre) = edits.genre {
        if genre.is_empty() {
            tag.remove_genre();
        } else {
            tag.set_genre(genre.clone());
        }
    }

    if let Some(ref composer) = edits.composer {
        if composer.is_empty() {
            tag.remove_key(ItemKey::Composer);
        } else {
            tag.insert(TagItem::new(
                ItemKey::Composer,
                ItemValue::Text(composer.clone()),
            ));
        }
    }
}

/// These re-parse rather than trusting the caller: an unparseable value must
/// fail the write, never vanish.
fn apply_number_edits(tag: &mut Tag, edits: &TagEdits) -> Result<()> {
    match parse_tag_number("Year", edits.year.as_deref())? {
        NumberEdit::Unchanged => {}
        NumberEdit::Clear => tag.remove_key(ItemKey::Year),
        NumberEdit::Set(year) => {
            tag.insert(TagItem::new(
                ItemKey::Year,
                ItemValue::Text(year.to_string()),
            ));
        }
    }

    match parse_tag_number("Track #", edits.track_number.as_deref())? {
        NumberEdit::Unchanged => {}
        NumberEdit::Clear => tag.remove_track(),
        NumberEdit::Set(track) => tag.set_track(track),
    }

    match parse_tag_number("Disc #", edits.disc_number.as_deref())? {
        NumberEdit::Unchanged => {}
        NumberEdit::Clear => tag.remove_disk(),
        NumberEdit::Set(disc) => tag.set_disk(disc),
    }

    Ok(())
}

fn apply_comment_edit(tag: &mut Tag, edits: &TagEdits) {
    if let Some(ref comment) = edits.comment {
        if comment.is_empty() {
            tag.remove_comment();
        } else {
            tag.set_comment(comment.clone());
        }
    }
}

/// Test-only seam: runs between the commit section's validation and the
/// staging of the tagged copy, driving the exact window an external writer
/// needs to displace an ancestor after validation and before staging. The
/// regression tests plant an impostor directory or symlink at the displaced
/// ancestor's old name here and assert the staged copy never appears on the
/// other side.
#[cfg(all(test, unix))]
type PreStagingInterpose = dyn Fn(&MountedMutationTarget) + Send + Sync;

#[cfg(all(test, unix))]
static PRE_STAGING_INTERPOSE: std::sync::Mutex<Option<Box<PreStagingInterpose>>> =
    std::sync::Mutex::new(None);

#[cfg(all(test, unix))]
static PRE_STAGING_INTERPOSE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, unix))]
fn run_pre_staging_interpose(target: &MountedMutationTarget) {
    if let Some(interpose) = PRE_STAGING_INTERPOSE.lock().unwrap().as_ref() {
        interpose(target);
    }
}

/// Serialize tests that use the pre-staging interposition seam.
#[cfg(all(test, unix))]
fn with_pre_staging_interpose(interpose: Box<PreStagingInterpose>, run: impl FnOnce()) {
    let _serial = PRE_STAGING_INTERPOSE_SERIAL.lock().unwrap();
    *PRE_STAGING_INTERPOSE.lock().unwrap() = Some(interpose);
    run();
    *PRE_STAGING_INTERPOSE.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("tributary-tagwrite-{label}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }

        /// Copy arbitrary fixture bytes into this test's isolated directory.
        fn audio_file(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.path.join(name);
            std::fs::write(&path, contents).expect("write test file");
            path
        }

        fn temp_files(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.path)
                .expect("read test directory")
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.to_string_lossy().contains(".tributary-tag-"))
                .collect()
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn year(value: &str) -> TagEdits {
        TagEdits {
            year: Some(value.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_number_field_accepts_a_value_and_a_deliberate_clear() {
        assert!(year("2026").validate().is_ok());
        assert!(year("").validate().is_ok());
        assert!(TagEdits::default().validate().is_ok());
    }

    #[test]
    fn a_malformed_number_is_rejected_and_names_the_field() {
        for (label, edits) in [
            (
                "Year",
                TagEdits {
                    year: Some("2026a".to_string()),
                    ..Default::default()
                },
            ),
            (
                "Track #",
                TagEdits {
                    track_number: Some("one".to_string()),
                    ..Default::default()
                },
            ),
            (
                "Disc #",
                TagEdits {
                    disc_number: Some("-1".to_string()),
                    ..Default::default()
                },
            ),
        ] {
            let error = edits
                .validate()
                .expect_err("a malformed number must be rejected")
                .to_string();
            assert!(error.contains(label), "error should name {label}: {error}");
        }
    }

    /// The bug this whole change exists for: a bad Year used to rewrite the
    /// file, drop the field, and report success.
    #[test]
    fn a_malformed_number_never_touches_the_file() {
        let directory = TestDirectory::new("reject");
        let track = directory.audio_file("song.mp3", b"original audio bytes");

        let edits = TagEdits {
            artist: Some("Foo Fighters".to_string()),
            year: Some("2026a".to_string()),
            ..Default::default()
        };
        write_tags(&track, &edits).expect_err("a malformed year must fail the write");

        assert_eq!(
            std::fs::read(&track).expect("read back"),
            b"original audio bytes",
            "the file must be byte-for-byte untouched"
        );
        assert!(
            directory.temp_files().is_empty(),
            "no temp file may be left behind"
        );
    }

    /// A failure *after* the copy must still not orphan the temp file. The file
    /// is not decodable audio, so tagging fails and the guard must clean up.
    #[test]
    fn a_failed_write_leaves_no_temp_file_behind() {
        let directory = TestDirectory::new("cleanup");
        let track = directory.audio_file("song.flac", b"not really a flac");

        write_tags(&track, &year("2026")).expect_err("tagging a non-audio file must fail");

        assert_eq!(
            std::fs::read(&track).expect("read back"),
            b"not really a flac",
            "the original must survive a failed tag write"
        );
        assert!(
            directory.temp_files().is_empty(),
            "the temp file must be removed on the failure path"
        );
    }

    #[test]
    fn a_capability_probe_checks_the_real_sibling_path_and_cleans_up() {
        let directory = TestDirectory::new("preflight");
        let track = directory.audio_file("song.FLAC", b"readable fixture bytes");

        preflight_tag_write(&track).expect("a readable file in a writable directory is editable");

        assert!(supports_tag_writes(&track));
        assert!(
            directory.temp_files().is_empty(),
            "a successful capability probe must remove its private sibling"
        );
    }

    #[cfg(unix)]
    #[test]
    fn writer_siblings_never_grant_group_or_other_access() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("private-mode");
        let track = directory.path.join("song.flac");
        let (temp, file) = TempFile::create_beside(&track, &track.to_string_lossy())
            .expect("create private sibling");
        let mode = std::fs::metadata(temp.path())
            .expect("read sibling metadata")
            .permissions()
            .mode();

        assert_eq!(
            mode & 0o077,
            0,
            "writer-owned copies must not be readable or writable by group/other"
        );
        drop(file);
        temp.remove().expect("remove private sibling");
    }

    /// The replacement must carry the permissions of the exact file object
    /// the bytes were copied from — captured from the source handle — not
    /// whatever the target pathname names when the carry-over runs: an
    /// external writer that temporarily swaps the pathname to a permissive
    /// file during the commit must not launder those permissions onto the
    /// replacement.
    #[cfg(unix)]
    #[test]
    fn tagged_copy_carries_permissions_from_the_source_handle_not_the_path() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("permissions-from-handle");
        let track = directory.audio_file("song.flac", b"original audio");
        std::fs::set_permissions(&track, std::fs::Permissions::from_mode(0o600))
            .expect("make the admitted file private");

        // The source handle is the exact object being copied. The pathname
        // meanwhile names a world-readable stranger, as a mid-commit swap
        // would present it.
        let source = std::fs::File::open(&track).expect("open the source handle");
        let stranger = directory.audio_file("stranger.flac", b"stranger audio");
        std::fs::set_permissions(&stranger, std::fs::Permissions::from_mode(0o644))
            .expect("make the stranger permissive");

        let (temp, destination) = TempFile::create_beside(&track, &track.to_string_lossy())
            .expect("create the staging copy");
        drop(destination);

        // Swap the pathname to the permissive stranger for the carry-over,
        // as the race would, then prepare the copy with the permissions
        // captured from the retained handle.
        let displaced = directory.path.join("displaced.flac");
        std::fs::rename(&track, &displaced).expect("displace the admitted file");
        std::fs::rename(&stranger, &track).expect("put the stranger at the target name");

        let retained_permissions = source
            .metadata()
            .expect("stat the retained source handle")
            .permissions();
        flush_and_prepare_tagged_copy(&temp, &track.to_string_lossy(), Some(retained_permissions))
            .expect("prepare the tagged copy");

        let mode = std::fs::metadata(temp.path())
            .expect("read the tagged copy's metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the tagged copy must carry the source handle's permissions, not the swapped pathname's"
        );
        drop(source);
        temp.remove().expect("remove private sibling");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn writer_siblings_install_the_source_dacl_while_exclusively_held() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let directory = TestDirectory::new("private-dacl");
        let track = directory.audio_file("song.flac", b"readable fixture bytes");
        let source = std::fs::File::open(&track).expect("open source");
        let source_dacl = WindowsDacl::read_from(&source).expect("read source DACL");
        let (temp, destination) = TempFile::create_beside(&track, &track.to_string_lossy())
            .expect("create private sibling");

        source_dacl
            .apply_to(&destination)
            .expect("install source DACL");
        let second_open = std::fs::File::open(temp.path())
            .expect_err("the copy handle must deny every competing open");
        assert_eq!(
            second_open.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION as i32)
        );

        let installed_dacl = WindowsDacl::read_from(&destination).expect("read installed DACL");
        assert_eq!(installed_dacl.protected, source_dacl.protected);
        assert_eq!(
            installed_dacl.dacl.is_null(),
            source_dacl.dacl.is_null(),
            "a NULL DACL must retain its original semantics"
        );

        drop(destination);
        let reopened = temp
            .reopen_exclusive_for_tagging()
            .expect("installed source DACL must allow a fresh exclusive write handle");
        assert_eq!(
            reopened.metadata().expect("read sibling metadata").len(),
            0,
            "the DACL must be proven before the first content byte"
        );
        drop(reopened);
        temp.remove().expect("remove private sibling");
    }

    #[cfg(unix)]
    #[test]
    fn preflight_matches_effective_create_access_on_a_read_only_parent() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("readonly-parent");
        let track = directory.audio_file("song.flac", b"readable fixture bytes");
        let original_permissions = std::fs::metadata(&directory.path)
            .expect("read directory metadata")
            .permissions();
        let mut read_only_permissions = original_permissions.clone();
        read_only_permissions.set_mode(0o500);
        std::fs::set_permissions(&directory.path, read_only_permissions)
            .expect("make parent read-only");

        // A privileged or ACL-granted test process may still create here.
        // Compare against the operation itself instead of guessing from mode
        // bits—the same rule the production preflight follows.
        let sentinel = directory.path.join("sentinel");
        let effective_create = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sentinel)
            .is_ok();
        if effective_create {
            std::fs::remove_file(&sentinel).expect("remove privileged sentinel");
        }
        let result = preflight_tag_write(&track);

        std::fs::set_permissions(&directory.path, original_permissions)
            .expect("restore directory permissions");
        if effective_create {
            assert!(result.is_ok(), "effective directory access should pass");
        } else {
            assert_eq!(result, Err(TagWritePreflightError::Unavailable));
        }
        assert!(directory.temp_files().is_empty());
    }

    #[test]
    fn preflight_distinguishes_format_and_invalid_file_failures() {
        let directory = TestDirectory::new("preflight-categories");
        let unsupported = directory.audio_file("song.wav", b"wave bytes");
        let missing = directory.path.join("missing.flac");
        let directory_named_like_audio = directory.path.join("album.flac");
        std::fs::create_dir(&directory_named_like_audio).expect("create directory fixture");

        assert_eq!(
            preflight_tag_write(&unsupported),
            Err(TagWritePreflightError::UnsupportedFormat)
        );
        assert_eq!(
            preflight_tag_write(&missing),
            Err(TagWritePreflightError::Unavailable)
        );
        assert_eq!(
            preflight_tag_write(&directory_named_like_audio),
            Err(TagWritePreflightError::NotRegularFile)
        );
        assert!(directory.temp_files().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn preflight_never_replaces_a_symlink_track() {
        let directory = TestDirectory::new("preflight-symlink");
        let target = directory.audio_file("target.flac", b"target bytes");
        let link = directory.path.join("linked.flac");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink fixture");

        assert_eq!(
            preflight_tag_write(&link),
            Err(TagWritePreflightError::NotRegularFile)
        );
        assert!(link.is_symlink());
        assert_eq!(std::fs::read(target).expect("read target"), b"target bytes");
    }

    #[test]
    fn probe_cleanup_failure_is_reported_before_success() {
        let directory = TestDirectory::new("preflight-cleanup-failure");
        let track = directory.path.join("song.flac");
        let (temp, file) = TempFile::create_beside(&track, &track.to_string_lossy())
            .expect("create private sibling");
        let temp_path = temp.path().to_path_buf();
        drop(file);

        std::fs::remove_file(&temp_path).expect("remove probe file");
        std::fs::create_dir(&temp_path).expect("replace probe file with directory fixture");
        assert!(
            temp.remove().is_err(),
            "cleanup failure must not be reported as writable"
        );

        std::fs::remove_dir(&temp_path).expect("remove directory fixture");
    }

    #[test]
    fn a_valid_flac_round_trips_every_supported_edit() {
        use std::time::Duration;

        use lofty::file::AudioFile;

        let directory = TestDirectory::new("happy-path");
        let track = directory.audio_file(
            "silence.flac",
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/audio/silence.flac"
            )),
        );
        let edits = TagEdits {
            title: Some("Fixture Title".to_string()),
            artist: Some("Fixture Artist".to_string()),
            album: Some("Fixture Album".to_string()),
            album_artist: Some("Fixture Album Artist".to_string()),
            genre: Some("Fixture Genre".to_string()),
            composer: Some("Fixture Composer".to_string()),
            year: Some("2026".to_string()),
            track_number: Some("7".to_string()),
            disc_number: Some("2".to_string()),
            comment: Some("Fixture comment".to_string()),
        };

        write_tags(&track, &edits).expect("write every supported edit to a valid FLAC");
        assert!(
            directory.temp_files().is_empty(),
            "a successful write must not leave its sibling temp file behind"
        );

        let tagged_file = lofty::read_from_path(&track).expect("reopen tagged FLAC");
        assert_eq!(
            tagged_file.properties().duration(),
            Duration::from_millis(100),
            "tagging must preserve the readable audio stream"
        );
        let tag = tagged_file
            .primary_tag()
            .expect("tagged FLAC must have a primary tag");
        assert_eq!(tag.title().as_deref(), Some("Fixture Title"));
        assert_eq!(tag.artist().as_deref(), Some("Fixture Artist"));
        assert_eq!(tag.album().as_deref(), Some("Fixture Album"));
        assert_eq!(
            tag.get_string(ItemKey::AlbumArtist),
            Some("Fixture Album Artist")
        );
        assert_eq!(tag.genre().as_deref(), Some("Fixture Genre"));
        assert_eq!(tag.get_string(ItemKey::Composer), Some("Fixture Composer"));
        assert_eq!(tag.get_string(ItemKey::Year), Some("2026"));
        assert_eq!(tag.track(), Some(7));
        assert_eq!(tag.disk(), Some(2));
        assert_eq!(tag.comment().as_deref(), Some("Fixture comment"));
    }

    /// A removable-media write through a retained mutation target must
    /// succeed exactly like the path-based happy path, and the in-section
    /// re-anchor must authorize a follow-up write through the same
    /// target object.
    #[test]
    fn a_mutation_target_write_round_trips_and_reanchors_for_a_follow_up() {
        let directory = TestDirectory::new("mutation-write");
        let track = directory.audio_file(
            "silence.flac",
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/audio/silence.flac"
            )),
        );
        let authority = std::sync::Arc::new(
            crate::local::root_authority::MountedRootAuthority::acquire(&directory.path)
                .expect("acquire mounted authority"),
        );
        let target = authority
            .open_mutation_target(Path::new("silence.flac"))
            .expect("open mutation target");

        write_tags_with_mutation_target(&target, &year("2026"))
            .expect("write through the retained authority");
        assert!(
            directory.temp_files().is_empty(),
            "a successful write must not leave its sibling temp file behind"
        );

        // The replacement retired the copied-from object; the in-section
        // re-anchor must have re-bound the target, so a second edit through
        // the same retained authority is still authorized.
        write_tags_with_mutation_target(&target, &year("2027"))
            .expect("follow-up write after the re-anchor");

        let tagged_file = lofty::read_from_path(&track).expect("reopen tagged FLAC");
        let tag = tagged_file
            .primary_tag()
            .expect("tagged FLAC must have a primary tag");
        assert_eq!(tag.get_string(ItemKey::Year), Some("2027"));
    }

    /// The bug this authority exists for: if the pathname is swapped between
    /// selection and commit, the write must fail closed — the swap stays
    /// untouched, the admitted file keeps its exact bytes, and no private
    /// sibling is left behind.
    #[test]
    fn a_mutation_target_write_refuses_a_swapped_file_and_leaves_both_untouched() {
        let directory = TestDirectory::new("mutation-refuse");
        let track = directory.audio_file(
            "silence.flac",
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/audio/silence.flac"
            )),
        );
        let original = std::fs::read(&track).expect("read fixture bytes");

        let authority = std::sync::Arc::new(
            crate::local::root_authority::MountedRootAuthority::acquire(&directory.path)
                .expect("acquire mounted authority"),
        );
        let target = authority
            .open_mutation_target(Path::new("silence.flac"))
            .expect("open mutation target");

        // Swap the pathname after the authority admitted the exact file.
        let displaced = directory.path.join("displaced.flac");
        std::fs::rename(&track, &displaced).expect("displace the admitted file");
        std::fs::write(&track, b"not the admitted file").expect("install different bytes");

        write_tags_with_mutation_target(&target, &year("2026"))
            .expect_err("the pathname no longer names the admitted file; the commit must refuse");

        assert_eq!(
            std::fs::read(&track).expect("read swapped path back"),
            b"not the admitted file",
            "the refused write must not replace whatever took the name"
        );
        assert_eq!(
            std::fs::read(&displaced).expect("read displaced file back"),
            original,
            "the admitted file must be byte-for-byte untouched"
        );
        assert!(
            directory.temp_files().is_empty(),
            "a refused commit leaves no private sibling behind"
        );
    }

    /// Create an album directory holding the silence.flac fixture — the
    /// common arrangement of the anchored-staging regression tests, whose
    /// retained authority admits `album/silence.flac` through its parent.
    #[cfg(unix)]
    fn anchored_album_fixture(name: &str) -> (TestDirectory, PathBuf, PathBuf) {
        let directory = TestDirectory::new(name);
        let album = directory.path.join("album");
        std::fs::create_dir(&album).expect("create album");
        let track = album.join("silence.flac");
        std::fs::write(
            &track,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/audio/silence.flac"
            )),
        )
        .expect("write fixture");
        (directory, album, track)
    }

    /// Acquire the mounted root authority over a test directory.
    #[cfg(unix)]
    fn mounted_root_authority(
        directory: &TestDirectory,
    ) -> std::sync::Arc<crate::local::root_authority::MountedRootAuthority> {
        std::sync::Arc::new(
            crate::local::root_authority::MountedRootAuthority::acquire(&directory.path)
                .expect("acquire mounted authority"),
        )
    }

    /// Assert the replacement landed beside the admitted file in the
    /// retained (displaced) directory, carrying the written year.
    #[cfg(unix)]
    fn assert_replacement_landed_beside_admitted_file(displaced_album: &Path) {
        let tagged_file = lofty::read_from_path(displaced_album.join("silence.flac"))
            .expect("reopen the replaced admitted file");
        assert_eq!(
            tagged_file
                .primary_tag()
                .expect("primary tag")
                .get_string(ItemKey::Year),
            Some("2026"),
            "the replacement must land beside the admitted file in the retained directory"
        );
    }

    /// A parent directory displaced between selection and commit must not
    /// strand the staging area inside whatever now occupies the old
    /// pathname: the retained authority stages and replaces only through its
    /// retained parent object, so the write lands beside the admitted file
    /// in the retained directory and the impostor never receives anything
    /// at all — not the staged sibling, not the tagged copy.
    #[cfg(unix)]
    #[test]
    fn a_mutation_target_write_lands_beside_the_retained_parent_when_it_was_displaced() {
        let (directory, album, track) = anchored_album_fixture("mutation-parent-e2e");

        let authority = mounted_root_authority(&directory);
        let target = authority
            .open_mutation_target(Path::new("album/silence.flac"))
            .expect("open mutation target");

        let displaced_album = directory.path.join("displaced-album");
        std::fs::rename(&album, &displaced_album).expect("displace retained parent");
        std::fs::create_dir(&album).expect("install impostor parent");
        std::fs::write(&track, b"impostor audio").expect("install impostor file");

        write_tags_with_mutation_target(&target, &year("2026"))
            .expect("staging and replacement run through the retained parent");

        // The replacement landed beside the admitted file in the retained
        // (displaced) directory.
        assert_replacement_landed_beside_admitted_file(&displaced_album);

        // The impostor keeps exactly what it had. Under path-resolved
        // staging it transiently received the staged sibling and the
        // complete tagged copy of the admitted file before the commit
        // refused; anchored staging never resolves its name at all.
        assert_eq!(
            std::fs::read(&track).expect("read impostor file"),
            b"impostor audio",
            "the impostor directory must never receive the replacement"
        );
        let impostor_entries: Vec<std::ffi::OsString> = std::fs::read_dir(&album)
            .expect("list impostor directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            impostor_entries,
            [std::ffi::OsString::from("silence.flac")],
            "the impostor directory must never receive a staged sibling"
        );
        assert_no_stranded_tag_write_siblings(&[&album, &displaced_album]);
    }

    /// The staging of a retained-authority copy must never traverse the
    /// absolute-path ancestry. This drives the exact window an external
    /// writer needs — after the commit section validated the retained chain,
    /// before the writer stages the sibling — and installs a symlink at the
    /// displaced ancestor's old name, pointing outside the mount. Path-
    /// resolved staging would create the staged sibling through that symlink
    /// and leak the complete tagged copy of the admitted file outside the
    /// retained authority; anchored staging resolves nothing, so the write
    /// lands beside the admitted file in the retained directory and the
    /// symlink's target stays empty.
    #[cfg(unix)]
    #[test]
    fn a_mutation_target_write_stages_beside_the_retained_parent_never_through_a_displaced_ancestor(
    ) {
        use std::os::unix::fs::symlink;

        let (directory, album, track) = anchored_album_fixture("mutation-stage-anchor");
        let outside = TestDirectory::new("mutation-stage-outside");

        let authority = mounted_root_authority(&directory);
        let target = authority
            .open_mutation_target(Path::new("album/silence.flac"))
            .expect("open mutation target");

        let displaced_album = directory.path.join("displaced-album");
        let impostor_album = album.clone();
        let outside_root = outside.path.clone();
        let watched = track.clone();
        let closure_displaced_album = displaced_album.clone();
        with_pre_staging_interpose(
            Box::new(move |interposed| {
                if interposed.replacement_path() != watched.as_path() {
                    return;
                }
                // Displace the retained ancestor inside the staging window
                // and plant a symlink at its old name.
                std::fs::rename(
                    watched.parent().expect("album parent"),
                    &closure_displaced_album,
                )
                .expect("displace retained parent");
                symlink(&outside_root, &impostor_album)
                    .expect("install symlink impostor at the old name");
            }),
            || {
                write_tags_with_mutation_target(&target, &year("2026"))
                    .expect("anchored staging must land the write through the retained parent");
            },
        );

        // The replacement landed beside the admitted file in the retained
        // (displaced) directory.
        assert_replacement_landed_beside_admitted_file(&displaced_album);

        // Nothing — staged sibling or tagged copy — may ever have crossed
        // the symlink into the impostor's target.
        let outside_entries: Vec<std::ffi::OsString> = std::fs::read_dir(&outside.path)
            .expect("list the symlink impostor's target")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            outside_entries.is_empty(),
            "the displaced ancestor's symlink target must never receive anything: \
             {outside_entries:?}"
        );
        assert_no_stranded_tag_write_siblings(&[&displaced_album]);
    }

    /// The removable preflight's directory rehearsal must probe the
    /// retained directory, not the pathname: after the retained parent was
    /// displaced and an impostor directory installed at the old name, the
    /// rehearsal's create, replace, and remove siblings happen beside the
    /// admitted file in the retained (displaced) directory, the impostor
    /// never receives anything, and no probe sibling survives.
    #[cfg(unix)]
    #[test]
    fn retained_preflight_rehearsal_probes_the_retained_directory_after_displacement() {
        let (directory, album, _) = anchored_album_fixture("preflight-retained-anchor");

        let authority = mounted_root_authority(&directory);
        let target = authority
            .open_mutation_target(Path::new("album/silence.flac"))
            .expect("open mutation target");
        let (parent, leaf) = target
            .retained_directory_handle()
            .expect("retain the directory anchor");

        let displaced_album = directory.path.join("displaced-album");
        std::fs::rename(&album, &displaced_album).expect("displace retained parent");
        std::fs::create_dir(&album).expect("install impostor directory at the old name");

        crate::local::tag_writer::preflight_tag_write_directory_retained(
            &parent,
            &leaf,
            "the removable mutation target",
        )
        .expect("the rehearsal must run through the retained parent");

        // The impostor at the old pathname never saw a probe sibling.
        let impostor_entries: Vec<std::ffi::OsString> = std::fs::read_dir(&album)
            .expect("list the impostor directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            impostor_entries.is_empty(),
            "the impostor directory must receive no probe siblings: {impostor_entries:?}"
        );

        // The retained directory holds exactly the admitted file: every
        // probe sibling was removed again through the retained parent.
        let retained_entries: Vec<std::ffi::OsString> = std::fs::read_dir(&displaced_album)
            .expect("list the retained directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            retained_entries,
            vec![std::ffi::OsString::from("silence.flac")],
            "the rehearsal must leave only the admitted file in the retained directory"
        );
    }

    /// A refused commit must clean up its stranded staging sibling in every
    /// directory the retained parent machinery could have staged one in.
    #[cfg(unix)]
    fn assert_no_stranded_tag_write_siblings(directories: &[&Path]) {
        for directory_path in directories {
            let leftovers: Vec<PathBuf> = std::fs::read_dir(directory_path)
                .expect("list displaced directories")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| is_tag_write_temp_file(path))
                .collect();
            assert!(
                leftovers.is_empty(),
                "a refused commit must clean up its stranded sibling: {leftovers:?}"
            );
        }
    }

    #[test]
    fn an_unsupported_format_is_refused_before_any_copy() {
        let directory = TestDirectory::new("format");
        let track = directory.audio_file("notes.txt", b"text");

        write_tags(&track, &year("2026")).expect_err("an unsupported format must be refused");
        assert!(directory.temp_files().is_empty());
    }

    /// `sync_all` is `FlushFileBuffers` on Windows and needs `GENERIC_WRITE`,
    /// so flushing through a read-only handle fails there with access-denied.
    /// Nothing else in this module reaches the flush, so without this test the
    /// break would only surface on a user's machine.
    #[test]
    fn a_tagged_copy_can_be_flushed_to_disk() {
        let directory = TestDirectory::new("flush");
        let track = directory.audio_file("song.mp3", b"audio");

        flush_to_disk(&track).expect("a file we just wrote must be flushable");
    }

    /// Two concurrent saves to the same track must not share a temp path. The
    /// old fixed `.tributary-tag-tmp` suffix meant they clobbered each other.
    #[test]
    fn temp_paths_are_unique_and_exclusively_created() {
        let directory = TestDirectory::new("exclusive");
        let track = directory.path.join(format!("{}.FLAC", "x".repeat(220)));

        let (first, first_handle) =
            TempFile::create_beside(&track, &track.to_string_lossy()).expect("first temp");
        let (second, second_handle) =
            TempFile::create_beside(&track, &track.to_string_lossy()).expect("second temp");

        assert_ne!(first.path(), second.path());
        assert!(first.path().exists());
        assert!(second.path().exists());
        assert_eq!(first.path().parent(), track.parent());
        assert_eq!(
            first.path().extension().and_then(|ext| ext.to_str()),
            Some("flac")
        );
        assert!(is_tag_write_temp_file(first.path()));
        assert!(
            first.path().file_name().unwrap().len() < 80,
            "the sibling component must not inherit the long source filename"
        );

        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        // Windows deliberately denies FILE_SHARE_DELETE while a tag-write
        // sibling is open. Match every production cleanup path by releasing
        // the exclusive handles before the TempFile guards remove their paths.
        drop(first_handle);
        drop(second_handle);
        drop(first);
        drop(second);
        assert!(
            !first_path.exists(),
            "the first unpersisted temp file must remove itself"
        );
        assert!(
            !second_path.exists(),
            "the second unpersisted temp file must remove itself"
        );
    }
}
