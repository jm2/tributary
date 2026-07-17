//! Album art extraction and remote fetching.
//!
//! This module handles:
//! - Extracting embedded album art from local audio files (FLAC, MP3, M4A, OGG)
//! - Fetching remote album art URLs (Subsonic, Jellyfin, Plex cover art)
//! - A persistent background worker thread with generation-based staleness detection

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use gtk::glib;

const REMOTE_ART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_REMOTE_ART_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ROUTED_ART_CLIENTS: usize = 64;

/// Global generation counter for album art requests.  Incremented on
/// every track change; the worker checks this before sending results
/// back to the GTK thread so stale fetches are silently dropped.
static ART_GENERATION: AtomicU64 = AtomicU64::new(0);

fn next_generation() -> u64 {
    ART_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

fn generation_is_current(generation: u64) -> bool {
    ART_GENERATION.load(Ordering::Relaxed) == generation
}

/// Invalidate every in-flight local extraction and remote fetch.
///
/// Playback resets call this before installing the generic placeholder so a
/// late worker result cannot restore artwork from the stopped/previous item.
pub fn invalidate() {
    next_generation();
}

/// Request sent to the album art worker thread.
struct ArtRequest {
    source: ArtSource,
    generation: u64,
    reply_tx: async_channel::Sender<Vec<u8>>,
}

/// Remote artwork input. Deliberately not `Debug`: the resolved variant owns
/// authentication material that must remain inside Tributary's fetch worker.
enum ArtSource {
    /// Ordinary credential-free URL path.
    Url(String),
    /// Credential-isolated request produced by a retained remote source.
    Resolved(Box<crate::architecture::media::ResolvedHttpRequest>),
}

impl ArtSource {
    fn is_active(&self) -> bool {
        match self {
            Self::Url(_) => true,
            Self::Resolved(request) => request.is_active(),
        }
    }
}

/// Get (or lazily create) the sender for the persistent art worker.
///
/// Returns `None` if the worker thread could not be spawned. Callers
/// should treat that as "remote album art unavailable" and skip
/// fetching — the local-tag-extraction path still works regardless.
fn art_worker_tx() -> Option<&'static std::sync::mpsc::Sender<ArtRequest>> {
    static TX: OnceLock<Option<std::sync::mpsc::Sender<ArtRequest>>> = OnceLock::new();
    TX.get_or_init(|| {
        // Build before spawning so a policy-construction failure disables
        // remote artwork instead of silently restoring reqwest's permissive
        // default redirect and Referer behavior.
        let default_client = match crate::http_security::authenticated_blocking_client_builder()
            .timeout(REMOTE_ART_TIMEOUT)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                let error = crate::http_security::strip_request_url(error);
                tracing::warn!(%error, "Failed to build secure album art HTTP client");
                return None;
            }
        };

        let (tx, rx) = std::sync::mpsc::channel::<ArtRequest>();
        let spawn_result = std::thread::Builder::new()
            .name("art-worker".into())
            .spawn(move || {
                let mut routed_clients = HashMap::<
                    crate::architecture::AdvertisedHttpRoute,
                    reqwest::blocking::Client,
                >::new();
                while let Ok(req) = rx.recv() {
                    // Check if this request is still current before fetching.
                    if ART_GENERATION.load(Ordering::Relaxed) != req.generation {
                        continue; // Stale — user already changed tracks.
                    }

                    if !req.source.is_active() {
                        continue;
                    }

                    let request = match &req.source {
                        ArtSource::Url(url) => default_client.get(url),
                        ArtSource::Resolved(resolved) => {
                            let client = match resolved.advertised_route() {
                                None => default_client.clone(),
                                Some(route) => match routed_clients.get(route) {
                                    Some(client) => client.clone(),
                                    None => {
                                        let Some(client) =
                                            build_routed_art_client(resolved.endpoint(), route)
                                        else {
                                            continue;
                                        };
                                        if routed_clients.len() >= MAX_ROUTED_ART_CLIENTS {
                                            routed_clients.clear();
                                        }
                                        routed_clients.insert(route.clone(), client.clone());
                                        client
                                    }
                                },
                            };
                            build_resolved_art_request(&client, resolved)
                        }
                    };

                    match request.timeout(REMOTE_ART_TIMEOUT).send() {
                        Ok(resp) if resp.status().is_success() => {
                            match crate::http_body::read_limited_blocking(
                                resp,
                                MAX_REMOTE_ART_BYTES,
                                REMOTE_ART_TIMEOUT,
                            ) {
                                Ok(bytes)
                                    if !bytes.is_empty()
                                        && req.source.is_active()
                                        && ART_GENERATION.load(Ordering::Relaxed)
                                            == req.generation =>
                                {
                                    let _ = req.reply_tx.send_blocking(bytes);
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    tracing::debug!(%error, "Failed to read remote album art body");
                                }
                            }
                        }
                        Ok(resp) => {
                            tracing::debug!(status = %resp.status(), "Remote album art HTTP error");
                        }
                        Err(error) => {
                            let error = crate::http_security::strip_request_url(error);
                            tracing::debug!(%error, "Failed to fetch remote album art");
                        }
                    }
                }
            });

        match spawn_result {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to spawn art-worker thread; remote album art will be skipped"
                );
                None
            }
        }
    })
    .as_ref()
}

