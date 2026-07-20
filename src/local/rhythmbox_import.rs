//! Bounded, data-only parsing for one-shot Rhythmbox migrations.
//!
//! This module deliberately knows nothing about GTK, Tributary's database, or
//! library matching. It turns the two Rhythmbox XML documents into a bounded
//! plan input. Only decoded, absolute local `file:` locations cross this
//! boundary; rejected locations are represented by typed, content-free issues.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use quick_xml::events::{BytesDecl, BytesRef, BytesStart, Event};
use quick_xml::reader::Reader;
use quick_xml::XmlVersion;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::db::entities::rhythmbox_import_receipt::RHYTHMBOX_SNAPSHOT_DIGEST_DOMAIN;

/// Explicit resource ceilings for untrusted migration documents.
#[allow(clippy::struct_field_names)] // The shared `max_` prefix makes overrides explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RhythmboxImportLimits {
    pub(crate) max_rhythmdb_bytes: usize,
    pub(crate) max_playlists_bytes: usize,
    pub(crate) max_xml_depth: usize,
    pub(crate) max_elements_per_document: usize,
    pub(crate) max_tracks: usize,
    pub(crate) max_playlists: usize,
    pub(crate) max_playlist_entries: usize,
    pub(crate) max_automatic_nodes: usize,
    pub(crate) max_attributes_per_element: usize,
    pub(crate) max_text_bytes: usize,
    pub(crate) max_retained_text_bytes: usize,
    pub(crate) max_issues: usize,
}

impl Default for RhythmboxImportLimits {
    fn default() -> Self {
        Self {
            // Capture bytes and the decoded model overlap while each document
            // is parsed. Keep the production ceilings useful for very large
            // libraries without allowing a profile to manufacture a
            // multi-gigabyte transient working set.
            max_rhythmdb_bytes: 128 * 1024 * 1024,
            max_playlists_bytes: 64 * 1024 * 1024,
            max_xml_depth: 16,
            max_elements_per_document: 4_000_000,
            max_tracks: 250_000,
            max_playlists: 10_000,
            max_playlist_entries: 500_000,
            max_automatic_nodes: 256,
            max_attributes_per_element: 32,
            max_text_bytes: 256 * 1024,
            max_retained_text_bytes: 128 * 1024 * 1024,
            max_issues: 100_000,
        }
    }
}

/// A fully parsed, still-unapplied Rhythmbox migration input.
#[derive(Clone, PartialEq, Eq)]
pub struct RhythmboxImport {
    pub(crate) tracks: Vec<RhythmboxTrack>,
    pub(crate) playlists: Vec<RhythmboxPlaylist>,
    pub(crate) issues: Vec<RhythmboxImportIssue>,
    pub(crate) semantic_digest: RhythmboxSemanticDigest,
}

impl fmt::Debug for RhythmboxImport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxImport")
            .field("tracks", &self.tracks.len())
            .field("playlists", &self.playlists.len())
            .field("issues", &self.issues.len())
            .field("semantic_digest", &"[REDACTED]")
            .finish()
    }
}

/// One local song row from `rhythmdb.xml`.
#[derive(Clone, PartialEq, Eq)]
pub struct RhythmboxTrack {
    /// One-based ordinal among direct `<entry>` children in the source file.
    pub(crate) source_ordinal: usize,
    pub(crate) location: RhythmboxFileLocation,
    /// `Some(0.0)` is an explicit Rhythmbox "unrated" value; `None` means the
    /// field was absent. Policy code decides whether either should be applied.
    pub(crate) rating: Option<RhythmboxRating>,
    pub(crate) play_count: Option<u32>,
    pub(crate) last_played_unix_seconds: Option<u64>,
}

impl fmt::Debug for RhythmboxTrack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxTrack")
            .field("source_ordinal", &self.source_ordinal)
            .field("location", &"[REDACTED]")
            .field("has_rating", &self.rating.is_some())
            .field("has_play_count", &self.play_count.is_some())
            .field("has_last_played", &self.last_played_unix_seconds.is_some())
            .finish()
    }
}

/// A validated Rhythmbox rating in the inclusive native 0..=5 range.
///
/// The canonicalized IEEE-754 representation avoids putting a non-`Eq` float
/// into the parser model while retaining fractional values for preview policy.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RhythmboxRating(u64);

impl RhythmboxRating {
    #[must_use]
    pub(crate) const fn value(self) -> f64 {
        f64::from_bits(self.0)
    }
}

impl fmt::Debug for RhythmboxRating {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RhythmboxRating([REDACTED])")
    }
}

/// A decoded, absolute, UTF-8 local path from a `file:` URI.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RhythmboxFileLocation(PathBuf);

impl RhythmboxFileLocation {
    #[must_use]
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for RhythmboxFileLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RhythmboxFileLocation([REDACTED])")
    }
}

/// One playlist from `playlists.xml`.
#[derive(Clone, PartialEq, Eq)]
pub struct RhythmboxPlaylist {
    /// One-based ordinal among direct `<playlist>` children.
    pub(crate) source_ordinal: usize,
    pub(crate) name: String,
    pub(crate) kind: RhythmboxPlaylistKind,
}

impl fmt::Debug for RhythmboxPlaylist {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxPlaylist")
            .field("source_ordinal", &self.source_ordinal)
            .field("name", &"[REDACTED]")
            .field("kind", &self.kind)
            .finish()
    }
}

/// The source playlist kind. Static and queue entries preserve source order,
/// duplicates, and invalid-location placeholders.
#[derive(Clone, PartialEq, Eq)]
pub enum RhythmboxPlaylistKind {
    Static(Vec<RhythmboxPlaylistEntry>),
    Queue(Vec<RhythmboxPlaylistEntry>),
    Automatic(RhythmboxAutomaticPlaylist),
    Unsupported { source_type: String },
}

impl fmt::Debug for RhythmboxPlaylistKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(entries) => formatter
                .debug_tuple("Static")
                .field(&format_args!("{} entries", entries.len()))
                .finish(),
            Self::Queue(entries) => formatter
                .debug_tuple("Queue")
                .field(&format_args!("{} entries", entries.len()))
                .finish(),
            Self::Automatic(automatic) => {
                formatter.debug_tuple("Automatic").field(automatic).finish()
            }
            Self::Unsupported { .. } => {
                formatter.write_str("Unsupported { source_type: [REDACTED] }")
            }
        }
    }
}

/// One ordered occurrence in a static playlist or queue.
#[derive(Clone, PartialEq, Eq)]
pub struct RhythmboxPlaylistEntry {
    /// One-based ordinal within the containing playlist.
    pub(crate) source_ordinal: usize,
    pub(crate) location: Option<RhythmboxFileLocation>,
}

impl fmt::Debug for RhythmboxPlaylistEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxPlaylistEntry")
            .field("source_ordinal", &self.source_ordinal)
            .field("has_valid_location", &self.location.is_some())
            .finish()
    }
}

/// A lossless, bounded representation of an automatic playlist's relevant XML.
///
/// Translation must use an explicit whitelist. Unknown elements and attributes
/// remain visible here so they can make a playlist unsupported rather than be
/// silently dropped or materialized.
#[derive(Clone, PartialEq, Eq)]
pub struct RhythmboxAutomaticPlaylist {
    pub(crate) attributes: Vec<RhythmboxXmlAttribute>,
    pub(crate) query: Vec<RhythmboxAutomaticNode>,
}

impl fmt::Debug for RhythmboxAutomaticPlaylist {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxAutomaticPlaylist")
            .field("attributes", &self.attributes.len())
            .field("query_roots", &self.query.len())
            .finish()
    }
}

/// One generic automatic-query element.
#[derive(Clone, PartialEq, Eq)]
pub struct RhythmboxAutomaticNode {
    pub(crate) element: String,
    pub(crate) attributes: Vec<RhythmboxXmlAttribute>,
    pub(crate) text: String,
    pub(crate) children: Vec<Self>,
}

impl fmt::Debug for RhythmboxAutomaticNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxAutomaticNode")
            .field("element", &"[REDACTED]")
            .field("attributes", &self.attributes.len())
            .field("text", &"[REDACTED]")
            .field("children", &self.children.len())
            .finish()
    }
}

/// One automatic-query or automatic-playlist attribute.
#[derive(Clone, PartialEq, Eq)]
pub struct RhythmboxXmlAttribute {
    pub(crate) name: String,
    pub(crate) value: String,
}

impl fmt::Debug for RhythmboxXmlAttribute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RhythmboxXmlAttribute([REDACTED])")
    }
}

/// SHA-256 of the parsed migration semantics, not the source formatting.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RhythmboxSemanticDigest([u8; 32]);

impl RhythmboxSemanticDigest {
    #[must_use]
    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for RhythmboxSemanticDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RhythmboxSemanticDigest([REDACTED])")
    }
}

