//! Playlist import/export — XSPF format.
//!
//! Exports regular and smart playlist track lists to XSPF (XML Shareable
//! Playlist Format), and imports XSPF files by matching tracks against
//! the local library using fingerprint reconciliation.
//!
//! M3U is intentionally not supported: it relies on filesystem paths
//! that break on library reorganisation, contradicting Tributary's
//! design of surviving library rebuilds via metadata fingerprinting.

use std::io::BufRead;
use std::path::Path;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use tracing::{debug, info, warn};

use crate::db::entities::track;

// ── Exported track data ─────────────────────────────────────────────

/// A track parsed from an imported playlist file.
#[derive(Debug, Clone)]
pub struct ImportedTrack {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub file_path: String,
    pub duration_secs: Option<u64>,
}

// ── XSPF Export ─────────────────────────────────────────────────────

/// Export a list of tracks to an XSPF file.
///
/// Writes a valid XSPF v1 XML document containing `<location>`, `<title>`,
/// `<creator>`, `<album>`, and `<duration>` for each track.
pub fn export_xspf(tracks: &[track::Model], path: &Path) -> anyhow::Result<()> {
    use std::io::Write;

    let mut file = std::fs::File::create(path)?;

    writeln!(file, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>")?;
    writeln!(
        file,
        "<playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">"
    )?;
    writeln!(file, "  <trackList>")?;

    for t in tracks {
        writeln!(file, "    <track>")?;

        // Location: file URI with XML escaping
        let location = xml_escape(&file_path_to_uri(&t.file_path));
        writeln!(file, "      <location>{location}</location>")?;

        if !t.title.is_empty() {
            writeln!(file, "      <title>{}</title>", xml_escape(&t.title))?;
        }
        if !t.artist_name.is_empty() {
            writeln!(
                file,
                "      <creator>{}</creator>",
                xml_escape(&t.artist_name)
            )?;
        }
        if !t.album_title.is_empty() {
            writeln!(file, "      <album>{}</album>", xml_escape(&t.album_title))?;
        }
        if let Some(dur) = t.duration_secs {
            // XSPF duration is in milliseconds.
            writeln!(file, "      <duration>{}</duration>", dur * 1000)?;
        }

        writeln!(file, "    </track>")?;
    }

    writeln!(file, "  </trackList>")?;
    writeln!(file, "</playlist>")?;

    info!(
        path = %path.display(),
        tracks = tracks.len(),
        "XSPF playlist exported"
    );
    Ok(())
}

// ── XSPF Import ─────────────────────────────────────────────────────

/// Import tracks from an XSPF file.
///
/// Performs a simple streaming parse (no full XML DOM needed).
/// Returns a list of `ImportedTrack` with whatever metadata the file provides.
pub fn import_xspf(path: &Path) -> anyhow::Result<Vec<ImportedTrack>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    let mut tracks = Vec::new();
    let mut in_track = false;
    let mut current = ImportedTrack {
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        file_path: String::new(),
        duration_secs: None,
    };

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();

        if trimmed.starts_with("<track>") || trimmed.starts_with("<track ") {
            in_track = true;
            current = ImportedTrack {
                title: String::new(),
                artist: String::new(),
                album: String::new(),
                file_path: String::new(),
                duration_secs: None,
            };
        } else if trimmed == "</track>" {
            if in_track {
                tracks.push(current.clone());
            }
            in_track = false;
        } else if in_track {
            if let Some(val) = extract_xml_value(trimmed, "location") {
                current.file_path = uri_to_file_path(&xml_unescape(&val));
            } else if let Some(val) = extract_xml_value(trimmed, "title") {
                current.title = xml_unescape(&val);
            } else if let Some(val) = extract_xml_value(trimmed, "creator") {
                current.artist = xml_unescape(&val);
            } else if let Some(val) = extract_xml_value(trimmed, "album") {
                current.album = xml_unescape(&val);
            } else if let Some(val) = extract_xml_value(trimmed, "duration") {
                if let Ok(ms) = val.parse::<u64>() {
                    current.duration_secs = Some(ms / 1000);
                }
            }
        }
    }

    info!(
        path = %path.display(),
        tracks = tracks.len(),
        "XSPF playlist imported"
    );
    Ok(tracks)
}

