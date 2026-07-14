//! Embedded HTTP server for streaming media to Chromecast devices.
//!
//! The Chromecast Default Media Receiver can only play HTTP(S) URLs — it
//! cannot access `file:///` URIs — so local files are served through a minimal,
//! LAN-only HTTP server keyed by random UUID.
//!
//! The same server also **proxies credential-bearing remote streams**. A
//! Subsonic, Jellyfin, or Plex stream URL carries the user's token in its query
//! string — and with Subsonic's plaintext auth mode, `p=enc:<hex>` is the
//! user's actual *password*, which unlike a token cannot be revoked. Handing
//! that URL to a Cast device would publish the credential to a device Tributary
//! does not control, on a LAN it does not control. Instead the receiver is given
//! an opaque ticket URL, and Tributary fetches the upstream itself, so the
//! credential never leaves this process.
//!
//! # Security
//!
//! - **Explicit-interface binding**: The Chromecast entry point binds to the
//!   machine's non-loopback LAN IPv4 address (via `local-ip-address`). Other
//!   in-process outputs may select a specific address, but wildcard addresses
//!   are rejected and the requested address family is preserved.
//! - **No directory listing**: Only pre-registered UUIDs are servable.
//! - **No path traversal**: File paths are stored in a `DashMap` keyed
//!   by random UUID — there is no URL-to-filesystem path mapping.
//! - **Not an open relay**: an upstream ticket resolves to a URL fixed at
//!   registration time. A caller cannot ask the proxy to fetch anything else,
//!   and only the `Range` header is forwarded upstream.
//! - **Credential tickets are explicitly revocable**: every new load revokes the
//!   previous credential ticket — including a load that turns out to be a local
//!   file or unauthenticated radio — and `stop()` revokes them all. At most one
//!   credential-bearing ticket is live at a time, and it dies when playback
//!   moves on rather than lingering until the next credentialed track.
//! - **Credential tickets expire**: upstream tickets have a hard, non-sliding
//!   24-hour lifetime from registration. Receiver requests, pause, and seek do
//!   not renew it. An already-admitted response may finish after expiration,
//!   but every later lookup receives the same 404 as an unknown or revoked
//!   ticket. Local-file routes keep their existing server-lifetime contract.
//! - **OS-assigned port**: Uses port 0 for dynamic assignment.
//! - **Graceful shutdown**: Can be stopped when no longer needed.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dashmap::DashMap;
use futures::TryStreamExt;
use tokio::net::TcpListener;
use tracing::{debug, error, info};
use url::Url;
use uuid::Uuid;

const OPAQUE_UPSTREAM_BODY_ERROR: &str = "upstream media body stream failed";

/// Hard maximum lifetime of a receiver-facing credential ticket.
///
/// This is deliberately absolute rather than sliding: receiver GET/Range
/// requests, pause, and seek must not let a compromised receiver extend a
/// ticket forever. Explicit playback lifecycle revocation can only shorten
/// this lifetime.
const UPSTREAM_TICKET_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// What a registered ticket resolves to.
///
/// `Clone` but deliberately **not** `Debug`: an `Upstream` holds a
/// credential-bearing URL, and the whole point of this type is that the URL
/// never gets printed, logged, or handed to a receiver.
#[derive(Clone)]
enum MediaSource {
    /// A local file, streamed from disk.
    Local(PathBuf),
    /// A remote stream that Tributary fetches on the receiver's behalf.
    Upstream {
        url: Box<Url>,
        /// Absolute, monotonic deadline. The entry is live only while the
        /// current instant is strictly before this value.
        expires_at: Instant,
    },
}

impl MediaSource {
    fn is_expired_at(&self, now: Instant) -> bool {
        matches!(
            self,
            Self::Upstream { expires_at, .. } if now >= *expires_at
        )
    }
}

/// Replace the credential-bearing registry entry with a newly issued ticket.
///
/// `registered_at` and `ttl` are injected so boundary behavior can be tested
/// without sleeping. Production always supplies [`Instant::now`] and
/// [`UPSTREAM_TICKET_TTL`]. An unrepresentable deadline expires immediately,
/// which is the fail-closed outcome.
fn replace_upstream_at(
    media: &DashMap<String, MediaSource>,
    ticket: String,
    url: &Url,
    registered_at: Instant,
    ttl: Duration,
) {
    revoke_upstreams_in(media);
    let expires_at = registered_at.checked_add(ttl).unwrap_or(registered_at);
    media.insert(
        ticket,
        MediaSource::Upstream {
            url: Box::new(url.clone()),
            expires_at,
        },
    );
}