/// A content-free issue which can be presented by source ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RhythmboxImportIssue {
    pub(crate) document: RhythmboxDocument,
    pub(crate) item_ordinal: usize,
    pub(crate) entry_ordinal: Option<usize>,
    pub(crate) kind: RhythmboxImportIssueKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RhythmboxImportIssueKind {
    MissingLocation,
    InvalidLocation(RhythmboxLocationIssue),
    InvalidNumeric(RhythmboxNumericField),
    UnsupportedEntryType,
    UnsupportedPlaylistType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RhythmboxLocationIssue {
    Malformed,
    NonFileScheme,
    RemoteAuthority,
    Credentials,
    Port,
    Query,
    Fragment,
    NotAbsolute,
    NotUtf8,
    ContainsNul,
    ParentTraversal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RhythmboxDocument {
    RhythmDb,
    Playlists,
}

impl fmt::Display for RhythmboxDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RhythmDb => formatter.write_str("rhythmdb.xml"),
            Self::Playlists => formatter.write_str("playlists.xml"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RhythmboxLimit {
    InputBytes,
    XmlDepth,
    Elements,
    Tracks,
    Playlists,
    PlaylistEntries,
    AutomaticNodes,
    Attributes,
    TextBytes,
    RetainedTextBytes,
    Issues,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RhythmboxNumericField {
    Rating,
    PlayCount,
    LastPlayed,
}

/// Parsing failures never embed source XML, paths, playlist names, or values.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RhythmboxParseError {
    #[error("{document} exceeds the configured {limit:?} limit")]
    LimitExceeded {
        document: RhythmboxDocument,
        limit: RhythmboxLimit,
    },
    #[error("{document} is not valid UTF-8 XML")]
    InvalidUtf8 { document: RhythmboxDocument },
    #[error("{document} is malformed XML")]
    MalformedXml { document: RhythmboxDocument },
    #[error("{document} has an invalid or unsupported XML declaration")]
    InvalidDeclaration { document: RhythmboxDocument },
    #[error("{document} contains a forbidden DTD declaration")]
    ForbiddenDtd { document: RhythmboxDocument },
    #[error("{document} contains a forbidden entity reference")]
    ForbiddenEntity { document: RhythmboxDocument },
    #[error("{document} contains a forbidden processing instruction")]
    ForbiddenProcessingInstruction { document: RhythmboxDocument },
    #[error("{document} has an unsupported root or version")]
    UnsupportedDocument { document: RhythmboxDocument },
    #[error("{document} has invalid element structure")]
    InvalidStructure { document: RhythmboxDocument },
    #[error("{document} repeats a scalar field at item {item_ordinal}")]
    DuplicateField {
        document: RhythmboxDocument,
        item_ordinal: usize,
    },
    #[error("{document} has an invalid {field:?} at item {item_ordinal}")]
    InvalidNumeric {
        document: RhythmboxDocument,
        item_ordinal: usize,
        field: RhythmboxNumericField,
    },
    #[error("{document} is missing a required playlist attribute at item {item_ordinal}")]
    MissingPlaylistAttribute {
        document: RhythmboxDocument,
        item_ordinal: usize,
    },
}

/// Parse Rhythmbox's database and optional playlist document without touching
/// GTK, the filesystem, or Tributary's database.
pub fn parse_rhythmbox_documents(
    rhythmdb_xml: &[u8],
    playlists_xml: Option<&[u8]>,
    limits: RhythmboxImportLimits,
) -> Result<RhythmboxImport, RhythmboxParseError> {
    let mut issues = Vec::new();
    let tracks = parse_rhythmdb(rhythmdb_xml, limits, &mut issues)?;
    let playlists = match playlists_xml {
        Some(xml) => parse_playlists(xml, limits, &mut issues)?,
        None => Vec::new(),
    };

    let mut import = RhythmboxImport {
        tracks,
        playlists,
        issues,
        semantic_digest: RhythmboxSemanticDigest([0; 32]),
    };
    if retained_text_bytes(&import).is_none_or(|bytes| bytes > limits.max_retained_text_bytes) {
        return Err(RhythmboxParseError::LimitExceeded {
            document: if playlists_xml.is_some() {
                RhythmboxDocument::Playlists
            } else {
                RhythmboxDocument::RhythmDb
            },
            limit: RhythmboxLimit::RetainedTextBytes,
        });
    }
    import.semantic_digest = semantic_digest(&import, playlists_xml.is_some());
    Ok(import)
}

#[derive(Clone, Copy)]
enum TrackField {
    Location,
    Rating,
    PlayCount,
    LastPlayed,
}

enum RhythmDbNode {
    Root,
    Entry,
    Field(TrackField, String),
    Ignored,
}

#[allow(clippy::struct_excessive_bools)] // Each flag guards one distinct scalar field.
#[derive(Default)]
struct TrackBuilder {
    source_ordinal: usize,
    location: Option<String>,
    rating: Option<String>,
    play_count: Option<String>,
    last_played: Option<String>,
    seen_location: bool,
    seen_rating: bool,
    seen_play_count: bool,
    seen_last_played: bool,
}

#[allow(clippy::too_many_lines)] // Keep the security-relevant streaming state machine contiguous.
fn parse_rhythmdb(
    input: &[u8],
    limits: RhythmboxImportLimits,
    issues: &mut Vec<RhythmboxImportIssue>,
) -> Result<Vec<RhythmboxTrack>, RhythmboxParseError> {
    let document = RhythmboxDocument::RhythmDb;
    let xml = prepare_xml(input, limits.max_rhythmdb_bytes, document)?;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_comments = true;
    reader.config_mut().expand_empty_elements = true;

    let mut preamble = XmlPreamble::default();
    let mut stack = Vec::new();
    let mut current_track = None;
    let mut direct_entry_ordinal = 0usize;
    let mut song_count = 0usize;
    let mut element_count = 0usize;
    let mut tracks = Vec::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
        match event {
            Event::Start(element) => {
                preamble.on_start(document)?;
                bump_limit(
                    &mut element_count,
                    limits.max_elements_per_document,
                    document,
                    RhythmboxLimit::Elements,
                )?;
                let name = element_name(&element, limits, document)?;
                let attributes = parse_attributes(&element, limits, document)?;

                if stack.is_empty() {
                    if preamble.root_closed || name != "rhythmdb" {
                        return Err(RhythmboxParseError::UnsupportedDocument { document });
                    }
                    let version = attribute_value(&attributes, "version");
                    if version != Some("2.0") {
                        return Err(RhythmboxParseError::UnsupportedDocument { document });
                    }
                    preamble.saw_root = true;
                    stack.push(RhythmDbNode::Root);
                } else {
                    let node = match stack.last() {
                        Some(RhythmDbNode::Root) if name == "entry" => {
                            direct_entry_ordinal = direct_entry_ordinal.checked_add(1).ok_or(
                                RhythmboxParseError::LimitExceeded {
                                    document,
                                    limit: RhythmboxLimit::Elements,
                                },
                            )?;
                            let is_song = attribute_value(&attributes, "type") == Some("song");
                            if is_song {
                                bump_limit(
                                    &mut song_count,
                                    limits.max_tracks,
                                    document,
                                    RhythmboxLimit::Tracks,
                                )?;
                                current_track = Some(TrackBuilder {
                                    source_ordinal: direct_entry_ordinal,
                                    ..TrackBuilder::default()
                                });
                            } else {
                                push_issue(
                                    issues,
                                    RhythmboxImportIssue {
                                        document,
                                        item_ordinal: direct_entry_ordinal,
                                        entry_ordinal: None,
                                        kind: RhythmboxImportIssueKind::UnsupportedEntryType,
                                    },
                                    limits,
                                )?;
                                current_track = None;
                            }
                            RhythmDbNode::Entry
                        }
                        Some(RhythmDbNode::Entry) => {
                            if let Some(field) = track_field(&name) {
                                if let Some(builder) = current_track.as_mut() {
                                    mark_track_field(builder, field, document)?;
                                    RhythmDbNode::Field(field, String::new())
                                } else {
                                    RhythmDbNode::Ignored
                                }
                            } else {
                                RhythmDbNode::Ignored
                            }
                        }
                        Some(RhythmDbNode::Field(_, _)) => {
                            return Err(RhythmboxParseError::InvalidStructure { document });
                        }
                        _ => RhythmDbNode::Ignored,
                    };
                    stack.push(node);
                }
                enforce_depth(stack.len(), limits, document)?;
            }
            Event::End(_) => {
                let node = stack
                    .pop()
                    .ok_or(RhythmboxParseError::InvalidStructure { document })?;
                match node {
                    RhythmDbNode::Root => preamble.root_closed = true,
                    RhythmDbNode::Entry => {
                        if let Some(builder) = current_track.take() {
                            if let Some(track) = finish_track(builder, limits, issues)? {
                                tracks.push(track);
                            }
                        }
                    }
                    RhythmDbNode::Field(field, value) => {
                        let builder = current_track
                            .as_mut()
                            .ok_or(RhythmboxParseError::InvalidStructure { document })?;
                        set_track_field(builder, field, value);
                    }
                    RhythmDbNode::Ignored => {}
                }
            }
            Event::Text(text) => {
                let value = text
                    .xml10_content()
                    .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
                validate_xml_10_characters(&value, document)?;
                append_document_text(
                    &mut stack,
                    &value,
                    limits,
                    document,
                    preamble.saw_root,
                    preamble.root_closed,
                )?;
                preamble.on_non_declaration_event();
            }
            Event::CData(text) => {
                let value = text
                    .xml10_content()
                    .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
                validate_xml_10_characters(&value, document)?;
                append_document_text(
                    &mut stack,
                    &value,
                    limits,
                    document,
                    preamble.saw_root,
                    preamble.root_closed,
                )?;
                preamble.on_non_declaration_event();
            }
            Event::GeneralRef(reference) => {
                let value = resolve_predefined_reference(&reference, document)?;
                validate_xml_10_characters(&value, document)?;
                append_document_text(
                    &mut stack,
                    &value,
                    limits,
                    document,
                    preamble.saw_root,
                    preamble.root_closed,
                )?;
                preamble.on_non_declaration_event();
            }
            Event::Decl(declaration) => preamble.on_declaration(&declaration, document)?,
            Event::DocType(_) => return Err(RhythmboxParseError::ForbiddenDtd { document }),
            Event::PI(_) => {
                return Err(RhythmboxParseError::ForbiddenProcessingInstruction { document });
            }
            Event::Comment(comment) => {
                let value = comment
                    .xml10_content()
                    .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
                validate_xml_10_characters(&value, document)?;
                preamble.on_non_declaration_event();
            }
            Event::Empty(_) => return Err(RhythmboxParseError::MalformedXml { document }),
            Event::Eof => break,
        }
    }

    if !preamble.saw_root || !preamble.root_closed || !stack.is_empty() {
        return Err(RhythmboxParseError::InvalidStructure { document });
    }
    Ok(tracks)
}

fn append_document_text(
    stack: &mut [RhythmDbNode],
    value: &str,
    limits: RhythmboxImportLimits,
    document: RhythmboxDocument,
    saw_root: bool,
    root_closed: bool,
) -> Result<(), RhythmboxParseError> {
    if stack.is_empty() {
        return if (!saw_root || root_closed) && !value.trim().is_empty() {
            Err(RhythmboxParseError::InvalidStructure { document })
        } else {
            Ok(())
        };
    }
    match stack.last_mut() {
        Some(RhythmDbNode::Field(_, collected)) => {
            append_bounded(collected, value, limits.max_text_bytes, document)
        }
        Some(RhythmDbNode::Root | RhythmDbNode::Entry) if !value.trim().is_empty() => {
            Err(RhythmboxParseError::InvalidStructure { document })
        }
        _ => Ok(()),
    }
}

fn finish_track(
    builder: TrackBuilder,
    limits: RhythmboxImportLimits,
    issues: &mut Vec<RhythmboxImportIssue>,
) -> Result<Option<RhythmboxTrack>, RhythmboxParseError> {
    let document = RhythmboxDocument::RhythmDb;
    let Some(raw_location) = builder.location else {
        push_issue(
            issues,
            RhythmboxImportIssue {
                document,
                item_ordinal: builder.source_ordinal,
                entry_ordinal: None,
                kind: RhythmboxImportIssueKind::MissingLocation,
            },
            limits,
        )?;
        return Ok(None);
    };
    let location = match parse_file_location(raw_location.trim()) {
        Ok(location) => location,
        Err(reason) => {
            push_issue(
                issues,
                RhythmboxImportIssue {
                    document,
                    item_ordinal: builder.source_ordinal,
                    entry_ordinal: None,
                    kind: RhythmboxImportIssueKind::InvalidLocation(reason),
                },
                limits,
            )?;
            return Ok(None);
        }
    };

    let rating = match builder.rating.as_deref() {
        Some(value) => retain_valid_numeric(
            parse_rating(value, builder.source_ordinal),
            builder.source_ordinal,
            limits,
            issues,
        )?,
        None => None,
    };
    let play_count = match builder.play_count.as_deref() {
        Some(value) => retain_valid_numeric(
            parse_u32_field(value, builder.source_ordinal),
            builder.source_ordinal,
            limits,
            issues,
        )?,
        None => None,
    };
    let last_played_unix_seconds = match builder.last_played.as_deref() {
        Some(value) => retain_valid_numeric(
            parse_last_played(value, builder.source_ordinal),
            builder.source_ordinal,
            limits,
            issues,
        )?,
        None => None,
    };

    Ok(Some(RhythmboxTrack {
        source_ordinal: builder.source_ordinal,
        location,
        rating,
        play_count,
        last_played_unix_seconds,
    }))
}

fn retain_valid_numeric<T>(
    result: Result<T, RhythmboxParseError>,
    item_ordinal: usize,
    limits: RhythmboxImportLimits,
    issues: &mut Vec<RhythmboxImportIssue>,
) -> Result<Option<T>, RhythmboxParseError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(RhythmboxParseError::InvalidNumeric { field, .. }) => {
            push_issue(
                issues,
                RhythmboxImportIssue {
                    document: RhythmboxDocument::RhythmDb,
                    item_ordinal,
                    entry_ordinal: None,
                    kind: RhythmboxImportIssueKind::InvalidNumeric(field),
                },
                limits,
            )?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn parse_rating(value: &str, item_ordinal: usize) -> Result<RhythmboxRating, RhythmboxParseError> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|_| RhythmboxParseError::InvalidNumeric {
            document: RhythmboxDocument::RhythmDb,
            item_ordinal,
            field: RhythmboxNumericField::Rating,
        })?;
    if !parsed.is_finite() || !(0.0..=5.0).contains(&parsed) {
        return Err(RhythmboxParseError::InvalidNumeric {
            document: RhythmboxDocument::RhythmDb,
            item_ordinal,
            field: RhythmboxNumericField::Rating,
        });
    }
    let canonical = if parsed == 0.0 { 0.0 } else { parsed };
    Ok(RhythmboxRating(canonical.to_bits()))
}

fn parse_u32_field(value: &str, item_ordinal: usize) -> Result<u32, RhythmboxParseError> {
    let parsed = value
        .trim()
        .parse::<u32>()
        .map_err(|_| RhythmboxParseError::InvalidNumeric {
            document: RhythmboxDocument::RhythmDb,
            item_ordinal,
            field: RhythmboxNumericField::PlayCount,
        })?;
    if i32::try_from(parsed).is_err() {
        return Err(RhythmboxParseError::InvalidNumeric {
            document: RhythmboxDocument::RhythmDb,
            item_ordinal,
            field: RhythmboxNumericField::PlayCount,
        });
    }
    Ok(parsed)
}

fn parse_last_played(value: &str, item_ordinal: usize) -> Result<u64, RhythmboxParseError> {
    let parsed = value
        .trim()
        .parse::<u64>()
        .map_err(|_| RhythmboxParseError::InvalidNumeric {
            document: RhythmboxDocument::RhythmDb,
            item_ordinal,
            field: RhythmboxNumericField::LastPlayed,
        })?;
    let milliseconds = i64::try_from(parsed)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000));
    if milliseconds.is_none_or(|value| DateTime::<Utc>::from_timestamp_millis(value).is_none()) {
        return Err(RhythmboxParseError::InvalidNumeric {
            document: RhythmboxDocument::RhythmDb,
            item_ordinal,
            field: RhythmboxNumericField::LastPlayed,
        });
    }
    Ok(parsed)
}

