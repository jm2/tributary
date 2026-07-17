//! Ownership registry for live DAAP sessions.
//!
//! DAAP is stateful: media requests remain authorized only while the backend
//! that owns the login session is alive. The UI retains connected backends
//! here and stores credential-free `daap://` references in its track models.
//! Those references are resolved to credential-isolated, revocable HTTP
//! requests only when media is about to be consumed.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use futures::future::join_all;
use tokio::sync::Notify;
use url::Url;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::ResolvedHttpRequest;

use super::DaapBackend;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RegistryGate {
    #[default]
    Running,
    ShuttingDown,
    ShutDown,
}

struct ActiveSession {
    generation: u64,
    backend: Arc<DaapBackend>,
}

#[derive(Default)]
struct SessionRegistry {
    gate: RegistryGate,
    next_generation: u64,
    latest_generation: HashMap<String, u64>,
    pending_attempts: HashSet<(String, u64)>,
    by_source: HashMap<String, ActiveSession>,
    by_session: HashMap<Uuid, Arc<DaapBackend>>,
    /// Backends displaced or released but not yet fully logged out. Keeping
    /// ownership here lets controlled shutdown join every in-flight logout.
    retiring: HashMap<Uuid, Arc<DaapBackend>>,
}

fn registry() -> &'static Mutex<SessionRegistry> {
    static REGISTRY: OnceLock<Mutex<SessionRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(SessionRegistry::default()))
}

fn registry_notify() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

fn lock_registry() -> MutexGuard<'static, SessionRegistry> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A generation-scoped connection attempt registered before network I/O.
/// Shutdown waits for every live attempt to finish or be dropped.
pub struct ConnectionAttempt {
    source_key: String,
    generation: u64,
    completed: bool,
}

impl ConnectionAttempt {
    /// Whether this remains the newest allowed attempt for its source.
    pub fn is_latest(&self) -> bool {
        let sessions = lock_registry();
        sessions.gate == RegistryGate::Running
            && sessions.latest_generation.get(&self.source_key) == Some(&self.generation)
    }

    /// Install the connected backend only if this attempt remains current.
    /// Displaced and rejected backends are logged out before this returns.
    pub async fn retain(mut self, backend: DaapBackend) -> Option<RetainedSession> {
        let backend = Arc::new(backend);
        let session_key = backend.session_key();

        let (accepted, replaced) = {
            let mut sessions = lock_registry();
            sessions
                .pending_attempts
                .remove(&(self.source_key.clone(), self.generation));
            self.completed = true;

            let accepted = sessions.gate == RegistryGate::Running
                && sessions.latest_generation.get(&self.source_key) == Some(&self.generation);

            if accepted {
                let replaced = sessions.by_source.insert(
                    self.source_key.clone(),
                    ActiveSession {
                        generation: self.generation,
                        backend: Arc::clone(&backend),
                    },
                );
                if let Some(previous) = &replaced {
                    previous.backend.revoke_media();
                    sessions.by_session.remove(&previous.backend.session_key());
                    sessions.retiring.insert(
                        previous.backend.session_key(),
                        Arc::clone(&previous.backend),
                    );
                }
                sessions
                    .by_session
                    .insert(session_key, Arc::clone(&backend));
                (true, replaced.map(|entry| entry.backend))
            } else {
                sessions.retiring.insert(session_key, Arc::clone(&backend));
                (false, None)
            }
        };
        registry_notify().notify_waiters();

        if let Some(previous) = replaced {
            disconnect_tracked(previous).await;
        }

        if !accepted {
            disconnect_tracked(backend).await;
            return None;
        }

        Some(RetainedSession {
            source_key: self.source_key.clone(),
            generation: self.generation,
            session_key,
            backend,
        })
    }
}

impl Drop for ConnectionAttempt {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut sessions = lock_registry();
        sessions
            .pending_attempts
            .remove(&(self.source_key.clone(), self.generation));
        if sessions.gate == RegistryGate::Running
            && sessions.latest_generation.get(&self.source_key) == Some(&self.generation)
        {
            if let Some(active_generation) = sessions
                .by_source
                .get(&self.source_key)
                .map(|entry| entry.generation)
            {
                sessions
                    .latest_generation
                    .insert(self.source_key.clone(), active_generation);
            } else {
                sessions.latest_generation.remove(&self.source_key);
            }
        }
        drop(sessions);
        registry_notify().notify_waiters();
    }
}

/// A retained backend plus the generation needed to validate queued UI work.
#[derive(Clone)]
pub struct RetainedSession {
    source_key: String,
    generation: u64,
    session_key: Uuid,
    backend: Arc<DaapBackend>,
}

