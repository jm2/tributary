//! Safe media preparation for Tributary-owned GStreamer pipelines.
//!
//! Backend stream URLs can carry account tokens or even a reversibly encoded
//! password. GStreamer must not own the fetch for those URLs because its
//! redirect and diagnostic behavior is outside Tributary's security boundary.
//! This module exchanges each protected URL for a dedicated loopback ticket;
//! the existing app-owned HTTP proxy performs the real exact-origin fetch.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};

use url::{Host, Url};

use super::cast_http_server::CastHttpServer;
use crate::http_security::{classify_media_uri, MediaUriSecurity};

const MEDIA_PREPARATION_FAILED: &str = "protected media preparation failed";

/// One URI that is safe to give to a Tributary-owned GStreamer pipeline.
///
/// Deliberately not `Debug`: the direct variant is credential-free by
/// classification, while the protected variant owns the proxy that retains
/// the original authenticated URL.
pub(super) enum PreparedGstreamerMedia {
    Direct(String),
    Protected(Arc<GstreamerMediaTicket>),
}

impl PreparedGstreamerMedia {
    pub(super) fn uri(&self) -> &str {
        match self {
            Self::Direct(uri) => uri,
            Self::Protected(ticket) => ticket.uri(),
        }
    }

    pub(super) fn ticket(&self) -> Option<Arc<GstreamerMediaTicket>> {
        match self {
            Self::Direct(_) => None,
            Self::Protected(ticket) => Some(Arc::clone(ticket)),
        }
    }
}

/// Dedicated app-owned proxy for one protected GStreamer load.
///
/// A new protected load gets a new server rather than sharing a registry with
/// its predecessor. Consequently a delayed terminal callback can revoke only
/// the ticket it captured, never a newer track's route.
pub(super) struct GstreamerMediaTicket {
    server: CastHttpServer,
    uri: String,
}

impl GstreamerMediaTicket {
    pub(super) fn uri(&self) -> &str {
        &self.uri
    }

    pub(super) fn revoke(&self) {
        self.server.revoke_upstreams();
    }
}

struct ProxyState {
    runtime: Option<tokio::runtime::Handle>,
    active: Option<Arc<GstreamerMediaTicket>>,
}

/// Stateful last-mile resolver shared by local and AirPlay outputs.
pub(super) struct GstreamerMediaProxy {
    state: Mutex<ProxyState>,
}

impl GstreamerMediaProxy {
    pub(super) fn new(runtime: Option<tokio::runtime::Handle>) -> Self {
        Self {
            state: Mutex::new(ProxyState {
                runtime,
                active: None,
            }),
        }
    }

    /// Supply the application runtime used to host loopback media tickets.
    pub(super) fn set_runtime(&self, runtime: tokio::runtime::Handle) {
        self.lock_state().runtime = Some(runtime);
    }

    /// Retire the previous load and prepare `raw_uri` for GStreamer.
    ///
    /// Direct local or credential-free media is preserved byte-for-byte.
    /// Supported authenticated HTTP(S) media receives an opaque loopback
    /// ticket. Malformed HTTP(S), credentials on unsupported schemes, missing
    /// runtime state, bind/client failure, or an invalid generated ticket all
    /// fail closed with the same URL-free category.
    pub(super) fn prepare(&self, raw_uri: &str) -> Result<PreparedGstreamerMedia, &'static str> {
        let classification = classify_media_uri(raw_uri);
        let mut state = self.lock_state();

        if let Some(previous) = state.active.take() {
            previous.revoke();
        }