fn track_field(name: &str) -> Option<TrackField> {
    match name {
        "location" => Some(TrackField::Location),
        "rating" => Some(TrackField::Rating),
        "play-count" => Some(TrackField::PlayCount),
        "last-played" => Some(TrackField::LastPlayed),
        _ => None,
    }
}

const fn mark_track_field(
    builder: &mut TrackBuilder,
    field: TrackField,
    document: RhythmboxDocument,
) -> Result<(), RhythmboxParseError> {
    let seen = match field {
        TrackField::Location => &mut builder.seen_location,
        TrackField::Rating => &mut builder.seen_rating,
        TrackField::PlayCount => &mut builder.seen_play_count,
        TrackField::LastPlayed => &mut builder.seen_last_played,
    };
    if *seen {
        return Err(RhythmboxParseError::DuplicateField {
            document,
            item_ordinal: builder.source_ordinal,
        });
    }
    *seen = true;
    Ok(())
}

fn set_track_field(builder: &mut TrackBuilder, field: TrackField, value: String) {
    match field {
        TrackField::Location => builder.location = Some(value),
        TrackField::Rating => builder.rating = Some(value),
        TrackField::PlayCount => builder.play_count = Some(value),
        TrackField::LastPlayed => builder.last_played = Some(value),
    }
}

enum PlaylistNode {
    Root,
    Playlist,
    Location { ordinal: usize, text: String },
    Automatic(RhythmboxAutomaticNode),
    Ignored,
}

enum PlaylistBuilderKind {
    Static(Vec<RhythmboxPlaylistEntry>),
    Queue(Vec<RhythmboxPlaylistEntry>),
    Automatic(RhythmboxAutomaticPlaylist),
    Unsupported(String),
}

struct PlaylistBuilder {
    source_ordinal: usize,
    name: String,
    kind: PlaylistBuilderKind,
    next_entry_ordinal: usize,
    automatic_node_count: usize,
}

#[allow(clippy::too_many_lines)] // Keep the security-relevant streaming state machine contiguous.
fn parse_playlists(
    input: &[u8],
    limits: RhythmboxImportLimits,
    issues: &mut Vec<RhythmboxImportIssue>,
) -> Result<Vec<RhythmboxPlaylist>, RhythmboxParseError> {
    let document = RhythmboxDocument::Playlists;
    let xml = prepare_xml(input, limits.max_playlists_bytes, document)?;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_comments = true;
    reader.config_mut().expand_empty_elements = true;

    let mut preamble = XmlPreamble::default();
    let mut stack = Vec::new();
    let mut current_playlist = None;
    let mut playlist_count = 0usize;
    let mut total_entry_count = 0usize;
    let mut element_count = 0usize;
    let mut playlists = Vec::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
        match event {
            Event::Start(element) => {
                preamble.on_start(document)?;
                bump_limit(
                    &mut element_count,
                    limits.max_elements_per_document,
                    document,
                    RhythmboxLimit::Elements,
                )?;
                let name = element_name(&element, limits, document)?;
                let attributes = parse_attributes(&element, limits, document)?;

                if stack.is_empty() {
                    if preamble.root_closed || name != "rhythmdb-playlists" {
                        return Err(RhythmboxParseError::UnsupportedDocument { document });
                    }
                    preamble.saw_root = true;
                    stack.push(PlaylistNode::Root);
                } else {
                    let node = match stack.last() {
                        Some(PlaylistNode::Root) if name == "playlist" => {
                            bump_limit(
                                &mut playlist_count,
                                limits.max_playlists,
                                document,
                                RhythmboxLimit::Playlists,
                            )?;
                            current_playlist =
                                Some(start_playlist(playlist_count, attributes, document)?);
                            PlaylistNode::Playlist
                        }
                        Some(PlaylistNode::Playlist) => start_playlist_child(
                            name,
                            attributes,
                            current_playlist
                                .as_mut()
                                .ok_or(RhythmboxParseError::InvalidStructure { document })?,
                            &mut total_entry_count,
                            limits,
                        )?,
                        Some(PlaylistNode::Automatic(_)) => {
                            let builder = current_playlist
                                .as_mut()
                                .ok_or(RhythmboxParseError::InvalidStructure { document })?;
                            bump_limit(
                                &mut builder.automatic_node_count,
                                limits.max_automatic_nodes,
                                document,
                                RhythmboxLimit::AutomaticNodes,
                            )?;
                            PlaylistNode::Automatic(RhythmboxAutomaticNode {
                                element: name,
                                attributes,
                                text: String::new(),
                                children: Vec::new(),
                            })
                        }
                        Some(PlaylistNode::Location { .. }) => {
                            return Err(RhythmboxParseError::InvalidStructure { document });
                        }
                        _ => PlaylistNode::Ignored,
                    };
                    stack.push(node);
                }
                enforce_depth(stack.len(), limits, document)?;
            }
            Event::End(_) => {
                let node = stack
                    .pop()
                    .ok_or(RhythmboxParseError::InvalidStructure { document })?;
                match node {
                    PlaylistNode::Root => preamble.root_closed = true,
                    PlaylistNode::Playlist => {
                        let builder = current_playlist
                            .take()
                            .ok_or(RhythmboxParseError::InvalidStructure { document })?;
                        playlists.push(finish_playlist(builder, issues, limits)?);
                    }
                    PlaylistNode::Location { ordinal, text } => {
                        finish_playlist_location(
                            current_playlist
                                .as_mut()
                                .ok_or(RhythmboxParseError::InvalidStructure { document })?,
                            ordinal,
                            &text,
                            issues,
                            limits,
                        )?;
                    }
                    PlaylistNode::Automatic(mut node) => {
                        if !node.children.is_empty() && node.text.trim().is_empty() {
                            node.text.clear();
                        }
                        if let Some(PlaylistNode::Automatic(parent)) = stack.last_mut() {
                            parent.children.push(node);
                        } else {
                            let builder = current_playlist
                                .as_mut()
                                .ok_or(RhythmboxParseError::InvalidStructure { document })?;
                            let PlaylistBuilderKind::Automatic(automatic) = &mut builder.kind
                            else {
                                return Err(RhythmboxParseError::InvalidStructure { document });
                            };
                            automatic.query.push(node);
                        }
                    }
                    PlaylistNode::Ignored => {}
                }
            }
            Event::Text(text) => {
                let value = text
                    .xml10_content()
                    .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
                validate_xml_10_characters(&value, document)?;
                append_playlist_text(
                    &mut stack,
                    &value,
                    limits,
                    document,
                    preamble.saw_root,
                    preamble.root_closed,
                )?;
                preamble.on_non_declaration_event();
            }
            Event::CData(text) => {
                let value = text
                    .xml10_content()
                    .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
                validate_xml_10_characters(&value, document)?;
                append_playlist_text(
                    &mut stack,
                    &value,
                    limits,
                    document,
                    preamble.saw_root,
                    preamble.root_closed,
                )?;
                preamble.on_non_declaration_event();
            }
            Event::GeneralRef(reference) => {
                let value = resolve_predefined_reference(&reference, document)?;
                validate_xml_10_characters(&value, document)?;
                append_playlist_text(
                    &mut stack,
                    &value,
                    limits,
                    document,
                    preamble.saw_root,
                    preamble.root_closed,
                )?;
                preamble.on_non_declaration_event();
            }
            Event::Decl(declaration) => preamble.on_declaration(&declaration, document)?,
            Event::DocType(_) => return Err(RhythmboxParseError::ForbiddenDtd { document }),
            Event::PI(_) => {
                return Err(RhythmboxParseError::ForbiddenProcessingInstruction { document });
            }
            Event::Comment(comment) => {
                let value = comment
                    .xml10_content()
                    .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
                validate_xml_10_characters(&value, document)?;
                preamble.on_non_declaration_event();
            }
            Event::Empty(_) => return Err(RhythmboxParseError::MalformedXml { document }),
            Event::Eof => break,
        }
    }

    if !preamble.saw_root || !preamble.root_closed || !stack.is_empty() {
        return Err(RhythmboxParseError::InvalidStructure { document });
    }
    Ok(playlists)
}

