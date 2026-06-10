//! Audio tag writer — wraps `lofty` to safely write metadata to audio files.
//!
//! Supports MP3 (ID3v2), M4A/AAC (MP4 atoms), OGG Vorbis, and FLAC.
//! All writes go through [`write_tags`] which opens the file, applies
//! only the changed fields, and saves.  The caller is responsible for
//! triggering a library re-scan of the modified file afterwards.

use std::path::Path;

use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{Accessor, TagExt};

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

    if !is_writable(path) {
        anyhow::bail!(
            "Unsupported format for tag writing: {}",
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("unknown")
        );
    }

    // Atomic-ish write: copy the file to a sibling temp path, apply tags
    // there, then atomically rename it back. This way a power loss /
    // panic / disk-full mid-write leaves the original audio file
    // untouched. The cost is a full file copy per save, which is fine
    // for tag editing (interactive, not a hot path).
    let temp_path = sibling_temp_path(path);
    std::fs::copy(path, &temp_path).with_context(|| {
        format!(
            "Failed to copy {} to {} for atomic tag write",
            path.display(),
            temp_path.display()
        )
    })?;

    // From here on, ensure the temp file is removed on any error path.
    let result = write_tags_to(&temp_path, edits);

    match result {
        Ok(()) => {
            // Best-effort: copy permissions from the original so the
            // renamed file matches what it replaces.
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = std::fs::set_permissions(&temp_path, meta.permissions());
            }
            std::fs::rename(&temp_path, path).with_context(|| {
                format!(
                    "Failed to atomically replace {} with tagged temp {}",
                    path.display(),
                    temp_path.display()
                )
            })?;
            tracing::debug!("Tags written successfully");
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(e)
        }
    }
}

/// Build a sibling temp path next to `path` so `rename` stays on the
/// same filesystem (cross-FS rename returns `EXDEV`).
fn sibling_temp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tributary-tag-tmp");
    let mut out = path.to_path_buf();
    out.set_file_name(name);
    out
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

    if let Some(ref genre) = edits.genre {
        if genre.is_empty() {
            tag.remove_genre();
        } else {
            tag.set_genre(genre.clone());
        }
    }

    if let Some(ref year_str) = edits.year {
        if year_str.is_empty() {
            tag.remove_key(lofty::tag::ItemKey::Year);
        } else if let Ok(_y) = year_str.parse::<u32>() {
            use lofty::tag::{ItemKey, ItemValue, TagItem};
            tag.insert(TagItem::new(
                ItemKey::Year,
                ItemValue::Text(year_str.clone()),
            ));
        }
    }

    if let Some(ref track_str) = edits.track_number {
        if track_str.is_empty() {
            tag.remove_track();
        } else if let Ok(n) = track_str.parse::<u32>() {
            tag.set_track(n);
        }
    }

    if let Some(ref disc_str) = edits.disc_number {
        if disc_str.is_empty() {
            tag.remove_disk();
        } else if let Ok(n) = disc_str.parse::<u32>() {
            tag.set_disk(n);
        }
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
