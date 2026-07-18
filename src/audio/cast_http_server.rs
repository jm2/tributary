//! Embedded HTTP server for streaming media to Chromecast devices.
//!
//! The Chromecast Default Media Receiver can only play HTTP(S) URLs — it
//! cannot access `file:///` URIs — so local files are served through a minimal,
//! LAN-only HTTP server keyed by random UUID.
//!
//! The same server also **proxies authenticated remote streams**. Newly
//! resolved Subsonic, Jellyfin, Plex, and DAAP requests keep authentication
//! separate from their clean endpoint.
//! Handing either form to a receiver would publish account access to a device
//! Tributary does not control. Instead the receiver receives an opaque ticket,
//! and Tributary performs the authenticated upstream fetch itself.
//!
//! # Security
//!
//! - **Explicit-interface binding**: The Chromecast entry point binds to the
//!   machine's non-loopback LAN IPv4 address (via `local-ip-address`). Other
//!   in-process outputs may select a specific address, but wildcard addresses
//!   are rejected and the requested address family is preserved.
//! - **No directory listing**: Only pre-registered UUIDs are servable.
//! - **No path traversal**: Legacy explicit paths and playback-time retained
//!   file authorities are stored in a `DashMap` keyed by random UUID — there
//!   is no URL-to-filesystem path mapping.
//! - **Not an open relay**: an upstream ticket resolves to a URL fixed at
//!   registration time. A caller cannot ask the proxy to fetch anything else,
//!   and only the `Range` header is forwarded upstream.
//! - **Credential tickets are explicitly revocable**: every new load revokes the
//!   previous credential ticket — including a load that turns out to be a local
//!   file or unauthenticated radio — and `stop()` revokes them all. Playback-time
//!   retained-file routes follow the same load lifecycle. At most one
//!   credential-bearing ticket is live at a time, and it dies when playback
//!   moves on rather than lingering until the next credentialed track.
//! - **Credential tickets expire**: upstream tickets have a hard, non-sliding
//!   24-hour lifetime from registration. Receiver requests, pause, and seek do
//!   not renew it. An already-admitted response may finish after expiration,
//!   but every later lookup receives the same 404 as an unknown or revoked
//!   ticket. Legacy explicit-file routes keep their server-lifetime contract;
//!   playback-time local-authority routes are revoked with their owning load.
//! - **OS-assigned port**: Uses port 0 for dynamic assignment.
//! - **Graceful shutdown**: Can be stopped when no longer needed.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dashmap::DashMap;
use futures::StreamExt;
use tokio::net::TcpListener;
use tracing::{debug, error, info};
use url::Url;
use uuid::Uuid;

use crate::architecture::media::ResolvedHttpRequest;
use crate::architecture::AdvertisedHttpRoute;
use crate::local::resolver::ResolvedLocalMedia;

const OPAQUE_UPSTREAM_BODY_ERROR: &str = "upstream media body stream failed";

/// Bound connection establishment independently from the lifetime of a media
/// stream. This is intentionally shorter than GStreamer's default 15-second
/// HTTP-source I/O timeout so the app-owned proxy reports the failure first.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum wall-clock time from dispatch until upstream response headers.
///
/// This also bounds DNS and TLS work that occurs outside reqwest's narrower
/// connect timeout. It is not applied to the response body.
const UPSTREAM_RESPONSE_HEADER_DEADLINE: Duration = Duration::from_secs(10);

/// Maximum silence between consecutive upstream body chunks.
///
/// The deadline restarts after every chunk. A valid stream can therefore run
/// indefinitely while a wedged upstream is cut off before the downstream
/// GStreamer source's own blocking-I/O timeout.
const UPSTREAM_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ROUTED_UPSTREAM_CLIENTS: usize = 64;

const STAGE_INBOUND_TICKET: &str = "inbound_ticket";
const STAGE_TICKET_REGISTRATION: &str = "ticket_registration";
const STAGE_UPSTREAM_START: &str = "upstream_start";
const STAGE_CONNECT: &str = "connect";
const STAGE_RESPONSE_HEADERS: &str = "response_headers";
const STAGE_UPSTREAM_STATUS: &str = "upstream_status";
const STAGE_BODY: &str = "body";

const CATEGORY_ACCEPTED: &str = "accepted";
const CATEGORY_ATTEMPT: &str = "attempt";
const CATEGORY_RECEIVED: &str = "received";
const CATEGORY_ISSUED: &str = "issued";
const CATEGORY_DEADLINE: &str = "deadline";
const CATEGORY_TRANSPORT: &str = "transport";
const CATEGORY_HTTP_FAILURE: &str = "http_failure";

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Hard maximum lifetime of a receiver-facing credential ticket.
///
/// This is deliberately absolute rather than sliding: receiver GET/Range
/// requests, pause, and seek must not let a compromised receiver extend a
/// ticket forever. Explicit playback lifecycle revocation can only shorten
/// this lifetime.
const UPSTREAM_TICKET_TTL: Duration = Duration::from_hours(24);

#[derive(Clone, Copy)]
struct UpstreamTimeouts {
    response_headers: Duration,
    body_idle: Duration,
}

impl Default for UpstreamTimeouts {
    fn default() -> Self {
        Self {
            response_headers: UPSTREAM_RESPONSE_HEADER_DEADLINE,
            body_idle: UPSTREAM_BODY_IDLE_TIMEOUT,
        }
    }
}

/// Cloneable, credential-free transport for protected upstream media.
///
/// `reqwest::Client` clones share their connection pool. Keeping this wrapper
/// outside an individual [`CastHttpServer`] therefore lets local/AirPlay
/// playback reuse established origin connections across per-track loopback
/// servers without moving any request credential into the client itself. The
/// exact-origin/no-Referer policy remains fixed by the private constructor.
#[derive(Clone)]
pub struct UpstreamMediaClient {
    http: reqwest::Client,
    routed_http: Arc<DashMap<AdvertisedHttpRoute, reqwest::Client>>,
    connect_timeout: Duration,
    timeouts: UpstreamTimeouts,
}

impl UpstreamMediaClient {
    /// Build the shared protected-media transport.
    pub(crate) fn new() -> anyhow::Result<Self> {
        Self::build_with_timeouts(UPSTREAM_CONNECT_TIMEOUT, UpstreamTimeouts::default())
    }