fn start_playlist(
    source_ordinal: usize,
    mut attributes: Vec<RhythmboxXmlAttribute>,
    document: RhythmboxDocument,
) -> Result<PlaylistBuilder, RhythmboxParseError> {
    let name_index = attributes
        .iter()
        .position(|attribute| attribute.name == "name");
    let type_index = attributes
        .iter()
        .position(|attribute| attribute.name == "type");
    let (Some(name_index), Some(type_index)) = (name_index, type_index) else {
        return Err(RhythmboxParseError::MissingPlaylistAttribute {
            document,
            item_ordinal: source_ordinal,
        });
    };
    let name = attributes[name_index].value.clone();
    let source_type = attributes[type_index].value.clone();
    attributes.retain(|attribute| !matches!(attribute.name.as_str(), "name" | "type"));

    let kind = match source_type.as_str() {
        "static" => PlaylistBuilderKind::Static(Vec::new()),
        "queue" => PlaylistBuilderKind::Queue(Vec::new()),
        "automatic" => PlaylistBuilderKind::Automatic(RhythmboxAutomaticPlaylist {
            attributes,
            query: Vec::new(),
        }),
        _ => PlaylistBuilderKind::Unsupported(source_type),
    };
    Ok(PlaylistBuilder {
        source_ordinal,
        name,
        kind,
        next_entry_ordinal: 0,
        automatic_node_count: 0,
    })
}

fn start_playlist_child(
    name: String,
    attributes: Vec<RhythmboxXmlAttribute>,
    builder: &mut PlaylistBuilder,
    total_entry_count: &mut usize,
    limits: RhythmboxImportLimits,
) -> Result<PlaylistNode, RhythmboxParseError> {
    let document = RhythmboxDocument::Playlists;
    match &builder.kind {
        PlaylistBuilderKind::Static(_) | PlaylistBuilderKind::Queue(_) if name == "location" => {
            bump_limit(
                total_entry_count,
                limits.max_playlist_entries,
                document,
                RhythmboxLimit::PlaylistEntries,
            )?;
            builder.next_entry_ordinal = builder.next_entry_ordinal.checked_add(1).ok_or(
                RhythmboxParseError::LimitExceeded {
                    document,
                    limit: RhythmboxLimit::PlaylistEntries,
                },
            )?;
            Ok(PlaylistNode::Location {
                ordinal: builder.next_entry_ordinal,
                text: String::new(),
            })
        }
        PlaylistBuilderKind::Automatic(_) => {
            bump_limit(
                &mut builder.automatic_node_count,
                limits.max_automatic_nodes,
                document,
                RhythmboxLimit::AutomaticNodes,
            )?;
            Ok(PlaylistNode::Automatic(RhythmboxAutomaticNode {
                element: name,
                attributes,
                text: String::new(),
                children: Vec::new(),
            }))
        }
        _ => Ok(PlaylistNode::Ignored),
    }
}

fn append_playlist_text(
    stack: &mut [PlaylistNode],
    value: &str,
    limits: RhythmboxImportLimits,
    document: RhythmboxDocument,
    saw_root: bool,
    root_closed: bool,
) -> Result<(), RhythmboxParseError> {
    if stack.is_empty() {
        return if (!saw_root || root_closed) && !value.trim().is_empty() {
            Err(RhythmboxParseError::InvalidStructure { document })
        } else {
            Ok(())
        };
    }
    match stack.last_mut() {
        Some(PlaylistNode::Location { text, .. }) => {
            append_bounded(text, value, limits.max_text_bytes, document)
        }
        Some(PlaylistNode::Automatic(node)) => {
            append_bounded(&mut node.text, value, limits.max_text_bytes, document)
        }
        Some(PlaylistNode::Root | PlaylistNode::Playlist) if !value.trim().is_empty() => {
            Err(RhythmboxParseError::InvalidStructure { document })
        }
        _ => Ok(()),
    }
}

fn finish_playlist_location(
    builder: &mut PlaylistBuilder,
    ordinal: usize,
    text: &str,
    issues: &mut Vec<RhythmboxImportIssue>,
    limits: RhythmboxImportLimits,
) -> Result<(), RhythmboxParseError> {
    let location = if text.trim().is_empty() {
        push_issue(
            issues,
            RhythmboxImportIssue {
                document: RhythmboxDocument::Playlists,
                item_ordinal: builder.source_ordinal,
                entry_ordinal: Some(ordinal),
                kind: RhythmboxImportIssueKind::MissingLocation,
            },
            limits,
        )?;
        None
    } else {
        match parse_file_location(text.trim()) {
            Ok(location) => Some(location),
            Err(reason) => {
                push_issue(
                    issues,
                    RhythmboxImportIssue {
                        document: RhythmboxDocument::Playlists,
                        item_ordinal: builder.source_ordinal,
                        entry_ordinal: Some(ordinal),
                        kind: RhythmboxImportIssueKind::InvalidLocation(reason),
                    },
                    limits,
                )?;
                None
            }
        }
    };
    let entry = RhythmboxPlaylistEntry {
        source_ordinal: ordinal,
        location,
    };
    match &mut builder.kind {
        PlaylistBuilderKind::Static(entries) | PlaylistBuilderKind::Queue(entries) => {
            entries.push(entry);
            Ok(())
        }
        _ => Err(RhythmboxParseError::InvalidStructure {
            document: RhythmboxDocument::Playlists,
        }),
    }
}

fn finish_playlist(
    builder: PlaylistBuilder,
    issues: &mut Vec<RhythmboxImportIssue>,
    limits: RhythmboxImportLimits,
) -> Result<RhythmboxPlaylist, RhythmboxParseError> {
    let kind = match builder.kind {
        PlaylistBuilderKind::Static(entries) => RhythmboxPlaylistKind::Static(entries),
        PlaylistBuilderKind::Queue(entries) => RhythmboxPlaylistKind::Queue(entries),
        PlaylistBuilderKind::Automatic(automatic) => RhythmboxPlaylistKind::Automatic(automatic),
        PlaylistBuilderKind::Unsupported(source_type) => {
            push_issue(
                issues,
                RhythmboxImportIssue {
                    document: RhythmboxDocument::Playlists,
                    item_ordinal: builder.source_ordinal,
                    entry_ordinal: None,
                    kind: RhythmboxImportIssueKind::UnsupportedPlaylistType,
                },
                limits,
            )?;
            RhythmboxPlaylistKind::Unsupported { source_type }
        }
    };
    Ok(RhythmboxPlaylist {
        source_ordinal: builder.source_ordinal,
        name: builder.name,
        kind,
    })
}

#[allow(clippy::struct_excessive_bools)] // These flags encode orthogonal XML invariants.
struct XmlPreamble {
    saw_declaration: bool,
    declaration_allowed: bool,
    saw_root: bool,
    root_closed: bool,
}

impl Default for XmlPreamble {
    fn default() -> Self {
        Self {
            saw_declaration: false,
            declaration_allowed: true,
            saw_root: false,
            root_closed: false,
        }
    }
}

impl XmlPreamble {
    const fn on_start(&mut self, document: RhythmboxDocument) -> Result<(), RhythmboxParseError> {
        if self.root_closed {
            return Err(RhythmboxParseError::InvalidStructure { document });
        }
        self.declaration_allowed = false;
        Ok(())
    }

    const fn on_non_declaration_event(&mut self) {
        self.declaration_allowed = false;
    }

    fn on_declaration(
        &mut self,
        declaration: &BytesDecl<'_>,
        document: RhythmboxDocument,
    ) -> Result<(), RhythmboxParseError> {
        // `Default` starts false so a declaration is allowed only before any
        // event; initialize lazily for the first declaration/start decision.
        if !self.declaration_allowed || self.saw_declaration || self.saw_root || self.root_closed {
            return Err(RhythmboxParseError::InvalidDeclaration { document });
        }
        validate_declaration(declaration, document)?;
        self.saw_declaration = true;
        self.declaration_allowed = false;
        Ok(())
    }
}

fn prepare_xml(
    input: &[u8],
    max_bytes: usize,
    document: RhythmboxDocument,
) -> Result<&str, RhythmboxParseError> {
    if input.len() > max_bytes {
        return Err(RhythmboxParseError::LimitExceeded {
            document,
            limit: RhythmboxLimit::InputBytes,
        });
    }
    let xml =
        std::str::from_utf8(input).map_err(|_| RhythmboxParseError::InvalidUtf8 { document })?;
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    validate_xml_10_characters(xml, document)?;
    Ok(xml)
}

fn element_name(
    element: &BytesStart<'_>,
    limits: RhythmboxImportLimits,
    document: RhythmboxDocument,
) -> Result<String, RhythmboxParseError> {
    let qualified_name = element.name();
    let name = std::str::from_utf8(qualified_name.as_ref())
        .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
    validate_xml_10_characters(name, document)?;
    if name.is_empty() || name.contains(':') {
        return Err(RhythmboxParseError::MalformedXml { document });
    }
    if name.len() > limits.max_text_bytes {
        return Err(RhythmboxParseError::LimitExceeded {
            document,
            limit: RhythmboxLimit::TextBytes,
        });
    }
    Ok(name.to_owned())
}