fn revoke_upstreams_in(media: &DashMap<String, MediaSource>) {
    media.retain(|_, source| !matches!(source, MediaSource::Upstream { .. }));
}

/// Resolve one ticket using a caller-supplied monotonic clock.
///
/// The DashMap entry guard makes the live check, clone, and expired-entry
/// removal one atomic operation with respect to registration and revocation on
/// the same key. The clock is sampled only after that guard is acquired, so a
/// lookup delayed across its deadline cannot be admitted with a stale instant.
/// Once a live source is cloned, that admitted request may finish even if the
/// deadline passes or the registry entry is revoked afterward.
fn resolve_media_with_clock<F>(
    media: &DashMap<String, MediaSource>,
    ticket: &str,
    now: F,
) -> Option<MediaSource>
where
    F: FnOnce() -> Instant,
{
    match media.entry(ticket.to_owned()) {
        dashmap::mapref::entry::Entry::Occupied(entry) => {
            if entry.get().is_expired_at(now()) {
                entry.remove();
                None
            } else {
                Some(entry.get().clone())
            }
        }
        dashmap::mapref::entry::Entry::Vacant(_) => None,
    }
}

/// Shared state for the cast HTTP server.
#[derive(Clone)]
struct ServerState {
    /// Map of ticket → media source.
    media: Arc<DashMap<String, MediaSource>>,
    /// Client used for upstream fetches. Carries the shared exact-origin
    /// redirect policy, so a hostile redirect cannot walk the credential to
    /// another host or downgrade it to plaintext.
    upstream: reqwest::Client,
}

/// A running cast HTTP server instance.
pub struct CastHttpServer {
    /// The socket address the server is listening on (LAN IP + port).
    addr: SocketAddr,
    /// Registered ticket map (shared with the axum handler).
    media: Arc<DashMap<String, MediaSource>>,
    /// Handle to abort the server task on shutdown.
    abort_handle: tokio::task::AbortHandle,
}

impl CastHttpServer {
    /// Start a new cast HTTP server bound to the machine's LAN IP.
    ///
    /// The server binds to port 0 (OS-assigned) on the first
    /// non-loopback IPv4 address.  Returns `Err` if no LAN IP can
    /// be determined or if the listener fails to bind.
    pub async fn start() -> anyhow::Result<Self> {
        let lan_ip = local_ip_address::local_ip()
            .map_err(|e| anyhow::anyhow!("Failed to determine LAN IP: {e}"))?;

        // Ensure we got an IPv4 address. A loopback address is unusable
        // here — Chromecasts on the LAN cannot reach 127.0.0.1, so fail
        // loud rather than silently bind to something the device can
        // never connect to.
        let ipv4 = match lan_ip {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() => v4,
            _ => local_ip_address::list_afinet_netifas()
                .map_err(|e| anyhow::anyhow!("Failed to list network interfaces: {e}"))?
                .into_iter()
                .find_map(|(_name, ip)| match ip {
                    std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => {
                        Some(v4)
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No LAN-routable IPv4 address available — Chromecast \
                         cannot reach this host. Connect to a network and retry."
                    )
                })?,
        };

        Self::start_on(SocketAddr::from((ipv4, 0))).await
    }

