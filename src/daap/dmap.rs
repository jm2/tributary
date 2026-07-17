//! DMAP (Digital Media Access Protocol) binary TLV parser.
//!
//! DAAP responses use a proprietary binary encoding where every node is:
//!
//! | Bytes | Meaning                                    |
//! |-------|--------------------------------------------|
//! | 0–3   | 4-byte ASCII tag code (e.g., `minm`)       |
//! | 4–7   | 4-byte big-endian `u32` content length      |
//! | 8+    | Content (container, string, or integer)     |
//!
//! This module parses only the subset of tags that Tributary needs.
//! Unknown tags are stored as raw bytes and silently skipped.

use nom::bytes::complete::take;
use nom::multi::many0;
use nom::number::complete::be_u32;
use nom::IResult;
use nom::Parser;

use crate::architecture::error::BackendError;

/// Maximum container nesting depth accepted by the parser.
///
/// DMAP containers can nest (`mlcl` → `mlit` → …), and the parser recurses
/// once per level.  A crafted DAAP response can request arbitrarily deep
/// nesting with only ~8 bytes per level, which would otherwise overflow the
/// worker-thread stack — an uncatchable abort that crashes the whole app.
/// Real DAAP responses nest only a handful of levels, so 32 is generous.
const MAX_DEPTH: usize = 32;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single DMAP tag-length-value node.
#[derive(Debug, Clone)]
pub struct DmapNode {
    /// 4-byte ASCII tag code.
    pub tag: [u8; 4],
    /// Parsed value.
    pub data: DmapValue,
}

/// The value carried by a [`DmapNode`].
#[derive(Debug, Clone)]
pub enum DmapValue {
    Container(Vec<DmapNode>),
    String(String),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    /// Fallback for unknown or unneeded tags.
    Raw(Vec<u8>),
}

// ---------------------------------------------------------------------------
// Tag classification
// ---------------------------------------------------------------------------

/// Internal classification of how a tag's content should be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmapType {
    Container,
    String,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    Raw,
}

