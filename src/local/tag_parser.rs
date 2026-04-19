//! Audio tag parser — wraps `lofty` to extract metadata from audio files.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;

/// Supported audio file extensions.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "wma", "aiff", "aif",
];

/// Returns `true` if the path has a supported audio extension.
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Parsed metadata from a single audio file.
#[derive(Debug, Clone)]
pub struct ParsedTrack {
    pub file_path: String,
    pub title: String,
    pub artist_name: String,
    pub album_artist_name: Option<String>,
    pub album_title: String,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub duration_secs: Option<u64>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub format: String,
    pub date_modified: DateTime<Utc>,
    pub file_size_bytes: Option<u64>,
}

/// Parse an audio file at `path` using lofty + filesystem metadata.
pub fn parse_audio_file(path: &Path) -> Result<ParsedTrack> {
    // Read tags via lofty
    let tagged_file = lofty::read_from_path(path)
        .with_context(|| format!("Failed to read tags from {}", path.display()))?;

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());
    let props = tagged_file.properties();

    // Extract tag fields
    let title = tag
        .and_then(|t| t.title().map(|s| s.to_string()))
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown")
                .to_string()
        });

    let artist_name = tag
        .and_then(|t| t.artist().map(|s| s.to_string()))
        .unwrap_or_else(|| "Unknown Artist".to_string());

    let album_artist_name = tag.and_then(|t| {
        use lofty::tag::ItemKey;
        t.get_string(&ItemKey::AlbumArtist).map(str::to_string)
    });

    let album_title = tag
        .and_then(|t| t.album().map(|s| s.to_string()))
        .unwrap_or_else(|| "Unknown Album".to_string());

    let genre = tag.and_then(|t| t.genre().map(|s| s.to_string()));
    let year = tag.and_then(|t| t.year());
    let track_number = tag.and_then(|t| t.track());
    let disc_number = tag.and_then(|t| t.disk());

    // Audio properties
    let duration_secs = Some(props.duration().as_secs());
    let bitrate_kbps = props.audio_bitrate();
    let sample_rate_hz = props.sample_rate();

    // File format from extension
    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown")
        .to_uppercase();

    // Filesystem metadata
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;

    let date_modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let date_modified: DateTime<Utc> = date_modified.into();

    let file_size_bytes = Some(metadata.len());

    Ok(ParsedTrack {
        file_path: path.to_string_lossy().to_string(),
        title,
        artist_name,
        album_artist_name,
        album_title,
        genre,
        year: year.map(|y| y as i32),
        track_number,
        disc_number,
        duration_secs,
        bitrate_kbps,
        sample_rate_hz,
        format,
        date_modified,
        file_size_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_is_audio_file_supported_extensions() {
        for ext in AUDIO_EXTENSIONS {
            let filename = format!("test.{ext}");
            let path = Path::new(&filename);
            assert!(is_audio_file(path), "Expected {ext} to be recognized");
        }
    }

    #[test]
    fn test_is_audio_file_case_insensitive() {
        assert!(is_audio_file(Path::new("song.FLAC")));
        assert!(is_audio_file(Path::new("song.Mp3")));
        assert!(is_audio_file(Path::new("song.M4A")));
    }

    #[test]
    fn test_is_audio_file_unsupported() {
        assert!(!is_audio_file(Path::new("image.png")));
        assert!(!is_audio_file(Path::new("document.pdf")));
        assert!(!is_audio_file(Path::new("video.mkv")));
        assert!(!is_audio_file(Path::new("playlist.m3u")));
        assert!(!is_audio_file(Path::new("readme.txt")));
    }

    #[test]
    fn test_is_audio_file_no_extension() {
        assert!(!is_audio_file(Path::new("noextension")));
        assert!(!is_audio_file(Path::new(".")));
        assert!(!is_audio_file(Path::new(".hidden")));
    }

    #[test]
    fn test_is_audio_file_empty_path() {
        assert!(!is_audio_file(Path::new("")));
    }

    #[test]
    fn test_is_audio_file_dotfile_with_audio_ext() {
        // .flac as a filename (no stem) — extension is "flac" on some platforms
        // but Path::extension() returns None for ".flac" (it's the stem).
        assert!(!is_audio_file(Path::new(".flac")));
    }

    #[test]
    fn test_is_audio_file_nested_path() {
        assert!(is_audio_file(Path::new(
            "/home/user/Music/Artist/Album/track.flac"
        )));
        assert!(is_audio_file(Path::new("C:\\Users\\Music\\song.mp3")));
    }

    #[test]
    fn test_audio_extensions_list_completeness() {
        // Verify the list contains the most common formats.
        assert!(AUDIO_EXTENSIONS.contains(&"flac"));
        assert!(AUDIO_EXTENSIONS.contains(&"mp3"));
        assert!(AUDIO_EXTENSIONS.contains(&"m4a"));
        assert!(AUDIO_EXTENSIONS.contains(&"ogg"));
        assert!(AUDIO_EXTENSIONS.contains(&"opus"));
        assert!(AUDIO_EXTENSIONS.contains(&"wav"));
        assert!(AUDIO_EXTENSIONS.contains(&"aac"));
        assert!(AUDIO_EXTENSIONS.contains(&"wma"));
        assert!(AUDIO_EXTENSIONS.contains(&"aiff"));
        assert!(AUDIO_EXTENSIONS.contains(&"aif"));
    }
}