    /// Start a cast-compatible media server on the requested local address.
    ///
    /// The supplied port is always replaced with `0`, allowing the OS to
    /// select an unused port. The IP address and address family are preserved
    /// exactly. Unspecified addresses cannot produce a receiver-usable ticket
    /// URL. Scoped and link-local IPv6 addresses are also rejected because a
    /// portable receiver URL cannot carry the required interface scope.
    pub(crate) async fn start_on(mut bind_addr: SocketAddr) -> anyhow::Result<Self> {
        if bind_addr.ip().is_unspecified() {
            anyhow::bail!("Cast HTTP server requires a specific bind address");
        }
        if let SocketAddr::V6(addr) = bind_addr {
            if addr.scope_id() != 0 || addr.ip().is_unicast_link_local() {
                anyhow::bail!(
                    "Cast HTTP server cannot form a portable receiver URL for scoped or \
                     link-local IPv6"
                );
            }
        }
        bind_addr.set_port(0);

        let media = Arc::new(DashMap::new());
        let state = ServerState {
            media: media.clone(),
            // No total timeout: a media stream is a length-unbounded playback
            // transport, the same reason P1.5 leaves audio streams uncapped.
            upstream: crate::http_security::authenticated_client_builder()
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to build the upstream media client: {e}"))?,
        };

        let app = Router::new()
            .route("/cast/{id}", get(serve_media))
            .with_state(state);

        let listener = TcpListener::bind(bind_addr).await?;
        let addr = listener.local_addr()?;

        info!(addr = %addr, "Cast HTTP server listening");

        let join_handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!(error = %e, "Cast HTTP server error");
            }
        });

        Ok(Self {
            addr,
            media,
            abort_handle: join_handle.abort_handle(),
        })
    }

    /// Register a local file for serving.
    ///
    /// Returns the full HTTP URL that a Chromecast can load to stream
    /// the file, e.g. `http://192.168.1.42:54321/cast/<uuid>.flac`.
    ///
    /// Local entries are insert-only: they are not expired and remain servable
    /// for the lifetime of the server (the app session). That is acceptable
    /// because access is gated by an unguessable random v4 UUID, the listener
    /// is LAN-only, the entry grants nothing but a file the user chose to cast,
    /// and the map is bounded by the number of distinct local files cast in a
    /// session. Credential-bearing entries are *not* treated this way — see
    /// [`Self::register_upstream`].
    pub fn register_file(&self, path: &std::path::Path) -> String {
        // Preserve the file extension so the Chromecast can detect
        // the content type from the URL.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("bin");
        let ticket = format!("{}.{ext}", Uuid::new_v4());

        self.media
            .insert(ticket.clone(), MediaSource::Local(path.to_path_buf()));

        let url = self.ticket_url(&ticket);
        debug!(url = %url, path = %path.display(), "Registered file for casting");
        url
    }

    /// Register a remote stream that Tributary will fetch on the receiver's
    /// behalf, and return an opaque ticket URL to hand to the device.
    ///
    /// `url` may carry a credential; it is held only in this process. The
    /// receiver sees nothing but the ticket.
    ///
    /// Registering a new upstream **revokes the previous one**, so at most one
    /// credential-bearing ticket is live at a time. A ticket that outlived its
    /// track would be a standing invitation to replay the user's stream, and
    /// unlike a local file it fronts a credential.
    ///
    /// The new ticket also receives a hard 24-hour lifetime from this
    /// registration. GET/Range requests do not slide that deadline, so pause,
    /// seek, and a restartable remote Stop retain the route only until it
    /// expires. Explicit revocation may end it sooner.
    pub fn register_upstream(&self, url: &Url) -> String {
        // Carry the upstream's media extension onto the ticket. The Cast
        // `content_type` is guessed from the URL it is handed, so an
        // extensionless ticket would advertise a proxied FLAC or Opus stream as
        // the default `audio/mpeg` and the receiver would refuse or misplay it.
        //
        // Only a known audio extension is copied: the ticket path must stay
        // opaque, and nothing from the upstream URL beyond this fixed set is
        // allowed to shape it.
        let ticket = match upstream_media_extension(url) {
            Some(extension) => format!("{}.{extension}", Uuid::new_v4()),
            None => Uuid::new_v4().to_string(),
        };

        replace_upstream_at(
            &self.media,
            ticket.clone(),
            url,
            Instant::now(),
            UPSTREAM_TICKET_TTL,
        );

        let ticket_url = self.ticket_url(&ticket);
        // The upstream URL is deliberately absent from this log line.
        debug!(url = %ticket_url, "Registered a proxied remote stream for casting");
        ticket_url
    }

    /// Drop every credential-bearing ticket, leaving local entries alone.
    pub fn revoke_upstreams(&self) {
        revoke_upstreams_in(&self.media);
    }

    fn ticket_url(&self, ticket: &str) -> String {
        format!("http://{}/cast/{}", self.addr, ticket)
    }

    /// The socket address the server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for CastHttpServer {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

/// The audio extension of an upstream URL, if it has a recognised one.
///
/// A Plex part key ends in `/file.flac`; a Subsonic `stream.view` has no
/// extension at all, in which case the receiver falls back to its default and
/// there is nothing more we can say from the URL alone.
///
/// The allow-list is deliberate: the ticket path is otherwise a bare UUID, and
/// only these fixed strings may ever be appended to it.
fn upstream_media_extension(url: &Url) -> Option<&'static str> {
    const AUDIO_EXTENSIONS: &[&str] = &[
        "mp3", "flac", "ogg", "oga", "opus", "wav", "aac", "m4a", "aiff", "aif", "wma",
    ];

    let last_segment = url.path_segments()?.next_back()?;
    let (_, extension) = last_segment.rsplit_once('.')?;
    AUDIO_EXTENSIONS
        .iter()
        .find(|known| known.eq_ignore_ascii_case(extension))
        .copied()
}

