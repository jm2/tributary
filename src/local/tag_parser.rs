//! Audio tag parser — wraps `lofty` to extract metadata from audio files.

use std::fs::File;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::Accessor;

use super::tag_writer::is_tag_write_temp_file;

/// Supported audio file extensions.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "wma", "aiff", "aif",
];

/// Returns `true` for an indexable path with a supported audio extension.
/// Private tag-write siblings deliberately remain outside the library.
pub fn is_audio_file(path: &Path) -> bool {
    let supported = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false);

    supported && !is_tag_write_temp_file(path)
}

/// Parsed metadata from a single audio file.
#[derive(Debug, Clone)]
pub struct ParsedTrack {
    pub file_path: String,
    pub title: String,
    /// Whether `title` came from an explicit audio tag. When false, `title` is
    /// only the parser's filename/display fallback and is not authoritative
    /// attribution metadata.
    pub title_from_tag: bool,
    pub artist_name: String,
    /// Whether `artist_name` came from an explicit audio tag. When false, the
    /// value is the presentation-only `Unknown Artist` fallback.
    pub artist_from_tag: bool,
    pub album_artist_name: Option<String>,
    pub album_title: String,
    /// Whether `album_title` came from an explicit audio tag. When false, the
    /// value is the presentation-only `Unknown Album` fallback.
    pub album_from_tag: bool,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub duration_secs: Option<u64>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub composer: Option<String>,
    pub format: String,
    pub date_modified: DateTime<Utc>,
    pub file_size_bytes: Option<u64>,
}

/// Parse an audio file at `path` using lofty + filesystem metadata.
///
/// This delegates parsing of untrusted file bytes to `lofty`, whose
/// contract is to return `Err` on malformed input rather than panic, so no
/// `catch_unwind` isolation is added here. Callers that invoke this directly
/// on the GTK main thread rely on that contract; the scan paths additionally
/// run it inside `spawn_blocking`, which already isolates any panic.
pub fn parse_audio_file(path: &Path) -> Result<ParsedTrack> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open audio file {}", path.display()))?;
    parse_audio_file_from_file(file, path)
}