    fn build_with_timeouts(
        connect_timeout: Duration,
        timeouts: UpstreamTimeouts,
    ) -> anyhow::Result<Self> {
        // Deliberately do not set a total request timeout or reqwest read
        // timeout. Header establishment and each body read are bounded at the
        // call sites below, so an active media stream has no total lifetime.
        let http = crate::http_security::authenticated_client_builder()
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|_| anyhow::anyhow!("Failed to build the upstream media client"))?;
        Ok(Self {
            http,
            routed_http: Arc::new(DashMap::new()),
            connect_timeout,
            timeouts,
        })
    }

    /// Select an immutable connection pool for one advertised route snapshot.
    /// The ordinary client remains the fallback for legacy and DNS-routed
    /// requests; a route never mutates a client already serving another
    /// source/session.
    fn http_for(&self, request: &UpstreamRequest) -> Result<reqwest::Client, ()> {
        let UpstreamRequest::Resolved(resolved) = request else {
            return Ok(self.http.clone());
        };
        let Some(route) = resolved.advertised_route() else {
            return Ok(self.http.clone());
        };
        if let Some(client) = self.routed_http.get(route) {
            return Ok(client.clone());
        }

        let builder = crate::http_security::authenticated_client_builder()
            .connect_timeout(self.connect_timeout);
        let builder = crate::http_security::apply_advertised_http_route(
            builder,
            resolved.endpoint(),
            Some(route),
        )
        .map_err(|_| ())?;
        let client = builder.build().map_err(|_| ())?;
        if self.routed_http.len() >= MAX_ROUTED_UPSTREAM_CLIENTS {
            self.routed_http.clear();
        }
        self.routed_http.insert(route.clone(), client.clone());
        Ok(client)
    }
}

/// What a registered ticket resolves to.
///
/// `Clone` but deliberately **not** `Debug`: an `Upstream` retains protected
/// request state that must never be printed, logged, or handed to a receiver.
#[derive(Clone)]
enum MediaSource {
    /// Legacy path-based local file registration used outside library-ID
    /// resolution.
    LocalPath(PathBuf),
    /// An exact library file and its retained root/file authority.
    LocalAuthority(ResolvedLocalMedia),
    /// A remote stream that Tributary fetches on the receiver's behalf.
    Upstream {
        request: UpstreamRequest,
        /// Absolute, monotonic deadline. The entry is live only while the
        /// current instant is strictly before this value.
        expires_at: Instant,
    },
}

/// The fixed request behind an upstream ticket.
///
/// Deliberately not `Debug`: both variants may retain credentials. The legacy
/// variant exists only for the URI-boundary defense in depth; resolved backend
/// media uses the typed variant so its clean endpoint and authentication
/// material cannot be separated or accidentally sent directly.
#[derive(Clone)]
enum UpstreamRequest {
    Legacy(Box<Url>),
    Resolved(Box<ResolvedHttpRequest>),
}

impl UpstreamRequest {
    fn endpoint(&self) -> &Url {
        match self {
            Self::Legacy(url) => url,
            Self::Resolved(request) => request.endpoint(),
        }
    }

    fn is_active(&self) -> bool {
        match self {
            Self::Legacy(_) => true,
            Self::Resolved(request) => request.is_active(),
        }
    }
}

