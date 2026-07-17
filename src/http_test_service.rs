//! Bounded HTTP fixture shared by service-integration tests.
//!
//! Routes match independently of arrival order so clients may issue requests
//! concurrently.  A route key consists of the HTTP method, exact path, and a
//! required subset of decoded query pairs; unrelated query pairs (for example,
//! random authentication salts) are ignored.

use std::collections::{BTreeMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri};
use axum::Router;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
const MAX_RECORDED_REQUESTS: usize = 4 * 1024;
const MAX_RECORDED_FAILURES: usize = 64;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// One request observed by the fixture.
#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// A response returned by a [`MockRoute`].
#[derive(Clone, Debug)]
pub struct MockResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl MockResponse {
    /// Builds a `200 OK` JSON response.
    #[must_use]
    pub fn json(value: impl Serialize) -> Self {
        let body = serde_json::to_vec(&value).expect("mock response JSON must serialize");
        let mut response = Self::status(StatusCode::OK);
        response.headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        response.body = body;
        response
    }

    /// Builds a `200 OK` UTF-8 text response.
    #[must_use]
    pub fn text(body: impl Into<String>) -> Self {
        let mut response = Self::status(StatusCode::OK);
        response.headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        response.body = body.into().into_bytes();
        response
    }

    /// Builds an empty response with the supplied status.
    #[must_use]
    pub fn status(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }

    /// Overrides the response status.
    #[must_use]
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    /// Adds or replaces one response header.
    #[must_use]
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    fn into_response(self) -> Response<Body> {
        let mut response = Response::new(Body::from(self.body));
        *response.status_mut() = self.status;
        *response.headers_mut() = self.headers;
        response
    }
}

/// An expected route and its ordered queue of replies.
#[derive(Debug)]
pub struct MockRoute {
    method: Method,
    path: String,
    required_query: BTreeMap<String, String>,
    replies: VecDeque<MockResponse>,
}

impl MockRoute {
    /// Builds a route for an arbitrary HTTP method and exact path.
    #[must_use]
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        let path = path.into();
        assert!(path.starts_with('/'), "mock route path must start with '/'");
        Self {
            method,
            path,
            required_query: BTreeMap::new(),
            replies: VecDeque::new(),
        }
    }

    /// Builds a `GET` route.
    #[must_use]
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path)
    }

    /// Requires a decoded query key/value pair while allowing extra pairs.
    #[must_use]
    pub fn with_query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.required_query.insert(key.into(), value.into());
        self
    }

    /// Appends one reply to this route's queue.
    #[must_use]
    pub fn reply(mut self, response: MockResponse) -> Self {
        self.replies.push_back(response);
        self
    }

    /// Appends multiple replies to this route's queue.
    #[must_use]
    pub fn replies(mut self, responses: impl IntoIterator<Item = MockResponse>) -> Self {
        self.replies.extend(responses);
        self
    }

    fn matches(&self, request: &RequestRecord) -> bool {
        if self.method != request.method || self.path != request.uri.path() {
            return false;
        }

        let request_query = query_pairs(&request.uri);
        self.required_query
            .iter()
            .all(|(expected_key, expected_value)| {
                request_query
                    .iter()
                    .any(|(key, value)| key == expected_key && value == expected_value)
            })
    }

    fn label(&self) -> String {
        if self.required_query.is_empty() {
            return format!("{} {}", self.method, self.path);
        }

        let query = self
            .required_query
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        format!("{} {}?{query}", self.method, self.path)
    }
}

#[derive(Debug)]
struct RouteState {
    route: MockRoute,
    expected_calls: usize,
    observed_calls: usize,
}

#[derive(Debug)]
struct FixtureState {
    routes: Mutex<Vec<RouteState>>,
    requests: Mutex<Vec<RequestRecord>>,
    failures: Mutex<Vec<String>>,
    requests_truncated: AtomicBool,
    failures_truncated: AtomicBool,
}

impl FixtureState {
    fn dispatch(&self, request: RequestRecord) -> Response<Body> {
        self.record_request(request.clone());

        let matching_indices = {
            let routes = lock(&self.routes);
            routes
                .iter()
                .enumerate()
                .filter_map(|(index, route)| route.route.matches(&request).then_some(index))
                .collect::<Vec<_>>()
        };

        let request_label = format!("{} {}", request.method, request.uri);
        match matching_indices.as_slice() {
            [] => self.failed_response(format!("unexpected request: {request_label}")),
            [index] => {
                let mut routes = lock(&self.routes);
                let route = &mut routes[*index];
                if let Some(response) = route.route.replies.pop_front() {
                    route.observed_calls += 1;
                    response.into_response()
                } else {
                    let label = route.route.label();
                    drop(routes);
                    self.failed_response(format!(
                        "route received more calls than expected: {label} ({request_label})"
                    ))
                }
            }
            _ => {
                let routes = lock(&self.routes);
                let labels = matching_indices
                    .iter()
                    .map(|index| routes[*index].route.label())
                    .collect::<Vec<_>>()
                    .join(", ");
                drop(routes);
                self.failed_response(format!(
                    "ambiguous request matched multiple routes: {request_label}; matches: {labels}"
                ))
            }
        }
    }

