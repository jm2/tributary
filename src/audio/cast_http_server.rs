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
//! - **LAN-only**: Binds exclusively to the machine's non-loopback LAN
//!   IPv4 address (via `local-ip-address`).
//! - **No directory listing**: Only pre-registered UUIDs are servable.
//! - **No path traversal**: File paths are stored in a `DashMap` keyed
//!   by random UUID — there is no URL-to-filesystem path mapping.
//! - **Not an open relay**: an upstream ticket resolves to a URL fixed at
//!   registration time. A caller cannot ask the proxy to fetch anything else,
//!   and only the `Range` header is forwarded upstream.
//! - **Credential tickets are short-lived**: registering a new upstream revokes
//!   the previous one, so at most one credential-bearing ticket is live.
//! - **OS-assigned port**: Uses port 0 for dynamic assignment.
//! - **Graceful shutdown**: Can be stopped when no longer needed.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dashmap::DashMap;
use tokio::net::TcpListener;
use tracing::{debug, error, info};
use url::Url;
use uuid::Uuid;

/// What a registered ticket resolves to.
///
/// Deliberately not `Debug`: an `Upstream` holds a credential-bearing URL, and
/// the whole point of this type is that the URL never gets printed, logged, or
/// handed to a receiver.
enum MediaSource {
    /// A local file, streamed from disk.
    Local(PathBuf),
    /// A remote stream that Tributary fetches on the receiver's behalf.
    Upstream(Box<Url>),
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

        let listener = TcpListener::bind(SocketAddr::from((ipv4, 0))).await?;
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
    pub fn register_upstream(&self, url: &Url) -> String {
        self.revoke_upstreams();

        let ticket = Uuid::new_v4().to_string();
        self.media
            .insert(ticket.clone(), MediaSource::Upstream(Box::new(url.clone())));

        let ticket_url = self.ticket_url(&ticket);
        // The upstream URL is deliberately absent from this log line.
        debug!(url = %ticket_url, "Registered a proxied remote stream for casting");
        ticket_url
    }

    /// Drop every credential-bearing ticket, leaving local entries alone.
    pub fn revoke_upstreams(&self) {
        self.media
            .retain(|_, source| !matches!(source, MediaSource::Upstream(_)));
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

/// Axum handler: serve a registered ticket.
///
/// A ticket is either a local file or a remote stream we fetch on the
/// receiver's behalf. Unregistered tickets are indistinguishable from any other
/// unknown path: 404.
async fn serve_media(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = state.media.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Clone out and release the DashMap ref before doing any I/O — holding it
    // across an await would block every other request on this shard.
    let source = match entry.value() {
        MediaSource::Local(path) => MediaSource::Local(path.clone()),
        MediaSource::Upstream(url) => MediaSource::Upstream(url.clone()),
    };
    drop(entry);

    match source {
        MediaSource::Local(path) => serve_local_file(&path, &headers).await,
        MediaSource::Upstream(url) => proxy_upstream(&state.upstream, &url, &headers).await,
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
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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
}
