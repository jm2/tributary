//! Album art extraction and remote fetching.
//!
//! This module handles:
//! - Extracting embedded album art from local audio files (FLAC, MP3, M4A, OGG)
//! - Fetching remote album art URLs (Subsonic, Jellyfin, Plex cover art)
//! - A persistent background worker thread with generation-based staleness detection

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use gtk::glib;

/// Global generation counter for album art requests.  Incremented on
/// every track change; the worker checks this before sending results
/// back to the GTK thread so stale fetches are silently dropped.
static ART_GENERATION: AtomicU32 = AtomicU32::new(0);

/// Request sent to the album art worker thread.
struct ArtRequest {
    url: String,
    generation: u32,
    reply_tx: async_channel::Sender<Vec<u8>>,
}

/// Get (or lazily create) the sender for the persistent art worker.
fn art_worker_tx() -> &'static std::sync::mpsc::Sender<ArtRequest> {
    static TX: OnceLock<std::sync::mpsc::Sender<ArtRequest>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<ArtRequest>();
        std::thread::Builder::new()
            .name("art-worker".into())
            .spawn(move || {
                // Build the HTTP client once for the lifetime of this thread.
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap_or_default();

                while let Ok(req) = rx.recv() {
                    // Check if this request is still current before fetching.
                    if ART_GENERATION.load(Ordering::Relaxed) != req.generation {
                        continue; // Stale — user already changed tracks.
                    }

                    match client.get(&req.url).send() {
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(bytes) = resp.bytes() {
                                if !bytes.is_empty()
                                    && ART_GENERATION.load(Ordering::Relaxed) == req.generation
                                {
                                    let _ = req.reply_tx.send_blocking(bytes.to_vec());
                                }
                            }
                        }
                        Ok(resp) => {
                            tracing::debug!(status = %resp.status(), "Remote album art HTTP error");
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "Failed to fetch remote album art");
                        }
                    }
                }
            })
            .expect("Failed to spawn art-worker thread");
        tx
    })
}

/// Extract embedded album art from a track's file and display it on the
/// header bar image widget.  Falls back to the generic placeholder icon
/// if no art is found or the URI is not a local file.
///
/// Tag reading is performed on a background thread to avoid blocking
/// the GTK main loop — large FLAC files can take hundreds of ms to parse.
pub fn update_album_art(image: &gtk::Image, uri: &str) {
    // Only attempt extraction for local file:// URIs.
    let path = match url::Url::parse(uri) {
        Ok(u) if u.scheme() == "file" => match u.to_file_path() {
            Ok(p) => p,
            Err(()) => {
                image.set_icon_name(Some("audio-x-generic-symbolic"));
                return;
            }
        },
        _ => {
            image.set_icon_name(Some("audio-x-generic-symbolic"));
            return;
        }
    };

    // Set placeholder immediately while extracting on background thread.
    image.set_icon_name(Some("audio-x-generic-symbolic"));
    let image = image.clone();

    let (tx, rx) = async_channel::bounded::<Vec<u8>>(1);

    // Extract album art bytes on a background thread to avoid blocking GTK.
    std::thread::spawn(move || {
        if let Some(bytes) = extract_album_art_bytes(&path) {
            let _ = tx.send_blocking(bytes);
        }
    });

    // Receive on the GTK main thread and create the texture.
    glib::MainContext::default().spawn_local(async move {
        if let Ok(data) = rx.recv().await {
            let bytes = glib::Bytes::from(&data);
            if let Ok(texture) = gtk::gdk::Texture::from_bytes(&bytes) {
                image.set_paintable(Some(&texture));
            }
        }
    });
}

