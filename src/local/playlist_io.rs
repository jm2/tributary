//! Playlist import/export — XSPF format.
//!
//! Exports regular and smart playlist track lists to XSPF (XML Shareable
//! Playlist Format), and imports XSPF files by matching tracks against
//! the local library using fingerprint reconciliation.
//!
//! M3U is intentionally not supported: it relies on filesystem paths
//! that break on library reorganisation, contradicting Tributary's
//! design of surviving library rebuilds via metadata fingerprinting.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use tracing::{info, warn};
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
/// `<creator>`, and `<album>` for each track, plus `<duration>` whenever the
/// stored value is non-negative and representable as `u64` milliseconds.
pub fn export_xspf(tracks: &[track::Model], path: &Path) -> anyhow::Result<()> {
    // Validate and render the whole document before touching the destination.
    let document = serialize_xspf(tracks)?;
    let parent = destination_parent(path);
    let prefix = temporary_file_prefix(path);
    let mut temporary = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(parent)?;

    temporary.write_all(&document)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;

    // `NamedTempFile::persist` renames within the destination directory and
    // replaces an existing file atomically. If any preceding operation or the
    // rename itself fails, dropping the temporary file removes it.
    temporary
        .persist(path)
        .map_err(|error| anyhow!("failed to replace {}: {}", path.display(), error.error))?;

    info!(
        path = %path.display(),
        tracks = tracks.len(),
        "XSPF playlist exported"
    );
    Ok(())
}

