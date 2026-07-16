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
//!   the copy cannot be redirected through a pre-planted symlink.
//! - **A Windows copy is never exposed through an inherited DACL.** Its handle
//!   denies competing opens from creation until the source DACL and protection
//!   state are installed; a fresh exclusive write handle must then pass that
//!   DACL before the first audio byte is copied.
//! - **The replacement is durable**: the tagged copy is `fsync`ed before the
//!   rename, so a crash cannot leave a truncated file in place of the original.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{Accessor, ItemKey, ItemValue, TagExt, TagItem};
use uuid::Uuid;

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
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
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
        if unsafe { GetSecurityDescriptorControl(snapshot.descriptor, &mut control, &mut revision) }
            == 0
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
}

impl TempFile {
    /// Create an empty, exclusively owned temp file beside `target`.
    ///
    /// A sibling keeps the later `rename` on one filesystem (a cross-device
    /// rename fails with `EXDEV`). The name is randomized and created with
    /// `create_new` (`O_EXCL`) so it cannot collide with a concurrent save or
    /// follow a symlink an attacker planted at a predictable path.
    fn create_beside(target: &Path) -> Result<(Self, std::fs::File)> {
        let directory = target.parent().unwrap_or_else(|| Path::new("."));
        let extension = target
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);

        for _ in 0..8 {
            // `lofty::read_from_path` infers the format from the final
            // extension. Preserve only that extension: including the whole
            // source name could overflow a filesystem's component-length
            // limit for an otherwise valid long filename.
            let mut candidate_name =
                std::ffi::OsString::from(format!("{TAG_WRITE_TEMP_PREFIX}{}", Uuid::new_v4()));
            if let Some(extension) = extension.as_deref() {
                candidate_name.push(".");
                candidate_name.push(extension);
            }
            let candidate = directory.join(candidate_name);

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
                        },
                        file,
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to create a temp file beside {}", target.display())
                    })
                }
            }
        }

        anyhow::bail!(
            "Failed to create a unique temp file beside {}",
            target.display()
        )
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
    fn reopen_exclusive_for_write(&self) -> std::io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;

        let mut options = std::fs::OpenOptions::new();
        options.write(true).share_mode(0);
        options.open(&self.path)
    }

    /// Atomically move the temp file onto `target`, disarming the cleanup.
    ///
    /// On failure `self` is dropped, so the temp file is still removed.
    fn persist_to(mut self, target: &Path) -> Result<()> {
        std::fs::rename(&self.path, target).with_context(|| {
            format!(
                "Failed to atomically replace {} with the tagged copy",
                target.display()
            )
        })?;
        self.persisted = true;
        Ok(())
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
/// Batch callers can validate every distinct track, then call
/// [`preflight_tag_write`] once per distinct parent directory to avoid
/// needlessly flushing probe files for every row in a large selection.
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

/// Check whether the atomic tag writer can currently operate on `path`.
///
/// In addition to validating the input, this exclusively creates, flushes,
/// replaces, and removes two empty private siblings beside the track. That is
/// the only reliable cross-platform way to observe the directory capability
/// the real writer needs: permission bits alone do not describe Flatpak
/// read-only bind mounts, portal grants, ACLs, FUSE filesystems, or Windows
/// access rules.
///
/// This performs blocking filesystem I/O and must not run on the GTK thread.
pub fn preflight_tag_write(path: &Path) -> Result<(), TagWritePreflightError> {
    validate_tag_write_target(path)?;

    #[cfg(target_os = "windows")]
    let source_dacl = {
        let source = std::fs::File::open(path).map_err(|_| TagWritePreflightError::Unavailable)?;
        WindowsDacl::read_from(&source).map_err(|_| TagWritePreflightError::Unavailable)?
    };

    // Rehearse the complete metadata-only shape of the atomic replacement:
    // create two exclusive siblings, flush them, replace an existing sibling,
    // and require explicit cleanup. The user's audio file is never modified.
    let (replacement, replacement_file) =
        TempFile::create_beside(path).map_err(|_| TagWritePreflightError::Unavailable)?;
    #[cfg(target_os = "windows")]
    let replacement_file = {
        let dacl_result = source_dacl.apply_to(&replacement_file);
        drop(replacement_file);
        dacl_result.map_err(|_| TagWritePreflightError::Unavailable)?;
        replacement
            .reopen_exclusive_for_write()
            .map_err(|_| TagWritePreflightError::Unavailable)?
    };
    let replacement_result = replacement_file.sync_all();
    drop(replacement_file);
    replacement_result.map_err(|_| TagWritePreflightError::Unavailable)?;

    let (destination, destination_file) =
        TempFile::create_beside(path).map_err(|_| TagWritePreflightError::Unavailable)?;
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

    // Copy the file to an exclusively created sibling, tag the copy, then
    // atomically rename it back, so a power loss, panic, or full disk
    // mid-write leaves the original audio file untouched. The cost is one full
    // file copy per save, which is fine for interactive tag editing.
    //
    // `temp` removes itself on every early return below.
    let mut source = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {} for tag writing", path.display()))?;
    #[cfg(target_os = "windows")]
    let source_dacl = WindowsDacl::read_from(&source)
        .with_context(|| format!("Failed to read the Windows DACL of {}", path.display()))?;

    let (temp, destination) = TempFile::create_beside(path)?;
    #[cfg(target_os = "windows")]
    let mut destination = {
        let security_result = source_dacl.apply_to(&destination).with_context(|| {
            format!(
                "Failed to protect the tagged copy of {} with its original Windows DACL",
                path.display()
            )
        });
        drop(destination);
        security_result?;
        temp.reopen_exclusive_for_write().with_context(|| {
            format!(
                "The original Windows DACL of {} does not permit writing the tagged copy",
                path.display()
            )
        })?
    };
    #[cfg(not(target_os = "windows"))]
    let mut destination = destination;
    let copy_result = std::io::copy(&mut source, &mut destination)
        .map(|_| ())
        .with_context(|| format!("Failed to copy {} for tag writing", path.display()));
    drop(source);
    drop(destination);
    copy_result?;

    write_tags_to(temp.path(), edits)?;

    // Flush the tagged copy before it becomes the user's file. Without this a
    // crash between rename and writeback can leave a truncated file where the
    // original used to be.
    //
    // This happens *before* the permission copy below: replacing a read-only
    // file would otherwise make the temp read-only too, and a read-only file
    // cannot be flushed.
    flush_to_disk(temp.path())
        .with_context(|| format!("Failed to flush the tagged copy of {}", path.display()))?;

    // Best-effort: match the Unix permissions of the file being replaced.
    // Windows installs the complete DACL before the first copied byte above;
    // its std Permissions value represents only the DOS read-only attribute.
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(temp.path(), metadata.permissions());
    }

    temp.persist_to(path)?;
    tracing::debug!("Tags written successfully");
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
fn write_tags_to(temp_path: &Path, edits: &TagEdits) -> Result<()> {
    let mut tagged_file = lofty::read_from_path(temp_path)
        .with_context(|| format!("Failed to read tags from {}", temp_path.display()))?;

    // Get or create the primary tag for this file type. Files with no
    // existing primary tag (e.g. a stripped MP3, or a FLAC without a Vorbis
    // comment block) need a fresh tag of the file's primary type so new
    // metadata can be authored on them — primary_tag_mut() alone never
    // creates one.
    if tagged_file.primary_tag_mut().is_none() {
        let tag_type = tagged_file.primary_tag_type();
        tagged_file.insert_tag(lofty::tag::Tag::new(tag_type));
    }

    let tag = tagged_file.primary_tag_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "No primary tag found and cannot create one for {}",
            temp_path.display()
        )
    })?;

    // Apply edits — only touch fields that are Some.
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

    // These re-parse rather than trusting the caller: an unparseable value must
    // fail the write, never vanish.
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

    if let Some(ref comment) = edits.comment {
        if comment.is_empty() {
            tag.remove_comment();
        } else {
            tag.set_comment(comment.clone());
        }
    }

    // Save back to the temp file.
    tag.save_to_path(temp_path, WriteOptions::default())
        .with_context(|| format!("Failed to write tags to {}", temp_path.display()))?;

    Ok(())
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
        let (temp, file) = TempFile::create_beside(&track).expect("create private sibling");
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

    #[cfg(target_os = "windows")]
    #[test]
    fn writer_siblings_install_the_source_dacl_while_exclusively_held() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let directory = TestDirectory::new("private-dacl");
        let track = directory.audio_file("song.flac", b"readable fixture bytes");
        let source = std::fs::File::open(&track).expect("open source");
        let source_dacl = WindowsDacl::read_from(&source).expect("read source DACL");
        let (temp, destination) = TempFile::create_beside(&track).expect("create private sibling");

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
            .reopen_exclusive_for_write()
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
        let (temp, file) = TempFile::create_beside(&track).expect("create private sibling");
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

        let (first, _first_handle) = TempFile::create_beside(&track).expect("first temp");
        let (second, _second_handle) = TempFile::create_beside(&track).expect("second temp");

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
        drop(first);
        assert!(
            !first_path.exists(),
            "an unpersisted temp file must remove itself"
        );
    }
}