impl MediaSource {
    fn is_expired_at(&self, now: Instant) -> bool {
        matches!(
            self,
            Self::Upstream {
                request,
                expires_at,
            } if now >= *expires_at || !request.is_active()
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
    request: UpstreamRequest,
    registered_at: Instant,
    ttl: Duration,
) {
    revoke_upstreams_in(media);
    let expires_at = registered_at.checked_add(ttl).unwrap_or(registered_at);
    media.insert(
        ticket,
        MediaSource::Upstream {
            request,
            expires_at,
        },
    );
}

fn revoke_upstreams_in(media: &DashMap<String, MediaSource>) {
    media.retain(|_, source| !matches!(source, MediaSource::Upstream { .. }));
}

/// Resolve one ticket using a caller-supplied monotonic clock.
///
/// The borrowed-key lookup avoids allocating a `String` for every media
/// request. The clock is sampled only after its entry guard is acquired, so a
/// lookup delayed across the deadline cannot be admitted with a stale instant.
/// An expired entry is removed conditionally after releasing that guard: if a
/// replacement somehow wins the gap, it is removed only when it too was
/// already expired at the sampled instant. Once a live source is cloned, that
/// admitted request may finish even if the deadline passes or the registry
/// entry is revoked afterward.
fn resolve_media_with_clock<F>(
    media: &DashMap<String, MediaSource>,
    ticket: &str,
    now: F,
) -> Option<MediaSource>
where
    F: FnOnce() -> Instant,
{
    let source = media.get(ticket)?;
    let observed_at = now();

    if !source.is_expired_at(observed_at) {
        return Some(source.clone());
    }

    drop(source);
    media.remove_if(ticket, |_, current| current.is_expired_at(observed_at));
    None
}

/// Shared state for the cast HTTP server.
#[derive(Clone)]
struct ServerState {
    /// Map of ticket → media source.
    media: Arc<DashMap<String, MediaSource>>,
    /// Client used for upstream fetches. Carries the shared exact-origin
    /// redirect policy, so a hostile redirect cannot walk the credential to
    /// another host or downgrade it to plaintext.
    upstream: UpstreamMediaClient,
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
    pub(crate) async fn start_on(bind_addr: SocketAddr) -> anyhow::Result<Self> {
        let upstream = UpstreamMediaClient::new()?;
        Self::start_on_with_upstream_client(bind_addr, upstream).await
    }

    /// Start a server with a clone of an existing protected-media transport.
    ///
    /// The injected wrapper cannot be constructed with a weaker redirect
    /// policy outside this module. Reusing it across per-track loopback
    /// servers preserves connection pooling without sharing ticket registries
    /// or their revocation lifecycles.
    pub(crate) async fn start_on_with_upstream_client(
        mut bind_addr: SocketAddr,
        upstream: UpstreamMediaClient,
    ) -> anyhow::Result<Self> {
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
            upstream,
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
            .insert(ticket.clone(), MediaSource::LocalPath(path.to_path_buf()));

        let url = self.ticket_url(&ticket);
        debug!(url = %url, path = %path.display(), "Registered file for casting");
        url
    }

    /// Register one playback-time local authority lease.
    ///
    /// The map owns the lease rather than a pathname. Each admitted request
    /// clones the exact retained file handle, so a later unlink/replacement at
    /// the database path cannot retarget a receiver to different bytes.
    /// Registration itself performs no filesystem I/O: the bounded blocking
    /// handler revalidates root and file authority immediately before every
    /// handle clone, so a dead network root cannot stall the UI handoff.
    pub(crate) fn register_local(&self, media: ResolvedLocalMedia) -> String {
        let extension = media.extension().and_then(|extension| {
            PROTECTED_TICKET_AUDIO_EXTENSIONS
                .iter()
                .find(|known| known.eq_ignore_ascii_case(extension))
                .copied()
        });
        let ticket = match extension {
            Some(extension) => format!("{}.{extension}", Uuid::new_v4()),
            None => Uuid::new_v4().to_string(),
        };
        self.media
            .insert(ticket.clone(), MediaSource::LocalAuthority(media));
        self.ticket_url(&ticket)
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
        self.register_upstream_request(UpstreamRequest::Legacy(Box::new(url.clone())))
    }

    /// Register a resolved backend request and return an opaque receiver URL.
    ///
    /// Typed requests are never eligible for direct playback: the clean
    /// endpoint, sensitive headers, and private query material remain joined
    /// behind this app-owned route. An already-retired source lease is
    /// rejected before a ticket is issued; retirement after registration is
    /// enforced again on every lookup.
    pub(crate) fn register_resolved(&self, request: ResolvedHttpRequest) -> Option<String> {
        if !request.is_active() {
            return None;
        }
        let lease_probe = request.clone();
        let ticket = self.register_upstream_request(UpstreamRequest::Resolved(Box::new(request)));
        if !lease_probe.is_active() {
            self.revoke_upstreams();
            return None;
        }
        Some(ticket)
    }

    fn register_upstream_request(&self, request: UpstreamRequest) -> String {
        // Carry the upstream's media extension onto the ticket. The Cast
        // `content_type` is guessed from the URL it is handed, so an
        // extensionless ticket would advertise a proxied FLAC or Opus stream as
        // the default `audio/mpeg` and the receiver would refuse or misplay it.
        //
        // Only a known audio extension is copied: the ticket path must stay
        // opaque, and nothing from the upstream URL beyond this fixed set is
        // allowed to shape it.
        let ticket = match upstream_media_extension(request.endpoint()) {
            Some(extension) => format!("{}.{extension}", Uuid::new_v4()),
            None => Uuid::new_v4().to_string(),
        };

        replace_upstream_at(
            &self.media,
            ticket.clone(),
            request,
            Instant::now(),
            UPSTREAM_TICKET_TTL,
        );

        let ticket_url = self.ticket_url(&ticket);
        // Neither the protected endpoint nor its bearer ticket belongs in
        // diagnostics. The fixed stage/category is enough to correlate setup.
        debug!(
            stage = STAGE_TICKET_REGISTRATION,
            category = CATEGORY_ISSUED,
            "Protected media proxy stage"
        );
        ticket_url
    }

    /// Drop every credential-bearing ticket, leaving local entries alone.
    pub fn revoke_upstreams(&self) {
        revoke_upstreams_in(&self.media);
    }

    /// Revoke every route owned by this output generation.
    pub(crate) fn revoke_all(&self) {
        self.media.clear();
    }

    fn ticket_url(&self, ticket: &str) -> String {
        format!("http://{}/cast/{}", self.addr, ticket)
    }

    /// The socket address the server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    #[cfg(test)]
    pub(crate) fn detached_for_test(runtime: &tokio::runtime::Handle, addr: SocketAddr) -> Self {
        let task = runtime.spawn(std::future::pending::<()>());
        Self {
            addr,
            media: Arc::new(DashMap::new()),
            abort_handle: task.abort_handle(),
        }
    }

    #[cfg(test)]
    pub(crate) fn registered_route_count(&self) -> usize {
        self.media.len()
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
pub(super) const PROTECTED_TICKET_AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "wav", "aac", "m4a", "aiff", "aif", "wma",
];

fn upstream_media_extension(url: &Url) -> Option<&'static str> {
    let last_segment = url.path_segments()?.next_back()?;
    let (_, extension) = last_segment.rsplit_once('.')?;
    PROTECTED_TICKET_AUDIO_EXTENSIONS
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
        MediaSource::LocalPath(path) => serve_local_file(&path, &headers).await,
        MediaSource::LocalAuthority(media) => serve_authorized_local_file(media, &headers).await,
        MediaSource::Upstream { request, .. } => {
            debug!(
                stage = STAGE_INBOUND_TICKET,
                category = CATEGORY_ACCEPTED,
                "Protected media proxy stage"
            );
            proxy_upstream(&state.upstream, &request, &headers).await
        }
    }
}

/// Fetch an authenticated stream and relay it to the receiver.
///
/// The upstream URL is fixed at registration, so this cannot be driven to fetch
/// an arbitrary target. Only `Range` is forwarded — none of the receiver's other
/// headers reach the user's music server. Fixed protocol headers belong to the
/// resolved request and are applied from its separate trusted allowlist.
/// Transport errors are classified without formatting them because a
/// `reqwest` error may retain the complete
/// credential-bearing URL.
async fn proxy_upstream(
    client: &UpstreamMediaClient,
    upstream_request: &UpstreamRequest,
    receiver_headers: &HeaderMap,
) -> Response {
    if !upstream_request.is_active() {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Private query material exists in a temporary request URL only for the
    // duration of this fetch. It is absent from the registry key, ticket URL,
    // logs, and every receiver-facing value.
    let mut upstream_url = upstream_request.endpoint().clone();
    if let UpstreamRequest::Resolved(resolved) = upstream_request {
        let mut query = upstream_url.query_pairs_mut();
        for (name, value) in resolved.private_query_pairs() {
            query.append_pair(name, value);
        }
    }

    let Ok(http) = client.http_for(upstream_request) else {
        error!(
            stage = STAGE_CONNECT,
            category = CATEGORY_TRANSPORT,
            elapsed_ms = 0_u64,
            "Protected media proxy failure"
        );
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let mut request = http.get(upstream_url);
    if let UpstreamRequest::Resolved(resolved) = upstream_request {
        request = request.headers(resolved.required_headers().clone());
        request = request.headers(resolved.sensitive_headers().clone());
    }
    if let Some(range) = receiver_headers.get(header::RANGE) {
        request = request.header(header::RANGE, range.clone());
    }

    let started_at = Instant::now();
    debug!(
        stage = STAGE_UPSTREAM_START,
        category = CATEGORY_ATTEMPT,
        "Protected media proxy stage"
    );
    let upstream =
        match tokio::time::timeout(client.timeouts.response_headers, request.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) if error.is_timeout() => {
                let stage = if error.is_connect() {
                    STAGE_CONNECT
                } else {
                    STAGE_RESPONSE_HEADERS
                };
                error!(
                    stage,
                    category = CATEGORY_DEADLINE,
                    elapsed_ms = elapsed_millis(started_at),
                    "Protected media proxy failure"
                );
                return StatusCode::GATEWAY_TIMEOUT.into_response();
            }
            Ok(Err(error)) => {
                let stage = if error.is_connect() {
                    STAGE_CONNECT
                } else {
                    STAGE_RESPONSE_HEADERS
                };
                error!(
                    stage,
                    category = CATEGORY_TRANSPORT,
                    elapsed_ms = elapsed_millis(started_at),
                    "Protected media proxy failure"
                );
                return StatusCode::BAD_GATEWAY.into_response();
            }
            Err(_) => {
                error!(
                    stage = STAGE_RESPONSE_HEADERS,
                    category = CATEGORY_DEADLINE,
                    elapsed_ms = elapsed_millis(started_at),
                    "Protected media proxy failure"
                );
                return StatusCode::GATEWAY_TIMEOUT.into_response();
            }
        };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    debug!(
        stage = STAGE_RESPONSE_HEADERS,
        category = CATEGORY_RECEIVED,
        elapsed_ms = elapsed_millis(started_at),
        "Protected media proxy stage"
    );
    debug!(
        stage = STAGE_UPSTREAM_STATUS,
        category = CATEGORY_RECEIVED,
        status = status.as_u16(),
        "Protected media proxy stage"
    );
    if !status.is_success() {
        error!(
            stage = STAGE_UPSTREAM_STATUS,
            category = CATEGORY_HTTP_FAILURE,
            status = status.as_u16(),
            elapsed_ms = elapsed_millis(started_at),
            "Protected media proxy failure"
        );
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
        .body(Body::from_stream(upstream_body_with_idle_timeout(
            upstream.bytes_stream(),
            client.timeouts.body_idle,
        )))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Bound each body read and discard transport errors before handing them to
/// Axum.
///
/// A reqwest body error may retain and display the credential-bearing request
/// URL. Mapping it to a fresh, fixed error keeps that URL out of Hyper/Axum
/// diagnostics if the upstream fails after response headers were received.
/// The timeout restarts after every successful chunk, so this is an idle
/// deadline rather than a total stream lifetime.
fn upstream_body_with_idle_timeout<S, T, E>(
    stream: S,
    idle_timeout: Duration,
) -> impl futures::Stream<Item = Result<T, std::io::Error>>
where
    S: futures::Stream<Item = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let stream: Pin<Box<S>> = Box::pin(stream);
    let started_at = Instant::now();
    futures::stream::unfold(Some(stream), move |state| async move {
        let mut stream = state?;
        match tokio::time::timeout(idle_timeout, stream.next()).await {
            Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(stream))),
            Ok(Some(Err(_))) => {
                error!(
                    stage = STAGE_BODY,
                    category = CATEGORY_TRANSPORT,
                    elapsed_ms = elapsed_millis(started_at),
                    "Protected media proxy failure"
                );
                Some((Err(std::io::Error::other(OPAQUE_UPSTREAM_BODY_ERROR)), None))
            }
            Ok(None) => None,
            Err(_) => {
                error!(
                    stage = STAGE_BODY,
                    category = CATEGORY_DEADLINE,
                    elapsed_ms = elapsed_millis(started_at),
                    "Protected media proxy failure"
                );
                Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        OPAQUE_UPSTREAM_BODY_ERROR,
                    )),
                    None,
                ))
            }
        }
    })
}

