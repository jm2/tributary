//! Packaged-Windows GStreamer playback and fail-closed runtime probe.
//!
//! This module is compiled only for Windows. The packaging workflow invokes
//! the hidden probe after assembling the distribution so discovery and
//! playback are proven against the copied plugins, not the build host.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;

use super::{is_protected_loopback_ticket_uri, Player, PROTECTED_LOOPBACK_TIMEOUT_SECONDS};

const AUDIO_BYTES: &[u8] = include_bytes!("../../tests/fixtures/audio/silence.flac");
const TICKET_ROUTE: &str = "/cast/550e8400-e29b-41d4-a716-446655440000.flac";
const BUS_DEADLINE: Duration = Duration::from_secs(20);
const SERVER_DEADLINE: Duration = Duration::from_secs(30);
const CONNECTION_DEADLINE: Duration = Duration::from_secs(2);
const SYNTHETIC_ERROR_DEADLINE: Duration = Duration::from_secs(2);
const TEARDOWN_DEADLINE: Duration = Duration::from_secs(3);
const MAX_REQUEST_HEADERS: usize = 32 * 1024;
const FIXED_ROUTING_ERROR: &str = "Protected loopback routing unavailable";

/// Exercise the packaged Windows audio runtime without opening a real output.
///
/// Diagnostics intentionally contain no URIs, proxy addresses, native error
/// text, or plugin paths. Any of those values can disclose protected request
/// material when this probe is extended to backend-shaped streams.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn run_packaged_windows_runtime_probe(plugin_dir: &Path) -> anyhow::Result<()> {
    if AUDIO_BYTES.is_empty() {
        bail!("packaged audio probe fixture is empty");
    }

    let canonical_plugin_dir = plugin_dir
        .canonicalize()
        .map_err(|_| anyhow!("packaged audio probe plugin directory is unavailable"))?;
    if !canonical_plugin_dir.is_dir() {
        bail!("packaged audio probe plugin directory is unavailable");
    }

    let mut media_server = ProbeServer::start(ServerKind::Media)?;
    let mut poison_server = ProbeServer::start(ServerKind::Poison)?;
    let _proxy_environment = ProxyEnvironment::install(poison_server.address);

    gst::init().map_err(|_| anyhow!("packaged audio probe could not initialize GStreamer"))?;

    let playbin_factory = bundled_factory("playbin3", &canonical_plugin_dir)?;
    let _soup_factory = bundled_factory("souphttpsrc", &canonical_plugin_dir)?;
    let fakesink_factory = bundled_factory("fakesink", &canonical_plugin_dir)?;
    let filesrc_factory = bundled_factory("filesrc", &canonical_plugin_dir)?;
    media_server.arm()?;
    poison_server.arm()?;

    let ticket_uri = format!("http://{}{}", media_server.address, TICKET_ROUTE);
    if !is_protected_loopback_ticket_uri(&ticket_uri) {
        bail!("packaged audio probe did not construct an opaque ticket");
    }

    let playbin = playbin_factory
        .create()
        .build()
        .map_err(|_| anyhow!("packaged audio probe could not create playbin3"))?;
    let _null_on_drop = NullOnDrop(playbin.clone());
    let sink = fakesink_factory
        .create()
        .property("sync", false)
        .property("signal-handoffs", true)
        .build()
        .map_err(|_| anyhow!("packaged audio probe could not create its sink"))?;

    let decoded_buffers = Arc::new(AtomicUsize::new(0));
    let decoded_buffers_callback = Arc::clone(&decoded_buffers);
    sink.connect("handoff", false, move |_| {
        decoded_buffers_callback.fetch_add(1, Ordering::SeqCst);
        None
    });
    playbin.set_property("audio-sink", &sink);

    let poison_proxy = format!("http://{}", poison_server.address);
    let poison_ticket = ticket_uri.clone();
    playbin.connect("source-setup", false, move |args| {
        let source = args
            .get(1)
            .and_then(|value| value.get::<gst::Element>().ok())?;
        let expected =
            string_property(&source, "location").as_deref() == Some(poison_ticket.as_str());
        if expected && writable_string_property(&source, "proxy") {
            // This handler runs before the production handler. A passing probe
            // therefore proves the latter replaced a real poisoned resolver.
            source.set_property("proxy", &poison_proxy);
        }
        None
    });

    Player::install_loopback_http_source_policy(&playbin);

    let source_observation = Arc::new(Mutex::new(SourceObservation::default()));
    let source_observation_callback = Arc::clone(&source_observation);
    let observed_ticket = ticket_uri.clone();
    playbin.connect("source-setup", true, move |args| {
        let source = args
            .get(1)
            .and_then(|value| value.get::<gst::Element>().ok())?;
        if string_property(&source, "location").as_deref() != Some(observed_ticket.as_str()) {
            return None;
        }

        let (is_soup, plugin_filename) = source.factory().map_or((false, None), |factory| {
            (
                factory.name() == "souphttpsrc",
                factory.plugin().and_then(|plugin| plugin.filename()),
            )
        });
        let complete = is_soup
            && string_property(&source, "proxy").is_some_and(|proxy| proxy.starts_with("direct:"))
            && i32_property(&source, "retries") == Some(0)
            && u32_property(&source, "timeout") == Some(PROTECTED_LOOPBACK_TIMEOUT_SECONDS);
        let mut observed = source_observation_callback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        observed.protected_sources = observed.protected_sources.saturating_add(1);
        observed.policy_complete &= complete;
        observed.plugin_filenames.push(plugin_filename);
        None
    });

    let element_observation = Arc::new(Mutex::new(ElementObservation::default()));
    let element_observation_callback = Arc::clone(&element_observation);
    // playbin3 documents element-setup as the convenient equivalent of
    // deep-element-added, so it observes dynamically autoplugged decoders.
    playbin.connect("element-setup", true, move |args| {
        let element = args
            .get(1)
            .and_then(|value| value.get::<gst::Element>().ok())?;
        let factory = element.factory()?;
        let decoder = factory
            .metadata(gst::ELEMENT_METADATA_KLASS)
            .is_some_and(|klass| klass.split('/').any(|part| part == "Decoder"));
        if !decoder {
            return None;
        }

        let plugin_filename = factory.plugin().and_then(|plugin| plugin.filename());
        let mut observed = element_observation_callback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        observed.decoder_plugins.push(plugin_filename);
        None
    });

    playbin.set_property("uri", &ticket_uri);
    let bus = playbin
        .bus()
        .ok_or_else(|| anyhow!("packaged audio probe playbin3 has no bus"))?;

    let playback_result = run_playback_to_eos(&playbin, &bus);
    // Publish media-read cancellation before NULL closes an accepted source,
    // but keep both listeners accepting until the transition is complete so
    // the poisoned-proxy observation covers teardown as well as playback.
    media_server.begin_teardown();
    poison_server.begin_teardown();
    let teardown_result = teardown_to_null(&playbin);

    let media_shutdown = media_server.finish_teardown();
    let poison_shutdown = poison_server.finish_teardown();

    // A successful set_state(NULL) alone is not enough: wait for the bounded
    // transition and prove the pipeline is actually NULL before using the
    // poison count as the final no-egress observation.
    teardown_result?;
    media_shutdown?;
    poison_shutdown?;
    if poison_server.connections.load(Ordering::SeqCst) != 0 {
        bail!("packaged audio probe used the poisoned proxy");
    }
    playback_result?;

    if media_server.valid_gets.load(Ordering::SeqCst) == 0 {
        bail!("packaged audio probe did not fetch its media fixture");
    }
    if decoded_buffers.load(Ordering::SeqCst) == 0 {
        bail!("packaged audio probe produced no decoded audio buffer");
    }
    let source_observation = source_observation
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if source_observation.protected_sources == 0 || !source_observation.policy_complete {
        bail!("packaged audio probe did not retain the protected source policy");
    }
    if source_observation
        .plugin_filenames
        .iter()
        .any(|filename| !plugin_filename_is_bundled(filename.as_deref(), &canonical_plugin_dir))
    {
        bail!("packaged audio probe HTTP source did not come from the package");
    }
    let element_observation = element_observation
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if element_observation.decoder_plugins.is_empty()
        || element_observation
            .decoder_plugins
            .iter()
            .any(|filename| !plugin_filename_is_bundled(filename.as_deref(), &canonical_plugin_dir))
    {
        bail!("packaged audio probe did not use a bundled audio decoder");
    }

    verify_non_soup_source_fails_closed(&playbin, &filesrc_factory, &ticket_uri)?;
    Ok(())
}

