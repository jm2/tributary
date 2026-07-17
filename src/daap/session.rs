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
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    static REGISTRY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct MockDaapServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl MockDaapServer {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock DAAP server");
            let address = listener.local_addr().expect("mock address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_for_task = Arc::clone(&requests);

            let task = tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let requests = Arc::clone(&requests_for_task);
                    tokio::spawn(async move {
                        let mut request = vec![0_u8; 16 * 1024];
                        let Ok(read) = stream.read(&mut request).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        let request = String::from_utf8_lossy(&request[..read]);
                        let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                        requests
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(path.clone());

                        let (status, content_type, body) = response_for(&path);
                        let headers = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(headers.as_bytes()).await;
                        let _ = stream.write_all(&body).await;
                    });
                }
            });

            Self {
                base_url: format!("http://{address}"),
                requests,
                task,
            }
        }

        fn paths(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Drop for MockDaapServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn reset_registry() {
        shutdown_all().await;
        *lock_registry() = SessionRegistry::default();
        registry_notify().notify_waiters();
    }

    fn logout_count(server: &MockDaapServer) -> usize {
        server
            .paths()
            .iter()
            .filter(|path| path.starts_with("/logout?"))
            .count()
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

    #[tokio::test]
    async fn lifecycle_retains_session_resolves_media_and_logs_out_once() {
        let _registry_guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry().await;
        let server = MockDaapServer::start().await;
        let backend = DaapBackend::connect("Mock DAAP", &server.base_url, None)
            .await
            .expect("connect and sync");
        let tracks = backend.all_tracks().await;
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
        let queued_reference = first.all_tracks().await[0]
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

    fn response_for(path: &str) -> (&'static str, &'static str, Vec<u8>) {
        if path.starts_with("/server-info") {
            let children = [tlv_u32(b"mstt", 200), tlv(b"minm", b"Mock DAAP")].concat();
            return (
                "200 OK",
                "application/x-dmap-tagged",
                tlv(b"msrv", &children),
            );
        }
        if path.starts_with("/login") {
            return (
                "200 OK",
                "application/x-dmap-tagged",
                tlv(b"mlog", &tlv_u32(b"mlid", 42)),
            );
        }
        if path.starts_with("/update?") {
            return (
                "200 OK",
                "application/x-dmap-tagged",
                tlv(b"mupd", &tlv_u32(b"musr", 7)),
            );
        }
        if path.starts_with("/databases?") {
            let database = [tlv_u32(b"miid", 1), tlv(b"minm", b"Music")].concat();
            let listing = tlv(b"mlcl", &tlv(b"mlit", &database));
            return (
                "200 OK",
                "application/x-dmap-tagged",
                tlv(b"avdb", &listing),
            );
        }
        if path.starts_with("/databases/1/items?") {
            let track = [
                tlv_u32(b"miid", 9),
                tlv(b"minm", b"Lifecycle Song"),
                tlv(b"asar", b"Mock Artist"),
                tlv(b"asal", b"Mock Album"),
                tlv(b"asfm", b"mp3"),
            ]
            .concat();
            let listing = tlv(b"mlcl", &tlv(b"mlit", &track));
            return (
                "200 OK",
                "application/x-dmap-tagged",
                tlv(b"adbs", &listing),
            );
        }
        if path.starts_with("/databases/1/items/9.mp3?") {
            return ("200 OK", "audio/mpeg", b"mock audio".to_vec());
        }
        if path.starts_with("/databases/1/items/9/extra_data/artwork?") {
            return ("200 OK", "image/png", b"mock artwork".to_vec());
        }
        if path.starts_with("/logout?") {
            return ("200 OK", "application/x-dmap-tagged", Vec::new());
        }
        ("404 Not Found", "text/plain", b"not found".to_vec())
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
