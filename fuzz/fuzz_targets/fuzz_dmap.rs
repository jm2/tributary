#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Exercise the DMAP binary parser with arbitrary input.
    // The parser should never panic — only return Ok or Err.
    let _ = tributary::daap::dmap::parse_dmap(data);
});
