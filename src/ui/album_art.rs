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
/// Raw MP4 fallback is intentionally bounded: unlike Lofty's tag parser it
/// scans a complete file image. Ordinary parsing remains available above this
/// limit, but a malformed or unusually large file cannot force an unbounded
/// allocation merely because the format-specific fallback was reached.
const MAX_RAW_MP4_FALLBACK_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LOCAL_EMBEDDED_ART_BYTES: usize = 32 * 1024 * 1024;

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

/// Extract embedded album art from a direct file URI and display it on the
/// header bar image widget.
///
/// This transitional path is retained for removable and OS-opened files until
/// their at-use adapters provide retained file authority. Local-library and
/// playlist playback must use [`update_resolved_file_album_art`] instead.
pub fn update_direct_file_album_art(image: &gtk::Image, uri: &str) {
    let generation = next_generation();
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

    image.set_icon_name(Some("audio-x-generic-symbolic"));
    let reply_rx = enqueue_local_art_job(generation, move || {
        extract_direct_file_album_art_bytes(&path)
    });
    display_local_album_art_reply(image, reply_rx, generation);
}

/// Extract embedded art through an exact retained local-file capability.
///
/// The background reader clones the already-authorized file handle; it never
/// receives or reopens the database pathname. Keeping `media` owned by the job
/// also retains its root, marker, ancestor, and exact-file authority through
/// the complete parse.
pub fn update_resolved_file_album_art(
    image: &gtk::Image,
    media: crate::local::resolver::ResolvedLocalMedia,
) {
    let generation = next_generation();
    image.set_icon_name(Some("audio-x-generic-symbolic"));
    let reply_rx = enqueue_local_art_job(generation, move || {
        extract_resolved_file_album_art_bytes(&media)
    });
    display_local_album_art_reply(image, reply_rx, generation);
}

fn enqueue_local_art_job<F>(generation: u64, extract: F) -> async_channel::Receiver<Vec<u8>>
where
    F: FnOnce() -> Option<Vec<u8>> + Send + 'static,
{
    let (tx, rx) = async_channel::bounded::<Vec<u8>>(1);
    let spawn_result = std::thread::Builder::new()
        .name("local-art-worker".into())
        .spawn(move || {
            if !generation_is_current(generation) {
                return;
            }
            if let Some(bytes) = extract() {
                if generation_is_current(generation) {
                    let _ = tx.send_blocking(bytes);
                }
            }
        });
    if let Err(error) = spawn_result {
        tracing::warn!(%error, "Failed to spawn local album-art worker");
    }
    rx
}

fn display_local_album_art_reply(
    image: &gtk::Image,
    reply_rx: async_channel::Receiver<Vec<u8>>,
    generation: u64,
) {
    let image = image.clone();
    glib::MainContext::default().spawn_local(async move {
        if let Ok(data) = reply_rx.recv().await {
            if generation_is_current(generation) {
                let bytes = glib::Bytes::from_owned(data);
                if let Ok(texture) = gtk::gdk::Texture::from_bytes(&bytes) {
                    image.set_paintable(Some(&texture));
                }
            }
        }
    });
}

fn extract_direct_file_album_art_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    let extension = path.extension().and_then(|extension| extension.to_str());
    let mut file = std::fs::File::open(path).ok()?;
    extract_album_art_bytes(&mut file, extension)
}

fn extract_resolved_file_album_art_bytes(
    media: &crate::local::resolver::ResolvedLocalMedia,
) -> Option<Vec<u8>> {
    let mut file = media.try_clone_file().ok()?;
    extract_album_art_bytes(&mut file, media.extension())
}

fn bounded_local_art_bytes(data: &[u8], max_bytes: usize) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() > max_bytes {
        return None;
    }
    Some(data.to_vec())
}