/// Serialize a valid XSPF v1 document before the atomic filesystem update.
fn serialize_xspf(tracks: &[track::Model]) -> anyhow::Result<Vec<u8>> {
    let mut document = Vec::new();

    writeln!(document, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>")?;
    writeln!(
        document,
        "<playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">"
    )?;
    writeln!(document, "  <trackList>")?;

    for t in tracks {
        writeln!(document, "    <track>")?;

        // Location: file URI with XML escaping
        let location = xml_escape(&file_path_to_uri(&t.file_path))?;
        writeln!(document, "      <location>{location}</location>")?;

        if !t.title.is_empty() {
            writeln!(document, "      <title>{}</title>", xml_escape(&t.title)?)?;
        }
        if !t.artist_name.is_empty() {
            writeln!(
                document,
                "      <creator>{}</creator>",
                xml_escape(&t.artist_name)?
            )?;
        }
        if !t.album_title.is_empty() {
            writeln!(
                document,
                "      <album>{}</album>",
                xml_escape(&t.album_title)?
            )?;
        }
        if let Some(dur) = t.duration_secs {
            // XSPF duration is in milliseconds.
            match u64::try_from(dur) {
                Ok(duration_secs) => match duration_secs.checked_mul(1000) {
                    Some(duration_ms) => {
                        writeln!(document, "      <duration>{duration_ms}</duration>")?;
                    }
                    None => warn!(
                        track_id = %t.id,
                        duration_secs,
                        "Omitting XSPF duration that overflows u64 milliseconds"
                    ),
                },
                Err(_) => warn!(
                    track_id = %t.id,
                    duration_secs = dur,
                    "Omitting negative duration from XSPF export"
                ),
            }
        }

        writeln!(document, "    </track>")?;
    }

    writeln!(document, "  </trackList>")?;
    writeln!(document, "</playlist>")?;
    Ok(document)
}

// ── XSPF Import ─────────────────────────────────────────────────────

/// Import tracks from an XSPF file.
///
/// Uses a namespace-aware XML parser, requires an XSPF v1 root in the
/// canonical namespace, rejects DTDs, and considers only direct XSPF
/// `trackList` / `track` children. Comments, CDATA, extensions, and unrelated
/// nesting therefore cannot manufacture phantom playlist entries.
///
/// Returns a list of `ImportedTrack` with whatever metadata the file provides.
pub fn import_xspf(path: &Path) -> anyhow::Result<Vec<ImportedTrack>> {
    let content = std::fs::read_to_string(path)?;
    let tracks = parse_xspf(&content)?;

    info!(
        path = %path.display(),
        tracks = tracks.len(),
        "XSPF playlist imported"
    );
    Ok(tracks)
}

const XSPF_NAMESPACE: &[u8] = b"http://xspf.org/ns/0/";

#[derive(Debug)]
enum XspfNode {
    Playlist,
    TrackList,
    Track,
    Field(XspfField, String),
    Other,
}

#[derive(Debug, Clone, Copy)]
enum XspfField {
    Location,
    Title,
    Creator,
    Album,
    Duration,
}

fn empty_imported_track() -> ImportedTrack {
    ImportedTrack {
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        file_path: String::new(),
        duration_secs: None,
    }
}

fn parse_xspf(content: &str) -> anyhow::Result<Vec<ImportedTrack>> {
    validate_xml_10_text(content)?;

    let mut reader = NsReader::from_str(content.trim_start_matches('\u{feff}'));
    reader.config_mut().check_comments = true;

    let mut stack = Vec::<XspfNode>::new();
    let mut tracks = Vec::new();
    let mut current_track = None;
    let mut saw_root = false;
    let mut root_closed = false;
    let mut saw_track_list = false;
    let mut saw_declaration = false;
    let mut declaration_must_be_next = true;

    loop {
        let (namespace, event) = reader
            .read_resolved_event()
            .map_err(|error| anyhow!("malformed XSPF XML: {error}"))?;

        match event {
            Event::Start(element) => {
                if root_closed {
                    bail!("malformed XSPF: unexpected element after </playlist>");
                }

                let is_xspf = is_xspf_namespace(&namespace)?;
                validate_element_attributes(&reader, &element)?;
                let local_name = element.local_name();
                let local_name = local_name.as_ref();
                let node = if stack.is_empty() {
                    validate_playlist_root(&element, is_xspf)?;
                    if saw_root {
                        bail!("malformed XSPF: multiple document roots");
                    }
                    saw_root = true;
                    declaration_must_be_next = false;
                    XspfNode::Playlist
                } else {
                    classify_xspf_node(
                        stack.last(),
                        is_xspf,
                        local_name,
                        &mut saw_track_list,
                        &mut current_track,
                    )?
                };
                stack.push(node);
            }
            Event::Empty(element) => {
                if root_closed {
                    bail!("malformed XSPF: unexpected element after </playlist>");
                }

                let is_xspf = is_xspf_namespace(&namespace)?;
                validate_element_attributes(&reader, &element)?;
                let local_name = element.local_name();
                let local_name = local_name.as_ref();
                if stack.is_empty() {
                    validate_playlist_root(&element, is_xspf)?;
                    if saw_root {
                        bail!("malformed XSPF: multiple document roots");
                    }
                    saw_root = true;
                    root_closed = true;
                    declaration_must_be_next = false;
                    continue;
                }

                match classify_xspf_node(
                    stack.last(),
                    is_xspf,
                    local_name,
                    &mut saw_track_list,
                    &mut current_track,
                )? {
                    XspfNode::Track => {
                        tracks.push(current_track.take().ok_or_else(|| {
                            anyhow!("malformed XSPF: missing track working state")
                        })?);
                    }
                    XspfNode::Field(field, value) => {
                        apply_xspf_field(
                            current_track
                                .as_mut()
                                .ok_or_else(|| anyhow!("malformed XSPF: field outside a track"))?,
                            field,
                            &value,
                        )?;
                    }
                    XspfNode::Playlist => root_closed = true,
                    XspfNode::TrackList | XspfNode::Other => {}
                }
            }
            Event::End(_) => {
                let node = stack
                    .pop()
                    .ok_or_else(|| anyhow!("malformed XSPF: unmatched closing element"))?;
                match node {
                    XspfNode::Field(field, value) => {
                        apply_xspf_field(
                            current_track
                                .as_mut()
                                .ok_or_else(|| anyhow!("malformed XSPF: field outside a track"))?,
                            field,
                            &value,
                        )?;
                    }
                    XspfNode::Track => {
                        tracks.push(current_track.take().ok_or_else(|| {
                            anyhow!("malformed XSPF: missing track working state")
                        })?);
                    }
                    XspfNode::Playlist => root_closed = true,
                    XspfNode::TrackList | XspfNode::Other => {}
                }
            }
            Event::Text(text) => {
                let value = text
                    .xml10_content()
                    .map_err(|error| anyhow!("malformed XSPF text: {error}"))?;
                if !saw_root {
                    declaration_must_be_next = false;
                }
                append_xspf_text(&mut stack, &value);
                reject_text_outside_root(&stack, saw_root, root_closed, &value)?;
            }
            Event::CData(text) => {
                reject_markup_outside_root(&stack, saw_root, root_closed, "CDATA")?;
                let value = text
                    .xml10_content()
                    .map_err(|error| anyhow!("malformed XSPF CDATA: {error}"))?;
                validate_xml_10_text(&value)?;
                append_xspf_text(&mut stack, &value);
                reject_text_outside_root(&stack, saw_root, root_closed, &value)?;
            }
            Event::GeneralRef(reference) => {
                reject_markup_outside_root(&stack, saw_root, root_closed, "entity reference")?;
                let value = resolve_xml_reference(&reference)?;
                validate_xml_10_text(&value)?;
                append_xspf_text(&mut stack, &value);
                reject_text_outside_root(&stack, saw_root, root_closed, &value)?;
            }
            Event::DocType(_) => bail!("unsupported XSPF: DTD declarations are not allowed"),
            Event::Decl(declaration) => {
                if saw_root || saw_declaration || !declaration_must_be_next {
                    bail!("malformed XSPF: XML declaration must be the first document event");
                }
                validate_xml_declaration(&declaration)?;
                saw_declaration = true;
                declaration_must_be_next = false;
            }
            Event::PI(_) | Event::Comment(_) => {
                if !saw_root {
                    declaration_must_be_next = false;
                }
            }
            Event::Eof => break,
        }
    }

    if !saw_root {
        bail!("unsupported playlist document: expected an XSPF v1 <playlist> root");
    }
    if !root_closed || !stack.is_empty() {
        bail!("malformed XSPF: unclosed <playlist> document");
    }
    if !saw_track_list {
        bail!("malformed XSPF: missing direct <trackList> child");
    }

    Ok(tracks)
}

fn is_xspf_namespace(namespace: &ResolveResult<'_>) -> anyhow::Result<bool> {
    match namespace {
        ResolveResult::Bound(value) => {
            let raw = std::str::from_utf8(value.as_ref())
                .map_err(|error| anyhow!("malformed XSPF namespace encoding: {error}"))?;
            let normalized = quick_xml::escape::unescape(raw)
                .map_err(|error| anyhow!("malformed XSPF namespace: {error}"))?;
            validate_xml_10_text(&normalized)?;
            Ok(normalized.as_bytes() == XSPF_NAMESPACE)
        }
        ResolveResult::Unbound => Ok(false),
        ResolveResult::Unknown(_) => {
            bail!("malformed XSPF: element uses an undeclared namespace prefix")
        }
    }
}

fn validate_element_attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
) -> anyhow::Result<()> {
    for attribute in element.attributes().with_checks(true) {
        let attribute = attribute.map_err(|error| anyhow!("malformed XSPF attribute: {error}"))?;
        if matches!(
            reader.resolver().resolve_attribute(attribute.key).0,
            ResolveResult::Unknown(_)
        ) {
            bail!("malformed XSPF: attribute uses an undeclared namespace prefix");
        }
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|error| anyhow!("malformed XSPF attribute value: {error}"))?;
        validate_xml_10_text(&value)?;
    }
    Ok(())
}

