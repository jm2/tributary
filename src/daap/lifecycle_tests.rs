//! Real-socket coverage for the DAAP adapter's centralized lifecycle wiring.
//!
//! The generic lifecycle tests pin the state machine. These fixtures prove
//! the concrete DAAP constructor crosses that boundary immediately after
//! `mlid`, keeps post-login catalogue work abortable, and sends one bounded
//! logout for every session that reached server-side ownership.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::architecture::SourceId;
use crate::source_lifecycle::{FailureCategory, SourceProvenance};
use crate::source_registry::RemoteSourceRegistry;

use super::DaapBackend;

const MOCK_DEADLINE: Duration = Duration::from_secs(5);
const MOCK_REQUEST_HEADER_CAP: usize = 16 * 1024;
// Smaller than every real request line so header assembly is always
// fragmented, even on loopback.
const MOCK_READ_CHUNK_BYTES: usize = 7;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum MockEndpoint {
    ServerInfo,
    Login,
    Update,
    Databases,
    Items,
    Stream,
    Artwork,
    Logout,
    Other,
}

impl MockEndpoint {
    fn classify(path: &str) -> Self {
        if path.starts_with("/server-info") {
            Self::ServerInfo
        } else if path.starts_with("/login") {
            Self::Login
        } else if path.starts_with("/update?") {
            Self::Update
        } else if path.starts_with("/databases?") {
            Self::Databases
        } else if path.starts_with("/databases/1/items?") {
            Self::Items
        } else if path.starts_with("/databases/1/items/9.mp3?") {
            Self::Stream
        } else if path.starts_with("/databases/1/items/9/extra_data/artwork?") {
            Self::Artwork
        } else if path.starts_with("/logout?") {
            Self::Logout
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Debug)]
struct MockResponse {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl MockResponse {
    fn dmap(body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/x-dmap-tagged",
            body,
        }
    }
}

#[derive(Default)]
struct MockDaapState {
    requests: Mutex<Vec<String>>,
    scripted: Mutex<HashMap<MockEndpoint, VecDeque<MockResponse>>>,
    held: Mutex<HashMap<MockEndpoint, Arc<Semaphore>>>,
    handler_failures: Mutex<Vec<String>>,
    request_changed: Notify,
}

struct MockDaapServer {
    base_url: String,
    state: Arc<MockDaapState>,
    task: tokio::task::JoinHandle<()>,
}

impl MockDaapServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock DAAP server");
        let address = listener.local_addr().expect("mock DAAP address");
        let state = Arc::new(MockDaapState::default());
        let state_for_task = Arc::clone(&state);

