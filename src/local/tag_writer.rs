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
//! - **The temp file never outlives a failure.** It is owned by an RAII guard
//!   that removes it on every error path, including a failed rename.
//! - **The temp path is unguessable and exclusively created** (`O_EXCL` via
//!   `create_new`), so two concurrent saves to the same file cannot collide and
//!   the copy cannot be redirected through a pre-planted symlink.
//! - **The replacement is durable**: the tagged copy is `fsync`ed before the
//!   rename, so a crash cannot leave a truncated file in place of the original.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{Accessor, ItemKey, ItemValue, TagExt, TagItem};
use uuid::Uuid;

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

/// A temp file that deletes itself unless it is explicitly persisted.
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
        let file_name = target.file_name().unwrap_or_default();

        for _ in 0..8 {
            let mut candidate_name = file_name.to_os_string();
            candidate_name.push(format!(".tributary-tag-{}.tmp", Uuid::new_v4()));
            let candidate = directory.join(candidate_name);

            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
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
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Supported formats for tag writing.
const WRITABLE_EXTENSIONS: &[&str] = &["mp3", "m4a", "aac", "ogg", "flac"];

/// Returns `true` if the file extension is a format we can write tags to.
pub fn is_writable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| WRITABLE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
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

    if !is_writable(path) {
        anyhow::bail!(
            "Unsupported format for tag writing: {}",
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("unknown")
        );
    }

    // Copy the file to an exclusively created sibling, tag the copy, then
    // atomically rename it back, so a power loss, panic, or full disk
    // mid-write leaves the original audio file untouched. The cost is one full
    // file copy per save, which is fine for interactive tag editing.
    //
    // `temp` removes itself on every early return below.
    let (temp, mut destination) = TempFile::create_beside(path)?;

    let mut source = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {} for tag writing", path.display()))?;
    std::io::copy(&mut source, &mut destination)
        .with_context(|| format!("Failed to copy {} for tag writing", path.display()))?;
    drop(source);
    drop(destination);

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

    // Best-effort: match the permissions of the file being replaced.
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

        /// A file that is *named* like audio but is not decodable, which is all
        /// these tests need: every assertion here is about what happens before
        /// lofty succeeds, or when it fails.
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
        let track = directory.audio_file("song.mp3", b"audio");

        let (first, _first_handle) = TempFile::create_beside(&track).expect("first temp");
        let (second, _second_handle) = TempFile::create_beside(&track).expect("second temp");

        assert_ne!(first.path(), second.path());
        assert!(first.path().exists());
        assert!(second.path().exists());
        assert_eq!(first.path().parent(), track.parent());

        let first_path = first.path().to_path_buf();
        drop(first);
        assert!(
            !first_path.exists(),
            "an unpersisted temp file must remove itself"
        );
    }
}
