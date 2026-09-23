//! Pure request-shape parsing for the cast relay's HTTP server.
//!
//! These helpers hold no server state, so the library target (and the fuzz
//! harness built on it) can reach them without compiling `cast_http_server`.

use url::Url;

/// The audio extension of an upstream URL, if it has a recognised one.
///
/// A Plex part key ends in `/file.flac`; a Subsonic `stream.view` has no
/// extension at all, in which case the receiver falls back to its default and
/// there is nothing more we can say from the URL alone. This is only a
/// fallback: a typed resolved request's validated `MediaRepresentation`
/// takes precedence over URL sniffing.
///
/// The allow-list is deliberate: the ticket path is otherwise a bare UUID, and
/// only these fixed strings may ever be appended to it.
pub const PROTECTED_TICKET_AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "wav", "aac", "m4a", "aiff", "aif", "wma",
];

pub fn upstream_media_extension(url: &Url) -> Option<&'static str> {
    let last_segment = url.path_segments()?.next_back()?;
    let (_, extension) = last_segment.rsplit_once('.')?;
    PROTECTED_TICKET_AUDIO_EXTENSIONS
        .iter()
        .find(|known| known.eq_ignore_ascii_case(extension))
        .copied()
}

/// Parse an HTTP `Range` header value like `bytes=0-1023`.
///
/// Returns `Some((start, end))` for a valid single byte range,
/// `None` for unsupported multi-range or invalid values.
pub fn parse_range_header(header: &str, file_size: u64) -> Option<(u64, u64)> {
    // Empty files have no valid byte ranges.
    if file_size == 0 {
        return None;
    }

    let range = header.strip_prefix("bytes=")?;

    // Only support a single range (no multi-range).
    if range.contains(',') {
        return None;
    }

    let parts: Vec<&str> = range.splitn(2, '-').collect();
    if parts.len() != 2 {
        return None;
    }

    // Suffix range: bytes=-500 means last 500 bytes.
    if parts[0].is_empty() {
        let suffix_len: u64 = parts[1].parse().ok()?;
        if suffix_len == 0 {
            return None;
        }
        let start = file_size.saturating_sub(suffix_len);
        return Some((start, file_size - 1));
    }

    let start: u64 = parts[0].parse().ok()?;

    let end = if parts[1].is_empty() {
        file_size - 1
    } else {
        parts[1].parse::<u64>().ok()?.min(file_size - 1)
    };

    if start > end || start >= file_size {
        return None;
    }

    Some((start, end))
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{parse_range_header, upstream_media_extension};

    #[test]
    fn test_parse_range_full() {
        assert_eq!(parse_range_header("bytes=0-999", 1000), Some((0, 999)));
    }

    #[test]
    fn test_parse_range_open_end() {
        assert_eq!(parse_range_header("bytes=500-", 1000), Some((500, 999)));
    }

    #[test]
    fn test_parse_range_suffix() {
        assert_eq!(parse_range_header("bytes=-200", 1000), Some((800, 999)));
    }

    #[test]
    fn test_parse_range_invalid() {
        assert_eq!(parse_range_header("bytes=500-200", 1000), None);
    }

    #[test]
    fn test_parse_range_out_of_bounds() {
        assert_eq!(parse_range_header("bytes=2000-3000", 1000), None);
    }

    #[test]
    fn test_parse_range_multi_not_supported() {
        assert_eq!(parse_range_header("bytes=0-100,200-300", 1000), None);
    }

    #[test]
    fn test_parse_range_clamp_end() {
        // End beyond file size should be clamped.
        assert_eq!(parse_range_header("bytes=0-5000", 1000), Some((0, 999)));
    }

    #[test]
    fn test_parse_range_zero_size_file() {
        // Zero-size files must not cause u64 underflow.
        assert_eq!(parse_range_header("bytes=0-0", 0), None);
        assert_eq!(parse_range_header("bytes=0-", 0), None);
        assert_eq!(parse_range_header("bytes=-1", 0), None);
    }

    fn url(value: &str) -> Url {
        Url::parse(value).expect("test URL")
    }

    /// The Cast `content_type` is guessed from the URL the device is handed, so
    /// an extensionless ticket advertises a proxied FLAC as `audio/mpeg` and the
    /// receiver misplays or refuses it.
    #[test]
    fn a_proxy_ticket_carries_the_upstream_media_extension() {
        assert_eq!(
            upstream_media_extension(&url(
                "https://plex.test/library/parts/1/track.flac?X-Plex-Token=secret"
            )),
            Some("flac")
        );
        assert_eq!(
            upstream_media_extension(&url("https://music.test/a/b/song.OPUS?api_key=secret")),
            Some("opus"),
            "extension matching is case-insensitive and normalizes to the known form"
        );
    }

    /// Only the known audio extensions may shape the ticket path. Anything else
    /// leaves the ticket a bare UUID rather than letting the upstream URL
    /// dictate what the route looks like.
    #[test]
    fn a_proxy_ticket_never_inherits_an_arbitrary_suffix() {
        for no_extension in [
            // Subsonic streams have no extension at all.
            "https://sub.test/rest/stream.view?u=me&t=tok&s=salt&c=Tributary&id=1",
            // Not an audio extension.
            "https://music.test/stream.php?api_key=secret",
            "https://music.test/stream.exe?api_key=secret",
            "https://music.test/stream?api_key=secret",
        ] {
            assert_eq!(
                upstream_media_extension(&url(no_extension)),
                None,
                "{no_extension} must not shape the ticket path"
            );
        }
    }
}