        let task = tokio::spawn(async move {
            let mut handlers = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let state = Arc::clone(&state_for_task);
                            handlers.spawn(serve_mock_connection(state, stream));
                        }
                        Err(_) => break,
                    },
                    result = handlers.join_next(), if !handlers.is_empty() => {
                        if result.is_some_and(|result| result.is_err()) {
                            record_mock_failure(&state_for_task, "mock DAAP handler panicked");
                        }
                    }
                }
            }
        });

        Self {
            base_url: format!("http://{address}"),
            state,
            task,
        }
    }

    fn enqueue(&self, endpoint: MockEndpoint, response: MockResponse) {
        self.state
            .scripted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(endpoint)
            .or_default()
            .push_back(response);
    }

    fn hold(&self, endpoint: MockEndpoint) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        let replaced = self
            .state
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(endpoint, Arc::clone(&gate));
        assert!(replaced.is_none(), "fixture endpoint already held");
        gate
    }

    fn paths(&self) -> Vec<String> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn request_count(&self, endpoint: MockEndpoint) -> usize {
        self.paths()
            .iter()
            .filter(|path| MockEndpoint::classify(path) == endpoint)
            .count()
    }

    async fn wait_for_requests(&self, endpoint: MockEndpoint, expected: usize) {
        tokio::time::timeout(MOCK_DEADLINE, async {
            loop {
                let changed = self.state.request_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.request_count(endpoint) >= expected {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("mock DAAP request must arrive before the deadline");
    }

    fn assert_healthy(&self) {
        let failures = self
            .state
            .handler_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(failures.is_empty(), "mock DAAP failures: {failures:?}");
        assert_eq!(
            self.request_count(MockEndpoint::Other),
            0,
            "unexpected mock DAAP routes: {:?}",
            self.paths()
        );
    }
}

impl Drop for MockDaapServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_mock_connection(state: Arc<MockDaapState>, mut stream: TcpStream) {
    let path = match read_mock_request_path(&mut stream).await {
        Ok(path) => path,
        Err(reason) => {
            record_mock_failure(&state, reason);
            return;
        }
    };
    let endpoint = MockEndpoint::classify(&path);
    state
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(path.clone());
    state.request_changed.notify_waiters();

    let gate = state
        .held
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&endpoint)
        .cloned();
    if let Some(gate) = gate {
        let Ok(Ok(permit)) = tokio::time::timeout(MOCK_DEADLINE, gate.acquire()).await else {
            record_mock_failure(&state, "mock DAAP response gate exceeded the deadline");
            return;
        };
        permit.forget();
    }

    let response = response_for(&state, endpoint);
    let headers = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    let write_result = tokio::time::timeout(MOCK_DEADLINE, async {
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(&response.body).await
    })
    .await;
    if !matches!(write_result, Ok(Ok(()))) {
        record_mock_failure(&state, "mock DAAP response write failed or timed out");
    }
}

async fn read_mock_request_path(stream: &mut TcpStream) -> Result<String, &'static str> {
    tokio::time::timeout(MOCK_DEADLINE, async {
        let mut request = Vec::with_capacity(1024);
        let mut chunk = [0_u8; MOCK_READ_CHUNK_BYTES];
        loop {
            let remaining = MOCK_REQUEST_HEADER_CAP.saturating_sub(request.len());
            if remaining == 0 {
                return Err("mock DAAP request headers exceeded the cap");
            }
            let read_limit = remaining.min(chunk.len());
            let read = stream
                .read(&mut chunk[..read_limit])
                .await
                .map_err(|_| "mock DAAP request read failed")?;
            if read == 0 {
                return Err("mock DAAP request ended before complete headers");
            }
            request.extend_from_slice(&chunk[..read]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let header = &request[..header_end];
            let request_line_end = header
                .windows(2)
                .position(|bytes| bytes == b"\r\n")
                .unwrap_or(header.len());
            let request_line = std::str::from_utf8(&header[..request_line_end])
                .map_err(|_| "mock DAAP request line was not UTF-8")?;
            let mut fields = request_line.split_whitespace();
            if fields.next() != Some("GET") {
                return Err("mock DAAP request did not use GET");
            }
            return fields
                .next()
                .filter(|path| path.starts_with('/'))
                .map(str::to_string)
                .ok_or("mock DAAP request path was missing");
        }
    })
    .await
    .map_err(|_| "mock DAAP request headers exceeded the deadline")?
}

fn record_mock_failure(state: &MockDaapState, reason: &str) {
    state
        .handler_failures
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(reason.to_string());
    state.request_changed.notify_waiters();
}

fn response_for(state: &MockDaapState, endpoint: MockEndpoint) -> MockResponse {
    let scripted = state
        .scripted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(&endpoint)
        .and_then(VecDeque::pop_front);
    if let Some(response) = scripted {
        return response;
    }

    match endpoint {
        MockEndpoint::ServerInfo => {
            let children = [tlv_u32(b"mstt", 200), tlv(b"minm", b"Mock DAAP")].concat();
            MockResponse::dmap(tlv(b"msrv", &children))
        }
        MockEndpoint::Login => {
            let children = [tlv_u32(b"mstt", 200), tlv_u32(b"mlid", 42)].concat();
            MockResponse::dmap(tlv(b"mlog", &children))
        }
        MockEndpoint::Update => {
            let children = [tlv_u32(b"mstt", 200), tlv_u32(b"musr", 7)].concat();
            MockResponse::dmap(tlv(b"mupd", &children))
        }
        MockEndpoint::Databases => {
            let database = [tlv_u32(b"miid", 1), tlv(b"minm", b"Music")].concat();
            let listing = tlv(b"mlcl", &tlv(b"mlit", &database));
            let children = [tlv_u32(b"mstt", 200), listing].concat();
            MockResponse::dmap(tlv(b"avdb", &children))
        }
        MockEndpoint::Items => {
            let track = [
                tlv_u32(b"miid", 9),
                tlv(b"minm", b"Lifecycle Song"),
                tlv(b"asar", b"Mock Artist"),
                tlv(b"asal", b"Mock Album"),
                tlv(b"asfm", b"mp3"),
            ]
            .concat();
            let listing = tlv(b"mlcl", &tlv(b"mlit", &track));
            let children = [tlv_u32(b"mstt", 200), listing].concat();
            MockResponse::dmap(tlv(b"adbs", &children))
        }
        MockEndpoint::Stream => MockResponse {
            status: "200 OK",
            content_type: "audio/mpeg",
            body: b"mock audio".to_vec(),
        },
        MockEndpoint::Artwork => MockResponse {
            status: "200 OK",
            content_type: "image/png",
            body: b"mock artwork".to_vec(),
        },
        MockEndpoint::Logout => MockResponse::dmap(Vec::new()),
        MockEndpoint::Other => MockResponse {
            status: "404 Not Found",
            content_type: "text/plain",
            body: b"not found".to_vec(),
        },
    }
}

fn tlv(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + payload.len());
    bytes.extend_from_slice(tag);
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn tlv_u32(tag: &[u8; 4], value: u32) -> Vec<u8> {
    tlv(tag, &value.to_be_bytes())
}

fn registry() -> RemoteSourceRegistry {
    RemoteSourceRegistry::new(tokio::runtime::Handle::current())
}

fn claim_saved(registry: &RemoteSourceRegistry, source_id: SourceId) {
    assert!(registry
        .claim_provenance(source_id, SourceProvenance::Saved)
        .is_some());
}

fn connect_daap(
    registry: &RemoteSourceRegistry,
    source_id: SourceId,
    name: &'static str,
    base_url: String,
) -> u64 {
    registry
        .connect_daap(
            source_id,
            |_| {},
            move || async move { DaapBackend::login(name, &base_url, None).await },
        )
        .expect("DAAP connect admitted")
}

async fn wait_for_catalogue(
    registry: &RemoteSourceRegistry,
    source_id: SourceId,
    generation: u64,
) -> (u64, Arc<Vec<crate::architecture::models::Track>>) {
    let mut invalidations = registry.subscribe_invalidations();
    tokio::time::timeout(MOCK_DEADLINE, async {
        loop {
            if let Some(catalogue) = registry
                .snapshot(source_id)
                .and_then(|snapshot| snapshot.catalogue)
                .filter(|catalogue| catalogue.generation == generation)
            {
                return (catalogue.session_epoch, catalogue.value);
            }
            assert!(
                invalidations.changed().await.is_ok(),
                "lifecycle observer closed before catalogue publication"
            );
        }
    })
    .await
    .expect("DAAP catalogue must publish before the deadline")
}

async fn wait_for_failed_retirement(
    registry: &RemoteSourceRegistry,
    source_id: SourceId,
) -> crate::source_lifecycle::LifecycleSnapshot<Vec<crate::architecture::models::Track>> {
    let mut invalidations = registry.subscribe_invalidations();
    tokio::time::timeout(MOCK_DEADLINE, async {
        loop {
            if let Some(snapshot) = registry.snapshot(source_id) {
                if snapshot.failure.is_some()
                    && snapshot.session_epoch.is_none()
                    && snapshot.pending_connect.is_none()
                    && snapshot.pending_retirements == 0
                {
                    return snapshot;
                }
            }
            assert!(
                invalidations.changed().await.is_ok(),
                "lifecycle observer closed before failed retirement"
            );
        }
    })
    .await
    .expect("failed DAAP session must retire before the deadline")
}

#[tokio::test]
async fn shutdown_during_held_login_waits_then_logs_out_without_catalogue_requests() {
    let server = MockDaapServer::start().await;
    let login_gate = server.hold(MockEndpoint::Login);
    let registry = registry();
    let source_id = SourceId::random();
    claim_saved(&registry, source_id);

    connect_daap(&registry, source_id, "Held login", server.base_url.clone());
    server.wait_for_requests(MockEndpoint::Login, 1).await;

    let barrier = registry.shutdown();
    assert!(!barrier.is_complete());
    login_gate.add_permits(1);
    tokio::time::timeout(MOCK_DEADLINE, barrier.wait())
        .await
        .expect("shutdown must join protected construction and logout");
    server.wait_for_requests(MockEndpoint::Logout, 1).await;

    let snapshot = registry
        .snapshot(source_id)
        .expect("retained source snapshot");
    assert!(snapshot.session_epoch.is_none());
    assert!(snapshot.catalogue.is_none());
    assert!(snapshot.pending_connect.is_none());
    assert_eq!(snapshot.pending_retirements, 0);
    assert_eq!(server.request_count(MockEndpoint::ServerInfo), 1);
    assert_eq!(server.request_count(MockEndpoint::Login), 1);
    assert_eq!(server.request_count(MockEndpoint::Logout), 1);
    assert_eq!(server.request_count(MockEndpoint::Update), 0);
    assert_eq!(server.request_count(MockEndpoint::Databases), 0);
    assert_eq!(server.request_count(MockEndpoint::Items), 0);
    server.assert_healthy();
}

#[tokio::test]
async fn superseding_held_login_retires_it_without_reaching_catalogue_routes() {
    let first_server = MockDaapServer::start().await;
    let first_login_gate = first_server.hold(MockEndpoint::Login);
    let second_server = MockDaapServer::start().await;
    let registry = registry();
    let source_id = SourceId::random();
    claim_saved(&registry, source_id);

    connect_daap(
        &registry,
        source_id,
        "Held predecessor",
        first_server.base_url.clone(),
    );
    first_server.wait_for_requests(MockEndpoint::Login, 1).await;

    let successor_generation = connect_daap(
        &registry,
        source_id,
        "Successor",
        second_server.base_url.clone(),
    );
    let (successor_epoch, _) = wait_for_catalogue(&registry, source_id, successor_generation).await;

    first_login_gate.add_permits(1);
    first_server
        .wait_for_requests(MockEndpoint::Logout, 1)
        .await;
    let snapshot = registry.snapshot(source_id).expect("successor snapshot");
    assert_eq!(snapshot.session_epoch, Some(successor_epoch));
    assert_eq!(
        snapshot.catalogue.as_ref().map(|value| value.generation),
        Some(successor_generation)
    );
    assert_eq!(first_server.request_count(MockEndpoint::Logout), 1);
    assert_eq!(first_server.request_count(MockEndpoint::Update), 0);
    assert_eq!(first_server.request_count(MockEndpoint::Databases), 0);
    assert_eq!(first_server.request_count(MockEndpoint::Items), 0);

    let barrier = registry.shutdown();
    tokio::time::timeout(MOCK_DEADLINE, barrier.wait())
        .await
        .expect("successor shutdown must finish");
    second_server
        .wait_for_requests(MockEndpoint::Logout, 1)
        .await;
    assert_eq!(first_server.request_count(MockEndpoint::ServerInfo), 1);
    assert_eq!(first_server.request_count(MockEndpoint::Login), 1);
    assert_eq!(first_server.request_count(MockEndpoint::Logout), 1);
    assert_eq!(second_server.request_count(MockEndpoint::ServerInfo), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Login), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Update), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Databases), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Items), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Logout), 1);
    first_server.assert_healthy();
    second_server.assert_healthy();
}

