#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Production import reads the file as UTF-8 first, so invalid UTF-8
    // never reaches the parser. The parser must only accept or reject.
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = tributary::local::playlist_io::parse_xspf(text);
    }
});