impl RetainedSession {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn session_key(&self) -> Uuid {
        self.session_key
    }

    pub fn is_current(&self) -> bool {
        is_current_session(&self.source_key, self.generation, self.session_key)
    }
}

impl Deref for RetainedSession {
    type Target = DaapBackend;

    fn deref(&self) -> &Self::Target {
        &self.backend
    }
}

/// Ownership transferred out of the active source map. The registry keeps a
/// second reference until [`ReleasedSession::disconnect`] or shutdown joins it.
pub struct ReleasedSession {
    backend: Arc<DaapBackend>,
}

impl ReleasedSession {
    pub async fn disconnect(self) -> bool {
        disconnect_tracked(self.backend).await
    }
}

/// Register an attempt before starting the DAAP handshake. Returns `None`
/// after controlled shutdown has closed the connection gate.
pub fn begin_connect(source_key: String) -> Option<ConnectionAttempt> {
    let generation = {
        let mut sessions = lock_registry();
        if sessions.gate != RegistryGate::Running {
            return None;
        }
        sessions.next_generation = sessions.next_generation.wrapping_add(1).max(1);
        let generation = sessions.next_generation;
        sessions
            .latest_generation
            .insert(source_key.clone(), generation);
        sessions
            .pending_attempts
            .insert((source_key.clone(), generation));
        generation
    };
    registry_notify().notify_waiters();
    Some(ConnectionAttempt {
        source_key,
        generation,
        completed: false,
    })
}

/// Invalidate pending attempts and transfer an active source into registry-
/// owned retirement before scheduling its logout.
pub fn release_source(source_key: &str) -> Option<ReleasedSession> {
    let backend = {
        let mut sessions = lock_registry();
        sessions.latest_generation.remove(source_key);
        let entry = sessions.by_source.remove(source_key)?;
        entry.backend.revoke_media();
        sessions.by_session.remove(&entry.backend.session_key());
        sessions
            .retiring
            .insert(entry.backend.session_key(), Arc::clone(&entry.backend));
        entry.backend
    };
    registry_notify().notify_waiters();
    Some(ReleasedSession { backend })
}

/// Verify that a queued DAAP sync still belongs to the current source owner.
pub fn is_current_session(source_key: &str, generation: u64, session_key: Uuid) -> bool {
    let sessions = lock_registry();
    if sessions.gate != RegistryGate::Running {
        return false;
    }
    sessions.latest_generation.get(source_key) == Some(&generation)
        && sessions.by_source.get(source_key).is_some_and(|entry| {
            entry.generation == generation && entry.backend.session_key() == session_key
        })
}

/// Close the registry gate synchronously so no connection can begin or become
/// active after the GTK close callback schedules asynchronous shutdown.
pub fn begin_shutdown() {
    let changed = {
        let mut sessions = lock_registry();
        if sessions.gate != RegistryGate::Running {
            false
        } else {
            sessions.gate = RegistryGate::ShuttingDown;
            sessions.latest_generation.clear();
            sessions.by_session.clear();
            let active = std::mem::take(&mut sessions.by_source);
            for entry in active.into_values() {
                entry.backend.revoke_media();
                sessions
                    .retiring
                    .insert(entry.backend.session_key(), entry.backend);
            }
            true
        }
    };
    if changed {
        registry_notify().notify_waiters();
    }
}

/// Explicitly close every active, retiring, and in-flight connection during
/// controlled shutdown. This does not return until registry ownership is empty.
pub async fn shutdown_all() {
    begin_shutdown();

    loop {
        let changed = registry_notify().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let (pending, backends) = {
            let sessions = lock_registry();
            (
                sessions.pending_attempts.len(),
                sessions.retiring.values().cloned().collect::<Vec<_>>(),
            )
        };

        if pending == 0 && backends.is_empty() {
            lock_registry().gate = RegistryGate::ShutDown;
            registry_notify().notify_waiters();
            return;
        }

        if backends.is_empty() {
            changed.await;
            continue;
        }

        join_all(backends.iter().map(|backend| backend.disconnect())).await;
        {
            let mut sessions = lock_registry();
            for backend in &backends {
                sessions.retiring.remove(&backend.session_key());
            }
        }
        registry_notify().notify_waiters();
    }
}

async fn disconnect_tracked(backend: Arc<DaapBackend>) -> bool {
    let disconnected = backend.disconnect().await;
    lock_registry().retiring.remove(&backend.session_key());
    registry_notify().notify_waiters();
    disconnected
}

