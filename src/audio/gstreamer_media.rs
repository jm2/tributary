//! Safe media preparation for Tributary-owned GStreamer pipelines.
//!
//! Backend stream URLs can carry account tokens or even a reversibly encoded
//! password. GStreamer must not own the fetch for those URLs because its
//! redirect and diagnostic behavior is outside Tributary's security boundary.
//! This module exchanges each protected URL for a dedicated loopback ticket;
//! the existing app-owned HTTP proxy performs the real exact-origin fetch.
//! Exact local-library file authority takes the same ticket boundary so the
//! pipeline never reopens a mutable database pathname.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};

use url::{Host, Url};

use super::cast_http_server::{CastHttpServer, UpstreamMediaClient};
use crate::architecture::media::ResolvedHttpRequest;
use crate::http_security::{classify_media_uri, MediaUriSecurity};
use crate::local::resolver::ResolvedLocalMedia;

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
        self.server.revoke_all();
    }
}

struct ProxyState {
    runtime: Option<tokio::runtime::Handle>,
    active: Option<Arc<GstreamerMediaTicket>>,
    /// Identity of the newest load or explicit revocation. An in-flight
    /// server start may install its ticket only while it still owns this
    /// generation.
    generation: Arc<PreparationGeneration>,
}

struct PreparationGeneration;

/// Stateful last-mile resolver shared by local and AirPlay outputs.
pub(super) struct GstreamerMediaProxy {
    state: Mutex<ProxyState>,
    /// Credential-free HTTP transport shared by every per-load ticket server
    /// owned by this output. Credentials remain request-scoped while the
    /// origin connection pool survives track changes.
    upstream: Option<UpstreamMediaClient>,
}

impl GstreamerMediaProxy {
    pub(super) fn new(runtime: Option<tokio::runtime::Handle>) -> Self {
        let upstream = UpstreamMediaClient::new().map_err(|_| {
            tracing::error!("Failed to build protected media transport");
        });
        Self {
            state: Mutex::new(ProxyState {
                runtime,
                active: None,
                generation: Arc::new(PreparationGeneration),
            }),
            upstream: upstream.ok(),
        }
    }

    /// Supply the application runtime used to host loopback media tickets.
    pub(super) fn set_runtime(&self, runtime: tokio::runtime::Handle) {
        let mut state = self.lock_state();
        state.runtime = Some(runtime);
        // A startup that captured the displaced handle must not install after
        // this update. An already-active server remains valid and is still
        // revoked through its ticket identity.
        state.generation = Arc::new(PreparationGeneration);
    }