fn validate_xml_declaration(declaration: &quick_xml::events::BytesDecl<'_>) -> anyhow::Result<()> {
    let raw = std::str::from_utf8(declaration)
        .map_err(|error| anyhow!("malformed XSPF XML declaration encoding: {error}"))?;
    let declaration = BytesStart::from_content(raw, 3);
    let mut stage = 0u8;

    for attribute in declaration.attributes().with_checks(true) {
        let attribute =
            attribute.map_err(|error| anyhow!("malformed XSPF XML declaration: {error}"))?;
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Explicit1_0)
            .map_err(|error| anyhow!("malformed XSPF XML declaration value: {error}"))?;
        validate_xml_10_text(&value)?;

        match attribute.key.as_ref() {
            b"version" if stage == 0 && value == "1.0" => stage = 1,
            b"version" if stage == 0 => {
                bail!("unsupported XSPF XML declaration: version must be 1.0")
            }
            b"encoding" if stage == 1 && value.eq_ignore_ascii_case("utf-8") => stage = 2,
            b"encoding" if stage == 1 => {
                bail!("unsupported XSPF XML declaration: encoding must be UTF-8")
            }
            b"standalone"
                if (stage == 1 || stage == 2) && matches!(value.as_ref(), "yes" | "no") =>
            {
                stage = 3;
            }
            _ => bail!("malformed XSPF XML declaration attributes"),
        }
    }

    if stage == 0 {
        bail!("malformed XSPF XML declaration: missing version");
    }
    Ok(())
}

fn validate_playlist_root(element: &BytesStart<'_>, is_xspf: bool) -> anyhow::Result<()> {
    if element.local_name().as_ref() != b"playlist" {
        bail!("unsupported playlist document: expected an XSPF v1 <playlist> root");
    }
    if !is_xspf {
        bail!("unsupported playlist document: missing the canonical XSPF v1 namespace");
    }

    let mut version = None;
    for attribute in element.attributes().with_checks(true) {
        let attribute = attribute.map_err(|error| anyhow!("malformed XSPF attribute: {error}"))?;
        if attribute.key.as_ref() == b"version" {
            if version.is_some() {
                bail!("malformed XSPF: duplicate playlist version attribute");
            }
            version = Some(
                attribute
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|error| anyhow!("malformed XSPF version: {error}"))?
                    .into_owned(),
            );
        }
    }

    if version.as_deref() != Some("1") {
        bail!("unsupported playlist document: XSPF version must be 1");
    }
    Ok(())
}

fn classify_xspf_node(
    parent: Option<&XspfNode>,
    is_xspf: bool,
    local_name: &[u8],
    saw_track_list: &mut bool,
    current_track: &mut Option<ImportedTrack>,
) -> anyhow::Result<XspfNode> {
    if !is_xspf {
        return Ok(XspfNode::Other);
    }

    match parent {
        Some(XspfNode::Playlist) if local_name == b"trackList" => {
            if *saw_track_list {
                bail!("malformed XSPF: multiple direct <trackList> children");
            }
            *saw_track_list = true;
            Ok(XspfNode::TrackList)
        }
        Some(XspfNode::TrackList) if local_name == b"track" => {
            if current_track.is_some() {
                bail!("malformed XSPF: nested playlist track state");
            }
            *current_track = Some(empty_imported_track());
            Ok(XspfNode::Track)
        }
        Some(XspfNode::Track) => Ok(xspf_field(local_name).map_or(XspfNode::Other, |field| {
            XspfNode::Field(field, String::new())
        })),
        _ => Ok(XspfNode::Other),
    }
}

fn xspf_field(local_name: &[u8]) -> Option<XspfField> {
    match local_name {
        b"location" => Some(XspfField::Location),
        b"title" => Some(XspfField::Title),
        b"creator" => Some(XspfField::Creator),
        b"album" => Some(XspfField::Album),
        b"duration" => Some(XspfField::Duration),
        _ => None,
    }
}