/// Extract the first embedded picture from an audio file as raw bytes.
///
/// This is a blocking operation — call from a background thread only.
fn extract_album_art_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    use lofty::file::TaggedFileExt;

    let tagged_file = lofty::read_from_path(path).ok()?;

    // ── Attempt 1: unified pictures() API ───────────────────────
    for tag in tagged_file.tags() {
        if let Some(picture) = tag.pictures().first() {
            return Some(picture.data().to_vec());
        }
    }

    // ── Attempt 2: MP4/M4A-specific fallback ────────────────────
    // lofty's unified `pictures()` API may not expose MP4 atom-based
    // cover art on all platforms.  Re-read with an explicit MP4 file
    // type hint and also try the Ilst (iTunes metadata) tag directly.
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(
        ext.to_lowercase().as_str(),
        "m4a" | "m4b" | "m4p" | "mp4" | "aac"
    ) {
        use lofty::file::FileType;
        use lofty::probe::Probe;

        if let Ok(probe) = Probe::open(path) {
            let probe = probe.set_file_type(FileType::Mp4);
            if let Ok(tagged) = probe.read() {
                // Try unified pictures() on the re-read file.
                for tag in tagged.tags() {
                    if let Some(picture) = tag.pictures().first() {
                        return Some(picture.data().to_vec());
                    }
                }
            }
        }

        // Attempt 3: Read the raw MP4 file and look for the `covr` atom
        // directly.  Some M4A files (especially Apple-encoded) store art
        // in a way that lofty's tag abstraction doesn't surface.
        if let Ok(data) = std::fs::read(path) {
            if let Some(art) = extract_mp4_covr_atom(&data) {
                return Some(art);
            }
        }
    }

    None
}