/// Axum handler: serve a registered ticket.
///
/// A ticket is either a local file or a remote stream we fetch on the
/// receiver's behalf. Expired, revoked, and unregistered tickets are
/// indistinguishable: all return 404. Resolving clones the source before any
/// I/O, so a response admitted before expiration may finish afterward while
/// subsequent lookups fail.
async fn serve_media(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(source) = resolve_media_with_clock(&state.media, &id, Instant::now) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    match source {
        MediaSource::Local(path) => serve_local_file(&path, &headers).await,
        MediaSource::Upstream { url, .. } => proxy_upstream(&state.upstream, &url, &headers).await,
    }
}

/// Fetch a credential-bearing stream and relay it to the receiver.
///
/// The upstream URL is fixed at registration, so this cannot be driven to fetch
/// an arbitrary target. Only `Range` is forwarded — none of the receiver's other
/// headers reach the user's music server. Errors never carry the URL, because a
/// `reqwest` error would otherwise print the credential straight into the log.
async fn proxy_upstream(client: &reqwest::Client, url: &Url, headers: &HeaderMap) -> Response {
    let mut request = client.get(url.clone());
    if let Some(range) = headers.get(header::RANGE) {
        request = request.header(header::RANGE, range.clone());
    }

    let upstream = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let error = crate::http_security::strip_request_url(error);
            error!(error = %error, "Upstream media fetch failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        error!(%status, "Upstream media fetch returned a failure status");
        return status.into_response();
    }

    let mut response = Response::builder().status(status);
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            response = response.header(name, value.clone());
        }
    }

    response
        .body(Body::from_stream(opaque_upstream_body_errors(
            upstream.bytes_stream(),
        )))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Discard a body-stream error before handing it to Axum.
///
/// A reqwest body error may retain and display the credential-bearing request
/// URL. Mapping it to a fresh, fixed error keeps that URL out of Hyper/Axum
/// diagnostics if the upstream fails after response headers were received.
fn opaque_upstream_body_errors<S, T, E>(
    stream: S,
) -> impl futures::Stream<Item = Result<T, std::io::Error>>
where
    S: futures::Stream<Item = Result<T, E>>,
{
    stream.map_err(|_| std::io::Error::other(OPAQUE_UPSTREAM_BODY_ERROR))
}