fn append_xspf_text(stack: &mut [XspfNode], value: &str) {
    if let Some(XspfNode::Field(_, collected)) = stack.last_mut() {
        collected.push_str(value);
    }
}

fn reject_text_outside_root(
    stack: &[XspfNode],
    saw_root: bool,
    root_closed: bool,
    value: &str,
) -> anyhow::Result<()> {
    if stack.is_empty() && (!saw_root || root_closed) && !value.trim().is_empty() {
        bail!("malformed XSPF: unexpected text outside <playlist>");
    }
    Ok(())
}

fn reject_markup_outside_root(
    stack: &[XspfNode],
    saw_root: bool,
    root_closed: bool,
    kind: &str,
) -> anyhow::Result<()> {
    if stack.is_empty() && (!saw_root || root_closed) {
        bail!("malformed XSPF: {kind} outside <playlist>");
    }
    Ok(())
}

fn resolve_xml_reference(reference: &quick_xml::events::BytesRef<'_>) -> anyhow::Result<String> {
    if let Some(value) = reference
        .resolve_char_ref()
        .map_err(|error| anyhow!("malformed XSPF character reference: {error}"))?
    {
        return Ok(value.to_string());
    }

    let name = reference
        .decode()
        .map_err(|error| anyhow!("malformed XSPF entity reference: {error}"))?;
    quick_xml::escape::resolve_xml_entity(&name)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("unsupported XSPF entity reference: &{name};"))
}

fn apply_xspf_field(
    track: &mut ImportedTrack,
    field: XspfField,
    value: &str,
) -> anyhow::Result<()> {
    match field {
        XspfField::Location if track.file_path.is_empty() => {
            let file_path = uri_to_file_path(value.trim());
            if !file_path.is_empty() {
                track.file_path = file_path;
            }
        }
        XspfField::Location => {}
        XspfField::Title => track.title = value.to_string(),
        XspfField::Creator => track.artist = value.to_string(),
        XspfField::Album => track.album = value.to_string(),
        XspfField::Duration => {
            let milliseconds = value
                .trim()
                .parse::<u64>()
                .map_err(|_| anyhow!("invalid XSPF duration: expected unsigned milliseconds"))?;
            track.duration_secs = Some(milliseconds / 1000);
        }
    }
    Ok(())
}

// ── Track matching ──────────────────────────────────────────────────

/// Inclusive duration tolerance for metadata-based import matching.
pub(super) const IMPORT_DURATION_TOLERANCE_SECS: u64 = 5;

/// Reusable resolver for one stable library-track snapshot.
///
/// Paths and normalized metadata are indexed once so importing or reconciling
/// many playlist entries does not repeatedly scan and normalize the complete
/// library. The index borrows the snapshot, ensuring every lookup observes the
/// same track models.
pub(super) struct ImportedTrackMatchIndex<'a> {
    by_path: HashMap<&'a str, &'a track::Model>,
    by_metadata: HashMap<(String, String), Vec<IndexedTrack<'a>>>,
}

struct IndexedTrack<'a> {
    model: &'a track::Model,
    album: String,
    duration_secs: Option<u64>,
}

impl<'a> ImportedTrackMatchIndex<'a> {
    pub(super) fn new(tracks: &'a [track::Model]) -> Self {
        let mut by_path = HashMap::with_capacity(tracks.len());
        let mut by_metadata: HashMap<(String, String), Vec<IndexedTrack<'a>>> = HashMap::new();

        for model in tracks {
            // `tracks.file_path` is unique in the schema. Retaining the first
            // value also keeps lookup deterministic if a malformed snapshot
            // somehow violates that invariant.
            by_path.entry(model.file_path.as_str()).or_insert(model);

            let title = normalized_metadata(&model.title);
            let artist = normalized_metadata(&model.artist_name);
            if title.is_empty() || artist.is_empty() {
                continue;
            }
            by_metadata
                .entry((title, artist))
                .or_default()
                .push(IndexedTrack {
                    model,
                    album: normalized_metadata(&model.album_title),
                    duration_secs: model
                        .duration_secs
                        .and_then(|duration| i32::try_from(duration).ok())
                        .and_then(|duration| u64::try_from(duration).ok()),
                });
        }

        Self {
            by_path,
            by_metadata,
        }
    }

    /// Match one imported entry using exact path first, followed by the
    /// deterministic normalized-metadata and duration rules.
    pub(super) fn find(&self, imported: &ImportedTrack) -> Option<&'a track::Model> {
        if !imported.file_path.is_empty() {
            if let Some(exact_path) = self.by_path.get(imported.file_path.as_str()) {
                return Some(*exact_path);
            }
        }

        let title = normalized_metadata(&imported.title);
        let artist = normalized_metadata(&imported.artist);
        if title.is_empty() || artist.is_empty() {
            return None;
        }
        let album = normalized_metadata(&imported.album);
        let candidates = self.by_metadata.get(&(title, artist))?;
        let metadata_matches = candidates
            .iter()
            .filter(|candidate| album.is_empty() || candidate.album == album);

        let Some(imported_duration) = imported.duration_secs else {
            return exactly_one(metadata_matches).map(|candidate| candidate.model);
        };