/// Extract the first embedded picture from an audio file as raw bytes.
///
/// This is a blocking operation — call from a background thread only.
fn extract_album_art_bytes(file: &mut std::fs::File, extension: Option<&str>) -> Option<Vec<u8>> {
    use lofty::file::TaggedFileExt;
    use std::io::{BufReader, Seek, SeekFrom};

    fn rewind(file: &mut std::fs::File) -> Option<()> {
        file.seek(SeekFrom::Start(0)).ok()?;
        Some(())
    }

    fn read_tagged(
        file: &mut std::fs::File,
        extension: Option<&str>,
    ) -> Option<lofty::file::TaggedFile> {
        use lofty::config::ParseOptions;
        use lofty::file::FileType;
        use lofty::probe::Probe;

        rewind(file)?;
        let reader = BufReader::new(file);
        let options = ParseOptions::new().read_properties(false);
        match extension.and_then(FileType::from_ext) {
            Some(file_type) => Probe::with_file_type(reader, file_type)
                .options(options)
                .read()
                .ok(),
            None => Probe::new(reader)
                .options(options)
                .guess_file_type()
                .ok()?
                .read()
                .ok(),
        }
    }

    fn extract(file: &mut std::fs::File, extension: Option<&str>) -> Option<Vec<u8>> {
        use lofty::config::ParseOptions;
        use lofty::file::FileType;
        use lofty::probe::Probe;

        // ── Attempt 1: unified pictures() API ───────────────────
        if let Some(tagged_file) = read_tagged(file, extension) {
            for tag in tagged_file.tags() {
                if let Some(picture) = tag.pictures().first() {
                    return bounded_local_art_bytes(picture.data(), MAX_LOCAL_EMBEDDED_ART_BYTES);
                }
            }
        }

        // ── Attempt 2: MP4/M4A-specific fallback ────────────────
        // Preserve the existing extension-gated behavior without recovering
        // a path. Every attempt rewinds the exact retained handle because OS
        // clones may share their file cursor.
        let extension = extension.unwrap_or_default();
        if !matches!(
            extension.to_ascii_lowercase().as_str(),
            "m4a" | "m4b" | "m4p" | "mp4" | "aac"
        ) {
            return None;
        }

        rewind(file)?;
        let probe = Probe::with_file_type(BufReader::new(&mut *file), FileType::Mp4)
            .options(ParseOptions::new().read_properties(false));
        if let Ok(tagged) = probe.read() {
            for tag in tagged.tags() {
                if let Some(picture) = tag.pictures().first() {
                    return bounded_local_art_bytes(picture.data(), MAX_LOCAL_EMBEDDED_ART_BYTES);
                }
            }
        }

        // Attempt 3 scans the same retained file object for a raw `covr`
        // atom. A pathname replacement cannot retarget this fallback.
        rewind(file)?;
        extract_raw_mp4_fallback(
            file,
            MAX_RAW_MP4_FALLBACK_BYTES,
            MAX_LOCAL_EMBEDDED_ART_BYTES,
        )
    }

    let result = extract(file, extension);
    // Leave the shared OS cursor in a deterministic state for any later clone.
    let _ = file.seek(SeekFrom::Start(0));
    result
}

fn extract_raw_mp4_fallback(
    file: &mut std::fs::File,
    max_file_bytes: u64,
    max_art_bytes: usize,
) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let file_size = file.metadata().ok()?.len();
    if file_size > max_file_bytes {
        return None;
    }
    let capacity = usize::try_from(file_size).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut data = Vec::with_capacity(capacity);
    let read_limit = max_file_bytes.checked_add(1)?;
    (&mut *file).take(read_limit).read_to_end(&mut data).ok()?;
    if u64::try_from(data.len()).ok()? > max_file_bytes {
        return None;
    }
    extract_mp4_covr_atom(&data, max_art_bytes)
}