#[tokio::test]
async fn disconnect_during_held_update_aborts_catalogue_and_logs_out_once() {
    let server = MockDaapServer::start().await;
    let update_gate = server.hold(MockEndpoint::Update);
    let registry = registry();
    let source_id = SourceId::random();
    claim_saved(&registry, source_id);

    connect_daap(&registry, source_id, "Held update", server.base_url.clone());
    server.wait_for_requests(MockEndpoint::Update, 1).await;
    let waiter = registry.disconnect(source_id).expect("staged DAAP session");
    // Do not let the fixture itself keep escaped catalogue work inert. If the
    // registry failed to abort the staged task, this valid update response can
    // now advance it to the database routes and make the assertions fail.
    update_gate.add_permits(1);
    let failure = tokio::time::timeout(MOCK_DEADLINE, waiter.wait())
        .await
        .expect("disconnect must abort update and join logout");
    assert!(failure.is_none());
    server.wait_for_requests(MockEndpoint::Logout, 1).await;

    let snapshot = registry
        .snapshot(source_id)
        .expect("retained source snapshot");
    assert!(snapshot.session_epoch.is_none());
    assert!(snapshot.catalogue.is_none());
    assert!(snapshot.pending_connect.is_none());
    assert_eq!(snapshot.pending_retirements, 0);
    assert_eq!(server.request_count(MockEndpoint::ServerInfo), 1);
    assert_eq!(server.request_count(MockEndpoint::Login), 1);
    assert_eq!(server.request_count(MockEndpoint::Update), 1);
    assert_eq!(server.request_count(MockEndpoint::Logout), 1);
    assert_eq!(server.request_count(MockEndpoint::Databases), 0);
    assert_eq!(server.request_count(MockEndpoint::Items), 0);
    server.assert_healthy();
}