/// Parse metadata from an already-open audio file.
///
/// Filesystem-authority callers use this entry point so tag parsing consumes
/// the exact object they opened beneath a retained library-root handle instead
/// of resolving the pathname a second time.
pub fn parse_audio_file_from_file(mut file: File, path: &Path) -> Result<ParsedTrack> {
    // Preserve `read_from_path`'s extension-based format selection while
    // giving lofty the already-authorized descriptor instead of letting it
    // reopen `path`. Fall back to content probing only for an unknown suffix.
    let tagged_file = match FileType::from_path(path) {
        Some(file_type) => Probe::with_file_type(&mut file, file_type).read(),
        None => lofty::read_from(&mut file),
    }
    .with_context(|| format!("Failed to read tags from {}", path.display()))?;

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());
    let props = tagged_file.properties();

    // Extract tag fields.
    //
    // Every tag-derived text value is trimmed of *trailing* whitespace only.
    // Legacy ID3v1 fields are fixed-width and space-padded to the end of the
    // field, and some other taggers write sloppy trailing whitespace; those
    // padding spaces must not be imported as part of the value. Leading and
    // internal whitespace are preserved, since they can be meaningful.
    let tagged_title = tag.and_then(|t| t.title().map(|s| s.trim_end().to_string()));
    let title_from_tag = tagged_title.is_some();
    let title = tagged_title.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string()
    });

    let tagged_artist = tag.and_then(|t| t.artist().map(|s| s.trim_end().to_string()));
    let artist_from_tag = tagged_artist.is_some();
    let artist_name = tagged_artist.unwrap_or_else(|| "Unknown Artist".to_string());

    let album_artist_name = tag.and_then(|t| {
        use lofty::tag::ItemKey;
        t.get_string(ItemKey::AlbumArtist)
            .map(|s| s.trim_end().to_string())
    });

    let composer = tag.and_then(|t| {
        use lofty::tag::ItemKey;
        t.get_string(ItemKey::Composer)
            .map(|s| s.trim_end().to_string())
    });

    let tagged_album = tag.and_then(|t| t.album().map(|s| s.trim_end().to_string()));
    let album_from_tag = tagged_album.is_some();
    let album_title = tagged_album.unwrap_or_else(|| "Unknown Album".to_string());

    let genre = tag.and_then(|t| t.genre().map(|s| s.trim_end().to_string()));
    // The year is not always exposed under `ItemKey::Year`. Vorbis-comment
    // formats (FLAC, Ogg, Opus) conventionally store it as the Xiph-standard
    // `DATE` field, and ID3v2 tags carry it as TYER/TDRC — lofty unifies all
    // of those under `ItemKey::RecordingDate`, while the non-standard vorbis
    // `YEAR` field and ID3v1's year land under `ItemKey::Year`. Reading only
    // `ItemKey::Year` silently dropped the year for Ogg/FLAC files tagged
    // with `DATE`. lofty's `Accessor::date()` reads `RecordingDate` first and
    // falls back to `Year`, and parses either as a relaxed timestamp (so a
    // full `2007-05-03` date also yields the year).
    let year = tag.and_then(|t| t.date().map(|date| i32::from(date.year)));
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
    let metadata = file
        .metadata()
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;

    let date_modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let date_modified: DateTime<Utc> = date_modified.into();

    let file_size_bytes = Some(metadata.len());

    Ok(ParsedTrack {
        file_path: path.to_string_lossy().to_string(),
        title,
        title_from_tag,
        artist_name,
        artist_from_tag,
        album_artist_name,
        album_title,
        album_from_tag,
        genre,
        composer,
        year,
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

    fn write_minimal_wav(path: &Path) {
        let data_size = 1_u32;
        let mut bytes = Vec::with_capacity(45);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.push(128);
        std::fs::write(path, bytes).expect("write minimal WAV fixture");
    }

    #[test]
    fn parses_from_the_supplied_file_handle() {
        let path = std::env::temp_dir().join(format!(
            "tributary-tag-parser-handle-{}.wav",
            uuid::Uuid::new_v4()
        ));
        write_minimal_wav(&path);
        let file = File::open(&path).expect("open WAV fixture");

        let parsed = parse_audio_file_from_file(file, &path).expect("parse supplied handle");

        assert_eq!(parsed.file_path, path.to_string_lossy());
        assert_eq!(parsed.format, "WAV");
        assert_eq!(parsed.file_size_bytes, Some(45));
        assert_eq!(parsed.title, path.file_stem().unwrap().to_string_lossy());
        assert!(!parsed.title_from_tag);
        assert_eq!(parsed.artist_name, "Unknown Artist");
        assert!(!parsed.artist_from_tag);
        assert_eq!(parsed.album_title, "Unknown Album");
        assert!(!parsed.album_from_tag);
        std::fs::remove_file(path).expect("remove WAV fixture");
    }

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
    fn test_is_audio_file_reserves_only_real_tag_write_siblings() {
        let sibling = Path::new("/music/.tributary-tag-00000000-0000-4000-8000-000000000000.flac");

        assert!(is_tag_write_temp_file(sibling));
        assert!(!is_audio_file(sibling));
        assert!(!is_tag_write_temp_file(Path::new(
            "/music/.tributary-tag-00000000000040008000000000000000.flac"
        )));
        assert!(!is_tag_write_temp_file(Path::new(
            "/music/.tributary-tag-00000000-0000-4000-8000-000000000000.wav"
        )));
        assert!(is_audio_file(Path::new(
            "/music/.tributary-tag-not-a-uuid.flac"
        )));
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

    /// Write fixture bytes to a uniquely named temp file with the right
    /// extension so `FileType::from_path` selects the intended parser.
    fn fixture_path(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tributary-tag-parser-fixture-{}-{}",
            uuid::Uuid::new_v4(),
            name
        ));
        std::fs::write(&path, contents).expect("write audio fixture");
        path
    }

    const FLAC_DATE_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/audio/flac_date_2007.flac"
    ));
    const OGG_DATE_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/audio/ogg_date_2007.ogg"
    ));
    const ID3V1_PADDED_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/audio/id3v1_padded.mp3"
    ));

    fn parse_fixture(name: &str, contents: &[u8]) -> ParsedTrack {
        let path = fixture_path(name, contents);
        let file = File::open(&path).expect("open fixture");
        let parsed = parse_audio_file_from_file(file, &path).expect("parse fixture");
        std::fs::remove_file(path).expect("remove fixture");
        parsed
    }

    /// Vorbis-comment formats (FLAC, Ogg, Opus) conventionally store the year
    /// in the Xiph-standard `DATE` field. lofty exposes that under
    /// `ItemKey::RecordingDate`, not `ItemKey::Year` — importing must still
    /// recognize it (issue #207).
    #[test]
    fn flac_date_field_year_is_recognized() {
        let parsed = parse_fixture("date.flac", FLAC_DATE_FIXTURE);

        assert_eq!(parsed.year, Some(2007));
        assert_eq!(parsed.format, "FLAC");
        assert!(parsed.title_from_tag);
        assert!(parsed.artist_from_tag);
        assert!(parsed.album_from_tag);
    }

    #[test]
    fn ogg_date_field_year_is_recognized() {
        let parsed = parse_fixture("date.ogg", OGG_DATE_FIXTURE);

        assert_eq!(parsed.year, Some(2007));
        assert_eq!(parsed.format, "OGG");
    }

    /// Trailing padding — including the space-padded fixed-width ID3v1 fields
    /// that legacy taggers wrote — must not be imported as part of the value,
    /// while leading and internal whitespace stay meaningful (issue #207).
    #[test]
    fn trailing_whitespace_is_trimmed_but_leading_and_internal_are_kept() {
        let parsed = parse_fixture("trim.flac", FLAC_DATE_FIXTURE);

        assert_eq!(parsed.title, "Two  Spaces  Trailing");
        assert_eq!(parsed.artist_name, "  Flac Artist");
        assert_eq!(parsed.album_title, "Pad Album");
        assert_eq!(parsed.genre.as_deref(), Some("Rock"));
        assert_eq!(parsed.composer.as_deref(), Some("Pad Composer"));
        assert_eq!(parsed.album_artist_name.as_deref(), Some("Pad AlbumArtist"));
    }

    #[test]
    fn trailing_whitespace_is_trimmed_for_ogg() {
        let parsed = parse_fixture("trim.ogg", OGG_DATE_FIXTURE);

        assert_eq!(parsed.title, "Two  Spaces  Trailing");
        assert_eq!(parsed.artist_name, "  Ogg Artist");
    }

    /// Legacy ID3v1 tags pad every fixed-width field with spaces to the end of
    /// the field (no NUL terminator). lofty decodes those bytes verbatim, so
    /// the importer must trim the trailing spaces — this is the exact case
    /// reported in issue #207.
    #[test]
    fn id3v1_space_padded_fields_are_trimmed() {
        let parsed = parse_fixture("legacy.mp3", ID3V1_PADDED_FIXTURE);

        assert_eq!(parsed.format, "MP3");
        assert_eq!(parsed.title, "Pad Title");
        assert_eq!(parsed.artist_name, "Pad Artist");
        assert_eq!(parsed.album_title, "Pad Album");
        // ID3v1 stores a fixed 4-digit year; it must still be recognized.
        assert_eq!(parsed.year, Some(2007));
    }

    /// A `DATE` value may carry a full date (e.g. `2007-05-03`); the relaxed
    /// timestamp parse used for the year must still yield the year.
    #[test]
    fn full_date_in_date_field_yields_the_year() {
        // Reuse the FLAC fixture through lofty's writer to set a full date,
        // then parse the result with the production entry point.
        let source = fixture_path("full.flac", FLAC_DATE_FIXTURE);
        {
            use lofty::config::WriteOptions;
            use lofty::tag::{ItemKey, TagExt};

            let mut tagged = lofty::read_from_path(&source).expect("reopen fixture");
            let tag = tagged
                .primary_tag_mut()
                .expect("fixture carries a vorbis comment");
            tag.insert_text(ItemKey::RecordingDate, "2007-05-03".to_string());
            tag.save_to_path(&source, WriteOptions::default())
                .expect("rewrite fixture with full date");
        }

        let file = File::open(&source).expect("open rewritten fixture");
        let parsed = parse_audio_file_from_file(file, &source).expect("parse rewritten fixture");
        std::fs::remove_file(source).expect("remove rewritten fixture");

        assert_eq!(parsed.year, Some(2007));
    }
}
