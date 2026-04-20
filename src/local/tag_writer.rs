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

    let mut tagged_file = lofty::read_from_path(path)
        .with_context(|| format!("Failed to read tags from {}", path.display()))?;

    // Get or create the primary tag for this file type.
    let tag = tagged_file.primary_tag_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "No primary tag found and cannot create one for {}",
            path.display()
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

    // Save back to the file.
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("Failed to write tags to {}", path.display()))?;

    tracing::debug!("Tags written successfully");
    Ok(())
}