/// Classify a 4-byte tag into its expected content type.
///
/// Only the tags Tributary actually uses are classified; everything
/// else falls through to [`DmapType::Raw`].
fn tag_type(tag: &[u8; 4]) -> DmapType {
    match tag {
        // Containers
        b"msrv" => DmapType::Container, // server-info response
        b"mlog" => DmapType::Container, // login response
        b"mupd" => DmapType::Container, // update response
        b"avdb" => DmapType::Container, // databases response
        b"adbs" => DmapType::Container, // database songs response
        b"mlcl" => DmapType::Container, // listing container
        b"mlit" => DmapType::Container, // listing item

        // U32 integers
        b"mlid" => DmapType::U32, // session-id
        b"musr" => DmapType::U32, // revision-number
        b"miid" => DmapType::U32, // item id
        b"astm" => DmapType::U32, // song time (ms)
        b"assr" => DmapType::U32, // song sample rate
        b"mstt" => DmapType::U32, // status code
        b"mimc" => DmapType::U32, // item count
        b"msau" => DmapType::U8,  // authentication method (0 = none)
        b"mikd" => DmapType::I8,  // media item kind

        // I64 integers
        b"mper" => DmapType::I64, // persistent id

        // U16 integers
        b"astn" => DmapType::U16, // song track number
        b"asdn" => DmapType::U16, // song disc number
        b"asyr" => DmapType::U16, // song year
        b"asbr" => DmapType::U16, // song bitrate

        // U32 integers (dates — Unix timestamps in practice)
        b"asdm" => DmapType::U32, // song date modified (Unix timestamp)

        // Strings
        b"minm" => DmapType::String, // item name
        b"asar" => DmapType::String, // song artist
        b"asal" => DmapType::String, // song album
        b"asgn" => DmapType::String, // song genre
        b"asfm" => DmapType::String, // song format

        _ => DmapType::Raw,
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a byte buffer into a list of DMAP nodes.
///
/// This is the top-level entry point. It consumes the entire input and
/// returns `BackendError::ParseError` on malformed data.
pub fn parse_dmap(input: &[u8]) -> Result<Vec<DmapNode>, BackendError> {
    if input.is_empty() {
        return Ok(Vec::new());
    }

    match parse_nodes(input, 0) {
        Ok((remaining, nodes)) => {
            if !remaining.is_empty() {
                return Err(BackendError::ParseError {
                    message: format!(
                        "Malformed DMAP data: {} trailing bytes after parsing",
                        remaining.len()
                    ),
                    source: None,
                });
            }
            Ok(nodes)
        }
        Err(error) => {
            // `nom`'s Debug/Display output includes the remaining input slice.
            // A hostile response could therefore turn one parse failure into a
            // very large, attacker-controlled log/error message. Keep the
            // public diagnostic fixed and classify only the parser reason.
            let reason = match error {
                nom::Err::Failure(error) if error.code == nom::error::ErrorKind::TooLarge => {
                    "container nesting exceeds the supported limit"
                }
                nom::Err::Failure(error) if error.code == nom::error::ErrorKind::Eof => {
                    "truncated nested container"
                }
                nom::Err::Failure(error) if error.code == nom::error::ErrorKind::Verify => {
                    "known scalar has an invalid width"
                }
                nom::Err::Error(_) | nom::Err::Failure(_) | nom::Err::Incomplete(_) => {
                    "invalid tag-length framing"
                }
            };
            Err(BackendError::ParseError {
                message: format!("Malformed DMAP data: {reason}"),
                source: None,
            })
        }
    }
}

/// Parse zero or more consecutive DMAP nodes from the input.
fn parse_nodes(input: &[u8], depth: usize) -> IResult<&[u8], Vec<DmapNode>> {
    many0(move |i| parse_single_node(i, depth)).parse(input)
}

/// Parse a single DMAP TLV node.
fn parse_single_node(input: &[u8], depth: usize) -> IResult<&[u8], DmapNode> {
    // Bound recursion depth.  Surface an unrecoverable `Failure` (not a
    // recoverable `Error`, which `many0` would silently swallow) so the
    // whole parse aborts with a clean `ParseError` instead of recursing
    // until the thread stack overflows.
    if depth > MAX_DEPTH {
        return Err(nom::Err::Failure(nom::error::Error::new(
            input,
            nom::error::ErrorKind::TooLarge,
        )));
    }

    // 4-byte tag
    let (input, tag_bytes) = take(4usize)(input)?;
    let mut tag = [0u8; 4];
    tag.copy_from_slice(tag_bytes);

    // 4-byte big-endian content length
    let (input, length) = be_u32(input)?;

    // Take exactly `length` bytes of content
    let (input, content) = take(length as usize)(input)?;

    let data = decode_value(&tag, content, depth)?;

    Ok((input, DmapNode { tag, data }))
}

/// Decode the content bytes of a node according to its tag type.
///
/// Returns `Err` for an unrecoverable depth-limit `Failure` or for malformed
/// framing inside a known container. Silently accepting the successfully
/// parsed prefix of a truncated container would let a response reach the
/// client as an empty/partial listing.
fn decode_value<'a>(
    tag: &[u8; 4],
    content: &'a [u8],
    depth: usize,
) -> Result<DmapValue, nom::Err<nom::error::Error<&'a [u8]>>> {
    let value = match tag_type(tag) {
        DmapType::Container => match parse_nodes(content, depth + 1) {
            Ok(([], children)) => DmapValue::Container(children),
            Ok((remaining, _)) => {
                return Err(nom::Err::Failure(nom::error::Error::new(
                    remaining,
                    nom::error::ErrorKind::Eof,
                )))
            }
            // A depth-limit breach is an unrecoverable `Failure` — pass it up
            // so the whole parse aborts instead of stack-overflowing.
            Err(e @ nom::Err::Failure(_)) => return Err(e),
            // Known containers must be structurally complete. Promote a
            // recoverable nested parse failure so the outer parser cannot
            // publish a partial response.
            Err(nom::Err::Error(error)) => return Err(nom::Err::Failure(error)),
            Err(nom::Err::Incomplete(needed)) => return Err(nom::Err::Incomplete(needed)),
        },
        DmapType::String => DmapValue::String(String::from_utf8_lossy(content).into_owned()),
        DmapType::U8 => DmapValue::U8(u8::from_be_bytes(exact_scalar_bytes(content)?)),
        DmapType::U16 => DmapValue::U16(u16::from_be_bytes(exact_scalar_bytes(content)?)),
        DmapType::U32 => DmapValue::U32(u32::from_be_bytes(exact_scalar_bytes(content)?)),
        DmapType::U64 => DmapValue::U64(u64::from_be_bytes(exact_scalar_bytes(content)?)),
        DmapType::I8 => DmapValue::I8(i8::from_be_bytes(exact_scalar_bytes(content)?)),
        DmapType::I16 => DmapValue::I16(i16::from_be_bytes(exact_scalar_bytes(content)?)),
        DmapType::I32 => DmapValue::I32(i32::from_be_bytes(exact_scalar_bytes(content)?)),
        DmapType::I64 => DmapValue::I64(i64::from_be_bytes(exact_scalar_bytes(content)?)),
        DmapType::Raw => DmapValue::Raw(content.to_vec()),
    };
    Ok(value)
}