    /// Retire the previous load and prepare `raw_uri` for GStreamer.
    ///
    /// Direct local or credential-free media is preserved byte-for-byte.
    /// Supported authenticated HTTP(S) media receives an opaque loopback
    /// ticket. Malformed HTTP(S), credentials on unsupported schemes, missing
    /// runtime state, bind/client failure, or an invalid generated ticket all
    /// fail closed with the same URL-free category.
    pub(super) fn prepare(&self, raw_uri: &str) -> Result<PreparedGstreamerMedia, &'static str> {
        let upstream = self.upstream.clone();
        self.prepare_with_server_start(raw_uri, move |runtime| {
            let upstream = upstream.ok_or_else(|| anyhow::anyhow!(MEDIA_PREPARATION_FAILED))?;
            runtime.block_on(CastHttpServer::start_on_with_upstream_client(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                upstream,
            ))
        })
    }

    /// Retire the previous load and prepare one typed protected request.
    ///
    /// Unlike [`Self::prepare`], this path performs no URI classification: a
    /// resolved request is protected by construction and always receives a
    /// dedicated app-owned loopback ticket. An inactive request or unavailable
    /// proxy fails closed.
    pub(super) fn prepare_resolved(
        &self,
        request: ResolvedHttpRequest,
    ) -> Result<PreparedGstreamerMedia, &'static str> {
        let upstream = self.upstream.clone();
        self.prepare_resolved_with_server_start(request, move |runtime| {
            let upstream = upstream.ok_or_else(|| anyhow::anyhow!(MEDIA_PREPARATION_FAILED))?;
            runtime.block_on(CastHttpServer::start_on_with_upstream_client(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                upstream,
            ))
        })
    }

    /// Retire the previous load and exchange retained local-file authority for
    /// a dedicated loopback ticket. GStreamer never reopens the database path;
    /// the server streams clones of the exact authorized file handle.
    pub(super) fn prepare_local(
        &self,
        media: ResolvedLocalMedia,
    ) -> Result<PreparedGstreamerMedia, &'static str> {
        let upstream = self.upstream.clone();
        self.prepare_local_with_server_start(media, move |runtime| {
            let upstream = upstream.ok_or_else(|| anyhow::anyhow!(MEDIA_PREPARATION_FAILED))?;
            runtime.block_on(CastHttpServer::start_on_with_upstream_client(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                upstream,
            ))
        })
    }

    fn prepare_local_with_server_start<F>(
        &self,
        media: ResolvedLocalMedia,
        start_server: F,
    ) -> Result<PreparedGstreamerMedia, &'static str>
    where
        F: FnOnce(&tokio::runtime::Handle) -> anyhow::Result<CastHttpServer>,
    {
        let generation = Arc::new(PreparationGeneration);
        let (previous, runtime) = {
            let mut state = self.lock_state();
            state.generation = Arc::clone(&generation);
            (state.active.take(), state.runtime.clone())
        };
        if let Some(previous) = previous {
            previous.revoke();
        }

        let runtime = runtime.ok_or(MEDIA_PREPARATION_FAILED)?;
        let server = start_server(&runtime).map_err(|_| MEDIA_PREPARATION_FAILED)?;
        let uri = server.register_local(media);
        if !valid_loopback_ticket(server.addr(), &uri) {
            server.revoke_all();
            return Err(MEDIA_PREPARATION_FAILED);
        }

        let ticket = Arc::new(GstreamerMediaTicket { server, uri });
        if !self.install_if_current(&generation, &ticket) {
            ticket.revoke();
            return Err(MEDIA_PREPARATION_FAILED);
        }
        Ok(PreparedGstreamerMedia::Protected(ticket))
    }

    fn prepare_resolved_with_server_start<F>(
        &self,
        request: ResolvedHttpRequest,
        start_server: F,
    ) -> Result<PreparedGstreamerMedia, &'static str>
    where
        F: FnOnce(&tokio::runtime::Handle) -> anyhow::Result<CastHttpServer>,
    {
        if !request.is_active() {
            self.revoke();
            return Err(MEDIA_PREPARATION_FAILED);
        }

        let generation = Arc::new(PreparationGeneration);
        let (previous, runtime) = {
            let mut state = self.lock_state();
            state.generation = Arc::clone(&generation);
            (state.active.take(), state.runtime.clone())
        };

        if let Some(previous) = previous {
            previous.revoke();
        }

        let runtime = runtime.ok_or(MEDIA_PREPARATION_FAILED)?;
        let server = start_server(&runtime).map_err(|_| MEDIA_PREPARATION_FAILED)?;
        let uri = server
            .register_resolved(request)
            .ok_or(MEDIA_PREPARATION_FAILED)?;
        if !valid_loopback_ticket(server.addr(), &uri) {
            server.revoke_upstreams();
            return Err(MEDIA_PREPARATION_FAILED);
        }

        let ticket = Arc::new(GstreamerMediaTicket { server, uri });
        if !self.install_if_current(&generation, &ticket) {
            ticket.revoke();
            return Err(MEDIA_PREPARATION_FAILED);
        }
        Ok(PreparedGstreamerMedia::Protected(ticket))
    }

    fn prepare_with_server_start<F>(
        &self,
        raw_uri: &str,
        start_server: F,
    ) -> Result<PreparedGstreamerMedia, &'static str>
    where
        F: FnOnce(&tokio::runtime::Handle) -> anyhow::Result<CastHttpServer>,
    {
        let classification = classify_media_uri(raw_uri);
        let generation = Arc::new(PreparationGeneration);
        let (previous, runtime) = {
            let mut state = self.lock_state();
            state.generation = Arc::clone(&generation);
            let runtime = match &classification {
                MediaUriSecurity::Protected(_) => state.runtime.clone(),
                MediaUriSecurity::Direct | MediaUriSecurity::Reject => None,
            };
            (state.active.take(), runtime)
        };

        // Revocation and server startup may both touch runtime-owned state.
        // Neither belongs under the proxy mutex: the generation above is the
        // only synchronization needed to prevent an older load from winning
        // after a newer load or Stop has already superseded it.
        if let Some(previous) = previous {
            previous.revoke();
        }

        match classification {
            MediaUriSecurity::Direct => {
                if self.is_current_generation(&generation) {
                    Ok(PreparedGstreamerMedia::Direct(raw_uri.to_string()))
                } else {
                    Err(MEDIA_PREPARATION_FAILED)
                }
            }
            MediaUriSecurity::Reject => Err(MEDIA_PREPARATION_FAILED),
            MediaUriSecurity::Protected(upstream) => {
                let runtime = runtime.ok_or(MEDIA_PREPARATION_FAILED)?;
                let server = start_server(&runtime).map_err(|_| MEDIA_PREPARATION_FAILED)?;
                let uri = server.register_upstream(&upstream);
                if !valid_loopback_ticket(server.addr(), &uri) {
                    server.revoke_upstreams();
                    return Err(MEDIA_PREPARATION_FAILED);
                }

                let ticket = Arc::new(GstreamerMediaTicket { server, uri });
                if !self.install_if_current(&generation, &ticket) {
                    ticket.revoke();
                    return Err(MEDIA_PREPARATION_FAILED);
                }
                Ok(PreparedGstreamerMedia::Protected(ticket))
            }
        }
    }

    /// Revoke and release the current ticket, if any.
    pub(super) fn revoke(&self) {
        let active = {
            let mut state = self.lock_state();
            // Stop/teardown also supersedes a server start that has released
            // the mutex but has not installed its ticket yet.
            state.generation = Arc::new(PreparationGeneration);
            state.active.take()
        };
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
                state.generation = Arc::new(PreparationGeneration);
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

    fn is_current_generation(&self, generation: &Arc<PreparationGeneration>) -> bool {
        Arc::ptr_eq(&self.lock_state().generation, generation)
    }

    fn install_if_current(
        &self,
        generation: &Arc<PreparationGeneration>,
        ticket: &Arc<GstreamerMediaTicket>,
    ) -> bool {
        let mut state = self.lock_state();
        if !Arc::ptr_eq(&state.generation, generation) || state.active.is_some() {
            return false;
        }

        state.active = Some(Arc::clone(ticket));
        true
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
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::{OriginalUri, State};
    use axum::http::{header, HeaderMap, StatusCode, Uri};
    use axum::response::Response;
    use axum::routing::get;
    use axum::Router;

    use super::*;

    fn authorized_local_media() -> (tempfile::TempDir, ResolvedLocalMedia) {
        let root = tempfile::tempdir().expect("temporary local-media root");
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .expect("write local-media marker");
        let path = root.path().join("track.flac");
        std::fs::write(&path, b"local media").expect("write local-media fixture");
        let media = ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &path)
            .expect("retain local-media authority");
        (root, media)
    }

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
    fn typed_requests_always_receive_a_ticket_even_when_the_endpoint_is_clean() {
        let runtime = runtime();
        let proxy = GstreamerMediaProxy::new(Some(runtime.handle().clone()));
        let endpoint =
            Url::parse("https://music.test/clean/track.flac?track=42").expect("clean endpoint");
        let request = ResolvedHttpRequest::new(endpoint.clone()).expect("resolved request");

        let prepared = proxy.prepare_resolved(request).expect("typed media ticket");
        assert!(matches!(&prepared, PreparedGstreamerMedia::Protected(_)));
        assert!(prepared.ticket().is_some());
        assert_ne!(prepared.uri(), endpoint.as_str());
        assert!(!prepared.uri().contains("music.test"));
        assert!(!prepared.uri().contains("track=42"));
    }

    #[test]
    fn typed_requests_fail_closed_without_proxy_runtime() {
        let proxy = GstreamerMediaProxy::new(None);
        let request = ResolvedHttpRequest::new(
            Url::parse("https://music.test/clean/track.flac").expect("clean endpoint"),
        )
        .expect("resolved request");

        assert_eq!(
            proxy.prepare_resolved(request).err(),
            Some(MEDIA_PREPARATION_FAILED)
        );
    }

    #[test]
    fn local_authority_becomes_an_opaque_generation_owned_loopback_ticket() {
        let runtime = runtime();
        let proxy = GstreamerMediaProxy::new(Some(runtime.handle().clone()));
        let (_root, media) = authorized_local_media();
        let prepared = proxy
            .prepare_local_with_server_start(media, |handle| {
                Ok(CastHttpServer::detached_for_test(
                    handle,
                    SocketAddr::from((Ipv4Addr::LOCALHOST, 46_000)),
                ))
            })
            .expect("prepare authorized local media");
        let ticket = prepared.ticket().expect("local media ticket");

        assert!(prepared.uri().starts_with("http://127.0.0.1:46000/cast/"));
        assert!(!prepared.uri().contains("track.flac"));
        assert_eq!(ticket.server.registered_route_count(), 1);

        proxy.revoke_if_current(&ticket);
        assert_eq!(ticket.server.registered_route_count(), 0);
    }

    #[test]
    fn delayed_typed_startup_cannot_replace_a_newer_load() {
        let runtime = runtime();
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            Url::parse("https://music.test/clean/track.flac").expect("clean endpoint"),
        )
        .expect("resolved request");
        let (startup_entered_tx, startup_entered_rx) = mpsc::channel();
        let (release_startup_tx, release_startup_rx) = mpsc::channel();

        let older_proxy = Arc::clone(&proxy);
        let older = thread::spawn(move || {
            older_proxy.prepare_resolved_with_server_start(request, move |runtime| {
                startup_entered_tx.send(()).expect("report startup");
                release_startup_rx.recv().expect("release startup");
                runtime.block_on(CastHttpServer::start_on(SocketAddr::from((
                    Ipv4Addr::LOCALHOST,
                    0,
                ))))
            })
        });
        startup_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("typed preparation reached startup");

        let newer = proxy
            .prepare("https://radio.test/live.mp3")
            .expect("newer direct load");
        assert!(matches!(newer, PreparedGstreamerMedia::Direct(_)));
        release_startup_tx.send(()).expect("finish startup");
        assert_eq!(
            older.join().expect("typed preparation thread").err(),
            Some(MEDIA_PREPARATION_FAILED)
        );
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
    fn delayed_startup_cannot_block_or_replace_a_newer_protected_load() {
        let runtime = runtime();
        let upstream = start_media_server(&runtime);
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let (startup_entered_tx, startup_entered_rx) = mpsc::channel();
        let (release_startup_tx, release_startup_rx) = mpsc::channel();

        let older_proxy = Arc::clone(&proxy);
        let older_uri = format!(
            "http://{}/first?api_key=older-inflight-secret",
            upstream.addr
        );
        let older = thread::spawn(move || {
            older_proxy.prepare_with_server_start(&older_uri, move |runtime| {
                startup_entered_tx
                    .send(())
                    .expect("report delayed server startup");
                release_startup_rx
                    .recv()
                    .expect("release delayed server startup");
                runtime.block_on(CastHttpServer::start_on(SocketAddr::from((
                    Ipv4Addr::LOCALHOST,
                    0,
                ))))
            })
        });
        startup_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("older preparation reached server startup");

        // A newer load must be able to acquire the proxy state while the old
        // load is blocked in server startup. It becomes the sole generation
        // allowed to install an active ticket.
        let (newer_result_tx, newer_result_rx) = mpsc::channel();
        let newer_proxy = Arc::clone(&proxy);
        let newer_uri = format!(
            "http://{}/second?api_key=newer-active-secret",
            upstream.addr
        );
        let newer = thread::spawn(move || {
            let result = newer_proxy.prepare(&newer_uri).map(|prepared| {
                (
                    prepared.uri().to_string(),
                    prepared.ticket().expect("newer protected ticket"),
                )
            });
            newer_result_tx
                .send(result)
                .expect("report newer preparation");
        });

        let (newer_ticket_uri, _newer_ticket) =
            match newer_result_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(result) => result.expect("newer preparation succeeds without waiting"),
                Err(error) => {
                    let _ = release_startup_tx.send(());
                    let _ = older.join();
                    let _ = newer.join();
                    panic!("newer preparation blocked behind server startup: {error}");
                }
            };
        newer.join().expect("newer preparation thread");
        assert_eq!(status(&newer_ticket_uri), StatusCode::OK);

        release_startup_tx
            .send(())
            .expect("finish older server startup");
        let older_result = older.join().expect("older preparation thread");
        assert_eq!(older_result.err(), Some(MEDIA_PREPARATION_FAILED));
        assert_eq!(status(&newer_ticket_uri), StatusCode::OK);
    }

    #[test]
    fn revoke_does_not_wait_for_and_invalidates_an_inflight_startup() {
        let runtime = runtime();
        let upstream = start_media_server(&runtime);
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let (startup_entered_tx, startup_entered_rx) = mpsc::channel();
        let (release_startup_tx, release_startup_rx) = mpsc::channel();

        let preparing_proxy = Arc::clone(&proxy);
        let protected_uri = format!(
            "http://{}/first?api_key=revoked-inflight-secret",
            upstream.addr
        );
        let preparing = thread::spawn(move || {
            preparing_proxy.prepare_with_server_start(&protected_uri, move |runtime| {
                startup_entered_tx
                    .send(())
                    .expect("report delayed server startup");
                release_startup_rx
                    .recv()
                    .expect("release delayed server startup");
                runtime.block_on(CastHttpServer::start_on(SocketAddr::from((
                    Ipv4Addr::LOCALHOST,
                    0,
                ))))
            })
        });
        startup_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("preparation reached server startup");

        let (revoked_tx, revoked_rx) = mpsc::channel();
        let revoking_proxy = Arc::clone(&proxy);
        let revoking = thread::spawn(move || {
            revoking_proxy.revoke();
            revoked_tx.send(()).expect("report revocation");
        });
        if let Err(error) = revoked_rx.recv_timeout(Duration::from_secs(2)) {
            let _ = release_startup_tx.send(());
            let _ = preparing.join();
            let _ = revoking.join();
            panic!("revocation blocked behind server startup: {error}");
        }
        revoking.join().expect("revocation thread");

        release_startup_tx
            .send(())
            .expect("finish revoked server startup");
        let result = preparing.join().expect("preparation thread");
        assert_eq!(result.err(), Some(MEDIA_PREPARATION_FAILED));
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