fn parse_attributes(
    element: &BytesStart<'_>,
    limits: RhythmboxImportLimits,
    document: RhythmboxDocument,
) -> Result<Vec<RhythmboxXmlAttribute>, RhythmboxParseError> {
    let mut parsed = Vec::new();
    for attribute in element.attributes().with_checks(true) {
        if parsed.len() >= limits.max_attributes_per_element {
            return Err(RhythmboxParseError::LimitExceeded {
                document,
                limit: RhythmboxLimit::Attributes,
            });
        }
        let attribute = attribute.map_err(|_| RhythmboxParseError::MalformedXml { document })?;
        let name = std::str::from_utf8(attribute.key.as_ref())
            .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
        validate_xml_10_characters(name, document)?;
        if name.is_empty() || name.contains(':') || name == "xmlns" {
            return Err(RhythmboxParseError::MalformedXml { document });
        }
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
        validate_xml_10_characters(&value, document)?;
        if name.len() > limits.max_text_bytes || value.len() > limits.max_text_bytes {
            return Err(RhythmboxParseError::LimitExceeded {
                document,
                limit: RhythmboxLimit::TextBytes,
            });
        }
        parsed.push(RhythmboxXmlAttribute {
            name: name.to_owned(),
            value: value.into_owned(),
        });
    }
    parsed.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.value.cmp(&right.value))
    });
    Ok(parsed)
}

fn attribute_value<'a>(attributes: &'a [RhythmboxXmlAttribute], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|attribute| attribute.name == name)
        .map(|attribute| attribute.value.as_str())
}

fn validate_declaration(
    declaration: &BytesDecl<'_>,
    document: RhythmboxDocument,
) -> Result<(), RhythmboxParseError> {
    let raw = std::str::from_utf8(declaration)
        .map_err(|_| RhythmboxParseError::InvalidDeclaration { document })?;
    let declaration = BytesStart::from_content(raw, 3);
    let mut stage = 0u8;
    for attribute in declaration.attributes().with_checks(true) {
        let attribute =
            attribute.map_err(|_| RhythmboxParseError::InvalidDeclaration { document })?;
        let value = attribute
            .normalized_value(XmlVersion::Explicit1_0)
            .map_err(|_| RhythmboxParseError::InvalidDeclaration { document })?;
        validate_xml_10_characters(&value, document)
            .map_err(|_| RhythmboxParseError::InvalidDeclaration { document })?;
        match attribute.key.as_ref() {
            b"version" if stage == 0 && value == "1.0" => stage = 1,
            b"encoding" if stage == 1 && value.eq_ignore_ascii_case("utf-8") => stage = 2,
            b"standalone"
                if (stage == 1 || stage == 2) && matches!(value.as_ref(), "yes" | "no") =>
            {
                stage = 3;
            }
            _ => return Err(RhythmboxParseError::InvalidDeclaration { document }),
        }
    }
    if stage == 0 {
        return Err(RhythmboxParseError::InvalidDeclaration { document });
    }
    Ok(())
}

fn resolve_predefined_reference(
    reference: &BytesRef<'_>,
    document: RhythmboxDocument,
) -> Result<String, RhythmboxParseError> {
    if let Some(value) = reference
        .resolve_char_ref()
        .map_err(|_| RhythmboxParseError::MalformedXml { document })?
    {
        let value = value.to_string();
        validate_xml_10_characters(&value, document)?;
        return Ok(value);
    }
    let name = reference
        .decode()
        .map_err(|_| RhythmboxParseError::MalformedXml { document })?;
    let value = quick_xml::escape::resolve_xml_entity(&name)
        .map(str::to_owned)
        .ok_or(RhythmboxParseError::ForbiddenEntity { document })?;
    validate_xml_10_characters(&value, document)?;
    Ok(value)
}

fn validate_xml_10_characters(
    value: &str,
    document: RhythmboxDocument,
) -> Result<(), RhythmboxParseError> {
    if value.chars().all(is_xml_10_character) {
        Ok(())
    } else {
        Err(RhythmboxParseError::MalformedXml { document })
    }
}

fn is_xml_10_character(character: char) -> bool {
    matches!(character, '\u{9}' | '\u{a}' | '\u{d}')
        || ('\u{20}'..='\u{d7ff}').contains(&character)
        || ('\u{e000}'..='\u{fffd}').contains(&character)
        || ('\u{10000}'..='\u{10ffff}').contains(&character)
}

fn parse_file_location(value: &str) -> Result<RhythmboxFileLocation, RhythmboxLocationIssue> {
    if has_malformed_percent_escape(value) {
        return Err(RhythmboxLocationIssue::Malformed);
    }
    let url = Url::parse(value).map_err(|_| RhythmboxLocationIssue::Malformed)?;
    if url.scheme() != "file" {
        return Err(RhythmboxLocationIssue::NonFileScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(RhythmboxLocationIssue::Credentials);
    }
    if url.port().is_some() {
        return Err(RhythmboxLocationIssue::Port);
    }
    if url.query().is_some() {
        return Err(RhythmboxLocationIssue::Query);
    }
    if url.fragment().is_some() {
        return Err(RhythmboxLocationIssue::Fragment);
    }
    if url
        .host_str()
        .is_some_and(|host| !host.eq_ignore_ascii_case("localhost"))
    {
        return Err(RhythmboxLocationIssue::RemoteAuthority);
    }
    if has_parent_path_component(value) {
        return Err(RhythmboxLocationIssue::ParentTraversal);
    }
    validate_decoded_path_text(url.path())?;
    let path = url
        .to_file_path()
        .map_err(|()| RhythmboxLocationIssue::NotAbsolute)?;
    if !path.is_absolute() {
        return Err(RhythmboxLocationIssue::NotAbsolute);
    }
    // Percent-decoded separators can create a parent component that was not
    // visible while inspecting the encoded URI (for example, `%2e%2e%2f`).
    // Validate the platform path as well as the raw URI spelling.
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(RhythmboxLocationIssue::ParentTraversal);
    }
    let path_text = path.to_str().ok_or(RhythmboxLocationIssue::NotUtf8)?;
    if path_text.contains('\0') {
        return Err(RhythmboxLocationIssue::ContainsNul);
    }
    Ok(RhythmboxFileLocation(path))
}

fn validate_decoded_path_text(path: &str) -> Result<(), RhythmboxLocationIssue> {
    let encoded = path.as_bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0usize;
    while index < encoded.len() {
        if encoded[index] == b'%' {
            let high = hex_value(encoded[index + 1]);
            let low = hex_value(encoded[index + 2]);
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(encoded[index]);
            index += 1;
        }
    }
    if decoded.contains(&0) {
        return Err(RhythmboxLocationIssue::ContainsNul);
    }
    std::str::from_utf8(&decoded)
        .map(|_| ())
        .map_err(|_| RhythmboxLocationIssue::NotUtf8)
}

const fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn has_malformed_percent_escape(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && (index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            return true;
        }
        index += if bytes[index] == b'%' { 3 } else { 1 };
    }
    false
}

fn has_parent_path_component(value: &str) -> bool {
    value
        .split(['/', '\\', '?', '#'])
        .any(is_encoded_parent_component)
}

fn is_encoded_parent_component(component: &str) -> bool {
    let bytes = component.as_bytes();
    let mut index = 0usize;
    let mut dots = 0u8;
    while index < bytes.len() {
        if bytes[index] == b'.' {
            dots = dots.saturating_add(1);
            index += 1;
        } else if index + 2 < bytes.len()
            && bytes[index] == b'%'
            && bytes[index + 1] == b'2'
            && matches!(bytes[index + 2], b'e' | b'E')
        {
            dots = dots.saturating_add(1);
            index += 3;
        } else {
            return false;
        }
    }
    dots == 2
}

fn append_bounded(
    destination: &mut String,
    value: &str,
    max_bytes: usize,
    document: RhythmboxDocument,
) -> Result<(), RhythmboxParseError> {
    if destination
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > max_bytes)
    {
        return Err(RhythmboxParseError::LimitExceeded {
            document,
            limit: RhythmboxLimit::TextBytes,
        });
    }
    destination.push_str(value);
    Ok(())
}

fn bump_limit(
    value: &mut usize,
    max: usize,
    document: RhythmboxDocument,
    limit: RhythmboxLimit,
) -> Result<(), RhythmboxParseError> {
    *value = value
        .checked_add(1)
        .ok_or(RhythmboxParseError::LimitExceeded { document, limit })?;
    if *value > max {
        return Err(RhythmboxParseError::LimitExceeded { document, limit });
    }
    Ok(())
}

const fn enforce_depth(
    depth: usize,
    limits: RhythmboxImportLimits,
    document: RhythmboxDocument,
) -> Result<(), RhythmboxParseError> {
    if depth > limits.max_xml_depth {
        return Err(RhythmboxParseError::LimitExceeded {
            document,
            limit: RhythmboxLimit::XmlDepth,
        });
    }
    Ok(())
}

fn push_issue(
    issues: &mut Vec<RhythmboxImportIssue>,
    issue: RhythmboxImportIssue,
    limits: RhythmboxImportLimits,
) -> Result<(), RhythmboxParseError> {
    if issues.len() >= limits.max_issues {
        return Err(RhythmboxParseError::LimitExceeded {
            document: issue.document,
            limit: RhythmboxLimit::Issues,
        });
    }
    issues.push(issue);
    Ok(())
}

fn retained_text_bytes(import: &RhythmboxImport) -> Option<usize> {
    let mut total = 0usize;
    for track in &import.tracks {
        checked_add_text(&mut total, track.location.as_path().to_str()?.len())?;
    }
    for playlist in &import.playlists {
        checked_add_text(&mut total, playlist.name.len())?;
        match &playlist.kind {
            RhythmboxPlaylistKind::Static(entries) | RhythmboxPlaylistKind::Queue(entries) => {
                for entry in entries {
                    if let Some(location) = &entry.location {
                        checked_add_text(&mut total, location.as_path().to_str()?.len())?;
                    }
                }
            }
            RhythmboxPlaylistKind::Automatic(automatic) => {
                retained_attribute_bytes(&mut total, &automatic.attributes)?;
                for node in &automatic.query {
                    retained_automatic_node_bytes(&mut total, node)?;
                }
            }
            RhythmboxPlaylistKind::Unsupported { source_type } => {
                checked_add_text(&mut total, source_type.len())?;
            }
        }
    }
    Some(total)
}