/// Copy one known integer payload only when its TLV length is exact.
///
/// Integer decoders normally accept a valid prefix and return the remaining
/// bytes. For protocol fields such as `mstt`, that would let both short and
/// overlong status values bypass validation. Promote every width mismatch to
/// a fatal, fixed-category parse failure instead.
fn exact_scalar_bytes<const N: usize>(
    content: &[u8],
) -> Result<[u8; N], nom::Err<nom::error::Error<&[u8]>>> {
    content.try_into().map_err(|_| {
        nom::Err::Failure(nom::error::Error::new(
            content,
            nom::error::ErrorKind::Verify,
        ))
    })
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Find the first node with the given 4-byte tag.
pub fn find_node<'a>(nodes: &'a [DmapNode], tag: &[u8; 4]) -> Option<&'a DmapNode> {
    nodes.iter().find(|n| &n.tag == tag)
}

/// Extract a `String` value from the first node matching the tag.
pub fn find_string(nodes: &[DmapNode], tag: &[u8; 4]) -> Option<String> {
    find_node(nodes, tag).and_then(|n| match &n.data {
        DmapValue::String(s) => Some(s.clone()),
        _ => None,
    })
}

/// Extract a `u32` value from the first node matching the tag.
///
/// Also handles `U8` and `U16` values by widening them.
pub fn find_u32(nodes: &[DmapNode], tag: &[u8; 4]) -> Option<u32> {
    find_node(nodes, tag).and_then(|n| match &n.data {
        DmapValue::U32(v) => Some(*v),
        DmapValue::U16(v) => Some(u32::from(*v)),
        DmapValue::U8(v) => Some(u32::from(*v)),
        _ => None,
    })
}

/// Extract a `u8` value from the first node matching the tag.
pub fn find_u8(nodes: &[DmapNode], tag: &[u8; 4]) -> Option<u8> {
    find_node(nodes, tag).and_then(|n| match &n.data {
        DmapValue::U8(v) => Some(*v),
        _ => None,
    })
}

/// Extract a `u16` value from the first node matching the tag.
///
/// Also handles `U8` values by widening.
pub fn find_u16(nodes: &[DmapNode], tag: &[u8; 4]) -> Option<u16> {
    find_node(nodes, tag).and_then(|n| match &n.data {
        DmapValue::U16(v) => Some(*v),
        DmapValue::U8(v) => Some(u16::from(*v)),
        _ => None,
    })
}