        match classification {
            MediaUriSecurity::Direct => Ok(PreparedGstreamerMedia::Direct(raw_uri.to_string())),
            MediaUriSecurity::Reject => Err(MEDIA_PREPARATION_FAILED),
            MediaUriSecurity::Protected(upstream) => {
                let runtime = state.runtime.as_ref().ok_or(MEDIA_PREPARATION_FAILED)?;
                let server = runtime
                    .block_on(CastHttpServer::start_on(SocketAddr::from((
                        Ipv4Addr::LOCALHOST,
                        0,
                    ))))
                    .map_err(|_| MEDIA_PREPARATION_FAILED)?;
                let uri = server.register_upstream(&upstream);
                if !valid_loopback_ticket(server.addr(), &uri) {
                    server.revoke_upstreams();
                    return Err(MEDIA_PREPARATION_FAILED);
                }

                let ticket = Arc::new(GstreamerMediaTicket { server, uri });
                state.active = Some(Arc::clone(&ticket));
                Ok(PreparedGstreamerMedia::Protected(ticket))
            }
        }
    }

    /// Revoke and release the current ticket, if any.
    pub(super) fn revoke(&self) {
        let active = self.lock_state().active.take();
        if let Some(ticket) = active {
            ticket.revoke();
        }
    }

    /// Revoke `ticket` only while it still owns this output's current load.
    ///
    /// The identity check is the stale-callback guard. A superseded callback
    /// still owns a dedicated server and is free to revoke that server
    /// directly, but it must not clear the proxy's newer active lease.
    pub(super) fn revoke_if_current(&self, ticket: &Arc<GstreamerMediaTicket>) {
        let active = {
            let mut state = self.lock_state();
            if state
                .active
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, ticket))
            {
                state.active.take()
            } else {
                None
            }
        };

        if let Some(active) = active {
            active.revoke();
        } else {
            // A stale ticket can still be retained by an already-queued bus
            // callback. Retire its own server without touching current state.
            ticket.revoke();
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, ProxyState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Drop for GstreamerMediaProxy {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(ticket) = state.active.take() {
            ticket.revoke();
        }
    }
}