fn retained_automatic_node_bytes(total: &mut usize, node: &RhythmboxAutomaticNode) -> Option<()> {
    checked_add_text(total, node.element.len())?;
    checked_add_text(total, node.text.len())?;
    retained_attribute_bytes(total, &node.attributes)?;
    for child in &node.children {
        retained_automatic_node_bytes(total, child)?;
    }
    Some(())
}

fn retained_attribute_bytes(total: &mut usize, attributes: &[RhythmboxXmlAttribute]) -> Option<()> {
    for attribute in attributes {
        checked_add_text(total, attribute.name.len())?;
        checked_add_text(total, attribute.value.len())?;
    }
    Some(())
}

fn checked_add_text(total: &mut usize, bytes: usize) -> Option<()> {
    *total = total.checked_add(bytes)?;
    Some(())
}

fn semantic_digest(import: &RhythmboxImport, playlists_present: bool) -> RhythmboxSemanticDigest {
    let mut encoder = SemanticEncoder::new();
    encoder.field(
        b"playlists-document-present",
        &[u8::from(playlists_present)],
    );

    let mut tracks = import.tracks.iter().collect::<Vec<_>>();
    tracks.sort_by(|left, right| {
        left.location
            .cmp(&right.location)
            .then_with(|| left.rating.cmp(&right.rating))
            .then_with(|| left.play_count.cmp(&right.play_count))
            .then_with(|| {
                left.last_played_unix_seconds
                    .cmp(&right.last_played_unix_seconds)
            })
    });
    encoder.count(b"tracks", tracks.len());
    for track in tracks {
        encoder.marker(b"track");
        encoder.field(
            b"location",
            track.location.as_path().to_string_lossy().as_bytes(),
        );
        encoder.optional_u64(b"rating", track.rating.map(|rating| rating.0));
        encoder.optional_u64(b"play-count", track.play_count.map(u64::from));
        encoder.optional_u64(b"last-played", track.last_played_unix_seconds);
    }

    encoder.count(b"playlists", import.playlists.len());
    for playlist in &import.playlists {
        encoder.marker(b"playlist");
        encoder.field(b"name", playlist.name.as_bytes());
        encode_playlist_kind(&mut encoder, &playlist.kind);
    }

    encoder.count(b"issues", import.issues.len());
    for issue in &import.issues {
        encoder.marker(b"issue");
        encoder.field(b"document", &[document_code(issue.document)]);
        encoder.usize(b"item-ordinal", issue.item_ordinal);
        encoder.optional_usize(b"entry-ordinal", issue.entry_ordinal);
        encode_issue_kind(&mut encoder, issue.kind);
    }
    RhythmboxSemanticDigest(encoder.finish())
}

fn encode_playlist_kind(encoder: &mut SemanticEncoder, kind: &RhythmboxPlaylistKind) {
    match kind {
        RhythmboxPlaylistKind::Static(entries) => {
            encoder.marker(b"playlist-static");
            encode_playlist_entries(encoder, entries);
        }
        RhythmboxPlaylistKind::Queue(entries) => {
            encoder.marker(b"playlist-queue");
            encode_playlist_entries(encoder, entries);
        }
        RhythmboxPlaylistKind::Automatic(automatic) => {
            encoder.marker(b"playlist-automatic");
            encode_automatic_playlist_attributes(encoder, &automatic.attributes);
            encoder.count(b"query-roots", automatic.query.len());
            for node in &automatic.query {
                encode_automatic_node(encoder, node);
            }
        }
        RhythmboxPlaylistKind::Unsupported { source_type } => {
            encoder.field(b"playlist-unsupported", source_type.as_bytes());
        }
    }
}

/// Rhythmbox persists these settings on the common playlist source, but they
/// only restore its browser/search-control presentation. Valid serialized
/// values cannot change playlist membership or Tributary's migration result.
pub(super) fn is_membership_inert_playlist_attribute(attribute: &RhythmboxXmlAttribute) -> bool {
    match attribute.name.as_str() {
        "show-browser" => matches!(attribute.value.as_str(), "true" | "false"),
        "browser-position" => attribute
            .value
            .parse::<i32>()
            .is_ok_and(|parsed| parsed.to_string() == attribute.value),
        "search-type" => true,
        _ => false,
    }
}

fn encode_automatic_playlist_attributes(
    encoder: &mut SemanticEncoder,
    attributes: &[RhythmboxXmlAttribute],
) {
    let migration_relevant_count = attributes
        .iter()
        .filter(|attribute| !is_membership_inert_playlist_attribute(attribute))
        .count();
    encoder.count(b"attributes", migration_relevant_count);
    for attribute in attributes
        .iter()
        .filter(|attribute| !is_membership_inert_playlist_attribute(attribute))
    {
        encoder.marker(b"attribute");
        encoder.field(b"name", attribute.name.as_bytes());
        encoder.field(b"value", attribute.value.as_bytes());
    }
}

fn encode_playlist_entries(encoder: &mut SemanticEncoder, entries: &[RhythmboxPlaylistEntry]) {
    encoder.count(b"entries", entries.len());
    for entry in entries {
        match &entry.location {
            Some(location) => encoder.field(
                b"entry-location",
                location.as_path().to_string_lossy().as_bytes(),
            ),
            None => encoder.marker(b"entry-invalid-location"),
        }
    }
}

fn encode_automatic_node(encoder: &mut SemanticEncoder, node: &RhythmboxAutomaticNode) {
    encoder.marker(b"automatic-node");
    encoder.field(b"element", node.element.as_bytes());
    encode_attributes(encoder, &node.attributes);
    encoder.field(b"text", node.text.as_bytes());
    encoder.count(b"children", node.children.len());
    for child in &node.children {
        encode_automatic_node(encoder, child);
    }
}

fn encode_attributes(encoder: &mut SemanticEncoder, attributes: &[RhythmboxXmlAttribute]) {
    encoder.count(b"attributes", attributes.len());
    for attribute in attributes {
        encoder.marker(b"attribute");
        encoder.field(b"name", attribute.name.as_bytes());
        encoder.field(b"value", attribute.value.as_bytes());
    }
}

fn encode_issue_kind(encoder: &mut SemanticEncoder, kind: RhythmboxImportIssueKind) {
    match kind {
        RhythmboxImportIssueKind::MissingLocation => encoder.marker(b"missing-location"),
        RhythmboxImportIssueKind::InvalidLocation(reason) => {
            encoder.field(b"invalid-location", &[location_issue_code(reason)]);
        }
        RhythmboxImportIssueKind::InvalidNumeric(field) => {
            encoder.field(b"invalid-numeric", &[numeric_field_code(field)]);
        }
        RhythmboxImportIssueKind::UnsupportedEntryType => {
            encoder.marker(b"unsupported-entry-type");
        }
        RhythmboxImportIssueKind::UnsupportedPlaylistType => {
            encoder.marker(b"unsupported-playlist-type");
        }
    }
}

const fn numeric_field_code(field: RhythmboxNumericField) -> u8 {
    match field {
        RhythmboxNumericField::Rating => 1,
        RhythmboxNumericField::PlayCount => 2,
        RhythmboxNumericField::LastPlayed => 3,
    }
}

const fn document_code(document: RhythmboxDocument) -> u8 {
    match document {
        RhythmboxDocument::RhythmDb => 1,
        RhythmboxDocument::Playlists => 2,
    }
}

const fn location_issue_code(issue: RhythmboxLocationIssue) -> u8 {
    match issue {
        RhythmboxLocationIssue::Malformed => 1,
        RhythmboxLocationIssue::NonFileScheme => 2,
        RhythmboxLocationIssue::RemoteAuthority => 3,
        RhythmboxLocationIssue::Credentials => 4,
        RhythmboxLocationIssue::Port => 5,
        RhythmboxLocationIssue::Query => 6,
        RhythmboxLocationIssue::Fragment => 7,
        RhythmboxLocationIssue::NotAbsolute => 8,
        RhythmboxLocationIssue::NotUtf8 => 9,
        RhythmboxLocationIssue::ContainsNul => 10,
        RhythmboxLocationIssue::ParentTraversal => 11,
    }
}

struct SemanticEncoder(Sha256);

impl SemanticEncoder {
    fn new() -> Self {
        let mut hasher = Sha256::new();
        Self::frame_raw(&mut hasher, b"domain", RHYTHMBOX_SNAPSHOT_DIGEST_DOMAIN);
        Self(hasher)
    }

    fn marker(&mut self, domain: &[u8]) {
        self.field(domain, &[]);
    }

    fn field(&mut self, domain: &[u8], value: &[u8]) {
        Self::frame_raw(&mut self.0, domain, value);
    }

    fn count(&mut self, domain: &[u8], value: usize) {
        self.usize(domain, value);
    }

    fn usize(&mut self, domain: &[u8], value: usize) {
        self.field(
            domain,
            &u64::try_from(value).unwrap_or(u64::MAX).to_be_bytes(),
        );
    }

    fn optional_usize(&mut self, domain: &[u8], value: Option<usize>) {
        match value {
            Some(value) => {
                self.field(domain, b"some");
                self.usize(b"value", value);
            }
            None => self.field(domain, b"none"),
        }
    }

    fn optional_u64(&mut self, domain: &[u8], value: Option<u64>) {
        match value {
            Some(value) => {
                self.field(domain, b"some");
                self.field(b"value", &value.to_be_bytes());
            }
            None => self.field(domain, b"none"),
        }
    }