/// Brute-force search for the `covr` atom in raw MP4 data.
///
/// The iTunes `covr` atom stores cover art as:
///   [4-byte size][4-byte "data"][8-byte flags][image bytes]
/// nested inside `moov.udta.meta.ilst.covr`.
///
/// This is a last-resort fallback when lofty's tag parser doesn't
/// expose the picture through its unified API.
fn extract_mp4_covr_atom(data: &[u8]) -> Option<Vec<u8>> {
    // Walk the MP4 atom tree following the standard iTunes metadata
    // path: moov → udta → meta → ilst → covr → data.
    //
    // This structured approach handles Apple-encoded files where:
    //  - The `meta` atom has a 4-byte version/flags prefix (full-box)
    //  - Atoms use extended 64-bit sizes
    //  - The `covr` atom is deeply nested
    //
    // Falls back to a brute-force scan if the structured walk fails.

    /// Read the size and 4-byte tag of an atom at `offset`.
    /// Returns `(total_size, tag_bytes, header_len)` or `None`.
    fn read_atom_header(data: &[u8], offset: usize) -> Option<(usize, [u8; 4], usize)> {
        if offset + 8 > data.len() {
            return None;
        }
        let size32 = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        let mut tag = [0u8; 4];
        tag.copy_from_slice(&data[offset + 4..offset + 8]);

        if size32 == 1 {
            // Extended 64-bit size.
            if offset + 16 > data.len() {
                return None;
            }
            let size64 = u64::from_be_bytes([
                data[offset + 8],
                data[offset + 9],
                data[offset + 10],
                data[offset + 11],
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ]) as usize;
            Some((size64, tag, 16))
        } else if size32 == 0 {
            // Atom extends to end of file.
            Some((data.len() - offset, tag, 8))
        } else {
            Some((size32, tag, 8))
        }
    }

    /// Find a child atom with the given tag inside `data[start..end]`.
    /// `meta_adjust` adds extra bytes after the header for the `meta`
    /// full-box version/flags field.
    fn find_atom(
        data: &[u8],
        start: usize,
        end: usize,
        target: &[u8; 4],
        meta_adjust: bool,
    ) -> Option<(usize, usize)> {
        let mut pos = start;
        while pos < end {
            let (size, tag, hdr) = read_atom_header(data, pos)?;
            if size == 0 || pos + size > end {
                break;
            }
            if &tag == target {
                let body_start = if meta_adjust {
                    pos + hdr + 4
                } else {
                    pos + hdr
                };
                return Some((body_start, pos + size));
            }
            pos += size;
        }
        None
    }

    // Structured walk: moov → udta → meta → ilst → covr → data
    if let Some((moov_body, moov_end)) = find_atom(data, 0, data.len(), b"moov", false) {
        if let Some((udta_body, udta_end)) = find_atom(data, moov_body, moov_end, b"udta", false) {
            // `meta` is a full-box: 4 extra bytes (version + flags) after the header.
            if let Some((meta_body, meta_end)) = find_atom(data, udta_body, udta_end, b"meta", true)
            {
                if let Some((ilst_body, ilst_end)) =
                    find_atom(data, meta_body, meta_end, b"ilst", false)
                {
                    if let Some((covr_body, covr_end)) =
                        find_atom(data, ilst_body, ilst_end, b"covr", false)
                    {
                        // Inside `covr`, find the `data` atom.
                        if let Some((data_body, data_end)) =
                            find_atom(data, covr_body, covr_end, b"data", false)
                        {
                            // The `data` atom body starts with 8 bytes of
                            // type indicator + locale (flags/reserved).
                            let img_start = data_body + 8;
                            if img_start < data_end {
                                return Some(data[img_start..data_end].to_vec());
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: brute-force scan for any `covr` atom in the raw data.
    // This catches files with non-standard atom nesting.
    let covr_tag = b"covr";
    let data_tag = b"data";

    for i in 4..data.len().saturating_sub(8) {
        if &data[i..i + 4] == covr_tag {
            let covr_size =
                u32::from_be_bytes([data[i - 4], data[i - 3], data[i - 2], data[i - 1]]) as usize;
            if covr_size < 16 || i - 4 + covr_size > data.len() {
                continue;
            }

            let covr_end = i - 4 + covr_size;
            let inner = &data[i + 4..covr_end];

            for j in 0..inner.len().saturating_sub(8) {
                if &inner[j + 4..j + 8] == data_tag {
                    let ds =
                        u32::from_be_bytes([inner[j], inner[j + 1], inner[j + 2], inner[j + 3]])
                            as usize;
                    if ds < 16 || j + ds > inner.len() {
                        continue;
                    }
                    let img_start = j + 16;
                    let img_end = j + ds;
                    if img_end <= inner.len() && img_start < img_end {
                        return Some(inner[img_start..img_end].to_vec());
                    }
                }
            }
        }
    }
    None
}

/// Fetch remote album art asynchronously and display it on the header
/// bar image widget.  Uses a background thread + one-shot channel to
/// avoid depending on a tokio runtime context (which the GTK main
/// thread does not have).
pub fn fetch_remote_album_art(image: &gtk::Image, cover_art_url: &str) {
    // Set placeholder immediately while fetching.
    image.set_icon_name(Some("audio-x-generic-symbolic"));

    // Bump the generation counter so any in-flight fetch for the
    // previous track is discarded when it completes.
    let generation = ART_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);

    let url = cover_art_url.to_string();
    let image = image.clone();

    let (reply_tx, reply_rx) = async_channel::bounded::<Vec<u8>>(1);

    // Send the request to the persistent art worker thread.
    // This reuses a single HTTP client with connection pooling,
    // avoiding the overhead of spawning a new thread + TLS handshake
    // for every track change.
    let _ = art_worker_tx().send(ArtRequest {
        url,
        generation,
        reply_tx,
    });

    // Receive on the GTK main thread.
    glib::MainContext::default().spawn_local(async move {
        if let Ok(data) = reply_rx.recv().await {
            // Double-check generation in case another track was selected
            // while we were waiting for the channel.
            if ART_GENERATION.load(Ordering::Relaxed) == generation {
                let bytes = glib::Bytes::from(&data);
                if let Ok(texture) = gtk::gdk::Texture::from_bytes(&bytes) {
                    image.set_paintable(Some(&texture));
                }
            }
        }
    });
}