/// Stream a local file, honoring `Range` requests so the receiver can seek.
async fn serve_local_file(path: &std::path::Path, headers: &HeaderMap) -> Response {
    let path = path.to_path_buf();

    // Open the file.
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) => {
            error!(error = %e, path = %path.display(), "Failed to stat registered file");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let file_size = metadata.len();

    // Determine content type from extension.
    let content_type = match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "aac" | "m4a" => "audio/mp4",
        "aiff" | "aif" => "audio/aiff",
        "wma" => "audio/x-ms-wma",
        _ => "application/octet-stream",
    };

    // Parse Range header for byte-range support.
    if let Some(range_header) = headers.get(header::RANGE) {
        if let Ok(range_str) = range_header.to_str() {
            if let Some(range) = parse_range_header(range_str, file_size) {
                let (start, end) = range;
                let length = end - start + 1;

                let file = match tokio::fs::File::open(&path).await {
                    Ok(f) => f,
                    Err(e) => {
                        error!(error = %e, "Failed to open file for range request");
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                };

                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut file = file;
                if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                    error!(error = %e, "Failed to seek in file");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }

                let stream = tokio_util::io::ReaderStream::new(file.take(length));
                let body = Body::from_stream(stream);

                return Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CONTENT_LENGTH, length.to_string())
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{file_size}"),
                    )
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(body)
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        }
    }

    // Full file response.
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            error!(error = %e, path = %path.display(), "Failed to open registered file");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Parse an HTTP `Range` header value like `bytes=0-1023`.
///
/// Returns `Some((start, end))` for a valid single byte range,
/// `None` for unsupported multi-range or invalid values.
fn parse_range_header(header: &str, file_size: u64) -> Option<(u64, u64)> {
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
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV6};

    use futures::StreamExt;

    use super::*;

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

    // ── Proxy ticket shape ──────────────────────────────────────────

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

    // ── Credential-ticket lifetime ───────────────────────────────────

    #[test]
    fn upstream_ticket_is_live_strictly_before_but_not_at_its_deadline() {
        let media = DashMap::new();
        let registered_at = Instant::now();
        let ttl = Duration::from_secs(10);
        let deadline = registered_at.checked_add(ttl).expect("test deadline");
        replace_upstream_at(
            &media,
            "ticket".to_string(),
            &url("https://music.test/stream?api_key=secret"),
            registered_at,
            ttl,
        );

        assert!(matches!(
            resolve_media_with_clock(&media, "ticket", || deadline
                .checked_sub(Duration::from_nanos(1))
                .expect("instant before deadline")),
            Some(MediaSource::Upstream { .. })
        ));
        assert!(resolve_media_with_clock(&media, "ticket", || deadline).is_none());
        assert!(
            !media.contains_key("ticket"),
            "the equality-boundary lookup must atomically remove the expired entry"
        );
    }

    #[test]
    fn upstream_lookups_do_not_slide_the_absolute_deadline() {
        let media = DashMap::new();
        let registered_at = Instant::now();
        let ttl = Duration::from_secs(12);
        let halfway = registered_at
            .checked_add(Duration::from_secs(6))
            .expect("halfway instant");
        let deadline = registered_at.checked_add(ttl).expect("test deadline");
        replace_upstream_at(
            &media,
            "ticket".to_string(),
            &url("https://music.test/stream?X-Plex-Token=secret"),
            registered_at,
            ttl,
        );

        assert!(resolve_media_with_clock(&media, "ticket", || halfway).is_some());
        assert!(resolve_media_with_clock(&media, "ticket", || deadline).is_none());
    }

    #[test]
    fn local_file_routes_keep_their_server_lifetime_contract() {
        let media = DashMap::new();
        let now = Instant::now();
        media.insert(
            "local.flac".to_string(),
            MediaSource::Local(PathBuf::from("/music/local.flac")),
        );
        let much_later = now
            .checked_add(Duration::from_secs(365 * 24 * 60 * 60))
            .expect("one year later");

        assert!(matches!(
            resolve_media_with_clock(&media, "local.flac", || much_later),
            Some(MediaSource::Local(_))
        ));
        assert!(media.contains_key("local.flac"));
    }

    #[test]
    fn explicit_revocation_and_supersession_end_tickets_before_their_ttl() {
        let media = DashMap::new();
        let first_registered = Instant::now();
        let ttl = Duration::from_secs(10);
        replace_upstream_at(
            &media,
            "first".to_string(),
            &url("https://music.test/first?api_key=secret"),
            first_registered,
            ttl,
        );
        revoke_upstreams_in(&media);
        assert!(resolve_media_with_clock(&media, "first", || first_registered).is_none());

        replace_upstream_at(
            &media,
            "old".to_string(),
            &url("https://music.test/old?api_key=secret"),
            first_registered,
            ttl,
        );
        let replacement_registered = first_registered
            .checked_add(Duration::from_secs(5))
            .expect("replacement instant");
        replace_upstream_at(
            &media,
            "new".to_string(),
            &url("https://music.test/new?api_key=secret"),
            replacement_registered,
            ttl,
        );

        assert!(resolve_media_with_clock(&media, "old", || replacement_registered).is_none());
        assert!(
            resolve_media_with_clock(&media, "new", || first_registered
                .checked_add(ttl)
                .expect("old deadline"))
            .is_some(),
            "replacement registration must receive a fresh deadline"
        );
        assert!(
            resolve_media_with_clock(&media, "new", || replacement_registered
                .checked_add(ttl)
                .expect("new deadline"))
            .is_none()
        );
    }

    #[test]
    fn an_admitted_source_may_finish_after_expiry_but_future_lookups_fail() {
        let media = DashMap::new();
        let registered_at = Instant::now();
        let ttl = Duration::from_secs(10);
        let deadline = registered_at.checked_add(ttl).expect("test deadline");
        let upstream = url("https://music.test/stream?api_key=admitted-secret");
        replace_upstream_at(&media, "ticket".to_string(), &upstream, registered_at, ttl);

        let admitted = resolve_media_with_clock(&media, "ticket", || registered_at)
            .expect("request admitted before expiry");
        assert!(resolve_media_with_clock(&media, "ticket", || deadline).is_none());
        match admitted {
            MediaSource::Upstream { url, .. } => assert_eq!(*url, upstream),
            MediaSource::Local(_) => panic!("expected admitted upstream source"),
        }
    }

    #[tokio::test]
    async fn expired_revoked_and_unknown_tickets_all_return_not_found() {
        let media = Arc::new(DashMap::new());
        let state = ServerState {
            media: Arc::clone(&media),
            upstream: crate::http_security::authenticated_client_builder()
                .build()
                .expect("test upstream client"),
        };

        replace_upstream_at(
            &media,
            "expired".to_string(),
            &url("https://music.test/expired?api_key=secret"),
            Instant::now(),
            Duration::ZERO,
        );
        let expired = serve_media(
            State(state.clone()),
            Path("expired".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(expired.status(), StatusCode::NOT_FOUND);
        assert!(!media.contains_key("expired"));

        replace_upstream_at(
            &media,
            "revoked".to_string(),
            &url("https://music.test/revoked?api_key=secret"),
            Instant::now(),
            UPSTREAM_TICKET_TTL,
        );
        revoke_upstreams_in(&media);
        for ticket in ["revoked", "unknown"] {
            let response = serve_media(
                State(state.clone()),
                Path(ticket.to_string()),
                HeaderMap::new(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{ticket}");
        }
    }

    // ── Listener binding and receiver URLs ─────────────────────────────

    #[tokio::test]
    async fn start_on_ipv4_loopback_preserves_ip_and_ignores_the_supplied_port() {
        let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("reserve an IPv4 port");
        let requested = reserved.local_addr().expect("reserved address");

        let server = CastHttpServer::start_on(requested)
            .await
            .expect("bind on IPv4 loopback with a fresh ephemeral port");

        assert_eq!(server.addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(server.addr().port(), 0);
        assert_ne!(
            server.addr().port(),
            requested.port(),
            "the occupied caller-supplied port must be replaced with zero"
        );
    }

    #[tokio::test]
    async fn start_on_ipv6_loopback_formats_a_bracketed_ticket_when_available() {
        let Ok(reserved) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await else {
            // IPv6 can be disabled by the test host or container.
            return;
        };
        let requested = reserved.local_addr().expect("reserved address");

        let server = CastHttpServer::start_on(requested)
            .await
            .expect("bind on IPv6 loopback with a fresh ephemeral port");
        let ticket = server.ticket_url("opaque.flac");

        assert_eq!(server.addr().ip(), IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_ne!(server.addr().port(), requested.port());
        assert_eq!(
            ticket,
            format!("http://[::1]:{}/cast/opaque.flac", server.addr().port())
        );
        Url::parse(&ticket).expect("the bracketed IPv6 ticket must be a valid URL");
    }

    #[tokio::test]
    async fn start_on_rejects_unspecified_addresses() {
        for requested in [
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 1234)),
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 1234)),
        ] {
            let error = CastHttpServer::start_on(requested)
                .await
                .err()
                .expect("an unspecified bind address must be rejected");
            assert!(error.to_string().contains("specific bind address"));
        }
    }

    #[tokio::test]
    async fn start_on_rejects_scoped_and_link_local_ipv6_addresses() {
        let scoped = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 1234, 0, 7));
        let link_local = SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
            1234,
            0,
            0,
        ));

        for requested in [scoped, link_local] {
            let error = CastHttpServer::start_on(requested)
                .await
                .err()
                .expect("a non-portable IPv6 address must be rejected");
            assert!(error.to_string().contains("portable receiver URL"));
        }
    }

    #[tokio::test]
    async fn upstream_body_errors_are_opaque_before_axum_observes_them() {
        const SECRET: &str = "https://music.test/stream?token=body-stream-secret";
        let original =
            futures::stream::once(async { Err::<Vec<u8>, _>(std::io::Error::other(SECRET)) });
        let mapped = opaque_upstream_body_errors(original);
        futures::pin_mut!(mapped);

        let error = mapped
            .next()
            .await
            .expect("one stream item")
            .expect_err("the body item should fail");
        let rendered = format!("{error:?} {error}");

        assert_eq!(error.to_string(), OPAQUE_UPSTREAM_BODY_ERROR);
        assert!(!rendered.contains(SECRET));
        assert!(!rendered.contains("body-stream-secret"));
    }
}