/// Return true for DAAP-owned references, including malformed values that
/// must fail closed instead of reaching GStreamer as ordinary URIs.
pub fn is_media_reference(reference: &str) -> bool {
    reference
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("daap"))
}

/// Resolve a credential-free DAAP stream reference through its retained live
/// session into a typed request whose bearer state stays isolated.
pub fn resolve_stream_reference(reference: &str) -> BackendResult<ResolvedHttpRequest> {
    resolve_media_reference(reference, "stream")
}

/// Resolve a credential-free DAAP artwork reference through its retained live
/// session into a typed request whose bearer state stays isolated.
pub fn resolve_artwork_reference(reference: &str) -> BackendResult<ResolvedHttpRequest> {
    resolve_media_reference(reference, "artwork")
}

fn resolve_media_reference(
    reference: &str,
    expected_kind: &str,
) -> BackendResult<ResolvedHttpRequest> {
    let reference =
        Url::parse(reference).map_err(|_| invalid_reference("malformed reference URL"))?;
    if reference.scheme() != "daap"
        || !reference.username().is_empty()
        || reference.password().is_some()
        || reference.port().is_some()
        || reference.fragment().is_some()
    {
        return Err(invalid_reference("invalid reference authority"));
    }

    let session_key = reference
        .host_str()
        .ok_or_else(|| invalid_reference("missing session key"))?
        .parse::<Uuid>()
        .map_err(|_| invalid_reference("invalid session key"))?;

    let mut segments = reference
        .path_segments()
        .ok_or_else(|| invalid_reference("missing media path"))?;
    let kind = segments
        .next()
        .ok_or_else(|| invalid_reference("missing media kind"))?;
    let song_id = segments
        .next()
        .ok_or_else(|| invalid_reference("missing item ID"))?
        .parse::<u32>()
        .map_err(|_| invalid_reference("invalid item ID"))?;
    if segments.next().is_some() {
        return Err(invalid_reference("unexpected media path component"));
    }
    if kind != expected_kind {
        return Err(invalid_reference("media reference has the wrong kind"));
    }

    let backend = lock_registry()
        .by_session
        .get(&session_key)
        .cloned()
        .ok_or_else(|| BackendError::ConnectionFailed {
            message: "DAAP source is no longer connected".to_string(),
            source: None,
        })?;

    match expected_kind {
        "stream" => {
            let query_pairs: Vec<_> = reference.query_pairs().collect();
            if query_pairs.len() != 1 || query_pairs[0].0 != "format" {
                return Err(invalid_reference("unexpected stream query state"));
            }
            let format = query_pairs
                .into_iter()
                .next()
                .map(|(_, value)| value.into_owned())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| invalid_reference("missing stream format"))?;
            backend.stream_request_for_item(song_id, &format)
        }
        "artwork" => {
            if reference.query().is_some() {
                return Err(invalid_reference("unexpected artwork query state"));
            }
            backend.artwork_request_for_item(song_id)
        }
        _ => Err(invalid_reference("unknown media kind")),
    }
}

