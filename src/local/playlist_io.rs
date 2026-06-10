//! Playlist import/export — XSPF format.
//!
//! Exports regular and smart playlist track lists to XSPF (XML Shareable
//! Playlist Format), and imports XSPF files by matching tracks against
//! the local library using fingerprint reconciliation.
//!
//! M3U is intentionally not supported: it relies on filesystem paths
//! that break on library reorganisation, contradicting Tributary's
//! design of surviving library rebuilds via metadata fingerprinting.

use std::path::Path;

use sea_orm::sea_query::{Expr, Func};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use tracing::{debug, info, warn};
use url::Url;

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
/// Scans the whole document tag-by-tag (rather than line-by-line) so it is
/// robust to minified files, multiple elements packed on one line, values
/// spanning multiple lines, attributes on elements, and closing tags that
/// share a line with their parent (e.g. `</track></trackList>`). Only the
/// five predefined XML entities are recognised (no DTD/entity expansion),
/// keeping the parser immune to XXE / entity-expansion attacks.
///
/// Returns a list of `ImportedTrack` with whatever metadata the file provides.
pub fn import_xspf(path: &Path) -> anyhow::Result<Vec<ImportedTrack>> {
    let content = std::fs::read_to_string(path)?;

    let mut tracks = Vec::new();
    let mut search_from = 0;

    // Walk each `<track>…</track>` block in document order.
    while let Some(rel) = content[search_from..].find("<track") {
        let track_open = search_from + rel;

        // Distinguish `<track>`/`<track …>` from `<trackList>` and similar:
        // the character after `<track` must terminate the element name.
        let after = content[track_open + "<track".len()..].chars().next();
        if !matches!(after, Some('>' | ' ' | '\t' | '\n' | '\r' | '/')) {
            search_from = track_open + "<track".len();
            continue;
        }

        let Some(close_rel) = content[track_open..].find("</track>") else {
            break;
        };
        let block_end = track_open + close_rel;
        let block = &content[track_open..block_end];

        let mut current = ImportedTrack {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            file_path: String::new(),
            duration_secs: None,
        };

        if let Some(val) = extract_xml_value(block, "location") {
            current.file_path = uri_to_file_path(&xml_unescape(&val));
        }
        if let Some(val) = extract_xml_value(block, "title") {
            current.title = xml_unescape(&val);
        }
        if let Some(val) = extract_xml_value(block, "creator") {
            current.artist = xml_unescape(&val);
        }
        if let Some(val) = extract_xml_value(block, "album") {
            current.album = xml_unescape(&val);
        }
        if let Some(val) = extract_xml_value(block, "duration") {
            if let Ok(ms) = val.trim().parse::<u64>() {
                current.duration_secs = Some(ms / 1000);
            }
        }

        tracks.push(current);
        search_from = block_end + "</track>".len();
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
        //
        // Compare case-insensitively (lower() on both sides), consistent with
        // PlaylistManager::reconcile_all, so trivial casing/whitespace
        // differences don't drop a clearly-present track.
        if !imp.title.is_empty() && !imp.artist.is_empty() {
            let mut query = track::Entity::find()
                .filter(
                    Expr::expr(Func::lower(Expr::col(track::Column::Title)))
                        .eq(imp.title.to_lowercase()),
                )
                .filter(
                    Expr::expr(Func::lower(Expr::col(track::Column::ArtistName)))
                        .eq(imp.artist.to_lowercase()),
                );

            if !imp.album.is_empty() {
                query = query.filter(
                    Expr::expr(Func::lower(Expr::col(track::Column::AlbumTitle)))
                        .eq(imp.album.to_lowercase()),
                );
            }

            if let Ok(candidates) = query.all(db).await {
                // Duration is a ranking signal, not a hard gate: pick the
                // closest-duration candidate when duration info is present,
                // otherwise the first candidate. A clear title+artist(+album)
                // match is never dropped over a missing or slightly-off
                // duration (which would otherwise fall through to the path
                // strategy and end up unmatched).
                let best = if let Some(target_dur) = imp.duration_secs {
                    candidates.iter().min_by_key(|t| {
                        let track_dur = t.duration_secs.unwrap_or(0);
                        (track_dur - target_dur as i64).unsigned_abs()
                    })
                } else {
                    candidates.first()
                };
                if let Some(t) = best {
                    debug!(title = %imp.title, "Matched by fingerprint");
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

/// Convert a filesystem path to a `file://` URI.
///
/// Uses `Url::from_file_path`, which percent-encodes reserved/space
/// characters and handles the Windows drive-letter form, producing a
/// conformant URI. Falls back to manual construction only for the rare
/// relative path (`from_file_path` requires an absolute path).
fn file_path_to_uri(path: &str) -> String {
    Url::from_file_path(path).map_or_else(
        |()| {
            // Relative path: best-effort manual encoding (preserves the
            // previous behaviour for non-absolute inputs).
            let normalized = path.replace('\\', "/");
            if normalized.starts_with('/') {
                format!("file://{normalized}")
            } else {
                format!("file:///{normalized}")
            }
        },
        |url| url.to_string(),
    )
}

/// Convert a `file://` URI back to a filesystem path.
///
/// Uses `Url::to_file_path`, which percent-decodes the path and keeps the
/// leading slash on Unix absolute paths (the old `strip_prefix("file:///")`
/// dropped it). Non-`file` or unparseable inputs are returned verbatim.
fn uri_to_file_path(uri: &str) -> String {
    Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .map_or_else(
            || uri.to_string(),
            |path| path.to_string_lossy().into_owned(),
        )
}

/// Extract the text content of an XML element like `<tag>value</tag>`.
///
/// Scans `content` (which may span multiple lines) for an opening `<tag>` or
/// `<tag …>` (attributes ignored) and reads up to the matching `</tag>`.
fn extract_xml_value(content: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");

    let mut from = 0;
    while let Some(rel) = content[from..].find(&open) {
        let open_at = from + rel;

        // The character after the tag name must terminate it, so searching
        // for "title" doesn't spuriously match "<titlebar>".
        let after = content[open_at + open.len()..].chars().next();
        if !matches!(after, Some('>' | ' ' | '\t' | '\n' | '\r' | '/')) {
            from = open_at + open.len();
            continue;
        }

        // Find the end of the opening tag.
        let gt_rel = content[open_at..].find('>')?;
        let value_start = open_at + gt_rel + 1;

        // Self-closing element (`<tag/>`) has no text content.
        if content[open_at..value_start].ends_with("/>") {
            from = value_start;
            continue;
        }

        let close_rel = content[value_start..].find(&close)?;
        return Some(content[value_start..value_start + close_rel].to_string());
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
///
/// `&amp;` must be resolved LAST: resolving it first would re-introduce a
/// leading `&` that the subsequent replacements then consume, corrupting
/// any text that contained a literal entity substring (e.g. `&lt;`).
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
