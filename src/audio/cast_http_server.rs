//! Embedded HTTP server for casting local files to Chromecast devices.
//!
//! The Chromecast Default Media Receiver can only play HTTP(S) URLs —
//! it cannot access `file:///` URIs.  This module provides a minimal,
//! LAN-only HTTP server that serves pre-registered local files via
//! UUID-keyed URLs.
//!
//! # Security
//!
//! - **LAN-only**: Binds exclusively to the machine's non-loopback LAN
//!   IPv4 address (via `local-ip-address`).
//! - **No directory listing**: Only pre-registered UUIDs are servable.
//! - **No path traversal**: File paths are stored in a `DashMap` keyed
//!   by random UUID — there is no URL-to-filesystem path mapping.
//! - **OS-assigned port**: Uses port 0 for dynamic assignment.
//! - **Graceful shutdown**: Can be stopped when no longer needed.

use std::net::{Ipv4Addr, SocketAddr};
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
use uuid::Uuid;

/// Shared state for the cast HTTP server.
#[derive(Clone)]
struct ServerState {
    /// Map of UUID → absolute file path for registered files.
    files: Arc<DashMap<String, PathBuf>>,
}

/// A running cast HTTP server instance.
pub struct CastHttpServer {
    /// The socket address the server is listening on (LAN IP + port).
    addr: SocketAddr,
    /// Registered file map (shared with the axum handler).
    files: Arc<DashMap<String, PathBuf>>,
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

        // Ensure we got an IPv4 address.
        let ipv4 = match lan_ip {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => {
                // Fallback: try to find an IPv4 address from the list.
                local_ip_address::list_afinet_netifas()
                    .map_err(|e| anyhow::anyhow!("Failed to list network interfaces: {e}"))?
                    .into_iter()
                    .find_map(|(_name, ip)| match ip {
                        std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => {
                            Some(v4)
                        }
                        _ => None,
                    })
                    .unwrap_or(Ipv4Addr::LOCALHOST)
            }
        };

        let files = Arc::new(DashMap::new());
        let state = ServerState {
            files: files.clone(),
        };

        let app = Router::new()
            .route("/cast/{id}", get(serve_file))
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
            files,
            abort_handle: join_handle.abort_handle(),
        })
    }

    /// Register a local file for serving.
    ///
    /// Returns the full HTTP URL that a Chromecast can load to stream
    /// the file, e.g. `http://192.168.1.42:54321/cast/<uuid>.flac`.
    pub fn register_file(&self, path: &std::path::Path) -> String {
        let id = Uuid::new_v4().to_string();

        // Preserve the file extension so the Chromecast can detect
        // the content type from the URL.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("bin");

        let url_id = format!("{id}.{ext}");
        self.files.insert(url_id.clone(), path.to_path_buf());

        let url = format!("http://{}/cast/{}", self.addr, url_id);
        debug!(url = %url, path = %path.display(), "Registered file for casting");
        url
    }

    /// The socket address the server is listening on.
    #[allow(dead_code)]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Shut down the server and clean up registered files.
    #[allow(dead_code)]
    pub fn shutdown(&self) {
        info!("Shutting down cast HTTP server");
        self.abort_handle.abort();
        self.files.clear();
    }
}

impl Drop for CastHttpServer {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

/// Axum handler: serve a registered file by UUID.
///
/// Supports HTTP byte-range requests (`Range: bytes=N-M`) for
/// Chromecast seeking.
async fn serve_file(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    // Look up the UUID in the registry.
    let Some(entry) = state.files.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = entry.value().clone();
    drop(entry); // Release the DashMap ref before I/O.

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
