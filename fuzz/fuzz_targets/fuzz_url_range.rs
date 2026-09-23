#![no_main]

use libfuzzer_sys::fuzz_target;
use tributary::audio::cast_http_parse::{parse_range_header, upstream_media_extension};
use tributary::http_security::{
    append_base_path_segments, classify_media_uri, parse_base_url, redact_url_secrets,
    url_carries_credentials, validate_base_url,
};

fuzz_target!(|data: &[u8]| {
    // Input: UTF-8 lines `URL`, `Range header value`, `file size`; missing
    // lines are empty and a missing or invalid size is 0.
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut lines = text.split('\n');
    let candidate = lines.next().unwrap_or_default();
    let range_header = lines.next().unwrap_or_default();
    let file_size = lines
        .next()
        .and_then(|line| line.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let _ = redact_url_secrets(candidate);
    let _ = classify_media_uri(candidate);

    if let Ok(mut base) = parse_base_url(candidate) {
        let _ = validate_base_url(&base);
        let _ = url_carries_credentials(&base);
        let _ = upstream_media_extension(&base);
        append_base_path_segments(&mut base, ["rest", "ping.view"]);
    }

    let _ = parse_range_header(range_header, file_size);
});