fn invalid_reference(detail: &str) -> BackendError {
    BackendError::ParseError {
        message: format!("Invalid DAAP media reference: {detail}"),
        source: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;
    use tokio::task::JoinSet;

    use super::*;

    static REGISTRY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    const MOCK_DEADLINE: Duration = Duration::from_secs(5);
    const MOCK_REQUEST_HEADER_CAP: usize = 16 * 1024;
    // Deliberately smaller than any real request line so every fixture test
    // exercises fragmented socket reads deterministically.
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

        fn status(status: &'static str) -> Self {
            Self {
                status,
                content_type: "text/plain",
                body: Vec::new(),
            }
        }
    }

    #[derive(Default)]
    struct MockDaapState {
        requests: Mutex<Vec<String>>,
        scripted: Mutex<HashMap<MockEndpoint, VecDeque<MockResponse>>>,
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
            let address = listener.local_addr().expect("mock address");
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
                            if let Some(Err(_)) = result {
                                record_mock_failure(&state_for_task, "mock DAAP handler panicked");
                            }
                        }
                    }
                }
                while let Some(result) = handlers.join_next().await {
                    if result.is_err() {
                        record_mock_failure(&state_for_task, "mock DAAP handler panicked");
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
            .expect("mock DAAP request must arrive before the test deadline");
        }

        fn assert_healthy(&self) {
            let failures = self
                .state
                .handler_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(failures.is_empty(), "mock DAAP failures: {failures:?}");
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
        state
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(path.clone());
        state.request_changed.notify_waiters();

        let response = response_for(&state, &path);
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

                let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                else {
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
                let path = fields
                    .next()
                    .filter(|path| path.starts_with('/'))
                    .ok_or("mock DAAP request path was missing")?;
                return Ok(path.to_string());
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

    async fn reset_registry() {
        shutdown_all().await;
        *lock_registry() = SessionRegistry::default();
        registry_notify().notify_waiters();
    }

    fn logout_count(server: &MockDaapServer) -> usize {
        server.request_count(MockEndpoint::Logout)
    }

    #[tokio::test]
    async fn discovery_loss_invalidates_an_attempt_before_handshake_start() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let source_key = "queued-daap-then-lost";
        let attempt = begin_connect(source_key.to_string()).expect("queued DAAP attempt");

        // No session exists to return, but release still removes the current
        // generation. A queued task that begins afterward must fail closed.
        assert!(release_source(source_key).is_none());
        assert!(!attempt.is_latest());
        drop(attempt);
        assert!(!lock_registry()
            .pending_attempts
            .iter()
            .any(|(key, _)| key == source_key));
        reset_registry().await;
    }

    fn malformed_items_responses() -> Vec<(&'static str, MockResponse)> {
        let wrong_top_level = MockResponse::dmap(tlv(
            b"avdb",
            &tlv(b"mlcl", &tlv(b"mlit", &tlv_u32(b"miid", 9))),
        ));
        let wrong_nested = MockResponse::dmap(tlv(
            b"adbs",
            &[tlv_u32(b"mstt", 200), tlv(b"mlit", &[])].concat(),
        ));

        let mut truncated_child = Vec::new();
        truncated_child.extend_from_slice(b"mlit");
        truncated_child.extend_from_slice(&16_u32.to_be_bytes());
        truncated_child.extend_from_slice(b"short");
        let malformed_nested = MockResponse::dmap(tlv(
            b"adbs",
            &[tlv_u32(b"mstt", 200), tlv(b"mlcl", &truncated_child)].concat(),
        ));

        let mut valid_prefix_then_truncated = tlv(b"mlit", &tlv_u32(b"miid", 9));
        valid_prefix_then_truncated.extend_from_slice(&truncated_child);
        let partial_listing = MockResponse::dmap(tlv(
            b"adbs",
            &[
                tlv_u32(b"mstt", 200),
                tlv(b"mlcl", &valid_prefix_then_truncated),
            ]
            .concat(),
        ));

        let mut truncated_top_level = Vec::new();
        truncated_top_level.extend_from_slice(b"adbs");
        truncated_top_level.extend_from_slice(&64_u32.to_be_bytes());
        truncated_top_level.extend_from_slice(b"short");

        let mut deep_listing = tlv(b"mlit", &[]);
        for _ in 0..40 {
            deep_listing = tlv(b"mlit", &deep_listing);
        }
        let excessive_nesting = MockResponse::dmap(tlv(
            b"adbs",
            &[tlv_u32(b"mstt", 200), tlv(b"mlcl", &deep_listing)].concat(),
        ));
        let short_status = MockResponse::dmap(tlv(
            b"adbs",
            &[tlv(b"mstt", &[0, 0, 200]), tlv(b"mlcl", &[])].concat(),
        ));
        let overlong_status = MockResponse::dmap(tlv(
            b"adbs",
            &[tlv(b"mstt", &[0, 0, 0, 200, 0]), tlv(b"mlcl", &[])].concat(),
        ));
        let duplicate_status = MockResponse::dmap(tlv(
            b"adbs",
            &[
                tlv_u32(b"mstt", 200),
                tlv_u32(b"mstt", 200),
                tlv(b"mlcl", &[]),
            ]
            .concat(),
        ));

        vec![
            ("wrong top-level container", wrong_top_level),
            ("wrong nested container", wrong_nested),
            ("malformed nested container", malformed_nested),
            (
                "valid item before malformed nested remainder",
                partial_listing,
            ),
            (
                "truncated top-level container",
                MockResponse::dmap(truncated_top_level),
            ),
            ("excessive container nesting", excessive_nesting),
            ("short response status", short_status),
            ("overlong response status", overlong_status),
            ("duplicate response status", duplicate_status),
        ]
    }

    fn assert_bounded_parse_error(error: BackendError, case: &str) {
        let BackendError::ParseError { message, .. } = error else {
            panic!("{case}: expected typed parse failure, got {error}");
        };
        assert!(
            message.len() <= 256,
            "{case}: parse diagnostic must stay bounded: {message}"
        );
        assert!(!message.contains("session-id"));
        assert!(!message.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn adversarial_item_containers_fail_before_publication_and_logout_once() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;

        for (case, response) in malformed_items_responses() {
            let server = MockDaapServer::start().await;
            server.enqueue(MockEndpoint::Items, response);
            let source_key = format!("adversarial:{case}");
            let attempt = begin_connect(source_key.clone()).expect("register connection attempt");

            let outcome = tokio::time::timeout(
                MOCK_DEADLINE,
                DaapBackend::connect("Adversarial DAAP", &server.base_url, None),
            )
            .await
            .unwrap_or_else(|_| panic!("{case}: failure and cleanup must be bounded"));
            let Err(error) = outcome else {
                panic!("{case}: malformed catalogue must not publish a backend");
            };
            assert_bounded_parse_error(error, case);

            drop(attempt);
            assert!(
                release_source(&source_key).is_none(),
                "{case}: failed initial sync must not enter the session registry"
            );
            server.wait_for_requests(MockEndpoint::Logout, 1).await;
            assert_eq!(server.request_count(MockEndpoint::Items), 1, "{case}");
            assert_eq!(logout_count(&server), 1, "{case}");
            assert_eq!(server.request_count(MockEndpoint::Stream), 0, "{case}");
            assert_eq!(server.request_count(MockEndpoint::Artwork), 0, "{case}");
            server.assert_healthy();
        }

        reset_registry().await;
    }

    fn in_band_items_status(status: u32) -> MockResponse {
        MockResponse::dmap(tlv(b"adbs", &tlv_u32(b"mstt", status)))
    }

    #[tokio::test]
    async fn session_expiration_fails_lifecycle_without_publication_and_logs_out_once() {
        // libdmapsharing rejects an invalid database session with HTTP 403,
        // while other peers use 401. Exercise both statuses on every
        // post-login HTTP route plus both in-band item-response forms.
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let cases = [
            (
                "update HTTP 401",
                MockEndpoint::Update,
                MockResponse::status("401 Unauthorized"),
            ),
            (
                "update HTTP 403",
                MockEndpoint::Update,
                MockResponse::status("403 Forbidden"),
            ),
            (
                "databases HTTP 401",
                MockEndpoint::Databases,
                MockResponse::status("401 Unauthorized"),
            ),
            (
                "databases HTTP 403",
                MockEndpoint::Databases,
                MockResponse::status("403 Forbidden"),
            ),
            (
                "items HTTP 401",
                MockEndpoint::Items,
                MockResponse::status("401 Unauthorized"),
            ),
            (
                "items HTTP 403",
                MockEndpoint::Items,
                MockResponse::status("403 Forbidden"),
            ),
            (
                "items DMAP mstt 401",
                MockEndpoint::Items,
                in_band_items_status(401),
            ),
            (
                "items DMAP mstt 403",
                MockEndpoint::Items,
                in_band_items_status(403),
            ),
        ];

        for (case, endpoint, response) in cases {
            let server = MockDaapServer::start().await;
            server.enqueue(endpoint, response);
            let source_key = format!("expired:{case}");
            let attempt = begin_connect(source_key.clone()).expect("register connection attempt");

            let outcome = tokio::time::timeout(
                MOCK_DEADLINE,
                DaapBackend::connect("Expired DAAP", &server.base_url, None),
            )
            .await
            .unwrap_or_else(|_| panic!("{case}: expiration must be bounded"));
            let Err(error) = outcome else {
                panic!("{case}: expired session must prevent backend construction");
            };
            assert!(
                matches!(
                    error,
                    BackendError::AuthenticationFailed { ref message }
                        if message == "DAAP session expired or unauthorized"
                ),
                "{case}: unexpected expiration error: {error}"
            );

            drop(attempt);
            assert!(
                release_source(&source_key).is_none(),
                "{case}: failed session must never enter the registry"
            );
            server.wait_for_requests(MockEndpoint::Logout, 1).await;
            assert_eq!(server.request_count(MockEndpoint::ServerInfo), 1, "{case}");
            assert_eq!(server.request_count(MockEndpoint::Login), 1, "{case}");
            assert_eq!(server.request_count(MockEndpoint::Update), 1, "{case}");
            assert_eq!(
                server.request_count(MockEndpoint::Databases),
                usize::from(endpoint != MockEndpoint::Update),
                "{case}"
            );
            assert_eq!(
                server.request_count(MockEndpoint::Items),
                usize::from(endpoint == MockEndpoint::Items),
                "{case}"
            );
            assert_eq!(logout_count(&server), 1, "{case}");
            assert_eq!(server.request_count(MockEndpoint::Stream), 0, "{case}");
            assert_eq!(server.request_count(MockEndpoint::Artwork), 0, "{case}");
            server.assert_healthy();
        }

        reset_registry().await;
    }

    #[tokio::test]
    async fn non_auth_dmap_status_fails_lifecycle_and_logs_out_once() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let server = MockDaapServer::start().await;
        server.enqueue(MockEndpoint::Items, in_band_items_status(500));
        let source_key = "non-auth-status".to_string();
        let attempt = begin_connect(source_key.clone()).expect("register connection attempt");

        let outcome = tokio::time::timeout(
            MOCK_DEADLINE,
            DaapBackend::connect("Failed DAAP", &server.base_url, None),
        )
        .await
        .expect("DMAP failure must be bounded");
        let Err(error) = outcome else {
            panic!("non-success DMAP status must fail");
        };
        assert!(matches!(
            error,
            BackendError::ConnectionFailed {
                ref message,
                source: None
            } if message == "DAAP items returned status 500"
        ));

        drop(attempt);
        assert!(release_source(&source_key).is_none());
        server.wait_for_requests(MockEndpoint::Logout, 1).await;
        assert_eq!(logout_count(&server), 1);
        assert_eq!(server.request_count(MockEndpoint::Stream), 0);
        assert_eq!(server.request_count(MockEndpoint::Artwork), 0);
        server.assert_healthy();
        reset_registry().await;
    }

    #[tokio::test]
    async fn lifecycle_retains_session_resolves_media_and_logs_out_once() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let server = MockDaapServer::start().await;
        let backend = DaapBackend::connect("Mock DAAP", &server.base_url, None)
            .await
            .expect("connect and sync");
        let tracks = crate::architecture::load_track_catalog(&backend)
            .await
            .expect("read DAAP track catalogue");
        assert_eq!(tracks.len(), 1);

        let stream_reference = tracks[0]
            .stream_url
            .as_ref()
            .expect("opaque stream reference");
        let artwork_reference = tracks[0]
            .cover_art_url
            .as_ref()
            .expect("opaque artwork reference");
        assert_eq!(stream_reference.scheme(), "daap");
        assert_eq!(artwork_reference.scheme(), "daap");
        assert!(!stream_reference.as_str().contains("session-id"));

        let direct_stream = backend
            .stream_request_for_track(&tracks[0].id)
            .await
            .expect("resolve backend stream request");
        assert_eq!(direct_stream.endpoint().scheme(), "http");
        assert!(!direct_stream.endpoint().as_str().contains("session-id"));
        assert_eq!(
            direct_stream.private_query_pairs(),
            &[("session-id".to_string(), "42".to_string())]
        );
        let retained = begin_connect(server.base_url.clone())
            .expect("open connection gate")
            .retain(backend)
            .await
            .expect("retain current session");
        let generation = retained.generation();
        let session_key = retained.session_key();
        assert!(retained.is_current());
        drop(retained);

        // The registry, rather than a temporary sync task, owns the backend.
        let stream_request =
            resolve_stream_reference(stream_reference.as_str()).expect("resolve stream reference");
        assert!(stream_request.is_active());
        assert!(!stream_request.endpoint().as_str().contains("session-id"));
        assert_eq!(
            stream_request.private_query_pairs(),
            &[("session-id".to_string(), "42".to_string())]
        );
        let mut stream_url = stream_request.endpoint().clone();
        for (key, value) in stream_request.private_query_pairs() {
            stream_url.query_pairs_mut().append_pair(key, value);
        }
        let body = reqwest::get(stream_url)
            .await
            .expect("play mock stream")
            .bytes()
            .await
            .expect("read mock stream");
        assert_eq!(body.as_ref(), b"mock audio");

        let artwork_request = resolve_artwork_reference(artwork_reference.as_str())
            .expect("resolve artwork reference");
        assert!(!artwork_request.endpoint().as_str().contains("session-id"));
        assert_eq!(
            artwork_request.private_query_pairs(),
            &[("session-id".to_string(), "42".to_string())]
        );

        let owned = release_source(&server.base_url).expect("retained source");
        assert!(!is_current_session(
            &server.base_url,
            generation,
            session_key
        ));
        assert!(!stream_request.is_active());
        assert!(!artwork_request.is_active());
        assert!(resolve_stream_reference(stream_reference.as_str()).is_err());
        assert!(owned.disconnect().await);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let paths = server.paths();
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.starts_with("/logout?"))
                .count(),
            1
        );
        assert!(paths.iter().any(|path| path.starts_with("/server-info")));
        assert!(paths.iter().any(|path| path.starts_with("/login")));
        assert!(paths.iter().any(|path| path.starts_with("/update?")));
        assert!(paths.iter().any(|path| path.starts_with("/databases?")));
        assert!(paths
            .iter()
            .any(|path| path.starts_with("/databases/1/items?")));
        assert!(paths
            .iter()
            .any(|path| path.starts_with("/databases/1/items/9.mp3?")));

        reset_registry().await;
    }

    #[tokio::test]
    async fn controlled_shutdown_logs_out_each_retained_session_once() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let server = MockDaapServer::start().await;
        let backend = DaapBackend::connect("Shutdown DAAP", &server.base_url, None)
            .await
            .expect("connect and sync");
        let source_key = format!("shutdown:{}", server.base_url);
        let retained = begin_connect(source_key)
            .expect("open connection gate")
            .retain(backend)
            .await
            .expect("retain current session");
        drop(retained);

        shutdown_all().await;
        shutdown_all().await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(logout_count(&server), 1);
        assert!(begin_connect("after-shutdown".to_string()).is_none());

        reset_registry().await;
    }

    #[tokio::test]
    async fn replacement_logs_out_displaced_session_once() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let first_server = MockDaapServer::start().await;
        let second_server = MockDaapServer::start().await;
        let source_key = "daap://logical-source".to_string();

        let first_backend = DaapBackend::connect("First", &first_server.base_url, None)
            .await
            .expect("connect first session");
        let first = begin_connect(source_key.clone())
            .expect("open first attempt")
            .retain(first_backend)
            .await
            .expect("retain first session");
        let first_generation = first.generation();
        let first_session_key = first.session_key();

        let second_backend = DaapBackend::connect("Second", &second_server.base_url, None)
            .await
            .expect("connect second session");
        let second = begin_connect(source_key.clone())
            .expect("open replacement attempt")
            .retain(second_backend)
            .await
            .expect("retain replacement session");

        assert!(!is_current_session(
            &source_key,
            first_generation,
            first_session_key
        ));
        assert!(second.is_current());
        assert_eq!(logout_count(&first_server), 1);
        assert_eq!(logout_count(&second_server), 0);

        release_source(&source_key)
            .expect("release replacement")
            .disconnect()
            .await;
        assert_eq!(logout_count(&second_server), 1);
        reset_registry().await;
    }

    #[tokio::test]
    async fn queued_sync_token_is_rejected_after_same_source_replacement() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let first_server = MockDaapServer::start().await;
        let second_server = MockDaapServer::start().await;
        let source_key = "daap://queued-sync-source".to_string();

        let first_backend = DaapBackend::connect("First", &first_server.base_url, None)
            .await
            .expect("connect first session");
        let first = begin_connect(source_key.clone())
            .expect("open first attempt")
            .retain(first_backend)
            .await
            .expect("retain first session");
        // This pair models the ownership token stored in a queued DaapSync.
        let queued_generation = first.generation();
        let queued_session_key = first.session_key();
        let queued_reference = crate::architecture::load_track_catalog(&*first)
            .await
            .expect("read retained DAAP track catalogue")[0]
            .stream_url
            .clone()
            .expect("queued stream reference");
        assert!(is_current_session(
            &source_key,
            queued_generation,
            queued_session_key
        ));

        let second_backend = DaapBackend::connect("Second", &second_server.base_url, None)
            .await
            .expect("connect replacement session");
        let second = begin_connect(source_key.clone())
            .expect("open replacement attempt")
            .retain(second_backend)
            .await
            .expect("retain replacement session");

        assert!(!is_current_session(
            &source_key,
            queued_generation,
            queued_session_key
        ));
        assert!(resolve_stream_reference(queued_reference.as_str()).is_err());
        assert!(is_current_session(
            &source_key,
            second.generation(),
            second.session_key()
        ));

        release_source(&source_key)
            .expect("release replacement")
            .disconnect()
            .await;
        reset_registry().await;
    }

    #[tokio::test]
    async fn retain_racing_shutdown_cannot_escape_registry_ownership() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let server = MockDaapServer::start().await;
        let source_key = "daap://retain-shutdown-race".to_string();
        let attempt = begin_connect(source_key).expect("open connection attempt");
        let backend = DaapBackend::connect("Racing", &server.base_url, None)
            .await
            .expect("connect session");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let retain_barrier = Arc::clone(&barrier);
        let retain_task = tokio::spawn(async move {
            retain_barrier.wait().await;
            attempt.retain(backend).await
        });
        let shutdown_task = tokio::spawn(async move {
            barrier.wait().await;
            shutdown_all().await;
        });

        let (retained, shutdown) = tokio::time::timeout(Duration::from_secs(2), async move {
            tokio::join!(retain_task, shutdown_task)
        })
        .await
        .expect("retain/shutdown race completed");
        shutdown.expect("shutdown task");
        if let Some(retained) = retained.expect("retain task") {
            assert!(!retained.is_current());
        }
        assert_eq!(logout_count(&server), 1);
        {
            let sessions = lock_registry();
            assert_eq!(sessions.gate, RegistryGate::ShutDown);
            assert!(sessions.pending_attempts.is_empty());
            assert!(sessions.by_source.is_empty());
            assert!(sessions.retiring.is_empty());
        }

        reset_registry().await;
    }

    #[tokio::test]
    async fn release_racing_shutdown_logs_out_once_and_is_joined() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let server = MockDaapServer::start().await;
        let source_key = "daap://release-shutdown-race".to_string();
        let backend = DaapBackend::connect("Racing", &server.base_url, None)
            .await
            .expect("connect session");
        let retained = begin_connect(source_key.clone())
            .expect("open connection attempt")
            .retain(backend)
            .await
            .expect("retain session");
        drop(retained);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let release_barrier = Arc::clone(&barrier);
        let release_task = tokio::spawn(async move {
            release_barrier.wait().await;
            if let Some(released) = release_source(&source_key) {
                released.disconnect().await;
            }
        });
        let shutdown_task = tokio::spawn(async move {
            barrier.wait().await;
            shutdown_all().await;
        });

        let (release, shutdown) = tokio::time::timeout(Duration::from_secs(2), async move {
            tokio::join!(release_task, shutdown_task)
        })
        .await
        .expect("release/shutdown race completed");
        release.expect("release task");
        shutdown.expect("shutdown task");
        assert_eq!(logout_count(&server), 1);
        {
            let sessions = lock_registry();
            assert_eq!(sessions.gate, RegistryGate::ShutDown);
            assert!(sessions.by_source.is_empty());
            assert!(sessions.retiring.is_empty());
        }

        reset_registry().await;
    }

    #[tokio::test]
    async fn dropping_backend_does_not_perform_network_logout() {
        let server = MockDaapServer::start().await;
        let backend = DaapBackend::connect("Mock DAAP", &server.base_url, None)
            .await
            .expect("connect and sync");
        drop(backend);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            server
                .paths()
                .iter()
                .filter(|path| path.starts_with("/logout?"))
                .count(),
            0
        );
    }

    fn response_for(state: &MockDaapState, path: &str) -> MockResponse {
        let endpoint = MockEndpoint::classify(path);
        let scripted_response = {
            let mut scripted = state
                .scripted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scripted.get_mut(&endpoint).and_then(VecDeque::pop_front)
        };
        if let Some(response) = scripted_response {
            return response;
        }

        if endpoint == MockEndpoint::ServerInfo {
            let children = [tlv_u32(b"mstt", 200), tlv(b"minm", b"Mock DAAP")].concat();
            return MockResponse::dmap(tlv(b"msrv", &children));
        }
        if endpoint == MockEndpoint::Login {
            let children = [tlv_u32(b"mstt", 200), tlv_u32(b"mlid", 42)].concat();
            return MockResponse::dmap(tlv(b"mlog", &children));
        }
        if endpoint == MockEndpoint::Update {
            let children = [tlv_u32(b"mstt", 200), tlv_u32(b"musr", 7)].concat();
            return MockResponse::dmap(tlv(b"mupd", &children));
        }
        if endpoint == MockEndpoint::Databases {
            let database = [tlv_u32(b"miid", 1), tlv(b"minm", b"Music")].concat();
            let listing = tlv(b"mlcl", &tlv(b"mlit", &database));
            let children = [tlv_u32(b"mstt", 200), listing].concat();
            return MockResponse::dmap(tlv(b"avdb", &children));
        }
        if endpoint == MockEndpoint::Items {
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
            return MockResponse::dmap(tlv(b"adbs", &children));
        }
        if endpoint == MockEndpoint::Stream {
            return MockResponse {
                status: "200 OK",
                content_type: "audio/mpeg",
                body: b"mock audio".to_vec(),
            };
        }
        if endpoint == MockEndpoint::Artwork {
            return MockResponse {
                status: "200 OK",
                content_type: "image/png",
                body: b"mock artwork".to_vec(),
            };
        }
        if endpoint == MockEndpoint::Logout {
            return MockResponse::dmap(Vec::new());
        }
        MockResponse {
            status: "404 Not Found",
            content_type: "text/plain",
            body: b"not found".to_vec(),
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
}