/// Checked raw search for the first bounded iTunes `covr` picture.
///
/// The structured walk covers `moov.udta.meta.ilst.covr.data`; a checked tag
/// search retains the historical non-standard nesting fallback. Every offset,
/// atom size, extended-size conversion, and image allocation is bounded.
fn extract_mp4_covr_atom(data: &[u8], max_art_bytes: usize) -> Option<Vec<u8>> {
    #[derive(Clone, Copy)]
    struct AtomBounds {
        tag: [u8; 4],
        body_start: usize,
        end: usize,
    }

    fn atom_header(data: &[u8], offset: usize, parent_end: usize) -> Option<AtomBounds> {
        let base_header_end = offset.checked_add(8)?;
        if base_header_end > parent_end || base_header_end > data.len() {
            return None;
        }
        let size32 = u32::from_be_bytes(data.get(offset..offset.checked_add(4)?)?.try_into().ok()?);
        let tag = data
            .get(offset.checked_add(4)?..base_header_end)?
            .try_into()
            .ok()?;
        let (size, header_len) = match size32 {
            0 => (parent_end.checked_sub(offset)?, 8_usize),
            1 => {
                let extended_end = offset.checked_add(16)?;
                if extended_end > parent_end || extended_end > data.len() {
                    return None;
                }
                let raw =
                    u64::from_be_bytes(data.get(base_header_end..extended_end)?.try_into().ok()?);
                (usize::try_from(raw).ok()?, 16_usize)
            }
            size => (usize::try_from(size).ok()?, 8_usize),
        };
        if size < header_len {
            return None;
        }
        let end = offset.checked_add(size)?;
        if end > parent_end || end > data.len() {
            return None;
        }
        Some(AtomBounds {
            tag,
            body_start: offset.checked_add(header_len)?,
            end,
        })
    }

    fn child_atom(
        data: &[u8],
        start: usize,
        end: usize,
        target: &[u8; 4],
        body_prefix: usize,
    ) -> Option<AtomBounds> {
        if start > end || end > data.len() {
            return None;
        }
        let mut offset = start;
        while offset < end {
            let mut atom = atom_header(data, offset, end)?;
            if &atom.tag == target {
                atom.body_start = atom.body_start.checked_add(body_prefix)?;
                if atom.body_start > atom.end {
                    return None;
                }
                return Some(atom);
            }
            if atom.end <= offset {
                return None;
            }
            offset = atom.end;
        }
        None
    }

    fn picture_bytes(data: &[u8], atom: AtomBounds, max_art_bytes: usize) -> Option<Vec<u8>> {
        let start = atom.body_start.checked_add(8)?;
        let length = atom.end.checked_sub(start)?;
        if length == 0 || length > max_art_bytes {
            return None;
        }
        bounded_local_art_bytes(data.get(start..atom.end)?, max_art_bytes)
    }

    fn next_tag(data: &[u8], start: usize, end: usize, target: &[u8; 4]) -> Option<usize> {
        data.get(start..end)?
            .windows(target.len())
            .position(|candidate| candidate == target)
            .and_then(|relative| start.checked_add(relative))
    }

    let structured = (|| {
        let moov = child_atom(data, 0, data.len(), b"moov", 0)?;
        let udta = child_atom(data, moov.body_start, moov.end, b"udta", 0)?;
        let meta = child_atom(data, udta.body_start, udta.end, b"meta", 4)?;
        let ilst = child_atom(data, meta.body_start, meta.end, b"ilst", 0)?;
        let covr = child_atom(data, ilst.body_start, ilst.end, b"covr", 0)?;
        let picture = child_atom(data, covr.body_start, covr.end, b"data", 0)?;
        picture_bytes(data, picture, max_art_bytes)
    })();
    if structured.is_some() {
        return structured;
    }

    let mut covr_search = 4_usize;
    while let Some(covr_tag) = next_tag(data, covr_search, data.len(), b"covr") {
        let covr_start = covr_tag.checked_sub(4)?;
        if let Some(covr) = atom_header(data, covr_start, data.len()) {
            if &covr.tag == b"covr" {
                let mut data_search = covr.body_start.checked_add(4)?;
                while data_search < covr.end {
                    let Some(data_tag) = next_tag(data, data_search, covr.end, b"data") else {
                        break;
                    };
                    let Some(data_start) = data_tag.checked_sub(4) else {
                        break;
                    };
                    if let Some(picture) = atom_header(data, data_start, covr.end) {
                        if &picture.tag == b"data" {
                            if let Some(bytes) = picture_bytes(data, picture, max_art_bytes) {
                                return Some(bytes);
                            }
                        }
                    }
                    let Some(next_search) = data_tag.checked_add(4) else {
                        break;
                    };
                    data_search = next_search;
                }
            }
        }
        covr_search = covr_tag.checked_add(4)?;
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
mod tests {
    use super::*;

    use reqwest::header::{HeaderName, HeaderValue, ACCEPT, AUTHORIZATION};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    static GENERATION_TEST_LOCK: Mutex<()> = Mutex::new(());
    const MARKER: &str = "marker:v1:00000000-0000-4000-8000-000000000001";
    const OTHER_MARKER: &str = "marker:v1:00000000-0000-4000-8000-000000000002";

    fn mp4_atom(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let atom_size = 8_usize
            .checked_add(body.len())
            .expect("test MP4 atom size does not overflow");
        let size = u32::try_from(atom_size).expect("test MP4 atom size fits u32");
        let mut atom = Vec::with_capacity(atom_size);
        atom.extend_from_slice(&size.to_be_bytes());
        atom.extend_from_slice(tag);
        atom.extend_from_slice(body);
        atom
    }

    fn mp4_with_cover_art(art: &[u8]) -> Vec<u8> {
        let mut data_body = vec![0_u8; 8];
        data_body.extend_from_slice(art);
        let data = mp4_atom(b"data", &data_body);
        let covr = mp4_atom(b"covr", &data);
        let ilst = mp4_atom(b"ilst", &covr);
        let mut meta_body = vec![0_u8; 4];
        meta_body.extend_from_slice(&ilst);
        let meta = mp4_atom(b"meta", &meta_body);
        let udta = mp4_atom(b"udta", &meta);
        mp4_atom(b"moov", &udta)
    }

    fn authorized_media(
        root: &std::path::Path,
        filename: &str,
        bytes: &[u8],
    ) -> crate::local::resolver::ResolvedLocalMedia {
        let path = root.join(filename);
        std::fs::write(&path, bytes).expect("write media fixture");
        authorize_existing_media(root, &path)
    }

    fn authorize_existing_media(
        root: &std::path::Path,
        path: &std::path::Path,
    ) -> crate::local::resolver::ResolvedLocalMedia {
        std::fs::write(root.join(".tributary-root-id"), format!("{MARKER}\n"))
            .expect("write root marker");
        crate::local::resolver::ResolvedLocalMedia::from_authorized_path_for_test(
            root, MARKER, path,
        )
        .expect("authorize media fixture")
    }

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
    fn delayed_local_art_result_cannot_cross_a_newer_generation() {
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (request_seen_tx, request_seen_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let stale_generation = next_generation();
        let stale_reply = enqueue_local_art_job(stale_generation, move || {
            request_seen_tx.send(()).expect("report delayed local read");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release delayed local read");
            Some(b"stale-local-art".to_vec())
        });
        request_seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("local artwork worker started");

        let current_generation = next_generation();
        let current_reply =
            enqueue_local_art_job(current_generation, || Some(b"current-local-art".to_vec()));
        release_tx.send(()).expect("release stale local read");

        assert!(stale_reply.recv_blocking().is_err());
        assert_eq!(
            current_reply.recv_blocking().expect("current local art"),
            b"current-local-art"
        );
    }

    #[test]
    fn resolved_handle_uses_lofty_for_extension_classified_flac_artwork() {
        use lofty::config::WriteOptions;
        use lofty::file::{FileType, TaggedFileExt};
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::probe::Probe;
        use lofty::tag::TagExt;
        use std::io::BufReader;

        let root = tempfile::tempdir().expect("temporary authority root");
        let path = root.path().join("track.FLAC");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/audio/silence.flac"
            ),
            &path,
        )
        .expect("copy deterministic FLAC fixture");
        let art = b"lofty-retained-handle-art";
        let fixture_file = std::fs::File::open(&path).expect("open FLAC fixture");
        let mut tagged = Probe::with_file_type(BufReader::new(fixture_file), FileType::Flac)
            .read()
            .expect("read FLAC fixture through handle");
        if tagged.primary_tag_mut().is_none() {
            let tag_type = tagged.primary_tag_type();
            tagged.insert_tag(lofty::tag::Tag::new(tag_type));
        }
        let tag = tagged.primary_tag_mut().expect("FLAC primary tag");
        tag.push_picture(
            Picture::unchecked(art.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
        tag.save_to_path(&path, WriteOptions::default())
            .expect("write FLAC picture");

        let media = authorize_existing_media(root.path(), &path);
        assert_eq!(
            extract_resolved_file_album_art_bytes(&media).as_deref(),
            Some(art.as_slice())
        );
    }

    #[test]
    fn raw_mp4_fallback_enforces_file_art_and_arithmetic_bounds() {
        let art = b"bounded-art";
        assert!(bounded_local_art_bytes(&[], art.len()).is_none());
        assert!(bounded_local_art_bytes(art, art.len() - 1).is_none());
        assert_eq!(
            bounded_local_art_bytes(art, art.len()).as_deref(),
            Some(art.as_slice())
        );
        let fixture = mp4_with_cover_art(art);
        assert_eq!(
            extract_mp4_covr_atom(&fixture, art.len()).as_deref(),
            Some(art.as_slice())
        );
        assert!(extract_mp4_covr_atom(&fixture, art.len() - 1).is_none());

        let root = tempfile::tempdir().expect("temporary fallback root");
        let path = root.path().join("fallback.m4a");
        std::fs::write(&path, &fixture).expect("write MP4 fallback fixture");
        let mut file = std::fs::File::open(&path).expect("open MP4 fallback fixture");
        let exact_file_limit = u64::try_from(fixture.len()).expect("fixture length fits u64");
        assert!(extract_raw_mp4_fallback(&mut file, exact_file_limit - 1, art.len(),).is_none());
        assert_eq!(
            extract_raw_mp4_fallback(&mut file, exact_file_limit, art.len()).as_deref(),
            Some(art.as_slice())
        );

        let mut extended_overflow = Vec::new();
        extended_overflow.extend_from_slice(&1_u32.to_be_bytes());
        extended_overflow.extend_from_slice(b"moov");
        extended_overflow.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(extract_mp4_covr_atom(&extended_overflow, art.len()).is_none());
    }

    #[test]
    fn local_art_extractor_rewinds_its_handle_before_and_after_parsing() {
        use std::io::{Seek, SeekFrom};

        let art = b"cursor-safe-art";
        let fixture = mp4_with_cover_art(art);
        let root = tempfile::tempdir().expect("temporary cursor root");
        let path = root.path().join("cursor.m4a");
        std::fs::write(&path, fixture).expect("write cursor fixture");
        let mut file = std::fs::File::open(&path).expect("open cursor fixture");
        file.seek(SeekFrom::Start(3)).expect("move fixture cursor");

        assert_eq!(
            extract_album_art_bytes(&mut file, Some("m4a")).as_deref(),
            Some(art.as_slice())
        );
        assert_eq!(file.stream_position().expect("read restored cursor"), 0);
    }

    #[test]
    fn resolved_artwork_fails_closed_after_root_authority_drift() {
        let root = tempfile::tempdir().expect("temporary authority root");
        let fixture = mp4_with_cover_art(b"authorized-art");
        let media = authorized_media(root.path(), "track.m4a", &fixture);
        assert_eq!(
            extract_resolved_file_album_art_bytes(&media).as_deref(),
            Some(b"authorized-art".as_slice())
        );

        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{OTHER_MARKER}\n"),
        )
        .expect("change retained marker");

        assert!(extract_resolved_file_album_art_bytes(&media).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resolved_artwork_reads_retained_file_after_path_replacement() {
        let root = tempfile::tempdir().expect("temporary authority root");
        let original = mp4_with_cover_art(b"original-authorized-art");
        let replacement = mp4_with_cover_art(b"replacement-path-art");
        let media = authorized_media(root.path(), "track.m4a", &original);
        let path = root.path().join("track.m4a");
        std::fs::rename(&path, root.path().join("displaced.m4a")).expect("move admitted file");
        std::fs::write(&path, replacement).expect("install path replacement");

        assert_eq!(
            extract_resolved_file_album_art_bytes(&media).as_deref(),
            Some(b"original-authorized-art".as_slice())
        );
        assert_eq!(
            extract_direct_file_album_art_bytes(&path).as_deref(),
            Some(b"replacement-path-art".as_slice())
        );
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