/// Stream a local file, honoring `Range` requests so the receiver can seek.
async fn serve_local_file(path: &std::path::Path, headers: &HeaderMap) -> Response {
    let path = path.to_path_buf();
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) => {
            error!(error = %e, path = %path.display(), "Failed to open registered file");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let file_size = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            error!(%error, "Failed to inspect registered file");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    serve_open_local_file(file, file_size, extension, headers).await
}

/// Stream a playback-time authorized file from its retained handle.
async fn serve_authorized_local_file(media: ResolvedLocalMedia, headers: &HeaderMap) -> Response {
    let extension = media.extension().unwrap_or("").to_string();
    let opened = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            let file = media.try_clone_file()?;
            let file_size = file.metadata()?.len();
            Ok::<_, std::io::Error>((file, file_size))
        }),
    )
    .await;
    let (file, file_size) = match opened {
        Ok(Ok(Ok(opened))) => opened,
        Ok(Ok(Err(error))) => {
            error!(
                category = ?error.kind(),
                "Retained local media authority is no longer usable"
            );
            return StatusCode::NOT_FOUND.into_response();
        }
        Ok(Err(_)) => {
            error!("Retained local media authority task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(_) => {
            error!("Retained local media authority check timed out");
            return StatusCode::GATEWAY_TIMEOUT.into_response();
        }
    };
    serve_open_authorized_file(file, file_size, &extension, headers)
}

