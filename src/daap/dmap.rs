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
use nom::number::complete::{be_i16, be_i32, be_i64, be_i8, be_u16, be_u32, be_u64, be_u8};
use nom::IResult;
use nom::Parser;

use crate::architecture::error::BackendError;

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

    match parse_nodes(input) {
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
        Err(e) => Err(BackendError::ParseError {
            message: format!("Malformed DMAP data: {e}"),
            source: None,
        }),
    }
}

/// Parse zero or more consecutive DMAP nodes from the input.
fn parse_nodes(input: &[u8]) -> IResult<&[u8], Vec<DmapNode>> {
    many0(parse_single_node).parse(input)
}

/// Parse a single DMAP TLV node.
fn parse_single_node(input: &[u8]) -> IResult<&[u8], DmapNode> {
    // 4-byte tag
    let (input, tag_bytes) = take(4usize)(input)?;
    let mut tag = [0u8; 4];
    tag.copy_from_slice(tag_bytes);

    // 4-byte big-endian content length
    let (input, length) = be_u32(input)?;

    // Take exactly `length` bytes of content
    let (input, content) = take(length as usize)(input)?;

    let data = decode_value(&tag, content);

    Ok((input, DmapNode { tag, data }))
}

/// Decode the content bytes of a node according to its tag type.
fn decode_value(tag: &[u8; 4], content: &[u8]) -> DmapValue {
    match tag_type(tag) {
        DmapType::Container => {
            match parse_nodes(content) {
                Ok((_, children)) => DmapValue::Container(children),
                // If container parsing fails, store as raw.
                Err(_) => DmapValue::Raw(content.to_vec()),
            }
        }
        DmapType::String => DmapValue::String(String::from_utf8_lossy(content).into_owned()),
        DmapType::U8 => match be_u8::<&[u8], nom::error::Error<&[u8]>>(content) {
            Ok((_, v)) => DmapValue::U8(v),
            Err(_) => DmapValue::Raw(content.to_vec()),
        },
        DmapType::U16 => match be_u16::<&[u8], nom::error::Error<&[u8]>>(content) {
            Ok((_, v)) => DmapValue::U16(v),
            Err(_) => DmapValue::Raw(content.to_vec()),
        },
        DmapType::U32 => match be_u32::<&[u8], nom::error::Error<&[u8]>>(content) {
            Ok((_, v)) => DmapValue::U32(v),
            Err(_) => DmapValue::Raw(content.to_vec()),
        },
        DmapType::U64 => match be_u64::<&[u8], nom::error::Error<&[u8]>>(content) {
            Ok((_, v)) => DmapValue::U64(v),
            Err(_) => DmapValue::Raw(content.to_vec()),
        },
        DmapType::I8 => match be_i8::<&[u8], nom::error::Error<&[u8]>>(content) {
            Ok((_, v)) => DmapValue::I8(v),
            Err(_) => DmapValue::Raw(content.to_vec()),
        },
        DmapType::I16 => match be_i16::<&[u8], nom::error::Error<&[u8]>>(content) {
            Ok((_, v)) => DmapValue::I16(v),
            Err(_) => DmapValue::Raw(content.to_vec()),
        },
        DmapType::I32 => match be_i32::<&[u8], nom::error::Error<&[u8]>>(content) {
            Ok((_, v)) => DmapValue::I32(v),
            Err(_) => DmapValue::Raw(content.to_vec()),
        },
        DmapType::I64 => match be_i64::<&[u8], nom::error::Error<&[u8]>>(content) {
            Ok((_, v)) => DmapValue::I64(v),
            Err(_) => DmapValue::Raw(content.to_vec()),
        },
        DmapType::Raw => DmapValue::Raw(content.to_vec()),
    }
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
}