/// Extract embedded album art from a track's file and display it on the
/// header bar image widget.  Falls back to the generic placeholder icon
/// if no art is found or the URI is not a local file.
///
/// Tag reading is performed on a background thread to avoid blocking
/// the GTK main loop — large FLAC files can take hundreds of ms to parse.
pub fn update_album_art(image: &gtk::Image, uri: &str) {
    let generation = next_generation();
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
            if generation_is_current(generation) {
                let _ = tx.send_blocking(bytes);
            }
        }
    });

    // Receive on the GTK main thread and create the texture.
    glib::MainContext::default().spawn_local(async move {
        if let Ok(data) = rx.recv().await {
            if !generation_is_current(generation) {
                return;
            }
            let bytes = glib::Bytes::from_owned(data);
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
    let generation = begin_remote_album_art(image);
    enqueue_remote_album_art(image, ArtSource::Url(cover_art_url.to_string()), generation);
}

/// Begin resolving protected artwork without allowing an older resolver to
/// supersede a newer track while it awaits its source session.
pub fn begin_remote_album_art(image: &gtk::Image) -> u64 {
    image.set_icon_name(Some("audio-x-generic-symbolic"));
    next_generation()
}

/// Fetch a credential-isolated artwork request for an already-reserved
/// generation. A stale resolver result is discarded before it reaches the
/// persistent worker, and the worker repeats both generation and lease checks.
pub fn fetch_resolved_album_art(
    image: &gtk::Image,
    request: crate::architecture::media::ResolvedHttpRequest,
    generation: u64,
) {
    if !generation_is_current(generation) || !request.is_active() {
        return;
    }
    enqueue_remote_album_art(image, ArtSource::Resolved(Box::new(request)), generation);
}

fn build_routed_art_client(
    endpoint: &url::Url,
    route: &crate::architecture::AdvertisedHttpRoute,
) -> Option<reqwest::blocking::Client> {
    let builder =
        crate::http_security::authenticated_blocking_client_builder().timeout(REMOTE_ART_TIMEOUT);
    let Ok(builder) =
        crate::http_security::apply_advertised_http_route_blocking(builder, endpoint, Some(route))
    else {
        tracing::warn!("Failed to apply advertised route to album art client");
        return None;
    };
    match builder.build() {
        Ok(client) => Some(client),
        Err(error) => {
            let error = crate::http_security::strip_request_url(error);
            tracing::warn!(%error, "Failed to build routed album art HTTP client");
            None
        }
    }
}

/// Build the exact protected artwork request at the last responsible moment.
///
/// Authentication query state and headers stay isolated on the resolved
/// request until the worker has selected the exact-origin HTTP client. Fixed
/// protocol headers are installed before sensitive authentication headers so
/// the ordering matches protected stream requests.
fn build_resolved_art_request(
    client: &reqwest::blocking::Client,
    resolved: &crate::architecture::media::ResolvedHttpRequest,
) -> reqwest::blocking::RequestBuilder {
    let mut endpoint = resolved.endpoint().clone();
    {
        let mut query = endpoint.query_pairs_mut();
        for (key, value) in resolved.private_query_pairs() {
            query.append_pair(key, value);
        }
    }

    client
        .get(endpoint)
        .headers(resolved.required_headers().clone())
        .headers(resolved.sensitive_headers().clone())
}

fn enqueue_remote_album_art(image: &gtk::Image, source: ArtSource, generation: u64) {
    let image = image.clone();

    let reply_rx = enqueue_art_request(source, generation);

    // Receive on the GTK main thread.
    glib::MainContext::default().spawn_local(async move {
        if let Ok(data) = reply_rx.recv().await {
            // Double-check generation in case another track was selected
            // while we were waiting for the channel.
            if generation_is_current(generation) {
                let bytes = glib::Bytes::from_owned(data);
                if let Ok(texture) = gtk::gdk::Texture::from_bytes(&bytes) {
                    image.set_paintable(Some(&texture));
                }
            }
        }
    });
}

/// Submit one request through the production persistent worker and return its
/// one-shot completion. Keeping this GTK-independent makes the full
/// request/fetch/generation boundary deterministic under headless CI; the UI
/// callback above adds the final generation check before mutating the widget.
fn enqueue_art_request(source: ArtSource, generation: u64) -> async_channel::Receiver<Vec<u8>> {
    let (reply_tx, reply_rx) = async_channel::bounded::<Vec<u8>>(1);

    // Send the request to the persistent art worker thread.
    // This reuses a single HTTP client with connection pooling,
    // avoiding the overhead of spawning a new thread + TLS handshake
    // for every track change. If the worker isn't available (thread
    // spawn failed at startup), silently skip — there's nothing to
    // fetch with and the placeholder icon will show instead.
    if let Some(tx) = art_worker_tx() {
        let _ = tx.send(ArtRequest {
            source,
            generation,
            reply_tx,
        });
    }

    reply_rx
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    use reqwest::header::{HeaderName, HeaderValue, ACCEPT, AUTHORIZATION};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    static GENERATION_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn spawn_art_fixture(
        body: &'static [u8],
        before_response: impl FnOnce() + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind artwork fixture");
        let address = listener.local_addr().expect("artwork fixture address");
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept artwork request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set artwork fixture read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("set artwork fixture write timeout");

            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(stream.read(&mut byte).expect("read artwork request"), 1);
                request.push(byte[0]);
                assert!(request.len() <= 16 * 1024, "artwork request header cap");
            }
            assert!(request.starts_with(b"GET /art HTTP/1.1\r\n"));

            before_response();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(headers.as_bytes())
                .expect("write artwork response headers");
            stream.write_all(body).expect("write artwork response body");
        });
        (format!("http://{address}/art"), thread)
    }

    #[test]
    fn reset_invalidates_local_and_remote_artwork_results() {
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale_generation = next_generation();
        assert!(generation_is_current(stale_generation));

        invalidate();

        assert!(!generation_is_current(stale_generation));
    }

    #[test]
    fn delayed_worker_result_cannot_cross_a_newer_artwork_generation() {
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (request_seen_tx, request_seen_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let (stale_url, stale_server) = spawn_art_fixture(b"stale-art", move || {
            request_seen_tx.send(()).expect("report delayed request");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release delayed response");
        });
        let (current_url, current_server) = spawn_art_fixture(b"current-art", || {});

        let stale_generation = next_generation();
        let stale_reply = enqueue_art_request(ArtSource::Url(stale_url), stale_generation);
        request_seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("production worker started delayed request");

        let current_generation = next_generation();
        let current_reply = enqueue_art_request(ArtSource::Url(current_url), current_generation);
        release_tx.send(()).expect("release stale response");

        assert!(
            stale_reply.recv_blocking().is_err(),
            "the worker must close a stale request without publishing its bytes"
        );
        assert_eq!(
            current_reply
                .recv_blocking()
                .expect("current artwork bytes"),
            b"current-art"
        );
        assert!(generation_is_current(current_generation));

        stale_server.join().expect("join delayed artwork fixture");
        current_server.join().expect("join current artwork fixture");
    }

    #[test]
    fn resolved_art_request_preserves_endpoint_and_isolated_http_state() {
        let required_name = HeaderName::from_static("client-daap-version");
        let resolved = crate::architecture::media::ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/share/databases/1/items/42.mp3?format=original")
                .unwrap(),
        )
        .unwrap()
        .with_private_query_pair("session-id", "private-session")
        .unwrap()
        .with_required_header(
            ACCEPT,
            HeaderValue::from_static("application/x-dmap-tagged"),
        )
        .unwrap()
        .with_required_header(required_name.clone(), HeaderValue::from_static("3.12"))
        .unwrap()
        .with_sensitive_header(
            AUTHORIZATION,
            HeaderValue::from_static("Basic private-authorization"),
        )
        .unwrap();

        let request = build_resolved_art_request(&reqwest::blocking::Client::new(), &resolved)
            .build()
            .unwrap();

        assert_eq!(request.url().path(), "/share/databases/1/items/42.mp3");
        assert_eq!(
            request.url().query(),
            Some("format=original&session-id=private-session")
        );
        assert_eq!(
            request.headers().get(ACCEPT).unwrap(),
            "application/x-dmap-tagged"
        );
        assert_eq!(request.headers().get(&required_name).unwrap(), "3.12");
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Basic private-authorization"
        );
        assert_eq!(request.headers().len(), 3);
    }
}