/// Serve an authorized handle with position-independent reads.
///
/// `File::try_clone` may share one cursor with the retained descriptor. Using
/// ordinary `Read`/`Seek` would therefore let concurrent or sequential Range
/// requests corrupt one another's offsets. A bounded blocking producer uses
/// `read_at`/`seek_read` against explicit offsets, preserving the exact handle
/// without reopening its pathname.
fn serve_open_authorized_file(
    file: std::fs::File,
    file_size: u64,
    extension: &str,
    headers: &HeaderMap,
) -> Response {
    let requested_range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range_header(value, file_size));
    let (status, start, length, content_range) = match requested_range {
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{file_size}")),
        ),
        None => (StatusCode::OK, 0, file_size, None),
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, local_content_type(extension))
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(content_range) = content_range {
        response = response.header(header::CONTENT_RANGE, content_range);
    }
    response
        .body(authorized_file_body(file, start, length))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn authorized_file_body(file: std::fs::File, start: u64, length: u64) -> Body {
    const CHUNK_BYTES: usize = 64 * 1024;
    const BUFFERED_CHUNKS: usize = 2;

    let (sender, receiver) =
        async_channel::bounded::<Result<Vec<u8>, std::io::Error>>(BUFFERED_CHUNKS);
    drop(tokio::task::spawn_blocking(move || {
        let mut offset = start;
        let mut remaining = length;
        while remaining > 0 {
            let chunk_len = usize::try_from(remaining.min(CHUNK_BYTES as u64))
                .expect("bounded authorized-media chunk length fits usize");
            let mut chunk = vec![0; chunk_len];
            match read_file_at(&file, &mut chunk, offset) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(0) => {
                    let _ = sender.send_blocking(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "authorized media ended before its retained size",
                    )));
                    break;
                }
                Ok(read) => {
                    chunk.truncate(read);
                    offset = offset.saturating_add(read as u64);
                    remaining = remaining.saturating_sub(read as u64);
                    if sender.send_blocking(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send_blocking(Err(error));
                    break;
                }
            }
        }
    }));
    Body::from_stream(receiver)
}

#[cfg(unix)]
fn read_file_at(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;

    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn read_file_at(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;

    file.seek_read(buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_file_at(_file: &std::fs::File, _buffer: &mut [u8], _offset: u64) -> std::io::Result<usize> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "position-independent authorized media reads are unsupported on this platform",
    ))
}

fn local_content_type(extension: &str) -> &'static str {
    match extension.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "aac" | "m4a" => "audio/mp4",
        "aiff" | "aif" => "audio/aiff",
        "wma" => "audio/x-ms-wma",
        _ => "application/octet-stream",
    }
}