#[derive(Clone)]
struct SourceObservation {
    protected_sources: usize,
    policy_complete: bool,
    plugin_filenames: Vec<Option<std::path::PathBuf>>,
}

impl Default for SourceObservation {
    fn default() -> Self {
        Self {
            protected_sources: 0,
            policy_complete: true,
            plugin_filenames: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
struct ElementObservation {
    decoder_plugins: Vec<Option<std::path::PathBuf>>,
}

fn run_playback_to_eos(playbin: &gst::Element, bus: &gst::Bus) -> anyhow::Result<()> {
    playbin
        .set_state(gst::State::Playing)
        .map_err(|_| anyhow!("packaged audio probe could not start playback"))?;
    let deadline = Instant::now() + BUS_DEADLINE;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            bail!("packaged audio probe playback timed out");
        };
        let timeout = duration_to_clock_time(remaining);
        let Some(message) =
            bus.timed_pop_filtered(timeout, &[gst::MessageType::Eos, gst::MessageType::Error])
        else {
            bail!("packaged audio probe playback timed out");
        };
        match message.view() {
            gst::MessageView::Eos(_) => return Ok(()),
            // Never format native GStreamer errors: their message/debug fields
            // can contain the complete source URI.
            gst::MessageView::Error(_) => {
                bail!("packaged audio probe playback failed");
            }
            _ => {}
        }
    }
}

fn teardown_to_null(element: &gst::Element) -> anyhow::Result<()> {
    element
        .set_state(gst::State::Null)
        .map_err(|_| anyhow!("packaged audio probe teardown failed"))?;
    let (transition, current, _) = element.state(duration_to_clock_time(TEARDOWN_DEADLINE));
    if transition.is_err() || current != gst::State::Null {
        bail!("packaged audio probe teardown did not reach NULL");
    }
    Ok(())
}

struct NullOnDrop(gst::Element);

impl Drop for NullOnDrop {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn bundled_factory(
    name: &'static str,
    canonical_plugin_dir: &Path,
) -> anyhow::Result<gst::ElementFactory> {
    let factory = gst::ElementFactory::find(name)
        .ok_or_else(|| anyhow!("required packaged GStreamer factory was not discovered: {name}"))?;
    verify_plugin_path(&factory, canonical_plugin_dir, name)?;
    Ok(factory)
}

fn verify_plugin_path(
    factory: &gst::ElementFactory,
    canonical_plugin_dir: &Path,
    name: &'static str,
) -> anyhow::Result<()> {
    if !plugin_is_bundled(factory, canonical_plugin_dir) {
        bail!("required GStreamer factory did not come from the package: {name}");
    }
    Ok(())
}

fn plugin_is_bundled(factory: &gst::ElementFactory, canonical_plugin_dir: &Path) -> bool {
    let filename = factory.plugin().and_then(|plugin| plugin.filename());
    plugin_filename_is_bundled(filename.as_deref(), canonical_plugin_dir)
}

fn plugin_filename_is_bundled(filename: Option<&Path>, canonical_plugin_dir: &Path) -> bool {
    filename
        .and_then(|filename| filename.canonicalize().ok())
        .is_some_and(|filename| filename.is_file() && filename.starts_with(canonical_plugin_dir))
}

fn verify_non_soup_source_fails_closed(
    playbin: &gst::Element,
    filesrc_factory: &gst::ElementFactory,
    ticket_uri: &str,
) -> anyhow::Result<()> {
    let pipeline = gst::Pipeline::new();
    let source = filesrc_factory
        .create()
        .build()
        .map_err(|_| anyhow!("packaged audio probe could not create synthetic source"))?;
    source.set_property("location", ticket_uri);
    pipeline
        .add(&source)
        .map_err(|_| anyhow!("packaged audio probe could not parent synthetic source"))?;
    let bus = pipeline
        .bus()
        .ok_or_else(|| anyhow!("packaged audio probe synthetic pipeline has no bus"))?;

    playbin.emit_by_name::<()>("source-setup", &[&source]);

    let Some(message) = bus.timed_pop_filtered(
        duration_to_clock_time(SYNTHETIC_ERROR_DEADLINE),
        &[gst::MessageType::Error],
    ) else {
        bail!("packaged audio probe synthetic source did not post an error");
    };
    let fixed_routing_error = match message.view() {
        gst::MessageView::Error(error) => {
            message
                .src()
                .is_some_and(|origin| origin == source.upcast_ref::<gst::Object>())
                && error.error().matches(gst::ResourceError::Settings)
                && error.error().message() == FIXED_ROUTING_ERROR
        }
        _ => false,
    };
    if !fixed_routing_error {
        bail!("packaged audio probe synthetic source posted the wrong error");
    }

    let (_, current, _) = source.state(gst::ClockTime::ZERO);
    if !source.is_locked_state() || current != gst::State::Null {
        bail!("packaged audio probe synthetic source did not fail closed");
    }
    Ok(())
}

fn duration_to_clock_time(duration: Duration) -> gst::ClockTime {
    let nanos = duration.as_nanos().min(u128::from(u64::MAX));
    gst::ClockTime::from_nseconds(nanos as u64)
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

fn writable_string_property(element: &gst::Element, name: &str) -> bool {
    element.find_property(name).is_some_and(|property| {
        property.flags().contains(glib::ParamFlags::WRITABLE)
            && property.value_type() == String::static_type()
    })
}

#[derive(Clone, Copy)]
enum ServerKind {
    Media,
    Poison,
}

struct ProbeServer {
    address: SocketAddr,
    lifecycle: Arc<ProbeServerLifecycle>,
    connections: Arc<AtomicUsize>,
    valid_gets: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
    armed_at: Arc<OnceLock<Instant>>,
    thread: Option<JoinHandle<()>>,
}

struct ProbeServerLifecycle {
    cancellation_requested: AtomicBool,
    stop_requested: AtomicBool,
}

impl ProbeServer {
    fn start(kind: ServerKind) -> anyhow::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| anyhow!("packaged audio probe could not bind a loopback server"))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| anyhow!("packaged audio probe could not configure a loopback server"))?;
        let address = listener
            .local_addr()
            .map_err(|_| anyhow!("packaged audio probe could not inspect a loopback server"))?;
        let lifecycle = Arc::new(ProbeServerLifecycle {
            cancellation_requested: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
        });
        let connections = Arc::new(AtomicUsize::new(0));
        let valid_gets = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let armed_at = Arc::new(OnceLock::new());
        let thread = {
            let lifecycle = Arc::clone(&lifecycle);
            let connections = Arc::clone(&connections);
            let valid_gets = Arc::clone(&valid_gets);
            let failed = Arc::clone(&failed);
            let armed_at = Arc::clone(&armed_at);
            thread::Builder::new()
                .name("tributary-audio-probe".to_owned())
                .spawn(move || {
                    serve_probe_listener(
                        listener,
                        kind,
                        &lifecycle,
                        &connections,
                        &valid_gets,
                        &failed,
                        &armed_at,
                    );
                })
                .map_err(|_| anyhow!("packaged audio probe could not start a loopback server"))?
        };
        Ok(Self {
            address,
            lifecycle,
            connections,
            valid_gets,
            failed,
            armed_at,
            thread: Some(thread),
        })
    }

