//! Album art extraction and remote fetching.
//!
//! This module handles:
//! - Extracting embedded album art from local audio files (FLAC, MP3, M4A, OGG)
//! - Fetching remote album art URLs (Subsonic, Jellyfin, Plex cover art)
//! - A persistent background worker thread with generation-based staleness detection

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

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
/// Requests a virtualized album pane may have pending in the pane lane at
/// once. Beyond this bound a newly scheduled pane request is dropped at
/// enqueue time (its reply closes; the row keeps its placeholder and
/// re-requests on the next bind), so a pathological catalog cannot queue
/// unbounded work behind the now-playing header (2026-09-10 review finding).
const MAX_PENDING_PANE_ART_REQUESTS: usize = 32;
/// Fixed worker count for local embedded-art extraction. A virtualized pane
/// can schedule one extraction per visible row per rebind; without a shared
/// pool that is one OS thread per bind (2026-09-10 review finding). Two
/// workers keep a slow local file from blocking the next row while still
/// bounding total extraction concurrency.
const MAX_LOCAL_ART_WORKERS: usize = 2;
/// Local extraction jobs may sit in the pool queue before new jobs are
/// refused. This bounds pending work independently of the worker count.
const MAX_PENDING_LOCAL_ART_JOBS: usize = 64;

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

/// Liveness gate carried by one in-flight album-art request.
///
/// Two scopes share the persistent worker:
///
/// * [`RequestLiveness::GlobalGeneration`] — the now-playing header. A
///   fetch is live only while its captured generation equals
///   [`ART_GENERATION`]; [`invalidate`] bumps the counter so a track
///   change silently drops every stale header fetch.
/// * [`RequestLiveness::Scoped`] — browser album-pane rows. These must
///   NOT share the header's counter: every visible row minting from
///   [`ART_GENERATION`] would invalidate the previous row's in-flight
///   fetch (and the header's), so concurrent thumbnails cancelled one
///   another and most rows never resolved (2026-09-08 PR #171 review).
///   A scoped request instead carries a private [`ScopedArtFetch`] token
///   that the owning widget revokes on rebind, unbind, teardown, and
///   factory swaps.
#[derive(Clone)]
enum RequestLiveness {
    /// Valid only while [`ART_GENERATION`] equals this generation.
    GlobalGeneration(u64),
    /// Valid until the owning widget revokes the token.
    Scoped(ScopedArtFetch),
}

impl RequestLiveness {
    /// `true` while the owning fetch may still run, reply, and paint.
    fn is_valid(&self) -> bool {
        match self {
            Self::GlobalGeneration(generation) => generation_is_current(*generation),
            Self::Scoped(token) => token.is_live(),
        }
    }
}

/// Revocable liveness token for one non-header album-art request.
///
/// The album pane mints one token per scheduled fetch and stores it on
/// the row's cell state; revoking the cell (rebind, unbind, teardown,
/// factory swap) revokes the token, which stops the persistent worker
/// *before* the network read and closes the reply before the result can
/// reach the widget. Tokens are `Send + Sync`, so the worker thread can
/// hold one across a fetch without pinning GTK state.
#[derive(Clone)]
pub struct ScopedArtFetch {
    valid: Arc<AtomicBool>,
}