    fn record_body_failure(&self, request: RequestRecord, error: &str) -> Response<Body> {
        let label = format!("{} {}", request.method, request.uri);
        self.record_request(request);
        self.failed_response_with_status(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "request body exceeded the {MAX_REQUEST_BODY_BYTES}-byte fixture limit for {label}: {error}"
            ),
        )
    }

    fn failed_response(&self, message: String) -> Response<Body> {
        self.failed_response_with_status(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    fn failed_response_with_status(&self, status: StatusCode, message: String) -> Response<Body> {
        let mut failures = lock(&self.failures);
        if failures.len() < MAX_RECORDED_FAILURES {
            failures.push(message.clone());
        } else {
            self.failures_truncated.store(true, Ordering::Relaxed);
        }
        drop(failures);
        MockResponse::text(message)
            .with_status(status)
            .into_response()
    }

    fn record_request(&self, request: RequestRecord) {
        let mut requests = lock(&self.requests);
        if requests.len() < MAX_RECORDED_REQUESTS {
            requests.push(request);
        } else {
            self.requests_truncated.store(true, Ordering::Relaxed);
        }
    }

    fn verification_failures(&self) -> Vec<String> {
        let mut failures = lock(&self.failures).clone();
        if self.requests_truncated.load(Ordering::Relaxed) {
            failures.push(format!(
                "fixture received more than the {MAX_RECORDED_REQUESTS}-request recording limit"
            ));
        }
        if self.failures_truncated.load(Ordering::Relaxed) {
            failures.push(format!(
                "fixture recorded more than the {MAX_RECORDED_FAILURES}-failure reporting limit"
            ));
        }
        failures.extend(
            lock(&self.routes)
                .iter()
                .filter(|route| route.observed_calls != route.expected_calls)
                .map(|route| {
                    format!(
                        "unmet route {}: expected {} call(s), observed {}",
                        route.route.label(),
                        route.expected_calls,
                        route.observed_calls
                    )
                }),
        );
        failures
    }
}

/// A local HTTP fixture that must be explicitly finished by each test.
#[derive(Debug)]
pub struct MockHttpService {
    address: SocketAddr,
    state: Arc<FixtureState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_task: Option<JoinHandle<std::io::Result<()>>>,
}

impl MockHttpService {
    /// Starts the fixture on an ephemeral IPv4 loopback port.
    pub async fn start(routes: Vec<MockRoute>) -> Self {
        for route in &routes {
            assert!(
                !route.replies.is_empty(),
                "mock route {} must have at least one reply",
                route.label()
            );
        }

        let routes = routes
            .into_iter()
            .map(|route| {
                let expected_calls = route.replies.len();
                RouteState {
                    route,
                    expected_calls,
                    observed_calls: 0,
                }
            })
            .collect();
        let state = Arc::new(FixtureState {
            routes: Mutex::new(routes),
            requests: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
            requests_truncated: AtomicBool::new(false),
            failures_truncated: AtomicBool::new(false),
        });

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock HTTP service must bind to IPv4 loopback");
        let address = listener
            .local_addr()
            .expect("mock HTTP listener must have a local address");
        let app = Router::new()
            .fallback(handle_request)
            .with_state(Arc::clone(&state));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        Self {
            address,
            state,
            shutdown_tx: Some(shutdown_tx),
            server_task: Some(server_task),
        }
    }

    /// Returns the fixture origin without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Returns a snapshot of all requests received so far.
    #[must_use]
    pub fn requests(&self) -> Vec<RequestRecord> {
        lock(&self.state.requests).clone()
    }

    /// Gracefully stops the server and verifies every route and request.
    pub async fn finish(mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }

        let mut lifecycle_failures = Vec::new();
        if let Some(mut task) = self.server_task.take() {
            match timeout(SHUTDOWN_TIMEOUT, &mut task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => {
                    lifecycle_failures.push(format!("mock HTTP server failed: {error}"));
                }
                Ok(Err(error)) => {
                    lifecycle_failures.push(format!("mock HTTP server task failed: {error}"));
                }
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    lifecycle_failures.push(format!(
                        "mock HTTP server did not stop within {} seconds",
                        SHUTDOWN_TIMEOUT.as_secs()
                    ));
                }
            }
        }

        lifecycle_failures.extend(self.state.verification_failures());
        assert!(
            lifecycle_failures.is_empty(),
            "mock HTTP service verification failed:\n- {}",
            lifecycle_failures.join("\n- ")
        );
    }
}

impl Drop for MockHttpService {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

async fn handle_request(
    State(state): State<Arc<FixtureState>>,
    request: Request<Body>,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => state.dispatch(RequestRecord {
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            body: body.to_vec(),
        }),
        Err(error) => state.record_body_failure(
            RequestRecord {
                method: parts.method,
                uri: parts.uri,
                headers: parts.headers,
                body: Vec::new(),
            },
            &error.to_string(),
        ),
    }
}

fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
    uri.query().map_or_else(Vec::new, |query| {
        url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect()
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