    fn frame_raw(hasher: &mut Sha256, domain: &[u8], value: &[u8]) {
        hasher.update(
            u64::try_from(domain.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        hasher.update(domain);
        hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(value);
    }

    fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_PLAYLISTS: &[u8] = b"<rhythmdb-playlists/>";

    fn rhythmdb(entries: &str) -> Vec<u8> {
        format!("<rhythmdb version=\"2.0\">{entries}</rhythmdb>").into_bytes()
    }

    fn song(location: &str, fields: &str) -> String {
        format!("<entry type=\"song\"><location>{location}</location>{fields}</entry>")
    }

    fn local_uri(suffix: &str) -> String {
        if cfg!(windows) {
            format!("file:///C:/{suffix}")
        } else {
            format!("file:///{suffix}")
        }
    }

    fn local_path(suffix: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", suffix.replace('/', r"\")))
        } else {
            PathBuf::from(format!("/{suffix}"))
        }
    }

    fn parse_db(entries: &str) -> Result<RhythmboxImport, RhythmboxParseError> {
        parse_rhythmbox_documents(
            &rhythmdb(entries),
            Some(EMPTY_PLAYLISTS),
            RhythmboxImportLimits::default(),
        )
    }

    #[test]
    fn production_limits_bound_the_overlapping_profile_working_set() {
        let limits = RhythmboxImportLimits::default();
        assert_eq!(limits.max_rhythmdb_bytes, 128 * 1024 * 1024);
        assert_eq!(limits.max_playlists_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_tracks, 250_000);
        assert_eq!(limits.max_playlist_entries, 500_000);
        assert_eq!(limits.max_retained_text_bytes, 128 * 1024 * 1024);
        assert_eq!(limits.max_issues, 100_000);
        assert!(
            limits.max_issues < limits.max_tracks + limits.max_playlist_entries,
            "invalid rows must not be able to allocate one issue per maximum-sized item set"
        );
    }

    #[test]
    fn parses_valid_song_metadata_and_ignores_non_song_entries() {
        let xml = format!(
            "{}<entry type=\"iradio\"><location>https://example.invalid</location></entry>",
            song(
                &local_uri("music/A%20Song.flac"),
                "<rating>3.5</rating><play-count>42</play-count><last-played>1700000000</last-played>"
            )
        );
        let parsed = parse_db(&xml).unwrap();

        assert_eq!(parsed.tracks.len(), 1);
        let track = &parsed.tracks[0];
        assert_eq!(track.location.as_path(), local_path("music/A Song.flac"));
        assert_eq!(track.rating.map(RhythmboxRating::value), Some(3.5));
        assert_eq!(track.play_count, Some(42));
        assert_eq!(track.last_played_unix_seconds, Some(1_700_000_000));
        assert_eq!(parsed.issues.len(), 1);
        assert_eq!(
            parsed.issues[0].kind,
            RhythmboxImportIssueKind::UnsupportedEntryType
        );
    }

    #[test]
    fn rejects_malformed_xml_dtds_and_custom_entities() {
        let malformed = b"<rhythmdb version=\"2.0\"><entry></rhythmdb>";
        assert!(matches!(
            parse_rhythmbox_documents(malformed, None, RhythmboxImportLimits::default()),
            Err(RhythmboxParseError::MalformedXml { .. }
                | RhythmboxParseError::InvalidStructure { .. })
        ));

        let dtd = br#"<!DOCTYPE rhythmdb [<!ENTITY x "secret">]><rhythmdb version="2.0"/>"#;
        assert!(matches!(
            parse_rhythmbox_documents(dtd, None, RhythmboxImportLimits::default()),
            Err(RhythmboxParseError::ForbiddenDtd { .. })
        ));

        let entity = rhythmdb(&song(&local_uri("music/&custom;.flac"), ""));
        assert!(matches!(
            parse_rhythmbox_documents(&entity, None, RhythmboxImportLimits::default()),
            Err(RhythmboxParseError::ForbiddenEntity { .. })
        ));
    }

    #[test]
    fn enforces_xml_10_legal_characters_after_literal_and_reference_decoding() {
        for illegal in ['\u{1}', '\u{b}', '\u{fffe}'] {
            let literal = rhythmdb(&format!(
                "<entry type=\"iradio\"><title>{illegal}</title></entry>"
            ));
            assert!(matches!(
                parse_rhythmbox_documents(
                    &literal,
                    Some(EMPTY_PLAYLISTS),
                    RhythmboxImportLimits::default(),
                ),
                Err(RhythmboxParseError::MalformedXml { .. })
            ));
        }

        for reference in ["&#x1;", "&#xB;", "&#xFFFE;"] {
            let referenced = rhythmdb(&song(
                &format!("{}{}", local_uri("music/reference"), reference),
                "",
            ));
            assert!(matches!(
                parse_rhythmbox_documents(
                    &referenced,
                    Some(EMPTY_PLAYLISTS),
                    RhythmboxImportLimits::default(),
                ),
                Err(RhythmboxParseError::MalformedXml { .. })
            ));

            let attribute = format!(
                "<rhythmdb-playlists><playlist name=\"bad{reference}\" type=\"static\"/></rhythmdb-playlists>"
            );
            assert!(matches!(
                parse_rhythmbox_documents(
                    b"<rhythmdb version=\"2.0\"/>",
                    Some(attribute.as_bytes()),
                    RhythmboxImportLimits::default(),
                ),
                Err(RhythmboxParseError::MalformedXml { .. })
            ));
        }

        let literal_boundaries = "\t\n\r\u{20}\u{d7ff}\u{e000}\u{fffd}\u{10000}\u{10ffff}";
        let literal_playlists = format!(
            "<rhythmdb-playlists><playlist name=\"legal\" type=\"automatic\"><equals prop=\"genre\"><![CDATA[{literal_boundaries}]]></equals></playlist></rhythmdb-playlists>"
        );
        assert!(parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(literal_playlists.as_bytes()),
            RhythmboxImportLimits::default(),
        )
        .is_ok());

        let referenced_boundaries = br#"<rhythmdb-playlists><playlist name="legal" type="automatic"><equals prop="genre">&#x9;&#xA;&#xD;&#x20;&#xD7FF;&#xE000;&#xFFFD;&#x10000;&#x10FFFF;</equals></playlist></rhythmdb-playlists>"#;
        assert!(parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(referenced_boundaries),
            RhythmboxImportLimits::default(),
        )
        .is_ok());
    }

    #[test]
    fn accepts_only_utf8_xml_10_declarations() {
        let valid =
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><rhythmdb version="2.0"/>"#;
        assert!(parse_rhythmbox_documents(valid, None, RhythmboxImportLimits::default()).is_ok());

        let invalid = br#"<?xml version="1.1"?><rhythmdb version="2.0"/>"#;
        assert!(matches!(
            parse_rhythmbox_documents(invalid, None, RhythmboxImportLimits::default()),
            Err(RhythmboxParseError::InvalidDeclaration { .. })
        ));
    }

    #[test]
    fn enforces_input_text_depth_element_track_and_issue_limits() {
        let base = RhythmboxImportLimits::default();

        let mut limits = base;
        limits.max_rhythmdb_bytes = 3;
        assert_limit(
            parse_rhythmbox_documents(b"<rhythmdb/>", None, limits),
            RhythmboxLimit::InputBytes,
        );

        limits = base;
        limits.max_text_bytes = 3;
        assert_limit(
            parse_rhythmbox_documents(&rhythmdb(&song(&local_uri("long"), "")), None, limits),
            RhythmboxLimit::TextBytes,
        );

        limits = base;
        limits.max_text_bytes = "rhythmdb".len();
        assert_limit(
            parse_rhythmbox_documents(&rhythmdb("<ignoredxx/>"), None, limits),
            RhythmboxLimit::TextBytes,
        );

        limits = base;
        limits.max_xml_depth = 2;
        assert_limit(
            parse_rhythmbox_documents(
                &rhythmdb("<entry type=\"iradio\"><nested/></entry>"),
                None,
                limits,
            ),
            RhythmboxLimit::XmlDepth,
        );

        limits = base;
        limits.max_elements_per_document = 1;
        assert_limit(
            parse_rhythmbox_documents(&rhythmdb("<entry type=\"iradio\"/>"), None, limits),
            RhythmboxLimit::Elements,
        );

        limits = base;
        limits.max_tracks = 1;
        assert_limit(
            parse_rhythmbox_documents(
                &rhythmdb(&format!(
                    "{}{}",
                    song(&local_uri("music/a.flac"), ""),
                    song(&local_uri("music/b.flac"), "")
                )),
                None,
                limits,
            ),
            RhythmboxLimit::Tracks,
        );

        limits = base;
        limits.max_issues = 0;
        assert_limit(
            parse_rhythmbox_documents(&rhythmdb("<entry type=\"song\"/>"), None, limits),
            RhythmboxLimit::Issues,
        );

        limits = base;
        limits.max_retained_text_bytes = 1;
        assert_limit(
            parse_rhythmbox_documents(
                &rhythmdb(&song(&local_uri("music/a.flac"), "")),
                None,
                limits,
            ),
            RhythmboxLimit::RetainedTextBytes,
        );
    }

    #[test]
    fn validates_rating_play_count_and_last_played() {
        for (fields, expected) in [
            ("<rating>NaN</rating>", RhythmboxNumericField::Rating),
            ("<rating>5.01</rating>", RhythmboxNumericField::Rating),
            (
                "<play-count>-1</play-count>",
                RhythmboxNumericField::PlayCount,
            ),
            (
                "<play-count>2147483648</play-count>",
                RhythmboxNumericField::PlayCount,
            ),
            (
                "<last-played>9223372036854776</last-played>",
                RhythmboxNumericField::LastPlayed,
            ),
        ] {
            let parsed = parse_db(&song(&local_uri("music/a.flac"), fields)).unwrap();
            assert_eq!(parsed.tracks.len(), 1);
            assert_eq!(
                parsed.issues.as_slice(),
                &[RhythmboxImportIssue {
                    document: RhythmboxDocument::RhythmDb,
                    item_ordinal: 1,
                    entry_ordinal: None,
                    kind: RhythmboxImportIssueKind::InvalidNumeric(expected),
                }]
            );
        }

        let zero = parse_db(&song(
            &local_uri("music/a.flac"),
            "<rating>-0</rating><play-count>0</play-count><last-played>0</last-played>",
        ))
        .unwrap();
        assert_eq!(
            zero.tracks[0].rating.unwrap().value().to_bits(),
            0.0f64.to_bits()
        );
    }

    #[test]
    fn invalid_numeric_fields_are_inert_without_discarding_other_song_or_playlist_data() {
        let location = local_uri("music/mixed.flac");
        let database = rhythmdb(&song(
            &location,
            "<rating>not-a-rating</rating><play-count>17</play-count><last-played>-1</last-played>",
        ));
        let playlists = format!(
            "<rhythmdb-playlists><playlist name=\"Mixed\" type=\"static\"><location>{location}</location></playlist></rhythmdb-playlists>"
        );
        let parsed = parse_rhythmbox_documents(
            &database,
            Some(playlists.as_bytes()),
            RhythmboxImportLimits::default(),
        )
        .unwrap();

        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.tracks[0].rating, None);
        assert_eq!(parsed.tracks[0].play_count, Some(17));
        assert_eq!(parsed.tracks[0].last_played_unix_seconds, None);
        assert_eq!(parsed.issues.len(), 2);
        assert_eq!(
            parsed.issues[0].kind,
            RhythmboxImportIssueKind::InvalidNumeric(RhythmboxNumericField::Rating)
        );
        assert_eq!(
            parsed.issues[1].kind,
            RhythmboxImportIssueKind::InvalidNumeric(RhythmboxNumericField::LastPlayed)
        );
        let RhythmboxPlaylistKind::Static(entries) = &parsed.playlists[0].kind else {
            panic!("expected retained static playlist");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].location, Some(parsed.tracks[0].location.clone()));
    }

    #[test]
    fn last_played_requires_a_chrono_representable_utc_millisecond() {
        let maximum = u64::try_from(DateTime::<Utc>::MAX_UTC.timestamp()).unwrap();
        let accepted = parse_db(&song(
            &local_uri("music/max.flac"),
            &format!("<last-played>{maximum}</last-played>"),
        ))
        .unwrap();
        assert_eq!(accepted.tracks[0].last_played_unix_seconds, Some(maximum));
        assert!(accepted.issues.is_empty());

        let rejected = parse_db(&song(
            &local_uri("music/beyond.flac"),
            &format!("<last-played>{}</last-played>", maximum + 1),
        ))
        .unwrap();
        assert_eq!(rejected.tracks[0].last_played_unix_seconds, None);
        assert_eq!(
            rejected.issues[0].kind,
            RhythmboxImportIssueKind::InvalidNumeric(RhythmboxNumericField::LastPlayed)
        );
    }

    #[test]
    fn local_file_uri_validation_is_typed_and_redacted() {
        let entries = [
            song("https://example.invalid/a", ""),
            song("file://server/share/a", ""),
            song(&format!("{}?query", local_uri("music/a")), ""),
            song(&format!("{}#fragment", local_uri("music/a")), ""),
            song(&local_uri("music/%00bad"), ""),
            song(&local_uri("music/%GGbad"), ""),
            song(&local_uri("music/%2e%2E/private"), ""),
            song(&local_uri("music/%2e%2e%2Fprivate"), ""),
        ]
        .concat();
        let parsed = parse_db(&entries).unwrap();
        assert!(parsed.tracks.is_empty());
        assert_eq!(parsed.issues.len(), 8);
        assert_eq!(
            parsed.issues[0].kind,
            RhythmboxImportIssueKind::InvalidLocation(RhythmboxLocationIssue::NonFileScheme)
        );
        assert_eq!(
            parsed.issues[1].kind,
            RhythmboxImportIssueKind::InvalidLocation(RhythmboxLocationIssue::RemoteAuthority)
        );
        assert_eq!(
            parsed.issues[6].kind,
            RhythmboxImportIssueKind::InvalidLocation(RhythmboxLocationIssue::ParentTraversal)
        );
        assert_eq!(
            parsed.issues[7].kind,
            RhythmboxImportIssueKind::InvalidLocation(RhythmboxLocationIssue::ParentTraversal)
        );
        let debug = format!("{parsed:?}");
        assert!(!debug.contains("music"));
        assert!(!debug.contains("server"));
    }

    #[test]
    fn static_and_queue_entries_preserve_order_duplicates_and_placeholders() {
        let b = local_uri("music/b.flac");
        let a = local_uri("music/a.flac");
        let q = local_uri("music/q.flac");
        let playlists = format!(
            r#"
            <rhythmdb-playlists>
              <playlist name="Ordered" type="static">
                <location>{b}</location>
                <location>{a}</location>
                <location>{b}</location>
                <location>https://example.invalid/nope</location>
              </playlist>
              <playlist name="Queue" type="queue">
                <location>{q}</location>
              </playlist>
            </rhythmdb-playlists>
        "#
        );
        let parsed = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(playlists.as_bytes()),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        let RhythmboxPlaylistKind::Static(entries) = &parsed.playlists[0].kind else {
            panic!("expected static playlist");
        };
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].location, entries[2].location);
        assert_eq!(
            entries[1].location.as_ref().unwrap().as_path(),
            local_path("music/a.flac")
        );
        assert!(entries[3].location.is_none());
        assert!(matches!(
            parsed.playlists[1].kind,
            RhythmboxPlaylistKind::Queue(_)
        ));
    }

    #[test]
    fn automatic_queries_preserve_unknown_structure_for_whitelist_translation() {
        let playlists = br#"
          <rhythmdb-playlists>
            <playlist name="Recent" type="automatic" limit-count="25" sort-key="LastPlayed">
              <conjunction>
                <equals prop="type">song</equals>
                <subquery><disjunction><like prop="genre">Rock</like></disjunction></subquery>
              </conjunction>
            </playlist>
          </rhythmdb-playlists>
        "#;
        let parsed = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(playlists),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        let RhythmboxPlaylistKind::Automatic(automatic) = &parsed.playlists[0].kind else {
            panic!("expected automatic playlist");
        };
        assert_eq!(automatic.attributes.len(), 2);
        assert_eq!(automatic.query[0].element, "conjunction");
        assert_eq!(automatic.query[0].children[0].element, "equals");
        assert_eq!(automatic.query[0].children[0].text, "song");
        assert_eq!(automatic.query[0].children[1].children.len(), 1);
    }

    #[test]
    fn enforces_playlist_entry_query_node_and_playlist_count_limits() {
        let base = RhythmboxImportLimits::default();
        let two_playlists = br#"<rhythmdb-playlists><playlist name="a" type="static"/><playlist name="b" type="static"/></rhythmdb-playlists>"#;
        let mut limits = base;
        limits.max_playlists = 1;
        assert_limit(
            parse_rhythmbox_documents(b"<rhythmdb version=\"2.0\"/>", Some(two_playlists), limits),
            RhythmboxLimit::Playlists,
        );

        let two_entries = format!(
            "<rhythmdb-playlists><playlist name=\"a\" type=\"static\"><location>{}</location><location>{}</location></playlist></rhythmdb-playlists>",
            local_uri("a"),
            local_uri("b")
        );
        limits = base;
        limits.max_playlist_entries = 1;
        assert_limit(
            parse_rhythmbox_documents(
                b"<rhythmdb version=\"2.0\"/>",
                Some(two_entries.as_bytes()),
                limits,
            ),
            RhythmboxLimit::PlaylistEntries,
        );

        let nested_query = br#"<rhythmdb-playlists><playlist name="a" type="automatic"><conjunction><equals prop="type">song</equals></conjunction></playlist></rhythmdb-playlists>"#;
        limits = base;
        limits.max_automatic_nodes = 1;
        assert_limit(
            parse_rhythmbox_documents(b"<rhythmdb version=\"2.0\"/>", Some(nested_query), limits),
            RhythmboxLimit::AutomaticNodes,
        );
    }

    #[test]
    fn semantic_digest_ignores_formatting_attribute_order_and_track_order() {
        let db_a = rhythmdb(&format!(
            "{}{}",
            song(&local_uri("music/a.flac"), "<play-count>1</play-count>"),
            song(&local_uri("music/b.flac"), "<rating>4</rating>")
        ));
        let db_b = format!(
            r#"
          <rhythmdb version="2.0">
            <entry type="song"><rating>4.0</rating><location>{}</location></entry>
            <entry type="song"><location>{}</location><play-count>1</play-count></entry>
          </rhythmdb>
        "#,
            local_uri("music/b.flac"),
            local_uri("music/a.flac")
        );
        let playlists_a = br#"<rhythmdb-playlists><playlist type="automatic" name="x" z="2" a="1"><equals prop="genre" mode="x">Rock</equals></playlist></rhythmdb-playlists>"#;
        let playlists_b = br#"
          <rhythmdb-playlists>
            <playlist a="1" name="x" type="automatic" z="2">
              <equals mode="x" prop="genre">Rock</equals>
            </playlist>
          </rhythmdb-playlists>
        "#;
        let first =
            parse_rhythmbox_documents(&db_a, Some(playlists_a), RhythmboxImportLimits::default())
                .unwrap();
        let second = parse_rhythmbox_documents(
            db_b.as_bytes(),
            Some(playlists_b),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        assert_eq!(first.semantic_digest, second.semantic_digest);
    }

    #[test]
    fn semantic_digest_ignores_only_valid_membership_inert_playlist_settings() {
        let query = "<conjunction><equals prop=\"type\">song</equals><subquery><conjunction><equals prop=\"play-count\">1</equals></conjunction></subquery></conjunction>";
        let playlists = |attributes: &str| {
            format!(
                "<rhythmdb-playlists><playlist name=\"x\" type=\"automatic\" {attributes}>{query}</playlist></rhythmdb-playlists>"
            )
        };
        let parse = |attributes: &str| {
            let playlists = playlists(attributes);
            parse_rhythmbox_documents(
                b"<rhythmdb version=\"2.0\"/>",
                Some(playlists.as_bytes()),
                RhythmboxImportLimits::default(),
            )
            .unwrap()
            .semantic_digest
        };

        let first =
            parse("show-browser=\"true\" browser-position=\"180\" search-type=\"search-match\"");
        let changed_presentation =
            parse("show-browser=\"false\" browser-position=\"420\" search-type=\"search-title\"");
        assert_eq!(first, changed_presentation);
        assert_ne!(first, parse("show-browser=\"1\""));
        assert_ne!(first, parse("future-membership-setting=\"enabled\""));
    }

    #[test]
    fn semantic_digest_is_length_framed_and_order_sensitive_where_required() {
        let one = parse_db(&song(&local_uri("ab"), "<play-count>1</play-count>")).unwrap();
        let two = parse_db(&song(&local_uri("a"), "<play-count>11</play-count>")).unwrap();
        assert_ne!(one.semantic_digest, two.semantic_digest);

        let ordered = format!(
            "<rhythmdb-playlists><playlist name=\"x\" type=\"static\"><location>{}</location><location>{}</location></playlist></rhythmdb-playlists>",
            local_uri("a"),
            local_uri("b")
        );
        let reversed = format!(
            "<rhythmdb-playlists><playlist name=\"x\" type=\"static\"><location>{}</location><location>{}</location></playlist></rhythmdb-playlists>",
            local_uri("b"),
            local_uri("a")
        );
        let first = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(ordered.as_bytes()),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        let second = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(reversed.as_bytes()),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        assert_ne!(first.semantic_digest, second.semantic_digest);
        assert_eq!(first.semantic_digest.as_bytes().len(), 32);

        let invalid_rating =
            parse_db(&song(&local_uri("same.flac"), "<rating>invalid</rating>")).unwrap();
        let invalid_count = parse_db(&song(
            &local_uri("same.flac"),
            "<play-count>invalid</play-count>",
        ))
        .unwrap();
        assert_ne!(
            invalid_rating.semantic_digest, invalid_count.semantic_digest,
            "the content-free numeric issue field remains part of migration semantics"
        );
    }

    #[allow(clippy::needless_pass_by_value)] // Test call sites intentionally pass temporary results.
    fn assert_limit(
        result: Result<RhythmboxImport, RhythmboxParseError>,
        expected: RhythmboxLimit,
    ) {
        assert!(matches!(
            result,
            Err(RhythmboxParseError::LimitExceeded { limit, .. }) if limit == expected
        ));
    }
}