#[tokio::test]
async fn malformed_post_login_routes_fail_and_logout_exactly_once() {
    let cases = [
        (MockEndpoint::Update, 0, 0),
        (MockEndpoint::Databases, 1, 0),
        (MockEndpoint::Items, 1, 1),
    ];

    for (endpoint, expected_databases, expected_items) in cases {
        let server = MockDaapServer::start().await;
        server.enqueue(endpoint, MockResponse::dmap(tlv(b"nope", &[])));
        let registry = registry();
        let source_id = SourceId::random();
        claim_saved(&registry, source_id);

        connect_daap(&registry, source_id, "Malformed", server.base_url.clone());
        server.wait_for_requests(MockEndpoint::Logout, 1).await;
        let snapshot = wait_for_failed_retirement(&registry, source_id).await;
        let failure = snapshot.failure.expect("correlated connect failure");
        assert_eq!(failure.failure.category(), FailureCategory::InvalidResponse);
        assert!(snapshot.catalogue.is_none());

        assert_eq!(server.request_count(MockEndpoint::ServerInfo), 1);
        assert_eq!(server.request_count(MockEndpoint::Login), 1);
        assert_eq!(
            server.request_count(MockEndpoint::Logout),
            1,
            "{endpoint:?}"
        );
        assert_eq!(
            server.request_count(MockEndpoint::Update),
            1,
            "{endpoint:?}"
        );
        assert_eq!(
            server.request_count(MockEndpoint::Databases),
            expected_databases,
            "{endpoint:?}"
        );
        assert_eq!(
            server.request_count(MockEndpoint::Items),
            expected_items,
            "{endpoint:?}"
        );
        assert_eq!(server.request_count(MockEndpoint::Stream), 0);
        assert_eq!(server.request_count(MockEndpoint::Artwork), 0);

        let barrier = registry.shutdown();
        tokio::time::timeout(MOCK_DEADLINE, barrier.wait())
            .await
            .expect("failed registry shutdown must finish");
        assert_eq!(
            server.request_count(MockEndpoint::Logout),
            1,
            "{endpoint:?} shutdown must not repeat logout"
        );
        server.assert_healthy();
    }
}

