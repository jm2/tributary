#![no_main]

use libfuzzer_sys::fuzz_target;
use tributary::local::rhythmbox_import::{parse_rhythmbox_documents, RhythmboxImportLimits};

fuzz_target!(|data: &[u8]| {
    // The first 0x1F byte (never valid in XML 1.0 text) splits the input
    // into `rhythmdb.xml` and an optional `playlists.xml`. Production
    // limits apply unchanged; the parser must only accept or reject.
    let (rhythmdb, playlists) = match data.iter().position(|&byte| byte == 0x1F) {
        Some(split) => (&data[..split], Some(&data[split + 1..])),
        None => (data, None),
    };
    let _ = parse_rhythmbox_documents(rhythmdb, playlists, RhythmboxImportLimits::default());
});
