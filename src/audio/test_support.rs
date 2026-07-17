//! Process-isolated integration support for protected GStreamer playback.
//!
//! Native GStreamer plugins and GLib's default main context are process-global.
//! Running this proof in a dedicated, bounded test process prevents parallel
//! unit tests from contending for that state and contains a broken plugin that
//! hangs during discovery, preroll, or teardown.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use url::Url;

use super::{
    is_protected_loopback_ticket_uri, Player, PlayerEvent, PlayerEventGeneration, PlayerState,
    PROTECTED_LOOPBACK_TIMEOUT_SECONDS,
};
use crate::architecture::ResolvedHttpRequest;

const CHILD_MARKER: &str = "TRIBUTARY_PROTECTED_GSTREAMER_CHILD";
const CHILD_MARKER_VALUE: &str = "tributary-protected-gstreamer-child-v1";
const CHILD_SENTINEL: &str = "TRIBUTARY_PROTECTED_GSTREAMER_SENTINEL";
const CHILD_DEADLINE: Duration = Duration::from_secs(90);
const CASE_DEADLINE: Duration = Duration::from_secs(20);
const AUDIO_BYTES: &[u8] = include_bytes!("../../tests/fixtures/audio/silence.flac");

/// One backend-shaped protected request and its expected upstream boundary.
///
/// Deliberately not `Debug`: query and containment values can be credentials.
pub struct ProtectedStreamCase {
    request: ResolvedHttpRequest,
    expected: ExpectedRequest,
}

impl ProtectedStreamCase {
    pub fn new(request: ResolvedHttpRequest, expected_path: impl Into<String>) -> Self {
        Self {
            request,
            expected: ExpectedRequest {
                path: expected_path.into(),
                query_pairs: Vec::new(),
                required_headers: Vec::new(),
                forbidden_headers: Vec::new(),
                private_values: Vec::new(),
            },
        }
    }

    pub fn with_query_pair(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.expected.query_pairs.push((name.into(), value.into()));
        self
    }

    pub fn with_required_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.expected.required_headers.push((name, value));
        self
    }

    pub fn with_forbidden_header(mut self, name: HeaderName) -> Self {
        self.expected.forbidden_headers.push(name);
        self
    }

    pub fn with_private_value(mut self, value: impl Into<String>) -> Self {
        self.expected.private_values.push(value.into());
        self
    }
}