fn valid_loopback_ticket(addr: SocketAddr, candidate: &str) -> bool {
    let Ok(url) = Url::parse(candidate) else {
        return false;
    };
    let route = url.path().strip_prefix("/cast/");

    addr.ip().is_loopback()
        && url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.host(), Some(Host::Ipv4(ip)) if ip == Ipv4Addr::LOCALHOST)
        && url.port() == Some(addr.port())
        && route.is_some_and(|ticket| !ticket.is_empty() && !ticket.contains('/'))
        && url.query().is_none()
        && url.fragment().is_none()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::{OriginalUri, State};
    use axum::http::{header, HeaderMap, StatusCode, Uri};
    use axum::response::Response;
    use axum::routing::get;
    use axum::Router;

    use super::*;

    struct MockServer {
        addr: SocketAddr,
        abort_handle: tokio::task::AbortHandle,
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.abort_handle.abort();
        }
    }

    #[derive(Clone, Default)]
    struct RequestCapture {
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    struct CapturedRequest {
        uri: Uri,
        headers: HeaderMap,
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime")
    }

    fn start_mock_server(runtime: &tokio::runtime::Runtime, app: Router) -> MockServer {
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind mock media server");
            let addr = listener.local_addr().expect("mock media server address");
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve mock media");
            });

            MockServer {
                addr,
                abort_handle: server.abort_handle(),
            }
        })
    }

    fn start_media_server(runtime: &tokio::runtime::Runtime) -> MockServer {
        start_mock_server(
            runtime,
            Router::new()
                .route("/first", get(|| async { "first media" }))
                .route("/second", get(|| async { "second media" }))
                .route("/revoked", get(|| async { "revoked media" }))
                .route("/dropped", get(|| async { "dropped media" })),
        )
    }

    fn capture_request(capture: &RequestCapture, uri: Uri, headers: HeaderMap) {
        capture
            .requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(CapturedRequest { uri, headers });
    }

    async fn redirect_start(
        State(capture): State<RequestCapture>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
    ) -> Response {
        capture_request(&capture, uri, headers);
        Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, "/media?api_key=integration-secret")
            .body(Body::empty())
            .expect("redirect response")
    }

    async fn redirected_media(
        State(capture): State<RequestCapture>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
    ) -> Response {
        capture_request(&capture, uri, headers);
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, "audio/mpeg")
            .header(header::CONTENT_RANGE, "bytes 2-5/8")
            .body(Body::from("3456"))
            .expect("media response")
    }

    fn status(uri: &str) -> StatusCode {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("test client")
            .get(uri)
            .send()
            .expect("loopback proxy response")
            .status()
    }

    #[test]
    fn direct_media_is_preserved_without_a_runtime() {
        let proxy = GstreamerMediaProxy::new(None);
        for direct in [
            "file:///music/track.flac",
            "https://radio.test/live.mp3?quality=high",
            "Albums/Artist/track.flac",
        ] {
            let prepared = proxy.prepare(direct).expect("direct media");
            assert_eq!(prepared.uri(), direct);
            assert!(prepared.ticket().is_none());
        }
    }

    #[test]
    fn protected_and_ambiguous_media_fail_closed_without_a_runtime() {
        let proxy = GstreamerMediaProxy::new(None);
        for rejected in [
            "https://music.test/stream?api_key=do-not-expose",
            "HTTP://[malformed",
            "ftp://user:password@music.test/stream.flac",
            "//music.test/stream?api_key=do-not-expose",
        ] {
            let error = proxy.prepare(rejected).err().expect("must reject");
            assert_eq!(error, MEDIA_PREPARATION_FAILED);
            assert!(!error.contains("do-not-expose"));
        }
    }

    #[test]
    fn every_supported_credential_shape_becomes_an_opaque_loopback_ticket() {
        let runtime = runtime();
        let proxy = GstreamerMediaProxy::new(Some(runtime.handle().clone()));
        for protected in [
            "https://username-only@music.test/stream.flac",
            "https://user:password@music.test/stream.flac",
            "https://plex.test/file.flac?X-Plex-Token=plex-secret",
            "https://jellyfin.test/stream?api_key=jellyfin-secret",
            "http://daap.test/item?session-id=daap-secret",
            "https://sub.test/stream?u=me&t=sub-token&s=sub-salt&c=Tributary",
            "https://sub.test/stream?u=me&p=enc%3A70617373&c=Tributary",
        ] {
            let prepared = proxy.prepare(protected).expect("protected media");
            let safe = Url::parse(prepared.uri()).expect("valid proxy URL");
            assert_eq!(safe.scheme(), "http");
            assert_eq!(safe.host(), Some(Host::Ipv4(Ipv4Addr::LOCALHOST)));
            assert!(prepared.ticket().is_some());
            for secret in [
                "username-only",
                "password",
                "plex-secret",
                "jellyfin-secret",
                "daap-secret",
                "sub-token",
                "sub-salt",
                "70617373",
            ] {
                assert!(!prepared.uri().contains(secret));
            }
        }
    }

    #[test]
    fn replacement_and_stale_cleanup_cannot_revoke_a_newer_ticket() {
        let runtime = runtime();
        let upstream = start_media_server(&runtime);
        let proxy = GstreamerMediaProxy::new(Some(runtime.handle().clone()));
        let first = proxy
            .prepare(&format!(
                "http://{}/first?api_key=first-secret",
                upstream.addr
            ))
            .expect("first ticket");
        let first_ticket = first.ticket().expect("first lease");

        let second = proxy
            .prepare(&format!(
                "http://{}/second?api_key=second-secret",
                upstream.addr
            ))
            .expect("second ticket");
        let second_ticket = second.ticket().expect("second lease");
        assert_eq!(status(first.uri()), StatusCode::NOT_FOUND);

        proxy.revoke_if_current(&first_ticket);
        assert_eq!(status(second.uri()), StatusCode::OK);

        proxy.revoke_if_current(&second_ticket);
        assert_eq!(status(second.uri()), StatusCode::NOT_FOUND);
    }

    #[test]
    fn direct_or_rejected_replacement_retires_the_previous_ticket() {
        let runtime = runtime();
        let upstream = start_media_server(&runtime);
        let proxy = GstreamerMediaProxy::new(Some(runtime.handle().clone()));

        let before_direct = proxy
            .prepare(&format!(
                "http://{}/first?api_key=first-secret",
                upstream.addr
            ))
            .expect("ticket before direct media");
        proxy
            .prepare("https://radio.test/live.mp3")
            .expect("direct replacement");
        assert_eq!(status(before_direct.uri()), StatusCode::NOT_FOUND);

        let before_reject = proxy
            .prepare(&format!(
                "http://{}/second?api_key=second-secret",
                upstream.addr
            ))
            .expect("ticket before rejected media");
        assert!(proxy.prepare("HTTP://[malformed").is_err());
        assert_eq!(status(before_reject.uri()), StatusCode::NOT_FOUND);
    }

    #[test]
    fn issued_ticket_follows_same_origin_redirect_without_receiver_headers() {
        const UPSTREAM_SECRET: &str = "integration-secret";

        let runtime = runtime();
        let capture = RequestCapture::default();
        let upstream = start_mock_server(
            &runtime,
            Router::new()
                .route("/start", get(redirect_start))
                .route("/media", get(redirected_media))
                .with_state(capture.clone()),
        );
        let proxy = GstreamerMediaProxy::new(Some(runtime.handle().clone()));
        let protected = format!("http://{}/start?api_key={UPSTREAM_SECRET}", upstream.addr);
        let prepared = proxy.prepare(&protected).expect("protected media ticket");

        assert!(!prepared.uri().contains(UPSTREAM_SECRET));
        assert!(!prepared.uri().contains("api_key"));

        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("receiver client")
            .get(prepared.uri())
            .header(header::RANGE, "bytes=2-5")
            .header(header::REFERER, "https://receiver.test/private")
            .header(header::COOKIE, "receiver-session=private")
            .header("x-receiver-private", "do-not-forward")
            .send()
            .expect("proxied media response");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.bytes().expect("proxied media body").as_ref(),
            b"3456"
        );

        let requests = capture
            .requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].uri.path(), "/start");
        assert_eq!(requests[1].uri.path(), "/media");
        for request in requests.iter() {
            assert_eq!(request.uri.query(), Some("api_key=integration-secret"));
            assert_eq!(
                request.headers.get(header::RANGE),
                Some(&header::HeaderValue::from_static("bytes=2-5"))
            );
            assert!(!request.headers.contains_key(header::REFERER));
            assert!(!request.headers.contains_key(header::COOKIE));
            assert!(!request.headers.contains_key("x-receiver-private"));
            assert!(request.headers.values().all(|value| !value
                .as_bytes()
                .windows(UPSTREAM_SECRET.len())
                .any(|window| { window == UPSTREAM_SECRET.as_bytes() })));
        }
    }

    #[test]
    fn explicit_revoke_and_proxy_drop_retire_their_tickets() {
        let runtime = runtime();
        let upstream = start_media_server(&runtime);

        let proxy = GstreamerMediaProxy::new(Some(runtime.handle().clone()));
        let explicitly_revoked = proxy
            .prepare(&format!(
                "http://{}/revoked?api_key=revoked-secret",
                upstream.addr
            ))
            .expect("ticket to revoke");
        assert_eq!(status(explicitly_revoked.uri()), StatusCode::OK);
        proxy.revoke();
        assert_eq!(status(explicitly_revoked.uri()), StatusCode::NOT_FOUND);

        let retained_after_drop = {
            let proxy = GstreamerMediaProxy::new(Some(runtime.handle().clone()));
            let prepared = proxy
                .prepare(&format!(
                    "http://{}/dropped?api_key=dropped-secret",
                    upstream.addr
                ))
                .expect("ticket owned by proxy");
            assert_eq!(status(prepared.uri()), StatusCode::OK);
            prepared
        };
        assert_eq!(status(retained_after_drop.uri()), StatusCode::NOT_FOUND);
    }
}