    fn arm(&self) -> anyhow::Result<()> {
        self.armed_at
            .set(Instant::now())
            .map_err(|_| anyhow!("packaged audio probe loopback server was armed twice"))
    }

    fn begin_teardown(&self) {
        self.lifecycle
            .cancellation_requested
            .store(true, Ordering::SeqCst);
    }

    fn finish_teardown(&mut self) -> anyhow::Result<()> {
        if !self.lifecycle.cancellation_requested.load(Ordering::SeqCst) {
            bail!("packaged audio probe loopback server teardown was not requested");
        }
        self.lifecycle.stop_requested.store(true, Ordering::SeqCst);
        if self
            .thread
            .take()
            .is_some_and(|thread| thread.join().is_err())
        {
            bail!("packaged audio probe loopback server failed");
        }
        if self.failed.load(Ordering::SeqCst) {
            bail!("packaged audio probe loopback server failed");
        }
        Ok(())
    }
}

impl Drop for ProbeServer {
    fn drop(&mut self) {
        self.begin_teardown();
        self.lifecycle.stop_requested.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_probe_listener(
    listener: TcpListener,
    kind: ServerKind,
    lifecycle: &ProbeServerLifecycle,
    connections: &AtomicUsize,
    valid_gets: &AtomicUsize,
    failed: &AtomicBool,
    armed_at: &OnceLock<Instant>,
) {
    loop {
        if armed_at
            .get()
            .is_some_and(|started| started.elapsed() >= SERVER_DEADLINE)
        {
            failed.store(true, Ordering::SeqCst);
            return;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Count every socket returned by accept, including one that
                // was already queued when final stop became visible.
                connections.fetch_add(1, Ordering::SeqCst);
                let _ = stream.set_read_timeout(Some(CONNECTION_DEADLINE));
                let _ = stream.set_write_timeout(Some(CONNECTION_DEADLINE));
                if matches!(kind, ServerKind::Media) {
                    match serve_media(&mut stream) {
                        MediaRequestOutcome::ValidGet => {
                            valid_gets.fetch_add(1, Ordering::SeqCst);
                        }
                        MediaRequestOutcome::ValidHead => {}
                        MediaRequestOutcome::IncompleteHeaders => {
                            // NULL teardown can close a connection that was
                            // accepted before the cancellation phase became
                            // visible. Only its incomplete-header
                            // EOF/reset/abort is expected; semantic request
                            // and response failures remain fatal below.
                            if !lifecycle.cancellation_requested.load(Ordering::SeqCst) {
                                failed.store(true, Ordering::SeqCst);
                                return;
                            }
                        }
                        MediaRequestOutcome::Invalid => {
                            failed.store(true, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // Final stop is complete only after accept proves the
                // platform queue is empty. This keeps the poison observer
                // live through NULL teardown and counts racing connections.
                if lifecycle.stop_requested.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                failed.store(true, Ordering::SeqCst);
                return;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaRequestOutcome {
    ValidGet,
    ValidHead,
    IncompleteHeaders,
    Invalid,
}

fn serve_media(stream: &mut TcpStream) -> MediaRequestOutcome {
    let request = match read_request_headers(stream) {
        Ok(request) => request,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
            ) =>
        {
            return MediaRequestOutcome::IncompleteHeaders;
        }
        Err(_) => return MediaRequestOutcome::Invalid,
    };
    let Ok(request) = std::str::from_utf8(&request) else {
        return MediaRequestOutcome::Invalid;
    };
    let Some(header_end) = request.find("\r\n\r\n") else {
        return MediaRequestOutcome::Invalid;
    };
    if header_end + 4 != request.len() {
        return MediaRequestOutcome::Invalid;
    }

    let mut lines = request[..header_end].split("\r\n");
    let Some(first_line) = lines.next() else {
        return MediaRequestOutcome::Invalid;
    };
    let mut request_line = first_line.split(' ');
    let Some(method) = request_line.next() else {
        return MediaRequestOutcome::Invalid;
    };
    let Some(target) = request_line.next() else {
        return MediaRequestOutcome::Invalid;
    };
    let Some(version) = request_line.next() else {
        return MediaRequestOutcome::Invalid;
    };
    if method.is_empty()
        || target.is_empty()
        || request_line.next().is_some()
        || (version != "HTTP/1.0" && version != "HTTP/1.1")
    {
        write_fixed_response(stream, "400 Bad Request");
        return MediaRequestOutcome::Invalid;
    }
    if target != TICKET_ROUTE {
        write_fixed_response(stream, "404 Not Found");
        return MediaRequestOutcome::Invalid;
    }
    if method != "GET" && method != "HEAD" {
        write_fixed_response(stream, "405 Method Not Allowed");
        return MediaRequestOutcome::Invalid;
    }

    let mut range = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            write_fixed_response(stream, "400 Bad Request");
            return MediaRequestOutcome::Invalid;
        };
        if !valid_http_token(name) {
            write_fixed_response(stream, "400 Bad Request");
            return MediaRequestOutcome::Invalid;
        }
        if name.eq_ignore_ascii_case("range") {
            if range.is_some() {
                write_range_not_satisfiable(stream, AUDIO_BYTES.len());
                return MediaRequestOutcome::Invalid;
            }
            range = Some(value.trim());
        }
    }

    let byte_range = match range {
        Some(raw) => match parse_single_range(raw, AUDIO_BYTES.len()) {
            Some(range) => Some(range),
            None => {
                write_range_not_satisfiable(stream, AUDIO_BYTES.len());
                return MediaRequestOutcome::Invalid;
            }
        },
        None => None,
    };
    if write_audio_response(stream, method, byte_range).is_err() {
        return MediaRequestOutcome::Invalid;
    }
    if method == "GET" {
        MediaRequestOutcome::ValidGet
    } else {
        MediaRequestOutcome::ValidHead
    }
}

fn valid_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn read_request_headers(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut request = Vec::with_capacity(4096);
    let mut buffer = [0_u8; 2048];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete request",
            ));
        }
        request.extend_from_slice(&buffer[..read]);
        if request.len() > MAX_REQUEST_HEADERS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
    }
}

fn write_audio_response(
    stream: &mut TcpStream,
    method: &str,
    range: Option<(usize, usize)>,
) -> io::Result<()> {
    let (status, start, end) = match range {
        Some((start, end)) => ("206 Partial Content", start, end),
        None => ("200 OK", 0, AUDIO_BYTES.len() - 1),
    };
    let content_length = end - start + 1;
    let mut headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: audio/flac\r\nAccept-Ranges: bytes\r\nContent-Length: {content_length}\r\nConnection: close\r\n"
    );
    if range.is_some() {
        let _ = write!(
            headers,
            "Content-Range: bytes {start}-{end}/{}\r\n",
            AUDIO_BYTES.len()
        );
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes())?;
    if method == "GET" {
        stream.write_all(&AUDIO_BYTES[start..=end])?;
    }
    stream.flush()
}

fn write_fixed_response(stream: &mut TcpStream, status: &str) {
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn write_range_not_satisfiable(stream: &mut TcpStream, full_len: usize) {
    let response = format!(
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{full_len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
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
        return Some((full_len.saturating_sub(suffix_len), full_len - 1));
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

struct ProxyEnvironment {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl ProxyEnvironment {
    fn install(address: SocketAddr) -> Self {
        let proxy = format!("http://{address}");
        let mut previous = Vec::new();
        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            previous.push((key, std::env::var_os(key)));
            std::env::set_var(key, &proxy);
        }
        for key in ["NO_PROXY", "no_proxy"] {
            previous.push((key, std::env::var_os(key)));
            std::env::remove_var(key);
        }
        Self { previous }
    }
}

impl Drop for ProxyEnvironment {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Shutdown;

    use super::*;

    #[test]
    fn runtime_probe_entrypoint_is_reachable() {
        std::hint::black_box(
            super::super::run_packaged_windows_runtime_probe as fn(&Path) -> anyhow::Result<()>,
        );
    }

    fn classify(request: &[u8]) -> (MediaRequestOutcome, Vec<u8>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind request fixture");
        let address = listener.local_addr().expect("request fixture address");
        let mut client = TcpStream::connect(address).expect("connect request fixture");
        client
            .set_read_timeout(Some(CONNECTION_DEADLINE))
            .expect("set request fixture read deadline");
        client.write_all(request).expect("write request fixture");
        client
            .shutdown(Shutdown::Write)
            .expect("finish request fixture");
        let (mut server, _) = listener.accept().expect("accept request fixture");
        server
            .set_read_timeout(Some(CONNECTION_DEADLINE))
            .expect("set request fixture server deadline");
        let outcome = serve_media(&mut server);
        drop(server);

        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .expect("read request fixture response");
        (outcome, response)
    }

    fn wait_for_probe_condition(mut condition: impl FnMut() -> bool, label: &str) {
        let deadline = Instant::now() + CONNECTION_DEADLINE;
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {label}");
            thread::yield_now();
        }
    }

    #[test]
    fn two_phase_teardown_cancels_an_accepted_connection_blocked_on_headers() {
        let mut server = ProbeServer::start(ServerKind::Media).expect("start media probe server");
        server.arm().expect("arm media probe server");
        let client = TcpStream::connect(server.address).expect("connect without sending headers");
        wait_for_probe_condition(
            || server.connections.load(Ordering::SeqCst) == 1,
            "the media server to accept the blocked client",
        );

        // Match production: publish cancellation before NULL teardown closes
        // the source, then publish final stop, drain, join, and inspect.
        server.begin_teardown();
        client
            .shutdown(Shutdown::Write)
            .expect("half-close the abandoned request");

        server
            .finish_teardown()
            .expect("teardown owns an abandoned in-flight connection");
        assert!(!server.failed.load(Ordering::SeqCst));
    }

    #[test]
    fn poison_observer_stays_live_and_drains_queued_accepts_through_teardown() {
        let mut server = ProbeServer::start(ServerKind::Poison).expect("start poison observer");
        server.arm().expect("arm poison observer");
        server.begin_teardown();

        let _accepted = TcpStream::connect(server.address).expect("connect during NULL phase");
        wait_for_probe_condition(
            || server.connections.load(Ordering::SeqCst) == 1,
            "the poison observer to accept during the NULL phase",
        );

        // Do not wait for this connection to be accepted. finish_teardown
        // must set final stop, drain any queued accept, and count it before
        // the listener observes WouldBlock and exits.
        let _queued =
            TcpStream::connect(server.address).expect("queue connection before final stop");
        server
            .finish_teardown()
            .expect("drain poison observer after NULL phase");

        assert_eq!(server.connections.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn malformed_request_completed_during_teardown_still_fails_the_server() {
        let mut server = ProbeServer::start(ServerKind::Media).expect("start media probe server");
        server.arm().expect("arm media probe server");
        let mut client = TcpStream::connect(server.address).expect("connect malformed request");
        client
            .set_read_timeout(Some(CONNECTION_DEADLINE))
            .expect("bound malformed response read");
        wait_for_probe_condition(
            || server.connections.load(Ordering::SeqCst) == 1,
            "the media server to accept the malformed client",
        );

        server.begin_teardown();
        let malformed = format!("GET {TICKET_ROUTE} HTTP/1.1\r\nBad Header: value\r\n\r\n");
        client
            .write_all(malformed.as_bytes())
            .expect("send malformed request");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .expect("read malformed request response");
        assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
        wait_for_probe_condition(
            || server.failed.load(Ordering::SeqCst),
            "the media server to reject semantic request drift during teardown",
        );

        assert!(server.finish_teardown().is_err());
    }

    #[test]
    fn media_fixture_classifies_valid_get_and_head() {
        let get =
            format!("GET {TICKET_ROUTE} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=-8\r\n\r\n");
        let (outcome, response) = classify(get.as_bytes());
        assert!(matches!(outcome, MediaRequestOutcome::ValidGet));
        assert!(response.starts_with(b"HTTP/1.1 206 Partial Content\r\n"));

        let head = format!("HEAD {TICKET_ROUTE} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let (outcome, response) = classify(head.as_bytes());
        assert!(matches!(outcome, MediaRequestOutcome::ValidHead));
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HEAD response terminator");
        assert_eq!(header_end + 4, response.len());
    }

    #[test]
    fn media_fixture_distinguishes_incomplete_header_io_from_semantic_drift() {
        let incomplete = format!("GET {TICKET_ROUTE} HTTP/1.1\r\nHost: localhost\r\n");
        let (outcome, _) = classify(incomplete.as_bytes());
        assert!(matches!(outcome, MediaRequestOutcome::IncompleteHeaders));

        let mut invalid_utf8 = format!("GET {TICKET_ROUTE} HTTP/1.1\r\nHeader: ").into_bytes();
        invalid_utf8.extend_from_slice(b"\xff\r\n\r\n");
        let (outcome, _) = classify(&invalid_utf8);
        assert!(matches!(outcome, MediaRequestOutcome::Invalid));
    }

    #[test]
    fn media_fixture_rejects_request_drift() {
        let requests = [
            "GET /wrong HTTP/1.1\r\nHost: localhost\r\n\r\n".to_owned(),
            format!("POST {TICKET_ROUTE} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            format!("GET {TICKET_ROUTE} HTTP/1.1\r\nRange: bytes=0-1\r\nRange: bytes=2-3\r\n\r\n"),
            format!("GET {TICKET_ROUTE} HTTP/1.1\r\nBad Header: value\r\n\r\n"),
            "not-http\r\n\r\n".to_owned(),
        ];

        for request in requests {
            let (outcome, _) = classify(request.as_bytes());
            assert!(matches!(outcome, MediaRequestOutcome::Invalid));
        }
    }
}