        let mut nearest = None;
        let mut nearest_distance = u64::MAX;
        let mut nearest_is_unique = false;
        for candidate in metadata_matches {
            let Some(candidate_duration) = candidate.duration_secs else {
                continue;
            };
            let distance = candidate_duration.abs_diff(imported_duration);
            if distance > IMPORT_DURATION_TOLERANCE_SECS {
                continue;
            }

            match distance.cmp(&nearest_distance) {
                std::cmp::Ordering::Less => {
                    nearest = Some(candidate.model);
                    nearest_distance = distance;
                    nearest_is_unique = true;
                }
                std::cmp::Ordering::Equal => nearest_is_unique = false,
                std::cmp::Ordering::Greater => {}
            }
        }

        nearest.filter(|_| nearest_is_unique)
    }
}

/// Deterministically match one imported entry against an in-memory library.
///
/// An exact stored path is authoritative. Otherwise title and artist must be
/// non-empty and match exactly after trimming and case normalization; a
/// non-empty imported album is an additional exact constraint. When duration
/// is present it is a hard ±5-second gate and only a unique nearest candidate
/// wins. Without duration, duplicate metadata matches remain ambiguous.
pub(super) fn match_imported_track<'a>(
    imported: &ImportedTrack,
    tracks: &'a [track::Model],
) -> Option<&'a track::Model> {
    ImportedTrackMatchIndex::new(tracks).find(imported)
}

// ── Helpers ─────────────────────────────────────────────────────────

fn destination_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn temporary_file_prefix(path: &Path) -> String {
    let destination_name = path
        .file_name()
        .map_or_else(|| "playlist".into(), |name| name.to_string_lossy());
    format!(".{destination_name}.")
}

fn normalized_metadata(value: &str) -> String {
    value.trim().to_lowercase()
}