impl Default for ScopedArtFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopedArtFetch {
    pub fn new() -> Self {
        Self {
            valid: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Stop the fetch this token was minted for. Idempotent and safe to
    /// call from any thread; every future [`ScopedArtFetch::is_live`]
    /// observation on this token returns `false`.
    pub fn revoke(&self) {
        self.valid.store(false, Ordering::Relaxed);
    }

    /// `true` while the fetch may still run, reply, and paint.
    pub fn is_live(&self) -> bool {
        self.valid.load(Ordering::Relaxed)
    }
}

/// Request sent to the album art worker thread.
struct ArtRequest {
    source: ArtSource,
    liveness: RequestLiveness,
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

/// Senders for the two isolated remote-artwork lanes.
///
/// The now-playing header and the browser album pane used to share ONE
/// serial worker: a pane thumbnail stuck on a slow (10-second-timeout)
/// response delayed every header fetch behind it, so track changes showed
/// stale artwork for the whole pane backlog (2026-09-10 review finding).
/// Each lane now has its own dedicated worker thread:
///
/// * `header_tx` — unbounded; header volume is one request per track
///   change, and its worker serves nothing else, so the header can never
///   queue behind pane work.
/// * `pane_tx` — bounded at [`MAX_PENDING_PANE_ART_REQUESTS`]; a saturated
///   pane lane drops new pane requests at enqueue time instead of growing
///   pending work without bound.
struct ArtWorkerHandle {
    header_tx: std::sync::mpsc::Sender<ArtRequest>,
    pane_tx: std::sync::mpsc::SyncSender<ArtRequest>,
}

/// Get (or lazily create) the senders for the persistent art workers.
///
/// Returns `None` if a worker thread could not be spawned. Callers
/// should treat that as "remote album art unavailable" and skip
/// fetching — the local-tag-extraction path still works regardless.
fn art_workers() -> Option<&'static ArtWorkerHandle> {
    static WORKERS: OnceLock<Option<ArtWorkerHandle>> = OnceLock::new();
    WORKERS
        .get_or_init(|| {
            // Build before spawning so a policy-construction failure disables
            // remote artwork instead of silently restoring reqwest's permissive
            // default redirect and Referer behavior.
            let default_client = build_default_art_client()?;

            let (header_tx, header_rx) = std::sync::mpsc::channel::<ArtRequest>();
            let (pane_tx, pane_rx) =
                std::sync::mpsc::sync_channel::<ArtRequest>(MAX_PENDING_PANE_ART_REQUESTS);

            let spawn_header = spawn_art_lane("art-worker", header_rx, &default_client);
            let spawn_pane = spawn_art_lane("art-worker-pane", pane_rx, &default_client);

            match (spawn_header, spawn_pane) {
                (Ok(_), Ok(_)) => Some(ArtWorkerHandle { header_tx, pane_tx }),
                (header, pane) => {
                    tracing::warn!(
                        header = header.is_err(),
                        pane = pane.is_err(),
                        "Failed to spawn album art worker thread; remote album art will be skipped"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Build the shared secure HTTP client both remote-artwork lanes fetch
/// through. A policy-construction failure returns `None`, which disables
/// remote artwork instead of silently restoring reqwest's permissive
/// default redirect and Referer behavior.
fn build_default_art_client() -> Option<reqwest::blocking::Client> {
    match crate::http_security::authenticated_blocking_client_builder()
        .timeout(REMOTE_ART_TIMEOUT)
        .build()
    {
        Ok(client) => Some(client),
        Err(error) => {
            let error = crate::http_security::strip_request_url(error);
            tracing::warn!(%error, "Failed to build secure album art HTTP client");
            None
        }
    }
}

/// Spawn one persistent remote-artwork worker lane that consumes `rx`
/// until the sender side is dropped. Both lanes run identical fetch
/// semantics; only their queues differ (header unbounded, pane bounded).
fn spawn_art_lane(
    name: &str,
    rx: std::sync::mpsc::Receiver<ArtRequest>,
    default_client: &reqwest::blocking::Client,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(name.into()).spawn({
        let default_client = default_client.clone();
        move || {
            let mut routed_clients = HashMap::new();
            while let Ok(req) = rx.recv() {
                process_art_request(req, &default_client, &mut routed_clients);
            }
        }
    })
}

/// Execute one queued remote-artwork request on a worker lane.
///
/// Extracted verbatim from the former single-worker loop so the header and
/// pane lanes run identical fetch semantics (liveness gate, activity gate,
/// per-route client reuse, bounded body read).
fn process_art_request(
    req: ArtRequest,
    default_client: &reqwest::blocking::Client,
    routed_clients: &mut HashMap<
        crate::architecture::AdvertisedHttpRoute,
        reqwest::blocking::Client,
    >,
) {
    // Check if this request is still current before fetching.
    // A stale header generation or a revoked scoped token
    // closes the request without touching the network —
    // this is what keeps cancelled rows (and superseded
    // tracks) from burning worker time.
    if !req.liveness.is_valid() {
        return;
    }

    if !req.source.is_active() {
        return;
    }

    let request = match &req.source {
        ArtSource::Url(url) => default_client.get(url),
        ArtSource::Resolved(resolved) => {
            let Some(client) = resolved_art_client(resolved, default_client, routed_clients) else {
                return;
            };
            build_resolved_art_request(&client, resolved)
        }
    };

    send_and_deliver_art(request, req);
}

/// Resolve the HTTP client for one resolved remote source: the advertised
/// route's dedicated client when the request advertises a route (built and
/// cached on first use, bounded at [`MAX_ROUTED_ART_CLIENTS`]), otherwise
/// the shared default client. `None` drops the request — the route's
/// client could not be built within policy.
fn resolved_art_client(
    resolved: &crate::architecture::media::ResolvedHttpRequest,
    default_client: &reqwest::blocking::Client,
    routed_clients: &mut HashMap<
        crate::architecture::AdvertisedHttpRoute,
        reqwest::blocking::Client,
    >,
) -> Option<reqwest::blocking::Client> {
    let Some(route) = resolved.advertised_route() else {
        return Some(default_client.clone());
    };
    if let Some(client) = routed_clients.get(route) {
        return Some(client.clone());
    }
    let client = build_routed_art_client(resolved.endpoint(), route)?;
    if routed_clients.len() >= MAX_ROUTED_ART_CLIENTS {
        routed_clients.clear();
    }
    routed_clients.insert(route.clone(), client.clone());
    Some(client)
}

/// Perform one remote-artwork HTTP fetch and deliver the body to the
/// request's reply channel on success. The post-response liveness and
/// activity re-checks keep a cancelled row (or superseded track) from
/// receiving bytes fetched after its revocation.
fn send_and_deliver_art(request: reqwest::blocking::RequestBuilder, req: ArtRequest) {
    match request.timeout(REMOTE_ART_TIMEOUT).send() {
        Ok(resp) if resp.status().is_success() => {
            match crate::http_body::read_limited_blocking(
                resp,
                MAX_REMOTE_ART_BYTES,
                REMOTE_ART_TIMEOUT,
            ) {
                Ok(bytes)
                    if !bytes.is_empty() && req.source.is_active() && req.liveness.is_valid() =>
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

/// Extract embedded album art from a direct file URI and display it on the
/// header bar image widget.
///
/// This transitional path is retained for removable-media rows until their
/// at-use adapter provides retained file authority. Local-library, playlist,
/// and OS-opened external playback use [`update_resolved_file_album_art`]
/// instead.
pub fn update_direct_file_album_art(image: &gtk::Image, uri: &str) {
    let generation = next_generation();
    let Some(path) = direct_file_art_target(uri) else {
        image.set_icon_name(Some("audio-x-generic-symbolic"));
        return;
    };

    image.set_icon_name(Some("audio-x-generic-symbolic"));
    let reply_rx =
        enqueue_local_art_job(RequestLiveness::GlobalGeneration(generation), move || {
            extract_direct_file_album_art_bytes(&path)
        });
    display_local_album_art_reply(
        image,
        reply_rx,
        RequestLiveness::GlobalGeneration(generation),
    );
}

/// Scoped variant of [`update_direct_file_album_art`] for the browser
/// album pane's TRANSITIONAL arm: rows with no retained authority chain
/// (e.g., OS-opened external files) have no capability to resolve, so the
/// extraction opens the exact `file://` target behind the supplied
/// per-request token. Rows that carry a source identity never take this
/// path — [`update_resolved_file_album_art_scoped`] is their only local
/// entry point (2026-09-10 review finding).
pub fn update_direct_file_album_art_scoped(
    image: &gtk::Image,
    uri: &str,
    liveness: &ScopedArtFetch,
) {
    let Some(path) = direct_file_art_target(uri) else {
        image.set_icon_name(Some("audio-x-generic-symbolic"));
        return;
    };

    image.set_icon_name(Some("audio-x-generic-symbolic"));
    let reply_rx = enqueue_local_art_job(RequestLiveness::Scoped(liveness.clone()), move || {
        extract_direct_file_album_art_bytes(&path)
    });
    display_local_album_art_reply(image, reply_rx, RequestLiveness::Scoped(liveness.clone()));
}

/// Scoped variant of [`update_resolved_file_album_art`] for the browser
/// album pane: identical retained-authority extraction path, but the job's
/// liveness is the supplied per-request token instead of the process-wide
/// header generation, so concurrent pane rows never cancel one another and
/// a re-bound row's token stops its extractor before the file read.
///
/// This is the pane's ONLY local-file entry point: the extraction consumes
/// an exact retained capability and never reopens a database or URI
/// pathname (2026-09-10 review finding — the former raw-`file://` path
/// bypassed retained removable-media authority).
pub fn update_resolved_file_album_art_scoped(
    image: &gtk::Image,
    media: crate::local::resolver::ResolvedLocalMedia,
    liveness: &ScopedArtFetch,
) {
    image.set_icon_name(Some("audio-x-generic-symbolic"));
    let reply_rx = enqueue_local_art_job(RequestLiveness::Scoped(liveness.clone()), move || {
        extract_resolved_file_album_art_bytes(&media)
    });
    display_local_album_art_reply(image, reply_rx, RequestLiveness::Scoped(liveness.clone()));
}

/// Resolve the filesystem path behind a `file://` URI for embedded-art
/// extraction. `None` for every other scheme and for URIs that cannot be
/// converted to a host path.
fn direct_file_art_target(uri: &str) -> Option<std::path::PathBuf> {
    match url::Url::parse(uri) {
        Ok(u) if u.scheme() == "file" => u.to_file_path().ok(),
        _ => None,
    }
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
    let reply_rx =
        enqueue_local_art_job(RequestLiveness::GlobalGeneration(generation), move || {
            extract_resolved_file_album_art_bytes(&media)
        });
    display_local_album_art_reply(
        image,
        reply_rx,
        RequestLiveness::GlobalGeneration(generation),
    );
}

/// One queued local embedded-art extraction.
struct LocalArtJob {
    liveness: RequestLiveness,
    extract: Box<dyn FnOnce() -> Option<Vec<u8>> + Send>,
    reply_tx: async_channel::Sender<Vec<u8>>,
}

/// Get (or lazily create) the bounded job queue for the local-art pool.
///
/// The pool runs a fixed [`MAX_LOCAL_ART_WORKERS`] threads fed by one
/// queue bounded at [`MAX_PENDING_LOCAL_ART_JOBS`]. The former design
/// spawned one OS thread per scheduled extraction — one per visible-row
/// bind in a virtualized pane — so a fast scroll could mint dozens of
/// threads (2026-09-10 review finding). Jobs beyond the queue bound are
/// refused at enqueue time and their receivers observe a closed channel.
fn local_art_queue() -> Option<&'static async_channel::Sender<LocalArtJob>> {
    static QUEUE: OnceLock<Option<async_channel::Sender<LocalArtJob>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (job_tx, job_rx) =
                async_channel::bounded::<LocalArtJob>(MAX_PENDING_LOCAL_ART_JOBS);
            let mut spawned = 0;
            for worker_index in 0..MAX_LOCAL_ART_WORKERS {
                let job_rx = job_rx.clone();
                let spawn_result = std::thread::Builder::new()
                    .name(format!("local-art-worker-{worker_index}"))
                    .spawn(move || {
                        while let Ok(job) = job_rx.recv_blocking() {
                            if !job.liveness.is_valid() {
                                continue;
                            }
                            if let Some(bytes) = (job.extract)() {
                                if job.liveness.is_valid() {
                                    let _ = job.reply_tx.send_blocking(bytes);
                                }
                            }
                        }
                    });
                if let Err(error) = spawn_result {
                    tracing::warn!(%error, "Failed to spawn local album-art worker");
                    break;
                }
                spawned += 1;
            }
            (spawned > 0).then_some(job_tx)
        })
        .as_ref()
}

fn enqueue_local_art_job<F>(
    liveness: RequestLiveness,
    extract: F,
) -> async_channel::Receiver<Vec<u8>>
where
    F: FnOnce() -> Option<Vec<u8>> + Send + 'static,
{
    let (tx, rx) = async_channel::bounded::<Vec<u8>>(1);
    // Never schedule extraction for an already-revoked request: the row was
    // re-bound (or the track superseded) between scheduling and the first
    // poll, and the worker would exit at its first liveness check anyway.
    // Dropping `tx` closes the channel, so the awaiting reply observes a
    // closed receiver instead of waiting forever.
    if !liveness.is_valid() {
        return rx;
    }
    let queued = local_art_queue().is_some_and(|queue| {
        queue
            .try_send(LocalArtJob {
                liveness,
                extract: Box::new(extract),
                reply_tx: tx,
            })
            .is_ok()
    });
    if !queued {
        // The job was refused (pool queue full) or the pool is gone; the
        // job — including its reply sender — is dropped and the awaiting
        // reply observes a closed channel.
    }
    rx
}

fn display_local_album_art_reply(
    image: &gtk::Image,
    reply_rx: async_channel::Receiver<Vec<u8>>,
    liveness: RequestLiveness,
) {
    let image = image.clone();
    glib::MainContext::default().spawn_local(async move {
        if let Ok(data) = reply_rx.recv().await {
            if liveness.is_valid() {
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
    let extension = media.extension().map(str::to_owned);
    media
        .with_serialized_seekable_file(|mut file| {
            extract_album_art_bytes(&mut file, extension.as_deref())
        })
        .ok()
        .flatten()
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
    enqueue_remote_album_art(
        image,
        ArtSource::Url(cover_art_url.to_string()),
        RequestLiveness::GlobalGeneration(generation),
    );
}

/// Scoped variant of [`fetch_remote_album_art`] for the browser album
/// pane: the fetch's liveness is the supplied per-request token instead
/// of the process-wide header generation, so one row's fetch can never
/// cancel another row's (or the header's) in-flight request.
pub fn fetch_remote_album_art_scoped(
    image: &gtk::Image,
    cover_art_url: &str,
    liveness: &ScopedArtFetch,
) {
    enqueue_remote_album_art(
        image,
        ArtSource::Url(cover_art_url.to_string()),
        RequestLiveness::Scoped(liveness.clone()),
    );
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
    enqueue_remote_album_art(
        image,
        ArtSource::Resolved(Box::new(request)),
        RequestLiveness::GlobalGeneration(generation),
    );
}

/// Scoped variant of [`fetch_resolved_album_art`] for the browser album
/// pane: the request's liveness is the supplied per-request token instead
/// of the process-wide header generation. The lease and activity checks
/// are unchanged — only the staleness scope differs.
pub fn fetch_resolved_album_art_scoped(
    image: &gtk::Image,
    request: crate::architecture::media::ResolvedHttpRequest,
    liveness: &ScopedArtFetch,
) {
    if !liveness.is_live() || !request.is_active() {
        return;
    }
    enqueue_remote_album_art(
        image,
        ArtSource::Resolved(Box::new(request)),
        RequestLiveness::Scoped(liveness.clone()),
    );
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

fn enqueue_remote_album_art(image: &gtk::Image, source: ArtSource, liveness: RequestLiveness) {
    let image = image.clone();

    let reply_rx = enqueue_art_request(source, liveness.clone());

    // Receive on the GTK main thread.
    glib::MainContext::default().spawn_local(async move {
        if let Ok(data) = reply_rx.recv().await {
            // Double-check liveness in case the request was superseded
            // (header: newer generation; pane: revoked token) while we
            // were waiting for the channel.
            if liveness.is_valid() {
                let bytes = glib::Bytes::from_owned(data);
                if let Ok(texture) = gtk::gdk::Texture::from_bytes(&bytes) {
                    image.set_paintable(Some(&texture));
                }
            }
        }
    });
}

/// Submit one request through the production persistent workers and return its
/// one-shot completion. Keeping this GTK-independent makes the full
/// request/fetch/liveness boundary deterministic under headless CI; the UI
/// callback above adds the final liveness check before mutating the widget.
///
/// The now-playing header and the album pane are routed to isolated lanes:
/// the header's request is served by its dedicated worker, while the pane's
/// request lands in a bounded queue — a full pane queue drops the request at
/// enqueue time, which closes the returned receiver instead of growing
/// pending work without bound (2026-09-10 review finding).
fn enqueue_art_request(
    source: ArtSource,
    liveness: RequestLiveness,
) -> async_channel::Receiver<Vec<u8>> {
    let (reply_tx, reply_rx) = async_channel::bounded::<Vec<u8>>(1);

    let Some(workers) = art_workers() else {
        // No worker is available (thread spawn failed at startup):
        // silently skip — there's nothing to fetch with and the
        // placeholder icon will show instead. `reply_tx` is dropped so
        // the awaiting receiver observes a closed channel.
        return reply_rx;
    };

    let request = ArtRequest {
        source,
        liveness,
        reply_tx,
    };
    let is_header_request = matches!(request.liveness, RequestLiveness::GlobalGeneration(_));
    let queued = if is_header_request {
        workers.header_tx.send(request).is_ok()
    } else {
        workers.pane_tx.try_send(request).is_ok()
    };
    if !queued {
        // The request — and with it its reply sender — was refused
        // (pane lane full) or the lane is gone; the receiver observes a
        // closed channel rather than waiting forever.
    }

    reply_rx
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_channel::TryRecvError;
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
        let stale_reply = enqueue_local_art_job(
            RequestLiveness::GlobalGeneration(stale_generation),
            move || {
                request_seen_tx.send(()).expect("report delayed local read");
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release delayed local read");
                Some(b"stale-local-art".to_vec())
            },
        );
        request_seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("local artwork worker started");

        let current_generation = next_generation();
        let current_reply = enqueue_local_art_job(
            RequestLiveness::GlobalGeneration(current_generation),
            || Some(b"current-local-art".to_vec()),
        );
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
        let stale_reply = enqueue_art_request(
            ArtSource::Url(stale_url),
            RequestLiveness::GlobalGeneration(stale_generation),
        );
        request_seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("production worker started delayed request");

        let current_generation = next_generation();
        let current_reply = enqueue_art_request(
            ArtSource::Url(current_url),
            RequestLiveness::GlobalGeneration(current_generation),
        );
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

    /// Wait for a one-shot artwork reply with a deadline, without
    /// blocking forever: `Ok(bytes)` when published, `None` when the
    /// channel closed (dropped request) or the deadline passed.
    fn wait_for_reply(
        reply: &async_channel::Receiver<Vec<u8>>,
        deadline: Duration,
    ) -> Option<Vec<u8>> {
        let start = std::time::Instant::now();
        loop {
            match reply.try_recv() {
                Ok(bytes) => return Some(bytes),
                Err(TryRecvError::Closed) => return None,
                Err(TryRecvError::Empty) => {}
            }
            if start.elapsed() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Occupy the pane lane deterministically: one pane request blocked
    /// inside its fixture server (so the pane worker cannot advance) plus
    /// enough queued requests to fill the bounded lane. The guard
    /// releases the blocked request on drop and joins its server; the
    /// queued fillers carry pre-revoked tokens, so at release the worker
    /// skips them at the dequeue-time liveness gate and the lane drains
    /// instantly.
    struct PaneLaneSaturation {
        release_tx: Option<mpsc::SyncSender<()>>,
        server: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for PaneLaneSaturation {
        fn drop(&mut self) {
            if let Some(release_tx) = self.release_tx.take() {
                let _ = release_tx.send(());
            }
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
        }
    }

    fn saturate_pane_lane() -> PaneLaneSaturation {
        let (request_seen_tx, request_seen_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let (blocked_url, server) = spawn_art_fixture(b"blocked-pane-art", move || {
            request_seen_tx
                .send(())
                .expect("report blocked pane request");
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("release blocked pane request");
        });
        // Occupy the pane worker with the blocked request. request_seen
        // fires only after the worker dispatched it on the wire, so from
        // here on the pane worker is provably busy.
        let occupying = enqueue_art_request(
            ArtSource::Url(blocked_url),
            RequestLiveness::Scoped(ScopedArtFetch::new()),
        );
        request_seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("pane worker took the blocked request");

        // Fill the bounded lane to capacity. Every filler carries an
        // already-revoked token: it is never contacted while the lane
        // stays saturated, and at release the worker skips it at the
        // dequeue-time liveness gate with no network attempt, so the
        // lane drains instantly regardless of proxy or resolver state.
        let workers = art_workers().expect("art workers initialized");
        for _ in 0..MAX_PENDING_PANE_ART_REQUESTS {
            let filler_token = ScopedArtFetch::new();
            filler_token.revoke();
            let (reply_tx, _reply_rx) = async_channel::bounded::<Vec<u8>>(1);
            workers
                .pane_tx
                .try_send(ArtRequest {
                    source: ArtSource::Url("http://127.0.0.1:1/art".to_string()),
                    liveness: RequestLiveness::Scoped(filler_token),
                    reply_tx,
                })
                .expect("fill bounded pane lane");
        }
        let _ = occupying; // the blocked request's bytes go nowhere
        PaneLaneSaturation {
            release_tx: Some(release_tx),
            server: Some(server),
        }
    }

    /// The now-playing header must never wait behind album-pane work:
    /// with every pane slot blocked or queued, a fresh header request is
    /// still served promptly. On the former single serial worker the
    /// header queued behind the blocked pane fetch and starved for its
    /// full duration (2026-09-10 review finding).
    #[test]
    fn pane_lane_saturation_cannot_starve_the_header_lane() {
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _saturation = saturate_pane_lane();

        let (url, server) = spawn_art_fixture(b"header-art", || {});
        let header_reply = enqueue_art_request(
            ArtSource::Url(url),
            RequestLiveness::GlobalGeneration(next_generation()),
        );
        assert_eq!(
            wait_for_reply(&header_reply, Duration::from_secs(5)).as_deref(),
            Some(b"header-art".as_slice()),
            "the header lane must be served while the pane lane is fully saturated"
        );
        server.join().expect("join header fixture");
    }

    /// The pane lane is bounded: one more request than the bound allows
    /// must be refused at enqueue time — its receiver closes immediately
    /// (no bytes ever flow), so a pathological catalog cannot grow the
    /// pending backlog without limit (2026-09-10 review finding).
    #[test]
    fn saturated_pane_lane_drops_new_requests_without_network() {
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _saturation = saturate_pane_lane();

        // The probe targets a working fixture: if it were accepted (bug),
        // it would eventually deliver; refused (correct), the receiver is
        // closed at once and the fixture is never contacted.
        let (url, _never_contacted) = spawn_art_fixture(b"overflow-art", || {});
        let overflow = enqueue_art_request(
            ArtSource::Url(url),
            RequestLiveness::Scoped(ScopedArtFetch::new()),
        );
        assert!(
            matches!(overflow.try_recv(), Err(TryRecvError::Closed)),
            "a pane request beyond the lane bound must be dropped at enqueue"
        );
    }

    /// The local extraction pool bounds BOTH its worker count and its
    /// pending backlog: with the pool's two workers held by blocking
    /// extractions, a third blocking job must not start (the former
    /// design spawned one OS thread per bind, so it started immediately),
    /// a full pending queue must refuse further jobs, and a queued job
    /// must run once a worker frees (2026-09-10 review finding).
    /// Enqueue one blocking extraction job that reports on `seen_tx` and
    /// then parks until `release_rx` receives, modelling a slow worker.
    /// Returns the job's reply receiver so the caller can assert the
    /// admitted bytes arrive.
    fn enqueue_blocking_pool_job(
        liveness: RequestLiveness,
        seen_tx: mpsc::SyncSender<()>,
        release_rx: mpsc::Receiver<()>,
        payload: &'static [u8],
    ) -> async_channel::Receiver<Vec<u8>> {
        enqueue_local_art_job(liveness, move || {
            seen_tx.send(()).expect("report pool worker");
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("release pool worker");
            Some(payload.to_vec())
        })
    }

    /// Enqueue one blocking extraction job and wait until a pool worker
    /// is occupied by it. Returns the job's reply receiver and the handle
    /// that releases the worker.
    fn occupy_pool_worker(
        liveness: RequestLiveness,
        payload: &'static [u8],
    ) -> (async_channel::Receiver<Vec<u8>>, mpsc::SyncSender<()>) {
        let (seen_tx, seen_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let reply = enqueue_blocking_pool_job(liveness, seen_tx, release_rx, payload);
        expect_worker_occupied(seen_rx);
        (reply, release_tx)
    }

    /// Wait until one pool worker reports it is occupied by its blocking
    /// job (bounded wait so a regression fails instead of hanging).
    fn expect_worker_occupied(seen_rx: mpsc::Receiver<()>) {
        seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("pool worker occupied");
    }

    /// Fill the local art pool's pending backlog to capacity, leaving
    /// `reserved` slots for jobs already in flight, asserting each filler
    /// is admitted.
    fn fill_pending_backlog(queue: &async_channel::Sender<LocalArtJob>, reserved: usize) {
        for pending in reserved..MAX_PENDING_LOCAL_ART_JOBS {
            let (reply_tx, _reply_rx) = async_channel::bounded::<Vec<u8>>(1);
            queue
                .try_send(LocalArtJob {
                    liveness: RequestLiveness::Scoped(ScopedArtFetch::new()),
                    extract: Box::new(move || {
                        let _ = pending;
                        Some(b"filler".to_vec())
                    }),
                    reply_tx,
                })
                .expect("fill local art queue");
        }
    }

    #[test]
    fn local_art_pool_bounds_workers_and_pending_jobs() {
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Scoped tokens (not generations): they stay live until revoked,
        // so queued jobs are not invalidated by unrelated liveness churn.
        let scoped = || RequestLiveness::Scoped(ScopedArtFetch::new());

        let (reply1, release1_tx) = occupy_pool_worker(scoped(), b"one");
        let (reply2, release2_tx) = occupy_pool_worker(scoped(), b"two");

        let (seen3_tx, seen3_rx) = mpsc::sync_channel(0);
        let (release3_tx, release3_rx) = mpsc::sync_channel(0);
        let reply3 = enqueue_blocking_pool_job(scoped(), seen3_tx, release3_rx, b"three");
        assert!(
            seen3_rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "the pool must not grow a third worker for a third blocking job"
        );

        // Fill the pending backlog to capacity (job three occupies one
        // pending slot), then refuse one more.
        let queue = local_art_queue().expect("local art pool initialized");
        fill_pending_backlog(queue, 1);
        let (seen4_tx, _seen4_rx) = mpsc::sync_channel(0);
        let refused = enqueue_local_art_job(scoped(), move || {
            seen4_tx.send(()).expect("a refused job must never run");
            None
        });
        assert!(
            matches!(refused.try_recv(), Err(TryRecvError::Closed)),
            "a job beyond the pending bound must be refused at enqueue"
        );

        // Free both workers: the queued job must then run on the freed
        // worker, and every admitted job's bytes must arrive.
        release1_tx.send(()).expect("release worker one");
        release2_tx.send(()).expect("release worker two");
        seen3_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("queued job ran after a worker freed");
        release3_tx.send(()).expect("release worker three");
        assert_eq!(reply1.recv_blocking().expect("worker one reply"), b"one");
        assert_eq!(reply2.recv_blocking().expect("worker two reply"), b"two");
        assert_eq!(
            reply3.recv_blocking().expect("worker three reply"),
            b"three"
        );
    }

    /// The pane's scoped retained-authority entry point must publish the
    /// ORIGINAL authorized file's bytes through the extraction pool: the
    /// media carries the retained capability, so a pathname replacement
    /// cannot swap the artwork (2026-09-10 review finding — the raw
    /// file:// path freshly opened whatever the pathname then pointed to).
    #[cfg(unix)]
    #[test]
    fn scoped_resolved_extraction_publishes_retained_handle_bytes() {
        let root = tempfile::tempdir().expect("temporary authority root");
        let original = mp4_with_cover_art(b"original-retained-art");
        let replacement = mp4_with_cover_art(b"replacement-path-art");
        let media = authorized_media(root.path(), "track.m4a", &original);
        let path = root.path().join("track.m4a");
        std::fs::rename(&path, root.path().join("displaced.m4a")).expect("move admitted file");
        std::fs::write(&path, replacement).expect("install path replacement");

        let liveness = ScopedArtFetch::new();
        let reply = enqueue_local_art_job(RequestLiveness::Scoped(liveness.clone()), move || {
            extract_resolved_file_album_art_bytes(&media)
        });
        assert_eq!(
            reply.recv_blocking().expect("retained-handle art bytes"),
            b"original-retained-art",
            "extraction must read the retained capability, not the replaced path"
        );
    }

    /// A scoped (album-pane) request revoked before it is handed to the
    /// worker must never be extracted or fetched: the local job path
    /// refuses to spawn its extractor, and the remote worker closes the
    /// request at its first liveness check without touching the network.
    /// Together with [`scoped_fetch_revoked_mid_flight_drops_the_reply`]
    /// this pins both halves of the pre-fetch gate.
    #[test]
    fn scoped_fetch_revoked_before_enqueue_never_runs() {
        let liveness = ScopedArtFetch::new();
        liveness.revoke();

        let local_reply = enqueue_local_art_job(RequestLiveness::Scoped(liveness.clone()), || {
            panic!("a revoked scoped job must not spawn its extractor");
        });
        let remote_reply = enqueue_art_request(
            ArtSource::Url("http://127.0.0.1:1/art".to_string()),
            RequestLiveness::Scoped(liveness),
        );

        assert!(
            local_reply.recv_blocking().is_err(),
            "revoked local job must publish nothing"
        );
        assert!(
            remote_reply.recv_blocking().is_err(),
            "revoked remote request must publish nothing"
        );
    }

    /// The worker must consult the scoped token again after the response
    /// arrives: a row revoked while its fetch was in flight gets a closed
    /// reply, so neither the widget callback nor the pane's cache probe
    /// can observe bytes the user will never see. This is the scoped
    /// counterpart of
    /// [`delayed_worker_result_cannot_cross_a_newer_artwork_generation`].
    #[test]
    fn scoped_fetch_revoked_mid_flight_drops_the_reply() {
        // Serializes against the lane-saturation tests: this request
        // needs a free pane-lane slot to reach its fixture.
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (request_seen_tx, request_seen_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let liveness = ScopedArtFetch::new();
        let (stale_url, server) = spawn_art_fixture(b"late-scoped-art", move || {
            request_seen_tx.send(()).expect("report scoped request");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release scoped response");
        });

        let reply = enqueue_art_request(
            ArtSource::Url(stale_url),
            RequestLiveness::Scoped(liveness.clone()),
        );
        request_seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("production worker started scoped request");

        // The row is re-bound while the fetch is on the wire.
        liveness.revoke();
        release_tx.send(()).expect("release scoped response");

        assert!(
            reply.recv_blocking().is_err(),
            "a revoked scoped request must not publish its bytes"
        );
        server.join().expect("join scoped artwork fixture");
    }

    /// The local extractor thread must honor the scoped token after the
    /// extraction completed, too: bytes extracted for a row that was
    /// revoked mid-read are dropped instead of published.
    #[test]
    fn scoped_local_job_revoked_during_extract_publishes_nothing() {
        let liveness = ScopedArtFetch::new();
        let revoker = liveness.clone();
        let reply = enqueue_local_art_job(RequestLiveness::Scoped(liveness), move || {
            revoker.revoke();
            Some(b"mid-flight-local-art".to_vec())
        });
        assert!(
            reply.recv_blocking().is_err(),
            "bytes extracted under a revoked token must be dropped"
        );
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
