#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Both strict auth-response parsers (token and session); the seam
    // drops every result, so parsed secrets never leave the parser.
    tributary::lastfm::client::fuzz_auth_responses(data);
});