fn exactly_one<T>(mut values: impl Iterator<Item = T>) -> Option<T> {
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

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
/// dropped it). Non-`file`, malformed, and non-local inputs deliberately yield
/// no path so a web URL can never be retained as a local-library identity.
fn uri_to_file_path(uri: &str) -> String {
    let Ok(url) = Url::parse(uri) else {
        return String::new();
    };
    if url.scheme() != "file" {
        return String::new();
    }
    if url.query().is_some() || url.fragment().is_some() {
        return String::new();
    }
    url.to_file_path()
        .ok()
        .map_or_else(String::new, |path| path.to_string_lossy().into_owned())
}

fn validate_xml_10_text(value: &str) -> anyhow::Result<()> {
    if let Some(character) = value.chars().find(|character| {
        !matches!(
            character,
            '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}'
        )
    }) {
        bail!(
            "value contains an XML 1.0-forbidden character U+{:04X}",
            u32::from(character)
        );
    }
    Ok(())
}

/// Basic XML escaping for text content.
fn xml_escape(s: &str) -> anyhow::Result<String> {
    validate_xml_10_text(s)?;
    Ok(s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{
        export_xspf, import_xspf, match_imported_track, serialize_xspf, temporary_file_prefix,
        ImportedTrack,
    };
    use crate::db::entities::track;

    fn library_track(
        id: &str,
        path: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_secs: Option<i64>,
    ) -> track::Model {
        track::Model {
            id: id.to_string(),
            file_path: path.to_string(),
            title: title.to_string(),
            artist_name: artist.to_string(),
            album_artist_name: None,
            album_title: album.to_string(),
            genre: None,
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            date_added: "2026-01-01T00:00:00Z".to_string(),
            date_modified: "2026-01-01T00:00:00Z".to_string(),
            file_size_bytes: None,
        }
    }

    fn imported_track(
        path: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_secs: Option<u64>,
    ) -> ImportedTrack {
        ImportedTrack {
            title: title.to_string(),
            artist: artist.to_string(),
            album: album.to_string(),
            file_path: path.to_string(),
            duration_secs,
        }
    }

    fn temporary_artifacts(parent: &Path, destination: &Path) -> Vec<PathBuf> {
        let prefix = temporary_file_prefix(destination);
        fs::read_dir(parent)
            .expect("read temporary directory")
            .map(|entry| entry.expect("read directory entry").path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect()
    }

    fn import_document(document: &str) -> anyhow::Result<Vec<ImportedTrack>> {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let path = directory.path().join("playlist.xspf");
        fs::write(&path, document).expect("write XSPF fixture");
        import_xspf(&path)
    }

    #[test]
    fn default_namespace_decodes_named_numeric_and_cdata_text() {
        let tracks = import_document(concat!(
            "<playlist version='1' xmlns='http://xspf.org/ns/0/'>",
            "<trackList><track>",
            "<title>Tom &amp; &#74;erry</title>",
            "<creator><![CDATA[A &amp; <Artist>]]></creator>",
            "<album><![CDATA[]]></album>",
            "</track></trackList></playlist>"
        ))
        .expect("parse namespace-aware XSPF");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Tom & Jerry");
        assert_eq!(tracks[0].artist, "A &amp; <Artist>");
        assert_eq!(tracks[0].album, "");
    }

    #[test]
    fn prefixed_xspf_namespace_is_accepted() {
        let tracks = import_document(concat!(
            "<x:playlist version='1' xmlns:x='http://xspf.org/ns/&#48;/'>",
            "<x:trackList><x:track><x:title>Prefixed</x:title></x:track>",
            "</x:trackList></x:playlist>"
        ))
        .expect("parse prefixed XSPF");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Prefixed");
    }

    #[test]
    fn every_child_attribute_is_validated_and_namespace_resolved() {
        for track in [
            "<track bad='&undefined;'/>",
            "<track bad='1' bad='2'/>",
            "<track bad:x='1'/>",
            "<track bad=unquoted/>",
        ] {
            let document = format!(
                "<playlist version='1' xmlns='http://xspf.org/ns/0/'>\
                 <trackList>{track}</trackList></playlist>"
            );
            import_document(&document).expect_err("malformed child attribute must fail parsing");
        }
    }

    #[test]
    fn xml_declaration_must_be_first_unique_and_valid_xml_1_0() {
        let root = "<playlist version='1' xmlns='http://xspf.org/ns/0/'><trackList/></playlist>";
        let valid = format!("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>{root}");
        assert!(import_document(&valid).is_ok());

        for document in [
            format!(" <?xml version='1.0'?>{root}"),
            format!("<!--before--><?xml version='1.0'?>{root}"),
            format!("<?xml version='1.0'?><?xml version='1.0'?>{root}"),
            format!("<?xml?>{root}"),
            format!("<?xml version='1.1'?>{root}"),
            format!("<?xml version='1.0' version='1.0'?>{root}"),
        ] {
            import_document(&document).expect_err("invalid XML declaration must fail parsing");
        }
    }

    #[test]
    fn undeclared_prefix_and_top_level_cdata_are_rejected() {
        let unknown_prefix = import_document(concat!(
            "<playlist version='1' xmlns='http://xspf.org/ns/0/'>",
            "<trackList><bad:track/></trackList></playlist>"
        ))
        .expect_err("an undeclared namespace prefix must fail");
        assert!(unknown_prefix
            .to_string()
            .contains("undeclared namespace prefix"));

        let top_level_cdata = import_document(concat!(
            "<![CDATA[]]>",
            "<playlist version='1' xmlns='http://xspf.org/ns/0/'><trackList/></playlist>"
        ))
        .expect_err("CDATA outside the root must fail");
        assert!(top_level_cdata
            .to_string()
            .contains("CDATA outside <playlist>"));
    }

    #[test]
    fn comments_cdata_extensions_and_nesting_cannot_create_phantom_tracks() {
        let tracks = import_document(concat!(
            "<x:playlist version='1' xmlns:x='http://xspf.org/ns/0/' ",
            "xmlns:ext='https://example.invalid/extension'>",
            "<x:trackList>",
            "<!-- <x:track><x:title>Comment</x:title></x:track> -->",
            "<ext:container><![CDATA[<x:track><x:title>CDATA</x:title></x:track>]]>",
            "<x:track><x:title>Nested</x:title></x:track></ext:container>",
            "<x:track/>",
            "<x:track><x:title>Real</x:title></x:track>",
            "</x:trackList></x:playlist>"
        ))
        .expect("parse XSPF containing inert track-like content");

        assert_eq!(tracks.len(), 2);
        assert!(tracks[0].title.is_empty());
        assert_eq!(tracks[1].title, "Real");
    }

    #[test]
    fn dtd_is_rejected_before_entity_expansion() {
        let error = import_document(concat!(
            "<!DOCTYPE playlist [<!ENTITY title 'Injected'>]>",
            "<playlist version='1' xmlns='http://xspf.org/ns/0/'>",
            "<trackList><track><title>&title;</title></track></trackList>",
            "</playlist>"
        ))
        .expect_err("DTD must be rejected");

        assert!(error
            .to_string()
            .contains("DTD declarations are not allowed"));
    }

    #[test]
    fn only_valid_local_file_uris_become_paths() {
        let tracks = import_document(concat!(
            "<playlist version='1' xmlns='http://xspf.org/ns/0/'><trackList>",
            "<track><location>https://example.test/watch/1</location>",
            "<title>Web</title><creator>Artist</creator></track>",
            "<track><location>not a URI</location>",
            "<title>Malformed</title><creator>Artist</creator></track>",
            "<track><location>https://example.test/not-local</location>",
            "<location>file:///music/Local%20Song.flac</location>",
            "<location>file:///music/Other.flac</location></track>",
            "<track><location>file:///music/Query.flac?token=private</location></track>",
            "</trackList></playlist>"
        ))
        .expect("parse location variants");

        assert_eq!(tracks.len(), 4);
        assert!(tracks[0].file_path.is_empty());
        assert!(tracks[1].file_path.is_empty());
        assert!(!tracks[2].file_path.is_empty());
        assert!(!tracks[2].file_path.starts_with("file:"));
        assert!(!tracks[2].file_path.contains("Other.flac"));
        assert!(tracks[3].file_path.is_empty());
    }

    #[test]
    fn invalid_or_out_of_range_xspf_duration_rejects_the_document() {
        for duration in ["", "   ", "not-a-number", "18446744073709551616"] {
            let document = format!(
                "<playlist version='1' xmlns='http://xspf.org/ns/0/'><trackList>\
                 <track><duration>{duration}</duration></track>\
                 </trackList></playlist>"
            );
            let error = import_document(&document).expect_err("invalid duration must fail parsing");
            assert!(error.to_string().contains("invalid XSPF duration"));
        }
    }

    #[test]
    fn exact_file_path_wins_before_conflicting_metadata() {
        let tracks = vec![
            library_track(
                "path",
                "/music/right.flac",
                "Different",
                "Different",
                "Different",
                Some(10),
            ),
            library_track(
                "metadata",
                "/music/other.flac",
                "Wanted",
                "Artist",
                "Album",
                Some(100),
            ),
        ];
        let imported = imported_track("/music/right.flac", "Wanted", "Artist", "Album", Some(100));

        assert_eq!(match_imported_track(&imported, &tracks).unwrap().id, "path");
    }

    #[test]
    fn metadata_matching_trims_case_and_honors_optional_album() {
        let tracks = vec![library_track(
            "match",
            "/music/song.flac",
            "  A Song ",
            "THE ARTIST",
            "The Album",
            Some(100),
        )];

        let without_album = imported_track("", "a song", " the artist ", "", Some(100));
        assert_eq!(
            match_imported_track(&without_album, &tracks).unwrap().id,
            "match"
        );

        let wrong_album = imported_track("", "a song", "the artist", "Another", Some(100));
        assert!(match_imported_track(&wrong_album, &tracks).is_none());
    }

    #[test]
    fn duration_tolerance_is_an_inclusive_hard_gate() {
        let at_lower_boundary = vec![library_track(
            "lower",
            "/lower",
            "Song",
            "Artist",
            "",
            Some(95),
        )];
        let at_upper_boundary = vec![library_track(
            "upper",
            "/upper",
            "Song",
            "Artist",
            "",
            Some(105),
        )];
        let outside = vec![library_track(
            "outside",
            "/outside",
            "Song",
            "Artist",
            "",
            Some(94),
        )];
        let imported = imported_track("", "Song", "Artist", "", Some(100));

        assert_eq!(
            match_imported_track(&imported, &at_lower_boundary)
                .unwrap()
                .id,
            "lower"
        );
        assert_eq!(
            match_imported_track(&imported, &at_upper_boundary)
                .unwrap()
                .id,
            "upper"
        );
        assert!(match_imported_track(&imported, &outside).is_none());
    }

    #[test]
    fn duration_selects_only_a_unique_nearest_candidate() {
        let imported = imported_track("", "Song", "Artist", "", Some(100));
        let unique = vec![
            library_track("near", "/near", "Song", "Artist", "", Some(98)),
            library_track("far", "/far", "Song", "Artist", "", Some(104)),
        ];
        assert_eq!(match_imported_track(&imported, &unique).unwrap().id, "near");

        let tied = vec![
            library_track("before", "/before", "Song", "Artist", "", Some(97)),
            library_track("after", "/after", "Song", "Artist", "", Some(103)),
        ];
        assert!(match_imported_track(&imported, &tied).is_none());
    }

    #[test]
    fn duplicates_without_imported_duration_are_ambiguous() {
        let tracks = vec![
            library_track("one", "/one", "Song", "Artist", "", Some(100)),
            library_track("two", "/two", "Song", "Artist", "", Some(101)),
        ];
        let imported = imported_track("", "Song", "Artist", "", None);

        assert!(match_imported_track(&imported, &tracks).is_none());
    }

    #[test]
    fn missing_candidate_duration_cannot_bypass_duration_gate() {
        let tracks = vec![library_track(
            "missing", "/missing", "Song", "Artist", "", None,
        )];
        let imported = imported_track("", "Song", "Artist", "", Some(100));

        assert!(match_imported_track(&imported, &tracks).is_none());
    }

    #[test]
    fn malformed_trailing_track_is_an_import_error() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let path = directory.path().join("malformed.xspf");
        fs::write(
            &path,
            concat!(
                "<playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\"><trackList>",
                "<track><title>Complete</title></track>",
                "<track><title>Truncated</title>",
                "</trackList></playlist>"
            ),
        )
        .expect("write malformed fixture");

        let error = import_xspf(&path).expect_err("trailing track must fail parsing");
        assert!(error.to_string().contains("malformed XSPF XML"));
    }

    #[test]
    fn non_xspf_xml_is_rejected_before_track_scanning() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let path = directory.path().join("renamed-apple-export.xspf");
        fs::write(
            &path,
            concat!(
                "<?xml version=\"1.0\"?>",
                "<plist version=\"1.0\"><dict>",
                "<key>Playlists</key><array></array>",
                "</dict></plist>"
            ),
        )
        .expect("write non-XSPF fixture");

        let error = import_xspf(&path).expect_err("Apple XML must not pass as XSPF");
        assert!(error.to_string().contains("expected an XSPF v1"));
    }

    #[test]
    fn wrong_xspf_version_or_namespace_is_rejected() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let wrong_version = directory.path().join("wrong-version.xspf");
        fs::write(
            &wrong_version,
            "<playlist xmlns='http://xspf.org/ns/0/' version='2'><trackList/></playlist>",
        )
        .expect("write wrong-version fixture");
        assert!(import_xspf(&wrong_version)
            .expect_err("wrong version must fail")
            .to_string()
            .contains("version must be 1"));

        let wrong_namespace = directory.path().join("wrong-namespace.xspf");
        fs::write(
            &wrong_namespace,
            "<playlist version = '1' xmlns = 'https://example.invalid'><trackList/></playlist>",
        )
        .expect("write wrong-namespace fixture");
        assert!(import_xspf(&wrong_namespace)
            .expect_err("wrong namespace must fail")
            .to_string()
            .contains("missing the canonical XSPF v1 namespace"));
    }

    #[test]
    fn track_markup_outside_the_playlist_root_is_rejected() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let path = directory.path().join("trailing-track.xspf");
        fs::write(
            &path,
            concat!(
                "<playlist version='1' xmlns='http://xspf.org/ns/0/'>",
                "<trackList/></playlist>",
                "<track><title>Outside</title></track>"
            ),
        )
        .expect("write trailing-content fixture");

        let error = import_xspf(&path).expect_err("content outside root must fail");
        assert!(error
            .to_string()
            .contains("unexpected element after </playlist>"));
    }

    #[test]
    fn export_atomically_overwrites_and_round_trips_xml() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let destination = directory.path().join("playlist.xspf");
        fs::write(&destination, "previous contents").expect("write previous export");
        let source_path = directory.path().join("Tom & Jerry.flac");
        let source_path = source_path.to_string_lossy();
        let tracks = vec![library_track(
            "track",
            &source_path,
            "Tom & <Jerry>",
            "A 'Creator'",
            "An \"Album\"",
            Some(123),
        )];

        export_xspf(&tracks, &destination).expect("replace XSPF export");
        let imported = import_xspf(&destination).expect("read replaced XSPF export");

        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].file_path, source_path);
        assert_eq!(imported[0].title, "Tom & <Jerry>");
        assert_eq!(imported[0].artist, "A 'Creator'");
        assert_eq!(imported[0].album, "An \"Album\"");
        assert_eq!(imported[0].duration_secs, Some(123));
        assert!(temporary_artifacts(directory.path(), &destination).is_empty());
    }

    #[test]
    fn invalid_duration_is_omitted_during_atomic_export() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let destination = directory.path().join("playlist.xspf");
        fs::write(&destination, "previous contents").expect("write previous export");
        let negative = vec![library_track(
            "negative",
            "/negative",
            "Song",
            "Artist",
            "Album",
            Some(-1),
        )];

        export_xspf(&negative, &destination)
            .expect("an invalid optional duration must not block export");
        let imported = import_xspf(&destination).expect("read replaced XSPF export");
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].file_path, "/negative");
        assert_eq!(imported[0].title, "Song");
        assert_eq!(imported[0].duration_secs, None);
        assert!(temporary_artifacts(directory.path(), &destination).is_empty());
    }

    #[test]
    fn forbidden_xml_control_preserves_existing_export_without_tempfiles() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let destination = directory.path().join("playlist.xspf");
        fs::write(&destination, "previous contents").expect("write previous export");
        let tracks = vec![library_track(
            "control",
            "/music/control.flac",
            "Invalid\u{1}Title",
            "Artist",
            "Album",
            Some(100),
        )];

        let error = export_xspf(&tracks, &destination).expect_err("control character must fail");
        assert!(error.to_string().contains("XML 1.0-forbidden"));
        assert_eq!(
            fs::read_to_string(&destination).expect("read preserved export"),
            "previous contents"
        );
        assert!(temporary_artifacts(directory.path(), &destination).is_empty());
    }

    #[test]
    fn invalid_stored_durations_are_omitted_without_losing_tracks() {
        let tracks = vec![
            library_track(
                "negative",
                "/negative",
                "Negative",
                "Artist",
                "Album",
                Some(-1),
            ),
            library_track(
                "overflow",
                "/overflow",
                "Overflow",
                "Artist",
                "Album",
                Some(i64::MAX),
            ),
            library_track("valid", "/valid", "Valid", "Artist", "Album", Some(100)),
        ];

        let document = String::from_utf8(
            serialize_xspf(&tracks).expect("invalid optional durations must not block export"),
        )
        .expect("serialized XSPF is UTF-8");
        assert_eq!(document.matches("    <track>").count(), 3);
        assert_eq!(document.matches("<duration>").count(), 1);
        assert!(document.contains("<duration>100000</duration>"));
        assert!(document.contains("<title>Negative</title>"));
        assert!(document.contains("<title>Overflow</title>"));
    }

    #[test]
    fn failed_atomic_replace_cleans_up_sibling_tempfile() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let destination = directory.path().join("occupied.xspf");
        fs::create_dir(&destination).expect("create directory at destination path");
        let tracks = vec![library_track(
            "track",
            "/track",
            "Song",
            "Artist",
            "Album",
            Some(100),
        )];

        assert!(export_xspf(&tracks, &destination).is_err());
        assert!(
            destination.is_dir(),
            "failed replacement changed destination"
        );
        assert!(temporary_artifacts(directory.path(), &destination).is_empty());
    }
}