// ── Track matching ──────────────────────────────────────────────────

/// Match imported tracks against the local database.
///
/// Uses fingerprint matching: `(title, artist, album, duration±5s)`.
/// Falls back to file_path if metadata matching fails.
///
/// Returns `(matched, unmatched)`.
pub async fn match_imported_tracks(
    db: &DatabaseConnection,
    imported: &[ImportedTrack],
) -> (Vec<track::Model>, Vec<ImportedTrack>) {
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();

    for imp in imported {
        // Strategy 1: metadata fingerprint match.
        if !imp.title.is_empty() && !imp.artist.is_empty() {
            let mut query = track::Entity::find()
                .filter(track::Column::Title.eq(&imp.title))
                .filter(track::Column::ArtistName.eq(&imp.artist));

            if !imp.album.is_empty() {
                query = query.filter(track::Column::AlbumTitle.eq(&imp.album));
            }

            if let Ok(candidates) = query.all(db).await {
                // If we have duration info, pick the closest match.
                if let Some(target_dur) = imp.duration_secs {
                    let best = candidates.iter().min_by_key(|t| {
                        let track_dur = t.duration_secs.unwrap_or(0);
                        (track_dur - target_dur as i64).unsigned_abs()
                    });
                    if let Some(t) = best {
                        let track_dur = t.duration_secs.unwrap_or(0);
                        if (track_dur - target_dur as i64).unsigned_abs() <= 5 {
                            debug!(title = %imp.title, "Matched by fingerprint");
                            matched.push(t.clone());
                            continue;
                        }
                    }
                } else if let Some(t) = candidates.first() {
                    debug!(title = %imp.title, "Matched by title+artist");
                    matched.push(t.clone());
                    continue;
                }
            }
        }

        // Strategy 2: file path match.
        if !imp.file_path.is_empty() {
            if let Ok(Some(t)) = track::Entity::find()
                .filter(track::Column::FilePath.eq(&imp.file_path))
                .one(db)
                .await
            {
                debug!(path = %imp.file_path, "Matched by file path");
                matched.push(t);
                continue;
            }
        }

        warn!(title = %imp.title, artist = %imp.artist, "No match found");
        unmatched.push(imp.clone());
    }

    info!(
        matched = matched.len(),
        unmatched = unmatched.len(),
        "Track matching complete"
    );
    (matched, unmatched)
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Convert a filesystem path to a `file:///` URI.
fn file_path_to_uri(path: &str) -> String {
    // On Windows, paths like C:\Music\... need to become file:///C:/Music/...
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        format!("file://{normalized}")
    } else {
        format!("file:///{normalized}")
    }
}

/// Convert a `file:///` URI back to a filesystem path.
fn uri_to_file_path(uri: &str) -> String {
    let path = uri
        .strip_prefix("file:///")
        .or_else(|| uri.strip_prefix("file://"))
        .unwrap_or(uri);

    // On Windows, convert forward slashes back.
    #[cfg(target_os = "windows")]
    let path = path.replace('/', "\\");

    #[cfg(not(target_os = "windows"))]
    let path = path.to_string();

    path
}

/// Extract the text content of a simple XML element like `<tag>value</tag>`.
fn extract_xml_value(line: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    if let Some(start) = line.find(&open) {
        if let Some(end) = line.find(&close) {
            let val_start = start + open.len();
            if val_start < end {
                return Some(line[val_start..end].to_string());
            }
        }
    }
    None
}

/// Basic XML escaping for text content.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Reverse XML entity escaping.
fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}