/// Run exactly two production protected-player cases in one bounded child.
///
/// The builder executes only in the child, after its loopback fixture has an
/// origin. A success sentinel prevents a misspelled `--exact` filter from
/// turning a zero-test child into a false positive.
pub fn assert_protected_stream_cases_play_to_eos<F>(exact_test_name: &str, build_cases: F)
where
    F: FnOnce(&Url) -> Vec<ProtectedStreamCase>,
{
    if std::env::var(CHILD_MARKER).as_deref() == Ok(CHILD_MARKER_VALUE) {
        run_child(build_cases);
        let sentinel = std::env::var_os(CHILD_SENTINEL).expect("child sentinel path");
        std::fs::write(sentinel, b"protected-gstreamer-ok").expect("write child sentinel");
        return;
    }

    let sentinel = std::env::temp_dir().join(format!(
        "tributary-protected-gstreamer-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut child = Command::new(std::env::current_exe().expect("current test executable"));
    child
        .args([
            "--exact",
            exact_test_name,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER, CHILD_MARKER_VALUE)
        .env(CHILD_SENTINEL, &sentinel)
        .env("NO_PROXY", "127.0.0.1,localhost,::1")
        .env("no_proxy", "127.0.0.1,localhost,::1")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        // Keep plugin discovery inside the deadline-owned child instead of
        // leaving a hung gst-plugin-scanner grandchild behind after kill().
        .env("GST_REGISTRY_FORK", "no")
        .env("RUST_TEST_THREADS", "1");

    let mut child = child
        .spawn()
        .expect("spawn isolated protected GStreamer test");
    let deadline = Instant::now() + CHILD_DEADLINE;
    let status = loop {
        match child.try_wait().expect("poll protected GStreamer child") {
            Some(status) => break status,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&sentinel);
                panic!("isolated protected GStreamer test exceeded its process deadline");
            }
        }
    };

    let sentinel_ok =
        std::fs::read(&sentinel).is_ok_and(|contents| contents == b"protected-gstreamer-ok");
    let _ = std::fs::remove_file(&sentinel);
    assert!(
        status.success() && sentinel_ok,
        "isolated protected GStreamer test failed"
    );
}

struct ExpectedRequest {
    path: String,
    query_pairs: Vec<(String, String)>,
    required_headers: Vec<(HeaderName, HeaderValue)>,
    forbidden_headers: Vec<HeaderName>,
    private_values: Vec<String>,
}

#[derive(Clone, Copy, Default)]
struct RequestObservation {
    get_count: usize,
    head_count: usize,
    invalid_count: usize,
}

#[derive(Clone, Copy, Default)]
struct SourceObservation {
    policy_complete: bool,
}

impl SourceObservation {
    fn policy_is_complete(self) -> bool {
        self.policy_complete
    }
}

#[derive(Clone)]
struct FixtureState {
    expected: Arc<Vec<ExpectedRequest>>,
    observed: Arc<Mutex<Vec<RequestObservation>>>,
    unexpected_request: Arc<Mutex<bool>>,
}

struct FixtureServer {
    address: std::net::SocketAddr,
    abort_handle: tokio::task::AbortHandle,
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

fn run_child<F>(build_cases: F)
where
    F: FnOnce(&Url) -> Vec<ProtectedStreamCase>,
{
    assert!(
        !AUDIO_BYTES.is_empty(),
        "protected playback fixture is empty"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("protected playback runtime");
    let listener = runtime.block_on(async {
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind protected playback fixture")
    });
    let address = listener
        .local_addr()
        .expect("protected playback fixture address");
    let origin = Url::parse(&format!("http://{address}/")).expect("fixture origin");
    let cases = build_cases(&origin);
    assert_eq!(
        cases.len(),
        2,
        "protected playback proof requires DAAP and Subsonic cases"
    );

    let (requests, expected): (Vec<_>, Vec<_>) = cases
        .into_iter()
        .map(|case| (case.request, case.expected))
        .unzip();
    assert_eq!(
        expected
            .iter()
            .map(|item| item.path.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        expected.len(),
        "protected playback fixture paths must be unique"
    );

    let observed = Arc::new(Mutex::new(vec![
        RequestObservation::default();
        expected.len()
    ]));
    let unexpected_request = Arc::new(Mutex::new(false));
    let fixture_state = FixtureState {
        expected: Arc::new(expected),
        observed: Arc::clone(&observed),
        unexpected_request: Arc::clone(&unexpected_request),
    };
    let app = Router::new()
        .fallback(any(serve_audio_fixture))
        .with_state(fixture_state.clone());
    let server_task = runtime.spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve protected playback fixture");
    });
    let fixture = FixtureServer {
        address,
        abort_handle: server_task.abort_handle(),
    };
    assert!(fixture.address.ip().is_loopback());

    let context = glib::MainContext::default();
    let _context_guard = context
        .acquire()
        .expect("acquire isolated GLib default context");
    let (player, events) = Player::new(runtime.handle().clone()).expect("production player");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("signal-handoffs", true)
        .build()
        .expect("GStreamer fakesink");
    let decoded_buffers = Arc::new(AtomicUsize::new(0));
    let decoded_buffers_callback = Arc::clone(&decoded_buffers);
    sink.connect("handoff", false, move |_| {
        decoded_buffers_callback.fetch_add(1, Ordering::SeqCst);
        None
    });
    player.playbin.set_property("audio-sink", &sink);

    let source_observations = Arc::new(Mutex::new(Vec::<SourceObservation>::new()));
    let source_observations_callback = Arc::clone(&source_observations);
    player.playbin.connect("source-setup", true, move |args| {
        let source = args
            .get(1)
            .and_then(|value| value.get::<gst::Element>().ok())?;
        let soup_http_source = source
            .factory()
            .is_some_and(|factory| factory.name() == "souphttpsrc");
        let policy_complete = soup_http_source
            && string_property(&source, "location")
                .as_deref()
                .is_some_and(is_protected_loopback_ticket_uri)
            && string_property(&source, "proxy").is_some_and(|proxy| proxy.starts_with("direct:"))
            && i32_property(&source, "retries") == Some(0)
            && u32_property(&source, "timeout") == Some(PROTECTED_LOOPBACK_TIMEOUT_SECONDS);
        let observation = SourceObservation { policy_complete };
        source_observations_callback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(observation);
        None
    });

    for (index, request) in requests.into_iter().enumerate() {
        let generation = PlayerEventGeneration::from_raw(
            u64::try_from(index + 1).expect("case generation fits u64"),
        );
        player.set_event_generation(generation);
        let source_count_before = source_observations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let decoded_buffers_before = decoded_buffers.load(Ordering::SeqCst);

        player.load_resolved(request);
        let ticket = player.playbin.property::<String>("uri");
        assert!(
            is_protected_loopback_ticket_uri(&ticket),
            "player did not receive an opaque protected loopback ticket"
        );
        assert!(
            fixture_state.expected[index]
                .private_values
                .iter()
                .all(|value| !value.is_empty() && !ticket.contains(value)),
            "private request material escaped into the GStreamer-facing ticket"
        );

        wait_for_generation_eos(&context, &events, generation);
        assert!(
            decoded_buffers.load(Ordering::SeqCst) > decoded_buffers_before,
            "protected player reached end-of-stream without a decoded audio buffer"
        );

        let source_observations = source_observations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert!(
            source_observations[source_count_before..]
                .iter()
                .copied()
                .any(SourceObservation::policy_is_complete),
            "protected GStreamer source did not retain the enforced direct policy"
        );
        drop(source_observations);

        let request_observation =
            observed.lock().unwrap_or_else(|poison| poison.into_inner())[index];
        assert!(
            request_observation.invalid_count == 0 && request_observation.get_count > 0,
            "protected upstream request did not match its backend contract"
        );
    }

    assert!(
        !*unexpected_request
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
        "protected playback fixture received an unexpected request"
    );

    player.stop();
    for _ in 0..64 {
        if !context.iteration(false) {
            break;
        }
    }
    drop(player);
    drop(fixture);
    runtime.shutdown_timeout(Duration::from_secs(5));
}

fn wait_for_generation_eos(
    context: &glib::MainContext,
    events: &async_channel::Receiver<PlayerEvent>,
    generation: PlayerEventGeneration,
) {
    let deadline = Instant::now() + CASE_DEADLINE;
    let mut buffering = false;
    let mut ended = false;

    while Instant::now() < deadline && !ended {
        for _ in 0..64 {
            if !context.iteration(false) {
                break;
            }
        }
        while let Ok(event) = events.try_recv() {
            if event.generation() != generation {
                continue;
            }
            match event {
                PlayerEvent::StateChanged {
                    state: PlayerState::Buffering,
                    ..
                } => buffering = true,
                PlayerEvent::TrackEnded { .. } => ended = true,
                PlayerEvent::Error { .. } => {
                    panic!("protected player emitted an error before end-of-stream");
                }
                PlayerEvent::StateChanged { .. } | PlayerEvent::PositionChanged { .. } => {}
            }
        }
        if !ended {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    assert!(buffering, "protected player did not emit Buffering");
    assert!(ended, "protected player did not reach end-of-stream");
}

fn string_property(element: &gst::Element, name: &str) -> Option<String> {
    let property = element.find_property(name)?;
    if !property.flags().contains(glib::ParamFlags::READABLE)
        || property.value_type() != String::static_type()
    {
        return None;
    }
    element.property_value(name).get::<String>().ok()
}

fn i32_property(element: &gst::Element, name: &str) -> Option<i32> {
    let property = element.find_property(name)?;
    if !property.flags().contains(glib::ParamFlags::READABLE)
        || property.value_type() != i32::static_type()
    {
        return None;
    }
    element.property_value(name).get::<i32>().ok()
}

fn u32_property(element: &gst::Element, name: &str) -> Option<u32> {
    let property = element.find_property(name)?;
    if !property.flags().contains(glib::ParamFlags::READABLE)
        || property.value_type() != u32::static_type()
    {
        return None;
    }
    element.property_value(name).get::<u32>().ok()
}

async fn serve_audio_fixture(
    State(state): State<FixtureState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let Some(index) = state
        .expected
        .iter()
        .position(|expected| expected.path == uri.path())
    else {
        *state
            .unexpected_request
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = true;
        return StatusCode::NOT_FOUND.into_response_without_sensitive_details();
    };

    let expected = &state.expected[index];
    let valid_method = method == Method::GET || method == Method::HEAD;
    let valid_query = normalized_query(&uri) == normalized_pairs(&expected.query_pairs);
    let valid_required_headers = expected.required_headers.iter().all(|(name, value)| {
        headers.get_all(name).iter().count() == 1 && headers.get(name) == Some(value)
    });
    let valid_forbidden_headers = expected
        .forbidden_headers
        .iter()
        .all(|name| !headers.contains_key(name));
    {
        let mut observed = state
            .observed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let observation = &mut observed[index];
        if !(valid_method && valid_query && valid_required_headers && valid_forbidden_headers) {
            observation.invalid_count += 1;
        }
        if method == Method::GET {
            observation.get_count += 1;
        } else if method == Method::HEAD {
            observation.head_count += 1;
        }
    }

    if !valid_method {
        return StatusCode::METHOD_NOT_ALLOWED.into_response_without_sensitive_details();
    }
    audio_response(&method, &headers)
}

trait FixedStatusResponse {
    fn into_response_without_sensitive_details(self) -> Response;
}

impl FixedStatusResponse for StatusCode {
    fn into_response_without_sensitive_details(self) -> Response {
        Response::builder()
            .status(self)
            .header(header::CONTENT_LENGTH, "0")
            .body(Body::empty())
            .expect("fixed fixture response")
    }
}

fn normalized_query(uri: &Uri) -> BTreeMap<(String, String), usize> {
    let pairs = uri
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    normalized_pairs(&pairs)
}

fn normalized_pairs(pairs: &[(String, String)]) -> BTreeMap<(String, String), usize> {
    let mut normalized = BTreeMap::new();
    for pair in pairs {
        *normalized.entry(pair.clone()).or_insert(0) += 1;
    }
    normalized
}

fn audio_response(method: &Method, headers: &HeaderMap) -> Response {
    let full_len = AUDIO_BYTES.len();
    let requested_range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let range = match requested_range {
        Some(raw) => match parse_single_range(raw, full_len) {
            Some(range) => Some(range),
            None => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{full_len}"))
                    .header(header::CONTENT_LENGTH, "0")
                    .body(Body::empty())
                    .expect("invalid range response");
            }
        },
        None => None,
    };

    let (status, start, end) = match range {
        Some((start, end)) => (StatusCode::PARTIAL_CONTENT, start, end),
        None => (StatusCode::OK, 0, full_len - 1),
    };
    let content_len = end - start + 1;
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "audio/flac")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, content_len.to_string());
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{full_len}"),
        );
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(AUDIO_BYTES[start..=end].to_vec())
    };
    response.body(body).expect("audio fixture response")
}

fn parse_single_range(raw: &str, full_len: usize) -> Option<(usize, usize)> {
    let value = raw.strip_prefix("bytes=")?;
    if value.contains(',') || full_len == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix_len = end.parse::<usize>().ok()?;
        if suffix_len == 0 {
            return None;
        }
        let start = full_len.saturating_sub(suffix_len);
        return Some((start, full_len - 1));
    }

    let start = start.parse::<usize>().ok()?;
    if start >= full_len {
        return None;
    }
    let end = if end.is_empty() {
        full_len - 1
    } else {
        end.parse::<usize>().ok()?.min(full_len - 1)
    };
    (start <= end).then_some((start, end))
}