#[tokio::test]
async fn replacement_rejects_stale_epoch_stream_and_art_before_adapter_invocation() {
    let first_server = MockDaapServer::start().await;
    let second_server = MockDaapServer::start().await;
    let registry = registry();
    let source_id = SourceId::random();
    claim_saved(&registry, source_id);

    let first_generation =
        connect_daap(&registry, source_id, "First", first_server.base_url.clone());
    let (first_epoch, first_tracks) =
        wait_for_catalogue(&registry, source_id, first_generation).await;
    let track_id = first_tracks[0]
        .native_track_id
        .clone()
        .expect("native DAAP item identity");
    let first_stream = registry
        .resolve_stream(source_id, first_epoch, track_id.clone())
        .await
        .expect("current stream resolves");
    let first_artwork = registry
        .resolve_artwork(source_id, first_epoch, track_id.clone())
        .await
        .expect("current artwork resolves")
        .expect("DAAP artwork request");
    assert!(first_stream.is_active());
    assert!(first_artwork.is_active());

    let second_generation = connect_daap(
        &registry,
        source_id,
        "Second",
        second_server.base_url.clone(),
    );
    let (second_epoch, _) = wait_for_catalogue(&registry, source_id, second_generation).await;
    assert_ne!(first_epoch, second_epoch);
    first_server
        .wait_for_requests(MockEndpoint::Logout, 1)
        .await;
    assert!(!first_stream.is_active());
    assert!(!first_artwork.is_active());

    assert!(registry
        .resolve_stream(source_id, first_epoch, track_id.clone())
        .await
        .is_err());
    assert!(registry
        .resolve_artwork(source_id, first_epoch, track_id.clone())
        .await
        .is_err());
    let current = registry
        .resolve_stream(source_id, second_epoch, track_id)
        .await
        .expect("successor stream resolves");
    assert!(current.is_active());
    assert_eq!(first_server.request_count(MockEndpoint::Stream), 0);
    assert_eq!(first_server.request_count(MockEndpoint::Artwork), 0);
    assert_eq!(second_server.request_count(MockEndpoint::Stream), 0);
    assert_eq!(second_server.request_count(MockEndpoint::Artwork), 0);

    let barrier = registry.shutdown();
    tokio::time::timeout(MOCK_DEADLINE, barrier.wait())
        .await
        .expect("replacement registry shutdown must finish");
    second_server
        .wait_for_requests(MockEndpoint::Logout, 1)
        .await;
    assert_eq!(first_server.request_count(MockEndpoint::ServerInfo), 1);
    assert_eq!(first_server.request_count(MockEndpoint::Login), 1);
    assert_eq!(first_server.request_count(MockEndpoint::Update), 1);
    assert_eq!(first_server.request_count(MockEndpoint::Databases), 1);
    assert_eq!(first_server.request_count(MockEndpoint::Items), 1);
    assert_eq!(second_server.request_count(MockEndpoint::ServerInfo), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Login), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Update), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Databases), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Items), 1);
    assert_eq!(first_server.request_count(MockEndpoint::Logout), 1);
    assert_eq!(second_server.request_count(MockEndpoint::Logout), 1);
    first_server.assert_healthy();
    second_server.assert_healthy();
}