/// Return the children of all container nodes matching the given tag.
///
/// This is the primary way to iterate track listings — e.g., find all
/// `mlit` items inside an `mlcl` container.
pub fn find_containers<'a>(nodes: &'a [DmapNode], tag: &[u8; 4]) -> Vec<&'a [DmapNode]> {
    nodes
        .iter()
        .filter(|n| &n.tag == tag)
        .filter_map(|n| match &n.data {
            DmapValue::Container(children) => Some(children.as_slice()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a DMAP TLV blob from tag + content bytes.
    fn make_tlv(tag: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(tag);
        buf.extend_from_slice(&(content.len() as u32).to_be_bytes());
        buf.extend_from_slice(content);
        buf
    }

    /// Helper: build a u32 content (4 bytes big-endian).
    fn u32_bytes(v: u32) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    /// Helper: build a u16 content (2 bytes big-endian).
    fn u16_bytes(v: u16) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    #[test]
    fn test_parse_string_and_u32() {
        // Build: mlit container with minm="Test Song" and miid=42
        let minm = make_tlv(b"minm", b"Test Song");
        let miid = make_tlv(b"miid", &u32_bytes(42));

        let mut container_content = Vec::new();
        container_content.extend_from_slice(&minm);
        container_content.extend_from_slice(&miid);

        let blob = make_tlv(b"mlit", &container_content);

        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(nodes.len(), 1);
        assert_eq!(&nodes[0].tag, b"mlit");

        let children = match &nodes[0].data {
            DmapValue::Container(c) => c.as_slice(),
            _ => panic!("expected container"),
        };

        assert_eq!(
            find_string(children, b"minm"),
            Some("Test Song".to_string())
        );
        assert_eq!(find_u32(children, b"miid"), Some(42));
    }

    #[test]
    fn test_find_containers_multiple_mlit() {
        // Build: mlcl container with two mlit children
        let mlit1_content = make_tlv(b"minm", b"Song A");
        let mlit2_content = make_tlv(b"minm", b"Song B");

        let mlit1 = make_tlv(b"mlit", &mlit1_content);
        let mlit2 = make_tlv(b"mlit", &mlit2_content);

        let mut mlcl_content = Vec::new();
        mlcl_content.extend_from_slice(&mlit1);
        mlcl_content.extend_from_slice(&mlit2);

        let blob = make_tlv(b"mlcl", &mlcl_content);

        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(nodes.len(), 1);

        let mlcl_children = match &nodes[0].data {
            DmapValue::Container(c) => c.as_slice(),
            _ => panic!("expected container"),
        };

        let items = find_containers(mlcl_children, b"mlit");
        assert_eq!(items.len(), 2);
        assert_eq!(find_string(items[0], b"minm"), Some("Song A".to_string()));
        assert_eq!(find_string(items[1], b"minm"), Some("Song B".to_string()));
    }

    #[test]
    fn test_unknown_tag_stored_as_raw() {
        let blob = make_tlv(b"zzzz", &[0xDE, 0xAD, 0xBE, 0xEF]);

        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(nodes.len(), 1);
        assert_eq!(&nodes[0].tag, b"zzzz");

        match &nodes[0].data {
            DmapValue::Raw(bytes) => assert_eq!(bytes, &[0xDE, 0xAD, 0xBE, 0xEF]),
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    #[test]
    fn known_integer_tags_require_their_exact_scalar_width() {
        // One representative tag for every integer type used by Tributary.
        // `mstt` is included explicitly because accepting it as Raw or
        // parsing a four-byte prefix would bypass response-status handling.
        for (tag, width) in [
            (b"msau", 1_usize),
            (b"mikd", 1),
            (b"astn", 2),
            (b"mstt", 4),
            (b"mper", 8),
        ] {
            parse_dmap(&make_tlv(tag, &vec![0; width]))
                .unwrap_or_else(|error| panic!("{tag:?}: exact width must parse: {error}"));

            for malformed_width in [width - 1, width + 1] {
                let error = parse_dmap(&make_tlv(tag, &vec![0; malformed_width]))
                    .expect_err("known integer with malformed width must fail");
                assert!(
                    error.to_string().contains("invalid width"),
                    "{tag:?}/{malformed_width}: unexpected error: {error}"
                );
            }
        }
    }

    #[test]
    fn test_truncated_input_returns_error() {
        // Tag says content is 100 bytes, but we only provide 2.
        let mut blob = Vec::new();
        blob.extend_from_slice(b"minm");
        blob.extend_from_slice(&100u32.to_be_bytes());
        blob.extend_from_slice(&[0x41, 0x42]); // only 2 bytes

        let result = parse_dmap(&blob);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Malformed DMAP data"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_empty_input() {
        let nodes = parse_dmap(&[]).expect("empty input should succeed");
        assert!(nodes.is_empty());
    }

    #[test]
    fn test_u16_extraction() {
        let blob = make_tlv(b"astn", &u16_bytes(7));

        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(find_u16(&nodes, b"astn"), Some(7));
        // find_u32 should also work via widening
        assert_eq!(find_u32(&nodes, b"astn"), Some(7));
    }

    #[test]
    fn test_multiple_top_level_nodes() {
        let node1 = make_tlv(b"minm", b"Hello");
        let node2 = make_tlv(b"miid", &u32_bytes(99));

        let mut blob = Vec::new();
        blob.extend_from_slice(&node1);
        blob.extend_from_slice(&node2);

        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(nodes.len(), 2);
        assert_eq!(find_string(&nodes, b"minm"), Some("Hello".to_string()));
        assert_eq!(find_u32(&nodes, b"miid"), Some(99));
    }

    #[test]
    fn test_i8_extraction() {
        let blob = make_tlv(b"mikd", &[0xFF]); // -1 as i8
        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(nodes.len(), 1);
        match &nodes[0].data {
            DmapValue::I8(v) => assert_eq!(*v, -1),
            other => panic!("expected I8, got {other:?}"),
        }
    }

    #[test]
    fn test_i64_extraction() {
        let blob = make_tlv(b"mper", &42i64.to_be_bytes());
        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(nodes.len(), 1);
        match &nodes[0].data {
            DmapValue::I64(v) => assert_eq!(*v, 42),
            other => panic!("expected I64, got {other:?}"),
        }
    }

    #[test]
    fn test_zero_length_string() {
        let blob = make_tlv(b"minm", b"");
        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(find_string(&nodes, b"minm"), Some(String::new()));
    }

    #[test]
    fn test_nested_containers() {
        // Build: msrv → mlcl → mlit → minm="Deep"
        let minm = make_tlv(b"minm", b"Deep");
        let mlit = make_tlv(b"mlit", &minm);
        let mlcl = make_tlv(b"mlcl", &mlit);
        let msrv = make_tlv(b"msrv", &mlcl);

        let nodes = parse_dmap(&msrv).expect("parse should succeed");
        assert_eq!(nodes.len(), 1);

        // Navigate: msrv → mlcl → mlit → minm
        let DmapValue::Container(msrv_children) = &nodes[0].data else {
            panic!("expected container");
        };
        let DmapValue::Container(mlcl_children) = &msrv_children[0].data else {
            panic!("expected container");
        };
        let items = find_containers(mlcl_children, b"mlit");
        assert_eq!(items.len(), 1);
        assert_eq!(find_string(items[0], b"minm"), Some("Deep".to_string()));
    }

    #[test]
    fn test_truncated_child_inside_known_container_is_rejected() {
        // The outer `adbs` length is valid, but its `mlcl` child claims a
        // payload longer than the bytes that remain. Accepting the parsed
        // prefix would turn this malformed response into an empty catalogue.
        let mut truncated_child = Vec::new();
        truncated_child.extend_from_slice(b"mlcl");
        truncated_child.extend_from_slice(&16_u32.to_be_bytes());
        truncated_child.extend_from_slice(b"short");
        let blob = make_tlv(b"adbs", &truncated_child);

        let error = parse_dmap(&blob).expect_err("truncated nested container must fail closed");
        assert!(
            error.to_string().contains("Malformed DMAP data"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_find_string_missing_tag() {
        let blob = make_tlv(b"minm", b"Hello");
        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(find_string(&nodes, b"miid"), None);
    }

    #[test]
    fn test_find_u32_missing_tag() {
        let blob = make_tlv(b"miid", &u32_bytes(42));
        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(find_u32(&nodes, b"minm"), None);
    }

    #[test]
    fn test_partial_header_returns_error() {
        // Only 6 bytes — not enough for a full 8-byte TLV header.
        let result = parse_dmap(&[b'm', b'i', b'n', b'm', 0, 0]);
        assert!(result.is_err());
    }

    #[test]
    fn test_large_u32_value() {
        let blob = make_tlv(b"miid", &u32_bytes(u32::MAX));
        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(find_u32(&nodes, b"miid"), Some(u32::MAX));
    }

    #[test]
    fn test_utf8_string() {
        let blob = make_tlv(b"minm", "日本語テスト".as_bytes());
        let nodes = parse_dmap(&blob).expect("parse should succeed");
        assert_eq!(
            find_string(&nodes, b"minm"),
            Some("日本語テスト".to_string())
        );
    }

    #[test]
    fn test_deeply_nested_containers_rejected() {
        // Build many nested `mlit` containers — far deeper than MAX_DEPTH.
        // Without the depth guard this would recurse until the worker-thread
        // stack overflows (an uncatchable abort); with it, parsing must
        // return a clean `ParseError`.
        let mut blob = make_tlv(b"mlit", b"");
        for _ in 0..(MAX_DEPTH + 5) {
            blob = make_tlv(b"mlit", &blob);
        }

        let result = parse_dmap(&blob);
        assert!(result.is_err(), "deep nesting should be rejected");
    }

    #[test]
    fn test_nesting_within_limit_accepted() {
        // A handful of nested containers (well under MAX_DEPTH) still parses.
        let mut blob = make_tlv(b"minm", b"OK");
        for _ in 0..5 {
            blob = make_tlv(b"mlit", &blob);
        }
        let nodes = parse_dmap(&blob).expect("shallow nesting should parse");
        assert_eq!(nodes.len(), 1);
        assert_eq!(&nodes[0].tag, b"mlit");
    }
}