async fn serve_open_local_file(
    mut file: tokio::fs::File,
    file_size: u64,
    extension: &str,
    headers: &HeaderMap,
) -> Response {
    let content_type = local_content_type(extension);

    // Parse Range header for byte-range support.
    if let Some(range_header) = headers.get(header::RANGE) {
        if let Ok(range_str) = range_header.to_str() {
            if let Some(range) = parse_range_header(range_str, file_size) {
                let (start, end) = range;
                let length = end - start + 1;

                use tokio::io::{AsyncReadExt, AsyncSeekExt};
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

    use axum::extract::OriginalUri;
    use axum::http::Uri;
    use futures::StreamExt;
    use reqwest::header::{
        HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, COOKIE, REFERER, USER_AGENT,
    };

    use crate::architecture::media::MediaLease;

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

    fn legacy(value: &str) -> UpstreamRequest {
        UpstreamRequest::Legacy(Box::new(url(value)))
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
            legacy("https://music.test/stream?api_key=secret"),
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
            legacy("https://music.test/stream?X-Plex-Token=secret"),
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
            MediaSource::LocalPath(PathBuf::from("/music/local.flac")),
        );
        let much_later = now
            .checked_add(Duration::from_hours(365 * 24))
            .expect("one year later");

        assert!(matches!(
            resolve_media_with_clock(&media, "local.flac", || much_later),
            Some(MediaSource::LocalPath(_))
        ));
        assert!(media.contains_key("local.flac"));
    }

    #[tokio::test]
    async fn authorized_local_ticket_serves_retained_file_and_revokes() {
        let root = tempfile::tempdir().expect("temporary library root");
        let marker = format!("marker:v1:{}", Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .expect("write root marker");
        let path = root.path().join("track.flac");
        let displaced = root.path().join("admitted.flac");
        std::fs::write(&path, b"authorized").expect("write admitted media");

        let media = ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
            .expect("retain local media authority");
        #[cfg(unix)]
        let invalidation_media = media.clone();
        let registry = Arc::new(DashMap::new());
        let server_task = tokio::spawn(std::future::pending::<()>());
        let server = CastHttpServer {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 46_000)),
            media: Arc::clone(&registry),
            abort_handle: server_task.abort_handle(),
        };
        let state = ServerState {
            media: registry,
            upstream: UpstreamMediaClient::new().expect("test upstream client"),
        };
        let ticket = server.register_local(media);
        let ticket_id = Url::parse(&ticket)
            .expect("parse ticket URL")
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .expect("ticket path")
            .to_string();

        match std::fs::rename(&path, &displaced) {
            Ok(()) => {
                std::fs::write(&path, b"replacement").expect("install pathname replacement");
            }
            Err(error) => {
                #[cfg(not(windows))]
                panic!("move admitted pathname: {error}");
                #[cfg(windows)]
                {
                    let _ = error;
                    // Windows retains the stronger namespace pin because
                    // authority handles intentionally omit delete sharing.
                    assert_eq!(
                        std::fs::read(&path).expect("read pinned path"),
                        b"authorized"
                    );
                }
            }
        }

        let response = serve_media(
            State(state.clone()),
            Path(ticket_id.clone()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read media response")
                .as_ref(),
            b"authorized"
        );

        let mut range_headers = HeaderMap::new();
        range_headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-5"));
        let range = serve_media(State(state.clone()), Path(ticket_id.clone()), range_headers).await;
        assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            axum::body::to_bytes(range.into_body(), usize::MAX)
                .await
                .expect("read ranged media response")
                .as_ref(),
            b"thor"
        );

        let replay = serve_media(
            State(state.clone()),
            Path(ticket_id.clone()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(replay.into_body(), usize::MAX)
                .await
                .expect("read replayed media response")
                .as_ref(),
            b"authorized"
        );

        server.revoke_all();
        let revoked = serve_media(State(state.clone()), Path(ticket_id), HeaderMap::new()).await;
        assert_eq!(revoked.status(), StatusCode::NOT_FOUND);

        #[cfg(unix)]
        {
            let invalidated_ticket = server.register_local(invalidation_media);
            let invalidated_id = Url::parse(&invalidated_ticket)
                .expect("parse invalidated ticket URL")
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .expect("invalidated ticket path")
                .to_string();
            let replacement_marker = format!("marker:v1:{}", Uuid::new_v4());
            std::fs::write(
                root.path().join(".tributary-root-id"),
                format!("{replacement_marker}\n"),
            )
            .expect("replace marker content");

            let invalidated =
                serve_media(State(state), Path(invalidated_id), HeaderMap::new()).await;
            assert_eq!(invalidated.status(), StatusCode::NOT_FOUND);
        }
    }

    #[test]
    fn explicit_revocation_and_supersession_end_tickets_before_their_ttl() {
        let media = DashMap::new();
        let first_registered = Instant::now();
        let ttl = Duration::from_secs(10);
        replace_upstream_at(
            &media,
            "first".to_string(),
            legacy("https://music.test/first?api_key=secret"),
            first_registered,
            ttl,
        );
        revoke_upstreams_in(&media);
        assert!(resolve_media_with_clock(&media, "first", || first_registered).is_none());

        replace_upstream_at(
            &media,
            "old".to_string(),
            legacy("https://music.test/old?api_key=secret"),
            first_registered,
            ttl,
        );
        let replacement_registered = first_registered
            .checked_add(Duration::from_secs(5))
            .expect("replacement instant");
        replace_upstream_at(
            &media,
            "new".to_string(),
            legacy("https://music.test/new?api_key=secret"),
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
        replace_upstream_at(
            &media,
            "ticket".to_string(),
            UpstreamRequest::Legacy(Box::new(upstream.clone())),
            registered_at,
            ttl,
        );

        let admitted = resolve_media_with_clock(&media, "ticket", || registered_at)
            .expect("request admitted before expiry");
        assert!(resolve_media_with_clock(&media, "ticket", || deadline).is_none());
        match admitted {
            MediaSource::Upstream { request, .. } => {
                assert_eq!(request.endpoint(), &upstream);
            }
            MediaSource::LocalPath(_) | MediaSource::LocalAuthority(_) => {
                panic!("expected admitted upstream source")
            }
        }
    }

    #[tokio::test]
    async fn expired_revoked_and_unknown_tickets_all_return_not_found() {
        let media = Arc::new(DashMap::new());
        let state = ServerState {
            media: Arc::clone(&media),
            upstream: UpstreamMediaClient::new().expect("test upstream client"),
        };

        replace_upstream_at(
            &media,
            "expired".to_string(),
            legacy("https://music.test/expired?api_key=secret"),
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
            legacy("https://music.test/revoked?api_key=secret"),
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

    async fn capture_request(
        State(tx): State<tokio::sync::mpsc::UnboundedSender<(Uri, HeaderMap)>>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
    ) -> Response {
        let _ = tx.send((uri, headers));
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "audio/mpeg")
            .body(Body::from("media"))
            .expect("capture response")
    }

    async fn start_capture_server() -> (
        SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<(Uri, HeaderMap)>,
        tokio::task::AbortHandle,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new()
            .route("/reverse-proxy/library/stream", get(capture_request))
            .route("/explicit-proxy/stream", get(capture_request))
            .with_state(tx);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("capture listener");
        let addr = listener.local_addr().expect("capture address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("capture server");
        });
        (addr, rx, task.abort_handle())
    }

    #[tokio::test]
    async fn routed_resolved_proxy_preserves_origin_and_contains_auth_and_lifetime() {
        const ADVERTISED_HOST: &str = "tributary-advertised-route.invalid";
        const PRIVATE_USER: &str = "proxy-user-value";
        const PRIVATE_PASSWORD: &str = "proxy-password-value";
        const EXPECTED_AUTH: &str = "Bearer request-owned-value";
        const EXPECTED_ACCEPT: &str = "application/x-dmap-tagged";
        const EXPECTED_USER_AGENT: &str = "Tributary/test-required-value";
        const EXPECTED_DAAP_VERSION: &str = "3.12";
        const EXPECTED_DAAP_ACCESS_INDEX: &str = "2";

        let (upstream_addr, mut captures, upstream_abort) = start_capture_server().await;
        let endpoint = Url::parse(&format!(
            "http://{ADVERTISED_HOST}:{}/reverse-proxy/library/stream?track=42",
            upstream_addr.port()
        ))
        .expect("clean advertised endpoint");
        let route_origin = Url::parse(&format!(
            "http://{ADVERTISED_HOST}:{}/",
            upstream_addr.port()
        ))
        .expect("advertised origin");
        let route = AdvertisedHttpRoute::new(&route_origin, [upstream_addr])
            .expect("exact-origin advertised route");
        let lease = MediaLease::new();
        let request = ResolvedHttpRequest::new(endpoint)
            .expect("resolved request")
            .with_required_header(ACCEPT, HeaderValue::from_static(EXPECTED_ACCEPT))
            .expect("allowlisted Accept")
            .with_required_header(USER_AGENT, HeaderValue::from_static(EXPECTED_USER_AGENT))
            .expect("allowlisted User-Agent")
            .with_required_header(
                HeaderName::from_static("client-daap-version"),
                HeaderValue::from_static(EXPECTED_DAAP_VERSION),
            )
            .expect("allowlisted DAAP version")
            .with_required_header(
                HeaderName::from_static("client-daap-access-index"),
                HeaderValue::from_static(EXPECTED_DAAP_ACCESS_INDEX),
            )
            .expect("allowlisted DAAP access index")
            .with_sensitive_header(AUTHORIZATION, HeaderValue::from_static(EXPECTED_AUTH))
            .expect("allowlisted header")
            .with_private_query_pair("u", PRIVATE_USER)
            .expect("private user")
            .with_private_query_pair("p", PRIVATE_PASSWORD)
            .expect("private password")
            .with_advertised_route(route)
            .expect("matching advertised route")
            .with_lease(lease.clone());

        let server = CastHttpServer::start_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("proxy server");
        let ticket = server
            .register_resolved(request.clone())
            .expect("active request gets a ticket");
        assert!(!ticket.contains(ADVERTISED_HOST));
        assert!(!ticket.contains(PRIVATE_USER));
        assert!(!ticket.contains(PRIVATE_PASSWORD));
        assert!(!ticket.contains("request-owned-value"));
        assert!(!ticket.contains("track=42"));

        let response = reqwest::Client::new()
            .get(&ticket)
            .header(header::RANGE, "bytes=7-11")
            .header(ACCEPT, "receiver/controlled")
            .header(USER_AGENT, "Receiver/controlled")
            .header("client-daap-version", "receiver-controlled")
            .header("client-daap-access-index", "receiver-controlled")
            .header(COOKIE, "receiver-cookie-value")
            .header(REFERER, "https://receiver.invalid/")
            .header(AUTHORIZATION, "Bearer receiver-owned-value")
            .header("x-plex-token", "receiver-token-value")
            .header("x-receiver-custom", "receiver-custom-value")
            .send()
            .await
            .expect("ticket fetch");
        assert!(response.status().is_success());
        assert_eq!(response.bytes().await.expect("proxied body"), "media");

        let (captured_uri, captured_headers) =
            tokio::time::timeout(Duration::from_secs(2), captures.recv())
                .await
                .expect("capture timeout")
                .expect("captured request");
        let query: Vec<_> = captured_uri
            .query()
            .unwrap_or_default()
            .split('&')
            .collect();
        let expected_host = format!("{ADVERTISED_HOST}:{}", upstream_addr.port());
        assert_eq!(
            captured_uri.path(),
            "/reverse-proxy/library/stream",
            "the reverse-proxy base path must survive the protected fetch"
        );
        assert_eq!(
            captured_headers
                .get(header::HOST)
                .expect("upstream Host header")
                .to_str()
                .expect("ASCII Host header"),
            expected_host,
            "the transport route must not replace the advertised HTTP origin"
        );
        assert!(
            query.contains(&"track=42"),
            "public endpoint query is preserved"
        );
        let expected_user = format!("u={PRIVATE_USER}");
        let expected_password = format!("p={PRIVATE_PASSWORD}");
        assert!(
            query.contains(&expected_user.as_str()),
            "private user pair is applied upstream"
        );
        assert!(
            query.contains(&expected_password.as_str()),
            "private password pair is applied upstream"
        );
        assert!(
            captured_headers.get(AUTHORIZATION) == Some(&HeaderValue::from_static(EXPECTED_AUTH)),
            "request-owned authorization is applied upstream"
        );
        for (name, expected) in [
            (ACCEPT, EXPECTED_ACCEPT),
            (USER_AGENT, EXPECTED_USER_AGENT),
            (
                HeaderName::from_static("client-daap-version"),
                EXPECTED_DAAP_VERSION,
            ),
            (
                HeaderName::from_static("client-daap-access-index"),
                EXPECTED_DAAP_ACCESS_INDEX,
            ),
        ] {
            assert_eq!(
                captured_headers.get(&name),
                Some(&HeaderValue::from_static(expected)),
                "trusted request-required header must beat a receiver conflict"
            );
        }
        assert!(
            captured_headers.get(header::RANGE) == Some(&HeaderValue::from_static("bytes=7-11")),
            "receiver Range is forwarded"
        );
        for absent in [
            COOKIE,
            REFERER,
            HeaderName::from_static("x-plex-token"),
            HeaderName::from_static("x-receiver-custom"),
        ] {
            assert!(
                captured_headers.get(absent).is_none(),
                "receiver-controlled headers are not forwarded"
            );
        }

        lease.revoke();
        assert!(
            server.register_resolved(request).is_none(),
            "an inactive source cannot mint another ticket"
        );
        let retired = reqwest::get(&ticket).await.expect("retired ticket fetch");
        assert_eq!(retired.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), captures.recv())
                .await
                .is_err(),
            "an inactive lease must fail before another upstream request"
        );

        upstream_abort.abort();
    }

    #[tokio::test]
    async fn resolved_fetch_uses_an_explicit_upstream_http_proxy() {
        const UPSTREAM_HOST: &str = "cast-explicit-upstream.invalid";

        let (proxy_addr, mut captures, proxy_abort) = start_capture_server().await;
        let endpoint = Url::parse(&format!(
            "http://{UPSTREAM_HOST}/explicit-proxy/stream?track=77"
        ))
        .expect("clean upstream endpoint");
        let request = ResolvedHttpRequest::new(endpoint)
            .expect("resolved request")
            .with_private_query_pair("session-id", "private-session")
            .expect("private session")
            .with_required_header(
                USER_AGENT,
                HeaderValue::from_static("Tributary/explicit-proxy-test"),
            )
            .expect("allowlisted User-Agent");

        let proxy =
            reqwest::Proxy::all(format!("http://{proxy_addr}")).expect("explicit local HTTP proxy");
        let http = crate::http_security::authenticated_client_builder()
            .proxy(proxy)
            .connect_timeout(Duration::from_secs(2))
            .build()
            .expect("explicit-proxy upstream client");
        let client = UpstreamMediaClient {
            http,
            routed_http: Arc::new(DashMap::new()),
            connect_timeout: Duration::from_secs(2),
            timeouts: UpstreamTimeouts {
                response_headers: Duration::from_secs(2),
                body_idle: Duration::from_secs(2),
            },
        };
        let mut receiver_headers = HeaderMap::new();
        receiver_headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-5"));

        let response = proxy_upstream(
            &client,
            &UpstreamRequest::Resolved(Box::new(request)),
            &receiver_headers,
        )
        .await;
        assert!(response.status().is_success());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("proxied response body");
        assert_eq!(body.as_ref(), b"media");

        let (captured_uri, captured_headers) =
            tokio::time::timeout(Duration::from_secs(2), captures.recv())
                .await
                .expect("explicit proxy capture timeout")
                .expect("explicit proxy captured request");
        assert_eq!(captured_uri.path(), "/explicit-proxy/stream");
        assert_eq!(
            captured_uri.query(),
            Some("track=77&session-id=private-session")
        );
        assert_eq!(
            captured_headers.get(header::HOST),
            Some(&HeaderValue::from_static(UPSTREAM_HOST)),
            "the selected proxy must retain the upstream HTTP origin"
        );
        assert_eq!(
            captured_headers.get(USER_AGENT),
            Some(&HeaderValue::from_static("Tributary/explicit-proxy-test"))
        );
        assert_eq!(
            captured_headers.get(header::RANGE),
            Some(&HeaderValue::from_static("bytes=2-5"))
        );

        proxy_abort.abort();
    }

    #[tokio::test]
    async fn retired_resolved_requests_are_rejected_and_existing_tickets_become_not_found() {
        let server = CastHttpServer::start_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("proxy server");
        let lease = MediaLease::new();
        let request = ResolvedHttpRequest::new(
            Url::parse("https://music.test/stream.flac").expect("clean endpoint"),
        )
        .expect("resolved request")
        .with_lease(lease.clone());
        let ticket = server
            .register_resolved(request.clone())
            .expect("active request gets a ticket");

        lease.revoke();
        assert!(server.register_resolved(request).is_none());
        let response = reqwest::get(ticket).await.expect("retired ticket fetch");
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    }

    fn test_upstream_client(
        connect_timeout: Duration,
        response_headers: Duration,
        body_idle: Duration,
    ) -> UpstreamMediaClient {
        let http = crate::http_security::authenticated_client_builder()
            .no_proxy()
            .connect_timeout(connect_timeout)
            .build()
            .expect("test upstream client");
        UpstreamMediaClient {
            http,
            routed_http: Arc::new(DashMap::new()),
            connect_timeout,
            timeouts: UpstreamTimeouts {
                response_headers,
                body_idle,
            },
        }
    }

    #[tokio::test]
    async fn accepted_connection_without_headers_returns_gateway_timeout() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("stall listener");
        let addr = listener.local_addr().expect("stall address");
        let stall = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.expect("accepted connection");
            futures::future::pending::<()>().await;
        });

        let header_deadline = Duration::from_millis(80);
        let client = test_upstream_client(
            Duration::from_secs(1),
            header_deadline,
            Duration::from_secs(1),
        );
        let request = legacy(&format!(
            "http://{addr}/stream?token=accepted-no-headers-secret"
        ));
        let started = Instant::now();
        let response = proxy_upstream(&client, &request, &HeaderMap::new()).await;
        let elapsed = started.elapsed();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(
            elapsed >= header_deadline,
            "the header deadline must not fire early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the header deadline must bound the stalled peer: {elapsed:?}"
        );
        stall.abort();
    }

    #[tokio::test]
    async fn immediate_upstream_transport_failure_returns_bad_gateway() {
        #[derive(Debug)]
        struct FailingResolver;

        impl reqwest::dns::Resolve for FailingResolver {
            fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
                Box::pin(async {
                    Err(std::io::Error::other("intentional test resolver failure").into())
                })
            }
        }

        let http = crate::http_security::authenticated_client_builder()
            .no_proxy()
            .dns_resolver(Arc::new(FailingResolver))
            .connect_timeout(Duration::from_secs(1))
            .build()
            .expect("test upstream client");
        let client = UpstreamMediaClient {
            http,
            routed_http: Arc::new(DashMap::new()),
            connect_timeout: Duration::from_secs(1),
            timeouts: UpstreamTimeouts {
                response_headers: Duration::from_secs(1),
                body_idle: Duration::from_secs(1),
            },
        };
        let request =
            legacy("http://transport-failure.invalid/stream?token=transport-failure-secret");
        let response = proxy_upstream(&client, &request, &HeaderMap::new()).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    async fn reject_upstream_request() -> StatusCode {
        StatusCode::UNAUTHORIZED
    }

    #[tokio::test]
    async fn genuine_upstream_failure_status_is_preserved() {
        let app = Router::new().route("/stream", get(reject_upstream_request));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("status listener");
        let addr = listener.local_addr().expect("status address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("status server");
        });

        let client = test_upstream_client(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let request = legacy(&format!(
            "http://{addr}/stream?token=status-preservation-secret"
        ));
        let response = proxy_upstream(&client, &request, &HeaderMap::new()).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        server.abort();
    }

    #[tokio::test]
    async fn upstream_body_errors_are_opaque_before_axum_observes_them() {
        const SECRET: &str = "https://music.test/stream?token=body-stream-secret";
        let original =
            futures::stream::once(async { Err::<Vec<u8>, _>(std::io::Error::other(SECRET)) });
        let mapped = upstream_body_with_idle_timeout(original, Duration::from_secs(1));
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

    #[tokio::test]
    async fn stalled_upstream_body_fails_on_an_opaque_idle_deadline() {
        let idle_timeout = Duration::from_millis(60);
        let original = futures::stream::pending::<Result<Vec<u8>, std::io::Error>>();
        let mapped = upstream_body_with_idle_timeout(original, idle_timeout);
        futures::pin_mut!(mapped);

        let started = Instant::now();
        let error = mapped
            .next()
            .await
            .expect("idle timeout produces one terminal item")
            .expect_err("stalled body must fail");
        let elapsed = started.elapsed();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), OPAQUE_UPSTREAM_BODY_ERROR);
        assert!(
            elapsed >= idle_timeout,
            "the body idle deadline must not fire early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the body idle deadline must bound the stalled stream: {elapsed:?}"
        );
        assert!(mapped.next().await.is_none(), "a body error is terminal");
    }

    #[tokio::test]
    async fn active_body_can_outlive_one_idle_interval() {
        let idle_timeout = Duration::from_millis(60);
        let original = futures::stream::unfold(0_u8, |index| async move {
            if index == 4 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            Some((Ok::<_, std::io::Error>(vec![index]), index + 1))
        });
        let mapped = upstream_body_with_idle_timeout(original, idle_timeout);
        futures::pin_mut!(mapped);

        let started = Instant::now();
        let mut chunks = Vec::new();
        while let Some(chunk) = mapped.next().await {
            chunks.push(chunk.expect("active body chunk"));
        }

        assert_eq!(chunks, vec![vec![0], vec![1], vec![2], vec![3]]);
        assert!(
            started.elapsed() > idle_timeout,
            "the idle timeout must reset rather than cap total stream lifetime"
        );
    }
}
