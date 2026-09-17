//! AirPlay 1 (RAOP) audio output.
//!
//! Streams audio to legacy AirPlay (RAOP) receivers discovered via
//! `_raop._tcp.local.` mDNS browsing.  Discovered devices surface as
//! `DiscoveryEvent::Found` with `service_type: "airplay"` and appear
//! automatically in the output selector popover.
//!
//! # Implementation strategy
//!
//! The output is a thin, sender-agnostic load path. It performs the
//! fail-closed availability gate, prepares the credential-safe media URI, and
//! hands the track to an [`AirplaySender`](super::airplay_sender::AirplaySender)
//! selected at load time — the GStreamer `raopsink` path today. The sender
//! owns the transport and returns a
//! [`SenderSession`](super::airplay_sender::SenderSession); the output only
//! pushes control and republishes the session's events.
//!
//! The GStreamer sender builds a dedicated pipeline per session and operates
//! it independently of the main `playbin3`:
//! `uridecodebin ! audioconvert ! avenc_alac ! raopsink`.
//!
//! `raopsink` is the only transmitter this sender has.  There is
//! deliberately no fallback: the one this module used to have piped decoded
//! PCM into a spawned `shairport-sync`, which is an AirPlay *receiver* — it
//! ignored the device the user selected and could never reach it (review
//! finding M3, tracker item P2.9).  A missing `raopsink` now fails the load
//! with a localized, honest unsupported message instead of silently spawning
//! a subprocess that cannot work.
//!
//! A bus watch on the dedicated pipeline forwards EOS / errors / state
//! changes into the same `PlayerEvent` channel the rest of the app
//! consumes. A weak, generation-scoped timer publishes position and
//! duration while the pipeline is actually playing, so progress accounting
//! and the header bar reflect the accepted RAOP session rather than
//! optimistic guesses.
//!
//! # Scope
//!
//! - **AirPlay 1 (RAOP) only.**  AirPlay 2 receivers (HomePod, recent
//!   Apple TVs, AirPlay-2-certified speakers) advertise via
//!   `_airplay._tcp.local.` and speak a different protocol stack that
//!   this output does not implement.  Such devices are filtered out
//!   of the output selector by [`crate::ui::discovery_handler`] until
//!   a sender-side AirPlay 2 implementation lands.
//! - **Requires a working external `raopsink` element.**  Current official
//!   GStreamer, Homebrew, and MSYS2 packages do not ship that element. The
//!   code retains the registry-gated integration seam but reports AirPlay 1
//!   unavailable unless a compatible third-party element is already present.
//! - **Seeking is not supported** for live RAOP streams.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::airplay_sender::{
    AirplaySender, OpenCancel, OpenOutcome, RecoveryOutcome, SenderError, SenderOpenContext,
    SenderPosition, SenderSession, SenderTarget, SenderWriteOutcome, SessionGate,
};
use super::gstreamer_media::{
    GstreamerMediaProxy, GstreamerMediaTicket, InFlightCancelRegistration, PreparedGstreamerMedia,
};
use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerEventGeneration, PlayerState};

use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use tracing::{debug, error, info};

use crate::architecture::media::ResolvedHttpRequest;
use crate::local::resolver::ResolvedLocalMedia;

/// One live GStreamer RAOP session.
///
/// The session sources its own decoder from the prepared URI, so audio is
/// never pushed through [`SenderSession::write_pcm`]; it reports every pushed
/// buffer as accepted.
struct GstreamerSenderSession {
    pipeline: gst::Pipeline,
    /// Exact protected-media ticket owned by this session. Credential-free
    /// media has no ticket and retains its existing direct-URI behavior.
    media_ticket: Option<Arc<GstreamerMediaTicket>>,
    /// Bus watch guard — dropping it removes the watch.
    _bus_watch: gst::bus::BusWatchGuard,
    generation: PlayerEventGeneration,
    media_proxy: Arc<GstreamerMediaProxy>,
    /// Shared Stop/start boundary with the load path (review U3): a Stop taken
    /// by [`AirPlayOutput::close_session`] serializes with `resume` so a start
    /// can never be authorized after a Stop.
    gate: Arc<SessionGate>,
}

impl GstreamerSenderSession {
    fn new(
        pipeline: gst::Pipeline,
        ctx: &SenderOpenContext,
        bus_watch: gst::bus::BusWatchGuard,
    ) -> Self {
        Self {
            pipeline,
            media_ticket: ctx.media_ticket.clone(),
            _bus_watch: bus_watch,
            generation: ctx.generation,
            media_proxy: Arc::clone(&ctx.media_proxy),
            gate: Arc::clone(&ctx.session_gate),
        }
    }

    /// Test-only constructor: attach the real bus watch and build the session
    /// around an injected pipeline, retaining the actual `resume`/`close` and
    /// state paths (review Z1). `resume` still runs the **production** effect
    /// (`pipeline.set_state(Playing)`); callers supply a pipeline whose real
    /// transition succeeds, is refused by the shared Stop/start gate, or fails.
    #[cfg(test)]
    fn for_test_session(pipeline: gst::Pipeline, ctx: &SenderOpenContext) -> Result<Self, String> {
        let bus_watch = attach_bus_watch(
            &pipeline,
            &ctx.event_tx,
            ctx.generation,
            &ctx.media_proxy,
            ctx.media_ticket.clone(),
        )?;
        Ok(Self::new(pipeline, ctx, bus_watch))
    }
}

impl SenderSession for GstreamerSenderSession {
    fn write_pcm(&mut self, samples: &[u8]) -> SenderWriteOutcome {
        // This session owns its decode pipeline and never consumes pushed
        // audio. Report the buffer accepted so a pump would not stall.
        SenderWriteOutcome::Accepted(samples.len())
    }

    fn set_volume(&mut self, level: f64) -> bool {
        if let Some(sink) = self.pipeline.by_name("raop") {
            sink.set_property("volume", AirPlayOutput::volume_to_db(level));
        }
        true
    }

    fn pause(&mut self) -> bool {
        let _ = self.pipeline.set_state(gst::State::Paused);
        true
    }

    fn resume(&mut self) -> bool {
        // Authorize and perform the pipeline start under the shared Stop/start
        // boundary: a Stop taken by the load path either wins first (refusing
        // the start) or loses and is followed by the session's own `Null`
        // teardown, which settles the already-started pipeline (review U3). The
        // effect is always the real `set_state(Playing)` — a regression must
        // inject a real pipeline whose transition succeeds or fails, never
        // substitute the effect.
        let pipeline = self.pipeline.clone();
        self.gate
            .start(move || pipeline.set_state(gst::State::Playing).is_ok())
    }

    fn confirm_started(&self, publish: &mut dyn FnMut(PlayerState)) -> bool {
        // The worker's start publication is one step with the shared Stop
        // boundary: a Stop that won after the pipeline transition suppresses
        // it, so no `Playing` for this generation can follow the Stop's own
        // `Stopped`. The session's `Null` teardown settles the transmitted start.
        self.gate.publish_if_live(|| publish(PlayerState::Playing))
    }

    fn flush(&mut self) {
        // A live RAOP stream exposes no seekable timeline to flush.
    }

    fn observe(&self) -> SenderPosition {
        let (_, state, _) = self.pipeline.state(gst::ClockTime::ZERO);
        let position_ms = pipeline_position_ms(&self.pipeline, state);
        let duration_ms = self
            .pipeline
            .query_duration::<gst::ClockTime>()
            .map(|duration| duration.mseconds());
        SenderPosition {
            generation: self.generation,
            position_ms,
            duration_ms,
            stale: false,
        }
    }

    fn state(&self) -> PlayerState {
        let (_, current, _) = self.pipeline.state(Some(gst::ClockTime::ZERO));
        match current {
            gst::State::Playing => PlayerState::Playing,
            gst::State::Paused => PlayerState::Paused,
            _ => PlayerState::Stopped,
        }
    }

    fn close(self: Box<Self>) {
        // Stop the pipeline before invalidating its loopback route. Doing this
        // in the opposite order can turn an intentional close into a transient
        // fetch error while GStreamer is still winding down. The shared
        // boundary is stopped first so no concurrent `resume` can restart the
        // pipeline after this teardown (review U3).
        let this = *self;
        this.gate.stop();
        let _ = this.pipeline.set_state(gst::State::Null);
        if let Some(ticket) = this.media_ticket.as_ref() {
            // Identity-bound terminal release: a superseded session's ticket may
            // have been moved into recovery custody by a replacement, so this
            // must remove that custody entry as well as the active lease and
            // revoke the route (review T4).
            this.media_proxy.take_and_release(ticket);
        }
    }
}

/// The GStreamer `raopsink` transmission path.
///
/// One registry-gated backend. It builds the dedicated RAOP pipeline and
/// forwards the pipeline's bus into the shared `PlayerEvent` channel.
pub(super) struct GstreamerRaopSender;

impl GstreamerRaopSender {
    /// True when GStreamer's registry has a `raopsink` element to transmit
    /// with. Requires an initialised GStreamer.
    fn raopsink_available() -> bool {
        gst::Registry::get()
            .find_feature("raopsink", gst::ElementFactory::static_type())
            .is_some()
    }

    /// Localized unsupported-sender guidance. `raopsink` is a technical
    /// identifier and stays untranslated inside every catalog entry.
    fn raopsink_missing_message(locale: &str) -> String {
        rust_i18n::t!("errors.playback.airplay_raopsink_missing", locale = locale).into_owned()
    }

    /// The `gst::parse::launch` description for the RAOP pipeline.
    ///
    /// `avenc_alac` is the encoder: it emits the 352-sample ALAC framing RAOP
    /// receivers expect (design §2.4), so the description is asserted in a
    /// unit test rather than only at runtime.
    fn pipeline_description(host: &str, port: u16, uri: &str) -> String {
        format!(
            "uridecodebin name=decoder uri=\"{}\" ! audioconvert ! avenc_alac ! raopsink name=raop host={} port={}",
            uri.replace('"', "\\\""),
            host,
            port,
        )
    }

    /// Build a pipeline using GStreamer's `raopsink`. The caller has
    /// already verified via [`Self::probe`] that the element is registered.
    fn build_pipeline(
        host: &str,
        port: u16,
        uri: &str,
        volume: f64,
    ) -> Result<gst::Pipeline, String> {
        let pipeline_str = Self::pipeline_description(host, port, uri);

        let element = gst::parse::launch(&pipeline_str)
            .map_err(|_| "Failed to build RAOP pipeline".to_string())?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| "RAOP launch did not yield a Pipeline".to_string())?;

        if let Some(sink) = pipeline.by_name("raop") {
            sink.set_property("volume", AirPlayOutput::volume_to_db(volume));
        }
        let decoder = pipeline
            .by_name("decoder")
            .ok_or_else(|| "RAOP pipeline has no URI decoder".to_string())?;
        super::Player::install_loopback_http_source_policy(&decoder);

        Ok(pipeline)
    }
}

impl AirplaySender for GstreamerRaopSender {
    fn name(&self) -> &'static str {
        "gstreamer-raopsink"
    }

    fn probe(&self) -> Result<(), SenderError> {
        if Self::raopsink_available() {
            Ok(())
        } else {
            Err(SenderError::Dependency(Self::raopsink_missing_message(
                &rust_i18n::locale(),
            )))
        }
    }

    fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome {
        // Cancellation is checked before any transport work; this adapter
        // transmits no mutating remote call, so a cancelled open has nothing
        // to unwind beyond the resources it has not yet created.
        if ctx.cancel.is_cancelled() {
            return OpenOutcome::Cancelled;
        }

        let pipeline = match Self::build_pipeline(
            &ctx.target.host,
            ctx.target.port,
            &ctx.prepared_uri,
            ctx.volume,
        ) {
            Ok(pipeline) => pipeline,
            Err(message) => return OpenOutcome::Failed(SenderError::Receiver(message)),
        };

        let bus_watch = match attach_bus_watch(
            &pipeline,
            &ctx.event_tx,
            ctx.generation,
            &ctx.media_proxy,
            ctx.media_ticket.clone(),
        ) {
            Ok(watch) => watch,
            Err(message) => {
                let _ = pipeline.set_state(gst::State::Null);
                return OpenOutcome::Failed(SenderError::Receiver(message));
            }
        };
        start_position_timer(&pipeline, &ctx.event_tx, ctx.generation);

        if pipeline.set_state(gst::State::Paused).is_err() {
            let _ = pipeline.set_state(gst::State::Null);
            return OpenOutcome::Failed(SenderError::Receiver(
                "RAOP pipeline preroll failed".to_string(),
            ));
        }
        info!(
            host = %ctx.target.host,
            port = ctx.target.port,
            "AirPlay: session opened via raopsink"
        );

        OpenOutcome::Opened(Box::new(GstreamerSenderSession::new(
            pipeline, ctx, bus_watch,
        )))
    }
}

/// Pipeline position for the shared progress contract.
///
/// Unknown duration remains zero, matching local, MPD, and Chromecast output
/// semantics; a stopped pipeline publishes no position.
fn pipeline_position_ms(pipeline: &gst::Pipeline, state: gst::State) -> Option<u64> {
    if state != gst::State::Playing {
        return None;
    }
    pipeline
        .query_position::<gst::ClockTime>()
        .map(|position| position.mseconds())
}

/// Construct an attached bus watch that forwards EOS / Error / state
/// changes to the shared `PlayerEvent` channel.
fn attach_bus_watch(
    pipeline: &gst::Pipeline,
    event_tx: &async_channel::Sender<PlayerEvent>,
    generation: PlayerEventGeneration,
    media_proxy: &Arc<GstreamerMediaProxy>,
    media_ticket: Option<Arc<GstreamerMediaTicket>>,
) -> Result<gst::bus::BusWatchGuard, String> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| "Pipeline has no bus".to_string())?;
    let tx = event_tx.clone();
    let pipeline_weak = pipeline.downgrade();
    let media_proxy = Arc::clone(media_proxy);
    let started_at = Instant::now();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                if let Some(ticket) = media_ticket.as_ref() {
                    // Identity-bound release removes a recovery-custody entry a
                    // replacement may have created, so a superseded server is
                    // never leaked (review T4).
                    media_proxy.take_and_release(ticket);
                }
                let _ = tx.try_send(PlayerEvent::ended(generation));
            }
            MessageView::Error(pipeline_error) => {
                if let Some(ticket) = media_ticket.as_ref() {
                    media_proxy.take_and_release(ticket);
                }
                // GStreamer error/debug strings can embed the authenticated
                // source URI. Keep only closed categories and numeric
                // codes, consistent with local protected playback.
                let error_value = pipeline_error.error();
                let source_category = super::pipeline_error_source_category(msg);
                let elapsed_ms =
                    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                error!(
                    protected = media_ticket.is_some(),
                    domain = super::pipeline_error_domain(&error_value),
                    code = error_value.code(),
                    source_category = source_category.as_str(),
                    elapsed_ms,
                    "AirPlay pipeline error"
                );
                let _ = tx.try_send(PlayerEvent::error(generation, "AirPlay playback failed"));
                return glib::ControlFlow::Break;
            }
            MessageView::StateChanged(s) => {
                if let Some(pipeline) = pipeline_weak.upgrade() {
                    if msg
                        .src()
                        .is_some_and(|src| src == pipeline.upcast_ref::<gst::Object>())
                    {
                        let mapped = match s.current() {
                            gst::State::Playing => Some(PlayerState::Playing),
                            gst::State::Paused => Some(PlayerState::Paused),
                            gst::State::Ready | gst::State::Null => Some(PlayerState::Stopped),
                            gst::State::VoidPending => None,
                        };
                        if let Some(state) = mapped {
                            let _ = tx.try_send(PlayerEvent::state(generation, state));
                        }
                    }
                }
            }
            _ => {}
        }
        glib::ControlFlow::Continue
    })
    .map_err(|e| format!("Failed to attach bus watch: {e}"))
}

/// Start a generation-scoped position timer for this exact RAOP session.
///
/// AirPlay uses a dedicated pipeline rather than the main player, so it
/// needs its own progress publisher. The weak reference makes teardown
/// self-cancelling; retaining the generation captured at load time means
/// even a final delayed tick cannot be attributed to a replacement load.
fn start_position_timer(
    pipeline: &gst::Pipeline,
    event_tx: &async_channel::Sender<PlayerEvent>,
    generation: PlayerEventGeneration,
) {
    let pipeline_weak = pipeline.downgrade();
    let tx = event_tx.clone();

    // `timeout_add` (not `timeout_add_local`) attaches to the default main
    // context from any thread, so the timer still fires on the GTK main loop
    // now that the session is opened on its own worker thread (review F2).
    glib::timeout_add(Duration::from_millis(500), move || {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };

        let (_, state, _) = pipeline.state(gst::ClockTime::ZERO);
        let position_ms = pipeline_position_ms(&pipeline, state);
        let duration_ms = pipeline
            .query_duration::<gst::ClockTime>()
            .map(|duration| duration.mseconds());
        if let Some(event) = position_sample_event(generation, state, position_ms, duration_ms) {
            let _ = tx.try_send(event);
        }

        glib::ControlFlow::Continue
    });
}

/// Turn one pipeline sample into the shared player-event contract.
///
/// Paused/stopped pipelines never publish progress; unknown duration remains
/// zero, matching local, MPD, and Chromecast output semantics.
fn position_sample_event(
    generation: PlayerEventGeneration,
    state: gst::State,
    position_ms: Option<u64>,
    duration_ms: Option<u64>,
) -> Option<PlayerEvent> {
    if state != gst::State::Playing {
        return None;
    }
    position_ms
        .map(|position_ms| PlayerEvent::position(generation, position_ms, duration_ms.unwrap_or(0)))
}

/// Select the transmission path for a new output.
///
/// Selection is explicit configuration, never a silent fallback (design §4.4):
/// the OwnTone process adapter is used only when
/// `TRIBUTARY_AIRPLAY_SENDER=owntone`, and the GStreamer `raopsink` adapter
/// remains the default. Both are independently probe-gated at load time.
fn select_sender() -> Arc<dyn AirplaySender> {
    if super::airplay_owntone::OwnToneSender::selected() {
        Arc::new(super::airplay_owntone::OwnToneSender::from_env())
    } else {
        Arc::new(GstreamerRaopSender)
    }
}

/// One command delivered to a load's worker thread. Sending is non-blocking on
/// the UI thread; the blocking daemon RPC or teardown runs on the worker
/// (review F2).
#[derive(Debug, Clone, Copy)]
enum SessionCommand {
    Pause,
    Resume,
    SetVolume(f64),
    Stop,
}

/// Owns the worker thread for one load, plus the caches the UI reads without
/// touching the session (which lives on the worker).
struct LoadController {
    /// Cancellation currency for this load's open and its worker loop.
    cancel: OpenCancel,
    /// The load's shared Stop/start boundary. `close_session` stops it so no
    /// concurrent start effect can be authorized after teardown (review U3).
    gate: Arc<SessionGate>,
    /// Command channel into the worker. `None` once the controller is closed.
    commands: Option<std::sync::mpsc::Sender<SessionCommand>>,
    /// Detached on close so the UI thread never joins a blocking teardown.
    handle: Option<std::thread::JoinHandle<()>>,
    /// Coarse state published by the worker.
    state: Arc<AtomicU8>,
    /// Latest position snapshot published by the worker.
    position: Arc<Mutex<SenderPosition>>,
}

/// Map the worker's cached state byte back to [`PlayerState`].
fn state_from_u8(raw: u8) -> PlayerState {
    match raw {
        value if value == PlayerState::Buffering as u8 => PlayerState::Buffering,
        value if value == PlayerState::Playing as u8 => PlayerState::Playing,
        value if value == PlayerState::Paused as u8 => PlayerState::Paused,
        _ => PlayerState::Stopped,
    }
}

/// AirPlay audio output — streams to a RAOP receiver.
pub struct AirPlayOutput {
    /// Human-readable name from mDNS discovery (e.g. "Living Room").
    /// Read by the `AudioOutput::name` trait method.
    #[allow(dead_code)]
    display_name: String,
    /// Receiver hostname or IP address.
    host: String,
    /// Receiver port (typically 7000 for AirPlay, varies for RAOP).
    port: u16,
    /// Normalized discovery identifier (MAC/`deviceid`) when discovery
    /// retained one.
    device_id: Option<String>,
    /// Event sender for relaying state changes to the GTK main thread.
    event_tx: async_channel::Sender<PlayerEvent>,
    /// Current load generation, shared with the worker so a stale open cannot
    /// start playback after its generation was superseded.
    event_generation: Arc<AtomicU64>,
    /// Cached volume level (0.0–1.0).
    volume: f64,
    /// App-owned exact-origin fetch boundary for authenticated media. The
    /// GStreamer pipelines receive only its opaque loopback ticket.
    media_proxy: Arc<GstreamerMediaProxy>,
    /// The transmission path selected for this output.
    sender: Arc<dyn AirplaySender>,
    /// Monotonic per-load identity used to key in-flight cancellation
    /// registration in the media proxy.
    load_seq: AtomicU64,
    /// The active load's controller, if any. The controller owns the worker
    /// thread that performs the (potentially blocking) open and controls, so
    /// neither the GTK thread nor a bus callback blocks on a daemon RPC
    /// (review F2).
    controller: Mutex<Option<LoadController>>,
}

impl AirPlayOutput {
    /// Create a new AirPlay output targeting the given receiver.
    ///
    /// Does **not** establish a connection — that happens lazily on the
    /// first playback command.
    pub fn new(
        display_name: &str,
        host: &str,
        port: u16,
        event_tx: async_channel::Sender<PlayerEvent>,
        initial_volume: f64,
    ) -> Self {
        info!(
            host = %host,
            port,
            name = %display_name,
            "AirPlay output configured"
        );
        Self {
            display_name: display_name.to_string(),
            host: host.to_string(),
            port,
            device_id: None,
            event_tx,
            event_generation: Arc::new(AtomicU64::new(0)),
            // Seed from the current slider value so switching to this device
            // doesn't reset the effective volume to maximum (0 dB) on the
            // first track load.
            volume: initial_volume.clamp(0.0, 1.0),
            media_proxy: Arc::new(GstreamerMediaProxy::new(None)),
            sender: select_sender(),
            load_seq: AtomicU64::new(0),
            controller: Mutex::new(None),
        }
    }

    /// Retain the normalized discovery identifier for this receiver.
    #[must_use]
    pub fn with_device_id(mut self, device_id: Option<String>) -> Self {
        self.device_id = device_id;
        self
    }

    /// Supply the application runtime used to host exact-route media tickets.
    #[must_use]
    pub fn with_runtime(self, handle: tokio::runtime::Handle) -> Self {
        self.media_proxy.set_runtime(handle);
        self
    }

    /// Lock the controller, recovering transparently from poisoning.
    ///
    /// A poisoned `Mutex` here means a previous holder panicked. The bus
    /// watch runs on the GLib main loop, so a panic in any of its branches
    /// would otherwise propagate as an app-wide crash on the next lock — even
    /// though we don't actually rely on any invariant the panicking thread
    /// might have left half-built.
    fn controller_guard(&self) -> std::sync::MutexGuard<'_, Option<LoadController>> {
        self.controller.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn event_generation(&self) -> PlayerEventGeneration {
        PlayerEventGeneration::from_raw(self.event_generation.load(Ordering::SeqCst))
    }

    /// The receiver identity carried to the selected sender.
    fn target(&self) -> SenderTarget {
        SenderTarget::new(
            &self.display_name,
            &self.host,
            self.port,
            self.device_id.clone(),
        )
    }

    /// Linear 0.0–1.0 volume → RAOP dB scale (-30.0 = quiet, 0.0 = max).
    fn volume_to_db(linear: f64) -> f64 {
        if linear <= 0.0 {
            -144.0
        } else {
            (linear - 1.0) * 30.0
        }
    }

    /// Start a load on its own worker thread. The availability gate and media
    /// preparation have already run on the caller, so this only marshals the
    /// owned context and spawns the worker.
    ///
    /// A worker-spawn failure is not swallowed: the prepared route is released
    /// and a `Stopped` failure is reported, so a spawn failure can never leave
    /// a minted loopback route alive or the output stuck `Buffering` (review
    /// S6).
    fn start_session_worker(
        &self,
        generation: PlayerEventGeneration,
        prepared: PreparedGstreamerMedia,
    ) {
        // Tear down any previous load before starting a new one.
        self.close_session();

        let open_id = self.load_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let cancel = OpenCancel::new();
        // The shared Stop/start boundary for this load (review U3).
        let gate = Arc::new(SessionGate::new());
        // Retain a handle for the failure path; the context owns its own clone.
        let failure_ticket = prepared.ticket();
        // Register the in-flight cancellation — bound to this preparation's
        // generation — **before scheduling the worker** (review U4). A
        // replacement preparation that runs after this sees the load counted
        // in-flight and preserves its route in custody instead of revoking it,
        // and a load whose preparation was already superseded registers nothing.
        let registration =
            self.media_proxy
                .register_in_flight_cancel(open_id, prepared.generation(), &cancel);
        if registration.is_superseded() {
            // Lost the prepare-to-open race before scheduling: no negotiation
            // and no events. Release the route on the caller.
            if let Some(ticket) = failure_ticket.as_ref() {
                self.media_proxy.take_and_release(ticket);
            }
            debug!("AirPlay: load superseded before its worker was scheduled");
            return;
        }
        let state_cache = Arc::new(AtomicU8::new(PlayerState::Buffering as u8));
        let position_cache = Arc::new(Mutex::new(SenderPosition::unknown(generation)));
        let (commands, command_rx) = mpsc::channel();

        let ctx = SenderOpenContext {
            target: self.target(),
            prepared_uri: prepared.uri().to_string(),
            event_tx: self.event_tx.clone(),
            generation,
            media_proxy: Arc::clone(&self.media_proxy),
            media_ticket: prepared.ticket(),
            volume: self.volume,
            cancel: cancel.clone(),
            session_gate: Arc::clone(&gate),
            open_id,
        };
        let sender = Arc::clone(&self.sender);
        let event_generation = Arc::clone(&self.event_generation);
        let worker_state = Arc::clone(&state_cache);
        let worker_position = Arc::clone(&position_cache);
        let spawned = std::thread::Builder::new()
            .name("airplay-load".to_string())
            .spawn(move || {
                run_session_worker(
                    sender,
                    ctx,
                    registration,
                    command_rx,
                    worker_state,
                    worker_position,
                    event_generation,
                );
            });

        let Ok(handle) = spawned else {
            if let Some(ticket) = failure_ticket.as_ref() {
                self.media_proxy.take_and_release(ticket);
            }
            self.report_load_failure(generation, "AirPlay session worker could not be started");
            return;
        };

        *self.controller_guard() = Some(LoadController {
            cancel,
            gate,
            commands: Some(commands),
            handle: Some(handle),
            state: state_cache,
            position: position_cache,
        });
    }

    /// Report a synchronous load failure (probe or media preparation) on the
    /// caller's thread.
    fn report_load_failure(&self, generation: PlayerEventGeneration, message: &str) {
        error!(error = %message, "AirPlay: failed to open session");
        let _ = self
            .event_tx
            .try_send(PlayerEvent::error(generation, message));
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
    }

    /// Run the availability gate and media preparation for one load, in that
    /// order (review S6). The gate runs **before** any per-track media work, so
    /// an unavailable sender can never mint a loopback route or open local
    /// media. `prepare` is only invoked once the probe succeeds; a successful
    /// gate starts the worker and returns immediately, so the caller never
    /// blocks on negotiation (review F2).
    fn begin_load<F>(&self, generation: PlayerEventGeneration, prepare: F) -> bool
    where
        F: FnOnce() -> Result<PreparedGstreamerMedia, String>,
    {
        match self.sender.probe() {
            Ok(()) => match prepare() {
                Ok(prepared) => self.start_session_worker(generation, prepared),
                Err(message) => {
                    self.close_session();
                    self.report_load_failure(generation, &message);
                }
            },
            Err(error) => {
                // A refused load still replaces the previous one: the old
                // session is torn down (as it was before the seam landed)
                // rather than streaming on behind a failure reported for the
                // new generation.
                self.close_session();
                self.report_load_failure(generation, error.message());
            }
        }
        true
    }

    /// Tear down the active load without blocking the caller: signal the
    /// worker, then detach it so teardown (a blocking daemon RPC or a pump
    /// join) runs on the worker, not the UI thread (review F2).
    fn close_session(&self) {
        let controller = self.controller_guard().take();
        if let Some(mut controller) = controller {
            // Stop the shared boundary before cancelling: a start effect that
            // races this teardown is either refused before it runs or already
            // transmitted, and the session's own close settles it (review U3).
            controller.gate.stop();
            controller.cancel.cancel();
            if let Some(commands) = controller.commands.take() {
                let _ = commands.send(SessionCommand::Stop);
            }
            // Dropping the handle detaches the worker; it owns session teardown.
            drop(controller.handle.take());
            controller
                .state
                .store(PlayerState::Stopped as u8, Ordering::SeqCst);
        }
    }

    /// Apply a state transition through the worker, if any.
    fn set_session_state(&self, target: PlayerState) -> bool {
        let command = match target {
            PlayerState::Playing => SessionCommand::Resume,
            PlayerState::Paused => SessionCommand::Pause,
            PlayerState::Buffering => return true,
            PlayerState::Stopped => {
                self.close_session();
                return true;
            }
        };
        let guard = self.controller_guard();
        let Some(controller) = guard.as_ref() else {
            debug!(?target, "AirPlay: no active session for state change");
            return false;
        };
        controller
            .commands
            .as_ref()
            .is_some_and(|commands| commands.send(command).is_ok())
    }

    /// Test-only direct outcome handling for the synchronous failure and
    /// cancellation paths; the live path goes through [`Self::start_session_worker`].
    #[cfg(test)]
    fn finish_load(&self, generation: PlayerEventGeneration, outcome: OpenOutcome) {
        match outcome {
            OpenOutcome::Opened(_) => {}
            OpenOutcome::Cancelled => {
                // A cancelled load is not a user-facing failure: no error
                // event and no `Stopped` for a generation the caller already
                // abandoned.
                debug!("AirPlay: load cancelled before the session opened");
            }
            OpenOutcome::Failed(error) => {
                self.report_load_failure(generation, error.message());
            }
        }
    }
}

/// Publish a load failure for its generation — unless the controller has
/// already stopped or replaced this load. The decision and the publication are
/// one step under the shared Stop boundary, which `close_session` stops before
/// it cancels, so a failure observed while (or because) a Stop interrupted the
/// open publishes nothing for the cancelled generation (design §4.1: no event
/// for a cancelled generation). The route release that accompanies the failure
/// stays unconditional.
fn publish_load_failure(
    ctx: &SenderOpenContext,
    event_generation: &AtomicU64,
    generation: PlayerEventGeneration,
    error: &SenderError,
) {
    if event_generation.load(Ordering::SeqCst) != generation.as_raw() {
        return;
    }
    let _ = ctx.session_gate.publish_if_live(|| {
        let _ = ctx
            .event_tx
            .try_send(PlayerEvent::error(generation, error.message()));
        let _ = ctx
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
    });
}

/// Worker for one load: its in-flight cancellation registration was taken by
/// the load path **before** this worker was scheduled (review U4), so the
/// worker only runs the blocking `open_session` off the UI thread, then owns
/// the live session and services control commands. Publishes coarse
/// state/position into caches the UI reads without touching the session
/// (review F2).
fn run_session_worker(
    sender: Arc<dyn AirplaySender>,
    ctx: SenderOpenContext,
    registration: InFlightCancelRegistration,
    commands: std::sync::mpsc::Receiver<SessionCommand>,
    state_cache: Arc<AtomicU8>,
    position_cache: Arc<Mutex<SenderPosition>>,
    event_generation: Arc<AtomicU64>,
) {
    let proxy = Arc::clone(&ctx.media_proxy);
    // The registration intentionally outlives the open: while the session is
    // live it keeps the load counted as in-flight in the proxy, so a replacement
    // preparation preserves the live route in recovery custody instead of
    // revoking it before the session's own close can release it (review T4). It
    // drops when this worker returns, after the session's terminal release.
    let _registration = registration;

    let generation = ctx.generation;
    let outcome = sender.open_session(&ctx);
    match outcome {
        OpenOutcome::Opened(mut session) => {
            let still_current = event_generation.load(Ordering::SeqCst) == generation.as_raw()
                && !ctx.cancel.is_cancelled();
            if !still_current {
                // A stale open must not start playback.
                session.close();
                return;
            }
            // `open_session` only prerolls; like every other output, a load
            // must actually start playback. Report the real outcome: a failed
            // or cancelled start is not `Playing` (review T3).
            if !session.resume() {
                state_cache.store(PlayerState::Stopped as u8, Ordering::SeqCst);
                let _ = ctx
                    .event_tx
                    .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
                session.close();
                return;
            }
            // Run the worker's own cache/event publication *through* the
            // session, so it is serialized with the session's terminal
            // transition (review X1, review Y1). Publishing from here after
            // `confirm_started` returned was the caller-side gap: the returned
            // value outlived the session's boundary and a concurrent terminal
            // restoration could publish `Stopped`/`TrackEnded` first. The
            // closure runs under that boundary and is skipped entirely once the
            // session is terminal, so no `Playing` can follow a terminal
            // `Stopped`/`TrackEnded`.
            let mut publish_started = |state: PlayerState| {
                state_cache.store(state as u8, Ordering::SeqCst);
                let _ = ctx.event_tx.try_send(PlayerEvent::state(generation, state));
            };
            session.confirm_started(&mut publish_started);
            loop {
                state_cache.store(session.state() as u8, Ordering::SeqCst);
                *position_cache.lock().unwrap_or_else(|p| p.into_inner()) = session.observe();
                if ctx.cancel.is_cancelled() || session.is_finished() {
                    break;
                }
                match commands.recv_timeout(Duration::from_millis(200)) {
                    Ok(SessionCommand::Pause) => {
                        if !session.pause() {
                            state_cache.store(PlayerState::Stopped as u8, Ordering::SeqCst);
                            break;
                        }
                    }
                    Ok(SessionCommand::Resume) => {
                        if !session.resume() {
                            // A failed live resume is terminal just like a
                            // failed initial start. Settle on this worker even
                            // when the UI (e.g. direct radio) sends no Stop.
                            state_cache.store(PlayerState::Stopped as u8, Ordering::SeqCst);
                            break;
                        }
                    }
                    Ok(SessionCommand::SetVolume(level)) => {
                        if !session.set_volume(level) {
                            state_cache.store(PlayerState::Stopped as u8, Ordering::SeqCst);
                            break;
                        }
                    }
                    Ok(SessionCommand::Stop) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            // The worker owns teardown, so the close's blocking restore/join
            // never runs on the UI thread.
            session.close();
            state_cache.store(PlayerState::Stopped as u8, Ordering::SeqCst);
        }
        OpenOutcome::Cancelled => {
            // Silent and fully unwound: release this load's ticket on receipt.
            release_ticket(&proxy, &ctx);
        }
        OpenOutcome::Failed(error) => {
            state_cache.store(PlayerState::Stopped as u8, Ordering::SeqCst);
            if let SenderError::RecoveryPending { completion, .. } = &error {
                // Recovery is still outstanding: keep the route in keyed
                // custody and release it only at the terminal recovery
                // outcome (review F3). The seat already moved it to custody as
                // part of producing this outcome (review S5); this idempotent
                // call backstops a ticket that reached custody another way.
                if let Some(ticket) = ctx.media_ticket.as_ref() {
                    proxy.move_to_recovery_custody(ticket);
                }
                publish_load_failure(&ctx, &event_generation, generation, &error);
                match completion.wait() {
                    // Quiescence was never established, so the recovery retains
                    // the lock and the route custody for the supervisor. Do not
                    // release the route (review S3).
                    RecoveryOutcome::Retained { .. } => {}
                    _ => release_ticket(&proxy, &ctx),
                }
            } else {
                // Restoration completed inside the seam; release on receipt.
                release_ticket(&proxy, &ctx);
                publish_load_failure(&ctx, &event_generation, generation, &error);
            }
        }
    }
}

/// Release this load's media ticket, if it had one.
fn release_ticket(proxy: &Arc<GstreamerMediaProxy>, ctx: &SenderOpenContext) {
    if let Some(ticket) = ctx.media_ticket.as_ref() {
        proxy.take_and_release(ticket);
    }
}

/// Test-only harness that drives the real [`run_session_worker`] for a
/// caller-supplied sender/session, so a regression can exercise the production
/// worker path (its own cache/event publication, command loop and teardown)
/// rather than a session in isolation (review Z2).
#[cfg(test)]
pub(super) struct TestSessionWorker {
    state_cache: Arc<AtomicU8>,
    commands: Option<std::sync::mpsc::Sender<SessionCommand>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
impl TestSessionWorker {
    /// The worker's own coarse state cache, exactly as the UI reads it.
    pub(super) fn cached_state(&self) -> PlayerState {
        state_from_u8(self.state_cache.load(Ordering::SeqCst))
    }

    /// Deliver a `Stop` and join the worker, releasing its teardown.
    pub(super) fn stop_and_join(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(SessionCommand::Stop);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn the real [`run_session_worker`] on its own thread for a test-supplied
/// sender and context. The returned harness exposes the worker's own caches and
/// a deterministic stop/join.
#[cfg(test)]
pub(super) fn spawn_test_session_worker(
    sender: Arc<dyn AirplaySender>,
    ctx: SenderOpenContext,
    registration: InFlightCancelRegistration,
    state_cache: Arc<AtomicU8>,
    position_cache: Arc<Mutex<SenderPosition>>,
    event_generation: Arc<AtomicU64>,
) -> TestSessionWorker {
    let (commands, command_rx) = mpsc::channel();
    let worker_state = Arc::clone(&state_cache);
    let worker_position = Arc::clone(&position_cache);
    let handle = std::thread::Builder::new()
        .name("airplay-test-load".to_string())
        .spawn(move || {
            run_session_worker(
                sender,
                ctx,
                registration,
                command_rx,
                worker_state,
                worker_position,
                event_generation,
            );
        })
        .expect("spawn test session worker");
    TestSessionWorker {
        state_cache,
        commands: Some(commands),
        handle: Some(handle),
    }
}

/// Test-only controller harness around a real [`AirPlayOutput`]: it exposes the
/// exact `begin_load`/`stop`/cache/route surface the production controller uses,
/// so a cross-module regression can drive the controller/replacement path
/// without reaching into private fields.
#[cfg(test)]
pub(super) struct ControllerHarness {
    output: AirPlayOutput,
}

#[cfg(test)]
impl ControllerHarness {
    pub(super) fn new(
        runtime: tokio::runtime::Handle,
        sender: Arc<dyn AirplaySender>,
        event_tx: async_channel::Sender<PlayerEvent>,
    ) -> Self {
        let mut output =
            AirPlayOutput::new("Test", "127.0.0.1", 7000, event_tx, 1.0).with_runtime(runtime);
        output.sender = sender;
        Self { output }
    }

    /// Retain the discovered receiver identity for the real OwnTone open path.
    pub(super) fn with_device_id(mut self, device_id: &str) -> Self {
        self.output = self.output.with_device_id(Some(device_id.to_string()));
        self
    }

    /// Mint a protected route through the output's own media proxy.
    pub(super) fn prepare(&self, request: ResolvedHttpRequest) -> PreparedGstreamerMedia {
        self.output
            .media_proxy
            .prepare_resolved(request)
            .expect("prepare protected media")
    }

    /// Start a load on the production controller with already-prepared media.
    pub(super) fn load(&self, generation: PlayerEventGeneration, prepared: PreparedGstreamerMedia) {
        assert!(
            self.output.begin_load(generation, || Ok(prepared)),
            "the controller must accept the load"
        );
    }

    pub(super) fn set_generation(&self, generation: PlayerEventGeneration) {
        self.output.set_event_generation(generation);
    }

    pub(super) fn proxy(&self) -> Arc<GstreamerMediaProxy> {
        Arc::clone(&self.output.media_proxy)
    }

    pub(super) fn state(&self) -> PlayerState {
        self.output.state()
    }

    /// The production non-blocking Stop path (signals and detaches the worker).
    pub(super) fn stop(&self) {
        self.output.stop();
    }

    /// Repoint the output at another sender, as production does when the
    /// selected transmission path changes. Used to drive a replacement load on
    /// a **separate** legitimate instance without rebuilding the controller.
    pub(super) fn set_sender(&mut self, sender: Arc<dyn AirplaySender>) {
        self.output.sender = sender;
    }

    /// Drive the production pause control through the live worker.
    pub(super) fn pause(&self) {
        self.output.pause();
    }

    /// Drive the production volume control through the live worker.
    pub(super) fn set_volume(&mut self, level: f64) {
        self.output.set_volume(level);
    }

    /// Drive the production resume control through the live worker.
    pub(super) fn play(&self) {
        self.output.play();
    }
}

impl Drop for AirPlayOutput {
    fn drop(&mut self) {
        self.close_session();
    }
}

impl AudioOutput for AirPlayOutput {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn output_type(&self) -> OutputType {
        OutputType::AirPlay
    }

    fn supports_volume(&self) -> bool {
        // raopsink forwards volume to the receiver, and the receiver's
        // hardware volume remains as a backstop.
        true
    }

    fn load_uri(&self, uri: &str) -> bool {
        info!("AirPlay: loading URI");
        let generation = self.event_generation();
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering));

        // The probe runs before this closure: an unavailable sender never
        // reaches media preparation (review S6).
        self.begin_load(generation, || {
            self.media_proxy
                .prepare(uri)
                .map_err(|_| "AirPlay media preparation failed".to_string())
        })
    }

    fn load_resolved(&self, request: ResolvedHttpRequest) -> bool {
        info!("AirPlay: loading resolved media");
        let generation = self.event_generation();
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering));
        self.begin_load(generation, || {
            self.media_proxy
                .prepare_resolved(request)
                .map_err(|_| "AirPlay media preparation failed".to_string())
        })
    }

    fn load_local(&self, media: ResolvedLocalMedia) -> bool {
        info!("AirPlay: loading authorized local media");
        let generation = self.event_generation();
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering));
        self.begin_load(generation, || {
            self.media_proxy
                .prepare_local(media)
                .map_err(|_| "AirPlay media preparation failed".to_string())
        })
    }

    fn set_event_generation(&self, generation: PlayerEventGeneration) {
        self.event_generation
            .store(generation.as_raw(), Ordering::SeqCst);
    }

    fn play(&self) {
        debug!("AirPlay: play");
        let _ = self.set_session_state(PlayerState::Playing);
    }

    fn pause(&self) {
        debug!("AirPlay: pause");
        let _ = self.set_session_state(PlayerState::Paused);
    }

    fn stop(&self) {
        debug!("AirPlay: stop");
        self.close_session();
        let _ = self.event_tx.try_send(PlayerEvent::state(
            self.event_generation(),
            PlayerState::Stopped,
        ));
    }

    fn toggle_play_pause(&self) {
        let target = {
            let guard = self.controller_guard();
            guard.as_ref().map(|controller| {
                match state_from_u8(controller.state.load(Ordering::SeqCst)) {
                    PlayerState::Playing => PlayerState::Paused,
                    PlayerState::Paused | PlayerState::Stopped | PlayerState::Buffering => {
                        PlayerState::Playing
                    }
                }
            })
        };
        if let Some(state) = target {
            let _ = self.set_session_state(state);
        }
    }

    fn seek_to(&self, _position_ms: u64) {
        // Live RAOP streams don't expose seekable timelines; skip.
        debug!("AirPlay: seek not supported on live streams");
    }

    fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
        let guard = self.controller_guard();
        if let Some(controller) = guard.as_ref() {
            if let Some(commands) = controller.commands.as_ref() {
                let _ = commands.send(SessionCommand::SetVolume(self.volume));
            }
        }
    }

    fn volume(&self) -> f64 {
        self.volume
    }

    fn state(&self) -> PlayerState {
        let guard = self.controller_guard();
        guard.as_ref().map_or(PlayerState::Stopped, |controller| {
            state_from_u8(controller.state.load(Ordering::SeqCst))
        })
    }

    fn position_ms(&self) -> Option<u64> {
        // A live RAOP stream owns its position internally; this must not
        // claim progress for a stopped session.
        let guard = self.controller_guard();
        let controller = guard.as_ref()?;
        let position = controller
            .position
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .position_ms;
        position
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn test_airplay_output_name() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Living Room", "192.168.1.100", 7000, tx, 1.0);
        assert_eq!(output.name(), "Living Room");
    }

    #[test]
    fn test_airplay_output_type() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        assert_eq!(output.output_type(), OutputType::AirPlay);
    }

    #[test]
    fn test_airplay_supports_volume() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        assert!(output.supports_volume());
    }

    #[test]
    fn test_airplay_volume_clamp() {
        let (tx, _rx) = async_channel::unbounded();
        let mut output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        output.set_volume(1.5);
        assert!((output.volume() - 1.0).abs() < f64::EPSILON);
        output.set_volume(-0.5);
        assert!((output.volume() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_airplay_volume_db_scale() {
        // 0.0 linear → -144 dB (mute), 1.0 → 0 dB (max), 0.5 → -15 dB.
        assert!((AirPlayOutput::volume_to_db(0.0) - -144.0).abs() < f64::EPSILON);
        assert!((AirPlayOutput::volume_to_db(1.0) - 0.0).abs() < f64::EPSILON);
        assert!((AirPlayOutput::volume_to_db(0.5) - -15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_airplay_no_position_without_session() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        assert!(output.position_ms().is_none());
    }

    #[test]
    fn test_airplay_initial_state() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        assert_eq!(output.state(), PlayerState::Stopped);
    }

    #[test]
    fn airplay_position_samples_are_playing_only_and_keep_the_load_generation() {
        let generation = PlayerEventGeneration::from_raw(73);

        assert!(
            position_sample_event(generation, gst::State::Paused, Some(1_500), Some(9_000),)
                .is_none()
        );
        assert!(
            position_sample_event(generation, gst::State::Playing, None, Some(9_000),).is_none()
        );

        assert!(matches!(
            position_sample_event(
                generation,
                gst::State::Playing,
                Some(1_500),
                Some(9_000),
            ),
            Some(PlayerEvent::PositionChanged {
                generation: event_generation,
                position_ms: 1_500,
                duration_ms: 9_000,
            }) if event_generation == generation
        ));
        assert!(matches!(
            position_sample_event(generation, gst::State::Playing, Some(2_000), None),
            Some(PlayerEvent::PositionChanged {
                generation: event_generation,
                position_ms: 2_000,
                duration_ms: 0,
            }) if event_generation == generation
        ));
    }

    /// P2.9's core guarantee: a missing `raopsink` is refused explicitly and
    /// never routed to a fallback that cannot transmit. No currently
    /// supported package is misrepresented as providing the element.
    #[test]
    fn a_missing_raopsink_is_refused_with_honest_guidance() {
        let error = SenderError::Dependency(GstreamerRaopSender::raopsink_missing_message("en"));
        assert!(matches!(error, SenderError::Dependency(_)));
        assert!(error.message().contains("raopsink"), "{}", error.message());
        assert!(
            !error.message().contains("gst-plugins-bad"),
            "{}",
            error.message()
        );
    }

    /// The GStreamer adapter's description must select the ALAC encoder, whose
    /// 352-sample framing RAOP receivers expect (design §2.4, §9.7). This is a
    /// string-level regression because a mis-framed stream only manifests as
    /// device-specific glitches.
    #[test]
    fn the_gstreamer_pipeline_requests_alac_framing_and_raopsink() {
        let description = GstreamerRaopSender::pipeline_description(
            "192.0.2.10",
            7000,
            "http://127.0.0.1:1234/audio",
        );
        assert!(description.contains("avenc_alac"), "{description}");
        assert!(description.contains("raopsink"), "{description}");
        assert!(description.contains("host=192.0.2.10"), "{description}");
        assert!(description.contains("port=7000"), "{description}");
    }

    /// A sender whose probe cannot pass refuses the load with its own
    /// `SenderError` guidance — design §9.1/§9.2's adapter-injection stub.
    struct FailingSender(SenderError);

    impl AirplaySender for FailingSender {
        fn name(&self) -> &'static str {
            "test-failing"
        }

        fn probe(&self) -> Result<(), SenderError> {
            Err(self.0.clone())
        }

        fn open_session(&self, _ctx: &SenderOpenContext) -> OpenOutcome {
            OpenOutcome::Failed(self.0.clone())
        }
    }

    #[test]
    fn a_failing_probe_refuses_the_load_without_opening_a_session() {
        let (tx, rx) = async_channel::unbounded();
        let mut output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        output.sender = Arc::new(FailingSender(SenderError::Dependency(
            "sender unavailable".to_string(),
        )));
        let generation = PlayerEventGeneration::from_raw(21);
        output.set_event_generation(generation);

        output.load_uri("https://music.test/stream");

        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::StateChanged {
                generation: event_generation,
                state: PlayerState::Buffering,
            }) if event_generation == generation
        ));
        match rx.try_recv() {
            Ok(PlayerEvent::Error {
                generation: event_generation,
                message,
            }) => {
                assert_eq!(event_generation, generation);
                assert_eq!(message, "sender unavailable");
            }
            event => panic!("expected sender-probe error, got {event:?}"),
        }
        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::StateChanged {
                generation: event_generation,
                state: PlayerState::Stopped,
            }) if event_generation == generation
        ));
        assert!(rx.try_recv().is_err());
        assert_eq!(output.state(), PlayerState::Stopped);
    }

    /// S6: the availability gate runs **before** media preparation. A failing
    /// sender must never reach preparation, so a valid runtime and a protected
    /// request cannot mint a loopback route for a session that will not open.
    #[test]
    fn a_failing_probe_never_prepares_or_mints_a_route() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let (tx, _rx) = async_channel::unbounded();
        let mut output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0)
            .with_runtime(runtime.handle().clone());
        output.sender = Arc::new(FailingSender(SenderError::Dependency(
            "sender unavailable".to_string(),
        )));
        let generation = PlayerEventGeneration::from_raw(88);
        output.set_event_generation(generation);

        // A protected URI that would mint a ticket if preparation ran.
        let protected = "https://music.test/stream?api_key=must-not-mint";
        let mut preparation_ran = false;
        output.begin_load(generation, || {
            preparation_ran = true;
            output
                .media_proxy
                .prepare(protected)
                .map_err(|_| "AirPlay media preparation failed".to_string())
        });

        assert!(
            !preparation_ran,
            "preparation must not run when the probe fails (S6)"
        );
        assert!(
            !output.media_proxy.has_active_lease(),
            "an unavailable sender must not mint a loopback route (S6)"
        );
        assert!(!output.media_proxy.has_custody_entries());
    }

    /// The guidance must be real in every catalog — present, mentioning the
    /// exact technical identifier, and not silently falling back to English.
    #[test]
    fn raopsink_guidance_is_localized_for_every_catalog() {
        let english = GstreamerRaopSender::raopsink_missing_message("en");
        assert!(!english.is_empty());

        for locale in rust_i18n::available_locales!() {
            let localized = GstreamerRaopSender::raopsink_missing_message(&locale);
            assert!(localized.contains("raopsink"), "{locale}: {localized}");
            assert!(
                !localized.contains("gst-plugins-bad"),
                "{locale}: {localized}"
            );
            if locale != "en" {
                assert_ne!(localized, english, "{locale} must not fall back to English");
            }
        }
    }

    /// A load without a transmitter must fail *loudly* — an `Error` event
    /// carrying the honest unavailable message followed by `Stopped` — never a
    /// silent no-op stream.
    #[test]
    fn a_missing_raopsink_load_fails_loudly_not_silently() {
        let (tx, rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        let generation = PlayerEventGeneration::from_raw(7);
        output.set_event_generation(generation);

        output.finish_load(
            generation,
            OpenOutcome::Failed(SenderError::Dependency(
                GstreamerRaopSender::raopsink_missing_message(&rust_i18n::locale()),
            )),
        );

        match rx.try_recv() {
            Ok(PlayerEvent::Error {
                generation: event_generation,
                message,
            }) => {
                assert_eq!(event_generation, generation);
                assert!(message.contains("raopsink"), "{message}");
                assert!(!message.contains("gst-plugins-bad"), "{message}");
            }
            event => panic!("expected explicit sender error, got {event:?}"),
        }
        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::StateChanged {
                generation: event_generation,
                state: PlayerState::Stopped,
            }) if event_generation == generation
        ));
        assert!(rx.try_recv().is_err());
    }

    /// R1/U6: the availability gate runs on the GTK caller, so a dedicated
    /// daemon that accepts the connection and then never answers must not
    /// freeze the load path.
    ///
    /// This is a **production-path** fixture: it compiles a hermetic fake
    /// daemon whose executable and `-c <state>/owntone.conf` launch bind it as
    /// the owned instance (the canonical config names the adapter's FIFO), so
    /// `verify_owned` passes and the load genuinely reaches the `/api/config`
    /// handshake before the endpoint stalls. `load_uri` and `stop` must both
    /// return promptly because the blocking handshake runs on the load worker.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stalled_owntone_endpoint_does_not_block_load_or_stop() {
        use std::net::{TcpListener, TcpStream};

        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let config = state_dir.join("owntone.conf");
        std::fs::write(
            &config,
            include_str!("../../tests/fixtures/owntone-29.3-library.conf").replace(
                "@PIPE_DIRECTORY@",
                state_dir.to_str().expect("UTF-8 state path"),
            ),
        )
        .expect("write config");

        // Compile the hermetic fake daemon: it binds the endpoint and never
        // answers, and its argv binds the canonical config so it is genuinely
        // the owned process the adapter verifies.
        let source = directory.path().join("fake_owntone.rs");
        std::fs::write(&source, FAKE_DAEMON_SOURCE).expect("write daemon source");
        let binary = directory.path().join("fake_owntone");
        let compiled = std::process::Command::new("rustc")
            .arg("-O")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .status()
            .expect("invoke rustc");
        assert!(compiled.success(), "the fake daemon must compile");

        let probe = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let port = probe.local_addr().expect("addr").port();
        drop(probe);
        // The production boundary this fixture asserts: the load's own request
        // must reach the daemon (and be observed there) before Stop is
        // measured. Without this barrier the test could pass merely because
        // cancellation prevented the worker from issuing `/api/config` at all
        // (review V3).
        let observed = directory.path().join("observed.txt");
        let mut child = std::process::Command::new(&binary)
            .arg("-c")
            .arg(&config)
            .env("TRIBUTARY_FAKE_LISTEN", format!("127.0.0.1:{port}"))
            .env("TRIBUTARY_FAKE_OBSERVED", &observed)
            .spawn()
            .expect("spawn fake daemon");
        // Wait for the fake daemon to bind before the load verifies ownership.
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "the fake daemon did not start listening"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let api_base = format!("http://127.0.0.1:{port}");
        let sender =
            crate::audio::airplay_owntone::test_owned_sender(&api_base, &state_dir, &binary);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let (tx, rx) = async_channel::unbounded();
        let mut output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0)
            .with_runtime(runtime.handle().clone());
        output.sender = Arc::new(sender);
        let generation = PlayerEventGeneration::from_raw(64);
        output.set_event_generation(generation);
        // Protected local media mints a loopback route, so the worker's
        // eventual release of that route is the observable end of the load.
        let root = tempfile::tempdir().expect("media root");
        let marker = format!("marker:v1:{}", uuid::Uuid::new_v4());
        std::fs::write(
            root.path().join(".tributary-root-id"),
            format!("{marker}\n"),
        )
        .expect("root marker");
        let media_path = root.path().join("stalled.wav");
        std::fs::write(&media_path, b"RIFF").expect("media file");
        let media =
            ResolvedLocalMedia::from_authorized_path_for_test(root.path(), &marker, &media_path)
                .expect("authorized media");

        let started = Instant::now();
        assert!(output.load_local(media));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "load blocked on the stalled OwnTone endpoint: {:?}",
            started.elapsed()
        );

        // Barrier: wait until the daemon has read the blocking handshake
        // request, proving the load worker genuinely reached `/api/config` and
        // is stalled there (not merely cancelled before it started).
        let barrier_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = std::fs::read_to_string(&observed).unwrap_or_default();
            if text.contains("/api/config") {
                break;
            }
            assert!(
                Instant::now() < barrier_deadline,
                "the load never reached /api/config (observed: {text:?})"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let stopped = Instant::now();
        output.stop();
        assert!(
            stopped.elapsed() < Duration::from_millis(500),
            "stop blocked on the stalled OwnTone endpoint: {:?}",
            stopped.elapsed()
        );

        // Dropping the daemon's held connection now fails the stalled
        // handshake *after* the Stop. That failure belongs to the cancelled
        // load, not to playback: the route is released, and neither an Error
        // nor a Playing is published for the stopped generation.
        let _ = child.kill();
        let _ = child.wait();
        let release_deadline = Instant::now() + Duration::from_secs(10);
        while output.media_proxy.has_active_lease() {
            assert!(
                Instant::now() < release_deadline,
                "the cancelled load never released its route"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::Error { .. }
                    | PlayerEvent::StateChanged {
                        state: PlayerState::Playing,
                        ..
                    }
            )),
            "a Stop-interrupted open published a playback outcome: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::StateChanged {
                state: PlayerState::Stopped,
                ..
            }
        )));
    }

    /// The fake OwnTone daemon source compiled by the stalled-endpoint fixture.
    /// It binds the loopback endpoint named in `TRIBUTARY_FAKE_LISTEN`, reads
    /// each accepted request line, appends it to the path named in
    /// `TRIBUTARY_FAKE_OBSERVED` (so the test can prove the exact request
    /// reached the server), then holds the connection without ever writing a
    /// response — a blocking `/api/config` handshake stalls until its client
    /// timeout.
    #[cfg(target_os = "linux")]
    const FAKE_DAEMON_SOURCE: &str = r#"
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

fn main() {
    let addr = std::env::var("TRIBUTARY_FAKE_LISTEN").expect("listen address");
    let observed = std::env::var("TRIBUTARY_FAKE_OBSERVED").ok();
    let listener = TcpListener::bind(&addr).expect("bind");
    for incoming in listener.incoming() {
        let observed = observed.clone();
        let Ok(stream) = incoming else { continue };
        std::thread::spawn(move || {
            let Ok(reader_stream) = stream.try_clone() else { return };
            let mut reader = BufReader::new(reader_stream);
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let trimmed = request_line.trim();
            if !trimmed.is_empty() {
                if let Some(path) = observed.as_ref() {
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(file, "{trimmed}");
                        let _ = file.flush();
                    }
                }
            }
            // Hold the connection open without ever responding, so the client's
            // blocking handshake stalls until its own timeout.
            let _held = stream;
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        });
    }
}
"#;

    /// S1: on a target other than Linux the unsupported shim is compiled
    /// instead of the real adapter. Selection is still recognized and the
    /// sender still refuses explicitly, so a load can never be misreported as
    /// an OwnTone session — and the Linux-only stalled-endpoint regression
    /// above is never compiled here.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn an_unsupported_platform_owntone_sender_refuses_the_load() {
        let sender = crate::audio::airplay_owntone::OwnToneSender::from_env();
        let error = sender
            .probe()
            .expect_err("the unsupported shim must refuse every load");
        assert!(error.message().contains("OwnTone"), "{}", error.message());
    }

    /// Bounded wait for a condition established by a detached worker.
    fn wait_for(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for condition");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A load whose availability gate fails still replaces the previous one:
    /// the old session is torn down instead of streaming on behind an
    /// Error/Stopped reported for the new generation.
    #[test]
    fn a_failing_probe_still_tears_down_the_previous_session() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let live = Arc::new(AtomicBool::new(true));
        let closed = Arc::new(AtomicBool::new(false));
        let (tx, rx) = async_channel::unbounded();
        let mut output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0)
            .with_runtime(runtime.handle().clone());
        output.sender = Arc::new(BoundarySender {
            live: Arc::clone(&live),
            closed: Arc::clone(&closed),
        });
        let first = PlayerEventGeneration::from_raw(31);
        output.set_event_generation(first);
        assert!(output.load_uri("https://music.test/first"));
        wait_for(|| output.state() == PlayerState::Playing);
        assert!(!closed.load(Ordering::SeqCst));
        while rx.try_recv().is_ok() {}

        output.sender = Arc::new(FailingSender(SenderError::Dependency(
            "sender unavailable".to_string(),
        )));
        let second = first.next();
        output.set_event_generation(second);
        assert!(output.load_uri("https://music.test/second"));
        wait_for(|| closed.load(Ordering::SeqCst));
        assert_eq!(output.state(), PlayerState::Stopped);
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            events
                .iter()
                .any(|event| event.generation() == second
                    && matches!(event, PlayerEvent::Error { .. })),
            "{events:?}"
        );
    }

    /// The GStreamer session's start confirmation is one step with the Stop
    /// boundary: once a Stop has won, the worker's `Playing` publication is
    /// suppressed rather than following the Stop's own `Stopped`.
    #[test]
    fn a_gstreamer_start_confirmation_is_suppressed_once_stop_has_won() {
        gst::init().expect("GStreamer init");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("url"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");
        let ticket = prepared.ticket().expect("protected media ticket");
        let (tx, _rx) = async_channel::unbounded();
        let gate = Arc::new(SessionGate::new());
        let ctx = test_session_ctx(
            &proxy,
            Some(Arc::clone(&ticket)),
            tx,
            Arc::clone(&gate),
            PlayerEventGeneration::from_raw(13),
        );
        let (pipeline, _counter) = test_pipeline_with_buffer_counter();
        let mut session =
            GstreamerSenderSession::for_test_session(pipeline, &ctx).expect("session");
        assert!(session.resume());
        let mut published = Vec::new();
        assert!(session.confirm_started(&mut |state| published.push(state)));
        assert_eq!(published, vec![PlayerState::Playing]);

        gate.stop();
        published.clear();
        assert!(!session.confirm_started(&mut |state| published.push(state)));
        assert!(published.is_empty(), "{published:?}");
        Box::new(session).close();
        assert_eq!(ticket.route_count(), 0);
    }

    /// Y1 regression double: a session whose `confirm_started` runs the
    /// caller's publication only while it is live. It models the OwnTone
    /// contract without a daemon, so the worker's routing can be driven
    /// directly.
    struct BoundarySession {
        live: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
        generation: PlayerEventGeneration,
    }

    impl SenderSession for BoundarySession {
        fn write_pcm(&mut self, samples: &[u8]) -> SenderWriteOutcome {
            SenderWriteOutcome::Accepted(samples.len())
        }
        fn set_volume(&mut self, _level: f64) -> bool {
            true
        }
        fn pause(&mut self) -> bool {
            true
        }
        fn resume(&mut self) -> bool {
            true
        }
        fn confirm_started(&self, publish: &mut dyn FnMut(PlayerState)) -> bool {
            if !self.live.load(Ordering::SeqCst) {
                return false;
            }
            publish(PlayerState::Playing);
            true
        }
        fn flush(&mut self) {}
        fn observe(&self) -> SenderPosition {
            SenderPosition::unknown(self.generation)
        }
        fn state(&self) -> PlayerState {
            PlayerState::Playing
        }
        fn close(self: Box<Self>) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    struct BoundarySender {
        live: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
    }

    impl AirplaySender for BoundarySender {
        fn name(&self) -> &'static str {
            "boundary"
        }
        fn probe(&self) -> Result<(), SenderError> {
            Ok(())
        }
        fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome {
            OpenOutcome::Opened(Box::new(BoundarySession {
                live: Arc::clone(&self.live),
                closed: Arc::clone(&self.closed),
                generation: ctx.generation,
            }))
        }
    }

    /// Drive the real `run_session_worker` for one `BoundarySession` and return
    /// the events it published, its final state cache, and whether the session
    /// was closed. A `Stop` command ends the control loop deterministically.
    fn drive_boundary_worker(
        live: bool,
    ) -> (Vec<PlayerEvent>, PlayerState, bool, PlayerEventGeneration) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("url"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");
        let generation = PlayerEventGeneration::from_raw(5);
        let cancel = OpenCancel::new();
        let registration = proxy.register_in_flight_cancel(77, prepared.generation(), &cancel);
        let (tx, rx) = async_channel::unbounded();

        let ctx = SenderOpenContext {
            target: SenderTarget::new("Test", "127.0.0.1", 7000, None),
            prepared_uri: prepared.uri().to_string(),
            event_tx: tx,
            generation,
            media_proxy: Arc::clone(&proxy),
            media_ticket: prepared.ticket(),
            volume: 1.0,
            cancel: cancel.clone(),
            session_gate: Arc::new(SessionGate::new()),
            open_id: 77,
        };
        let sender = Arc::new(BoundarySender {
            live: Arc::new(AtomicBool::new(live)),
            closed: Arc::new(AtomicBool::new(false)),
        });
        let closed = Arc::clone(&sender.closed);
        let state_cache = Arc::new(AtomicU8::new(PlayerState::Buffering as u8));
        let position_cache = Arc::new(Mutex::new(SenderPosition::unknown(generation)));
        let event_generation = Arc::new(AtomicU64::new(generation.as_raw()));
        let (commands, command_rx) = mpsc::channel();

        let worker_state = Arc::clone(&state_cache);
        let handle = std::thread::spawn(move || {
            run_session_worker(
                sender,
                ctx,
                registration,
                command_rx,
                worker_state,
                position_cache,
                event_generation,
            );
        });

        // Let the worker's start path run (or be skipped), then snapshot the
        // cache while the worker is still live; the `Stop` teardown overwrites
        // it with `Stopped` afterwards.
        let deadline = Instant::now() + Duration::from_secs(2);
        while state_cache.load(Ordering::SeqCst) == PlayerState::Buffering as u8
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let cache = state_from_u8(state_cache.load(Ordering::SeqCst));
        commands.send(SessionCommand::Stop).expect("stop");
        handle.join().expect("worker");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (events, cache, closed.load(Ordering::SeqCst), generation)
    }

    /// Y1: the worker's coarse start publication is routed *through* the session
    /// (`confirm_started`) under its terminal-ordering boundary, not published
    /// by the worker after the call returns. A live session's publication
    /// reaches the event channel and the output cache; a session that has
    /// already gone terminal publishes nothing, yet the worker still owns
    /// teardown.
    #[test]
    fn run_session_worker_publishes_the_start_through_the_session_boundary() {
        let (events, cache, closed, generation) = drive_boundary_worker(true);
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    generation: g,
                    state: PlayerState::Playing,
                } if *g == generation
            )),
            "the session's own boundary publication must reach the worker's channel: {events:?}"
        );
        assert_eq!(
            cache,
            PlayerState::Playing,
            "the worker cache must reflect the accepted start"
        );
        assert!(closed, "the worker owns session teardown");

        let (terminal_events, _, terminal_closed, _) = drive_boundary_worker(false);
        assert!(
            !terminal_events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing | PlayerState::Paused,
                    ..
                }
            )),
            "a terminal session must publish no Playing/Paused through the worker: {terminal_events:?}"
        );
        assert!(
            terminal_closed,
            "the worker must tear down a terminal session too"
        );
    }

    /// X2: a session orphaned by a superseding generation releases its route
    /// through the real `run_session_worker` stale-open path, not merely at the
    /// session level. The route must be released by identity (lease gone,
    /// custody empty, route shut down) with no start published.
    struct StaleSession {
        proxy: Arc<GstreamerMediaProxy>,
        ticket: Option<Arc<GstreamerMediaTicket>>,
    }

    impl SenderSession for StaleSession {
        fn write_pcm(&mut self, samples: &[u8]) -> SenderWriteOutcome {
            SenderWriteOutcome::Accepted(samples.len())
        }
        fn set_volume(&mut self, _level: f64) -> bool {
            true
        }
        fn pause(&mut self) -> bool {
            true
        }
        fn resume(&mut self) -> bool {
            true
        }
        fn confirm_started(&self, _publish: &mut dyn FnMut(PlayerState)) -> bool {
            false
        }
        fn flush(&mut self) {}
        fn observe(&self) -> SenderPosition {
            SenderPosition::unknown(PlayerEventGeneration::from_raw(0))
        }
        fn state(&self) -> PlayerState {
            PlayerState::Stopped
        }
        fn close(self: Box<Self>) {
            if let Some(ticket) = self.ticket.as_ref() {
                self.proxy.take_and_release(ticket);
            }
        }
    }

    struct StaleSender {
        proxy: Arc<GstreamerMediaProxy>,
    }

    impl AirplaySender for StaleSender {
        fn name(&self) -> &'static str {
            "stale"
        }
        fn probe(&self) -> Result<(), SenderError> {
            Ok(())
        }
        fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome {
            OpenOutcome::Opened(Box::new(StaleSession {
                proxy: Arc::clone(&self.proxy),
                ticket: ctx.media_ticket.clone(),
            }))
        }
    }

    #[test]
    fn a_stale_opened_session_releases_its_route_through_the_worker() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("url"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");
        let ticket = prepared.ticket().expect("protected media ticket");
        assert!(proxy.has_active_lease());
        assert_eq!(ticket.route_count(), 1);

        let generation = PlayerEventGeneration::from_raw(9);
        let cancel = OpenCancel::new();
        let registration = proxy.register_in_flight_cancel(91, prepared.generation(), &cancel);
        let (tx, _rx) = async_channel::unbounded();
        // The generation moved on before the worker ran, so the open is stale.
        let event_generation = Arc::new(AtomicU64::new(generation.as_raw() + 1));
        let ctx = SenderOpenContext {
            target: SenderTarget::new("Test", "127.0.0.1", 7000, None),
            prepared_uri: prepared.uri().to_string(),
            event_tx: tx,
            generation,
            media_proxy: Arc::clone(&proxy),
            media_ticket: prepared.ticket(),
            volume: 1.0,
            cancel: cancel.clone(),
            session_gate: Arc::new(SessionGate::new()),
            open_id: 91,
        };
        let sender = Arc::new(StaleSender {
            proxy: Arc::clone(&proxy),
        });
        let state_cache = Arc::new(AtomicU8::new(PlayerState::Buffering as u8));
        let position_cache = Arc::new(Mutex::new(SenderPosition::unknown(generation)));
        let (_commands, command_rx) = mpsc::channel();

        run_session_worker(
            sender,
            ctx,
            registration,
            command_rx,
            state_cache,
            position_cache,
            event_generation,
        );

        assert!(
            !proxy.has_active_lease() && !proxy.has_custody_entries(),
            "a stale opened session must release its route by identity"
        );
        assert_eq!(
            ticket.route_count(),
            0,
            "the released route must be shut down"
        );
    }

    /// A cancelled open is never reported as a user-facing failure.
    #[test]
    fn a_cancelled_open_publishes_no_error() {
        let (tx, rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        let generation = PlayerEventGeneration::from_raw(11);
        output.set_event_generation(generation);

        output.finish_load(generation, OpenOutcome::Cancelled);

        assert!(rx.try_recv().is_err());
        assert_eq!(output.state(), PlayerState::Stopped);
    }

    #[test]
    fn protected_load_fails_closed_before_any_pipeline_sees_the_secret() {
        const SECRET: &str = "airplay-secret-must-not-leak";

        gst::init().expect("GStreamer init");

        let (tx, rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        let generation = PlayerEventGeneration::from_raw(42);
        output.set_event_generation(generation);

        // The transmitter gate runs before any per-track proxy work, so on
        // a machine without `raopsink` the honest unavailable guidance wins.
        // With `raopsink` registered, preparation is reached next and must
        // fail — a protected URI requires the app runtime to mint its
        // loopback ticket, and none is configured. Either way the failure
        // is a fixed message and no pipeline is ever constructed around the
        // credential-bearing URI.
        let expected = if GstreamerRaopSender::raopsink_available() {
            "AirPlay media preparation failed".to_string()
        } else {
            GstreamerRaopSender::raopsink_missing_message(&rust_i18n::locale())
        };

        output.load_uri(&format!("https://music.test/stream?api_key={SECRET}"));

        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::StateChanged {
                generation: event_generation,
                state: PlayerState::Buffering,
            }) if event_generation == generation
        ));

        match rx.try_recv() {
            Ok(PlayerEvent::Error {
                generation: event_generation,
                message,
            }) => {
                assert_eq!(event_generation, generation);
                assert_eq!(message, expected);
                assert!(!message.contains(SECRET));
                assert!(!message.contains("api_key"));
                assert!(!message.contains("music.test"));
            }
            event => panic!("expected fixed load error, got {event:?}"),
        }

        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::StateChanged {
                generation: event_generation,
                state: PlayerState::Stopped,
            }) if event_generation == generation
        ));
        assert!(rx.try_recv().is_err());
    }

    // ----- Z1: GStreamer adapter start/Stop, failed-start and route cleanup -----

    /// The count of buffers the injected pipeline's sink has consumed, signalled
    /// on a condvar so a regression waits on the real effect rather than a
    /// sleep.
    type BufferCounter = Arc<(Mutex<usize>, std::sync::Condvar)>;

    /// Attach a buffer probe to the injected pipeline's `raop` sink pad, so
    /// consumption of that element (the one [`GstreamerSenderSession`] looks
    /// up) is observable without the unavailable production `raopsink`.
    fn attach_buffer_counter(pipeline: &gst::Pipeline) -> BufferCounter {
        let sink = pipeline.by_name("raop").expect("test sink");
        let counter: BufferCounter = Arc::new((Mutex::new(0usize), std::sync::Condvar::new()));
        let probe_counter = Arc::clone(&counter);
        sink.static_pad("sink").expect("test sink pad").add_probe(
            gst::PadProbeType::BUFFER,
            move |_, _| {
                let (lock, cvar) = &*probe_counter;
                *lock.lock().unwrap_or_else(|p| p.into_inner()) += 1;
                cvar.notify_all();
                gst::PadProbeReturn::Ok
            },
        );
        counter
    }

    /// Build the injected RAOP-shaped pipeline around a `fakesink` named
    /// `raop`, with a buffer probe on its sink pad so consumption is
    /// observable. A real source drives real buffers once the pipeline plays.
    fn test_pipeline_with_buffer_counter() -> (gst::Pipeline, BufferCounter) {
        let pipeline = gst::parse::launch(
            "audiotestsrc ! audioconvert ! audio/x-raw,format=S16LE,rate=44100,channels=2 ! fakesink name=raop sync=false",
        )
        .expect("parse test pipeline")
        .downcast::<gst::Pipeline>()
        .expect("test pipeline");
        let counter = attach_buffer_counter(&pipeline);
        (pipeline, counter)
    }

    /// Build an injected RAOP-shaped pipeline whose real transition genuinely
    /// fails: `filesrc` cannot open a missing file, so the element posts an
    /// error and `pipeline.set_state(Playing)` returns `StateChange::Failure`
    /// synchronously. This is a real GStreamer transition failure, not a
    /// test-only false return.
    fn failing_transition_pipeline() -> (gst::Pipeline, BufferCounter) {
        let directory = tempfile::tempdir().expect("tempdir for the missing source");
        let missing = directory.path().join("definitely-missing.flac");
        let pipeline = gst::parse::launch(&format!(
            "filesrc location=\"{}\" ! fakesink name=raop sync=false",
            missing.display()
        ))
        .expect("parse failing test pipeline")
        .downcast::<gst::Pipeline>()
        .expect("test pipeline");
        let counter = attach_buffer_counter(&pipeline);
        // `directory` is intentionally dropped here: the path must not exist.
        (pipeline, counter)
    }

    /// Build an injected pipeline whose real transition parks *inside*
    /// `filesrc`'s open of a named FIFO with no writer. `set_state(Playing)`
    /// therefore blocks at the real transition boundary until a writer opens
    /// the FIFO — a deterministic parking point for a start-vs-Stop
    /// interposition, using only real GStreamer behavior.
    #[cfg(unix)]
    fn parked_transition_pipeline(fifo: &std::path::Path) -> (gst::Pipeline, BufferCounter) {
        let status = std::process::Command::new("mkfifo")
            .arg(fifo)
            .status()
            .expect("invoke mkfifo");
        assert!(
            status.success(),
            "mkfifo must create the parked-source FIFO"
        );
        let pipeline = gst::parse::launch(&format!(
            "filesrc location=\"{}\" ! fakesink name=raop sync=false",
            fifo.display()
        ))
        .expect("parse parked test pipeline")
        .downcast::<gst::Pipeline>()
        .expect("test pipeline");
        let counter = attach_buffer_counter(&pipeline);
        (pipeline, counter)
    }

    /// Observes, synchronously and without a main loop, the bus messages a real
    /// pipeline transition emits, so a regression can prove a transition was
    /// actually *attempted* on the injected pipeline and observe its failure.
    /// The handler is observational only: every message is passed through
    /// unchanged, and the session's own bus watch (if its main context ever
    /// runs) still sees them.
    #[derive(Default)]
    struct TransitionRecorder {
        state_changes: Mutex<usize>,
        errors: Mutex<usize>,
    }

    impl TransitionRecorder {
        fn install(pipeline: &gst::Pipeline) -> Arc<Self> {
            let recorder = Arc::new(Self::default());
            let bus = pipeline.bus().expect("injected pipeline has a bus");
            let seen = Arc::clone(&recorder);
            bus.set_sync_handler(move |_, message| {
                match message.view() {
                    gst::MessageView::StateChanged(_) => {
                        *seen.state_changes.lock().unwrap_or_else(|p| p.into_inner()) += 1;
                    }
                    gst::MessageView::Error(_) => {
                        *seen.errors.lock().unwrap_or_else(|p| p.into_inner()) += 1;
                    }
                    _ => {}
                }
                gst::BusSyncReply::Pass
            });
            recorder
        }

        fn state_changes(&self) -> usize {
            *self.state_changes.lock().unwrap_or_else(|p| p.into_inner())
        }

        fn errors(&self) -> usize {
            *self.errors.lock().unwrap_or_else(|p| p.into_inner())
        }
    }

    /// A test sender that hands the real [`run_session_worker`] a real
    /// [`GstreamerSenderSession`] built around an injected pipeline, so the
    /// worker's real start/refusal/teardown routing is exercised against a
    /// pipeline whose production transition succeeds, parks, or fails.
    struct InjectedPipelineSender {
        pipeline: Mutex<Option<gst::Pipeline>>,
    }

    impl InjectedPipelineSender {
        fn new(pipeline: gst::Pipeline) -> Self {
            Self {
                pipeline: Mutex::new(Some(pipeline)),
            }
        }
    }

    impl AirplaySender for InjectedPipelineSender {
        fn name(&self) -> &'static str {
            "test-injected-pipeline"
        }

        fn probe(&self) -> Result<(), SenderError> {
            Ok(())
        }

        fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome {
            let pipeline = self
                .pipeline
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            match pipeline {
                Some(pipeline) => match GstreamerSenderSession::for_test_session(pipeline, ctx) {
                    Ok(session) => OpenOutcome::Opened(Box::new(session)),
                    Err(message) => OpenOutcome::Failed(SenderError::Receiver(message)),
                },
                None => OpenOutcome::Failed(SenderError::Receiver(
                    "no injected pipeline was prepared".to_string(),
                )),
            }
        }
    }

    /// Everything a worker-driven GStreamer regression needs: a real protected
    /// route/ticket, the worker's context, its in-flight registration, the
    /// worker-visible event receiver and the generation.
    struct WorkerGstreamerFixture {
        proxy: Arc<GstreamerMediaProxy>,
        ticket: Arc<GstreamerMediaTicket>,
        ctx: SenderOpenContext,
        registration: InFlightCancelRegistration,
        generation: PlayerEventGeneration,
        events: async_channel::Receiver<PlayerEvent>,
        // Kept alive so the prepared route stays valid for the whole fixture.
        _prepared: PreparedGstreamerMedia,
    }

    fn worker_gstreamer_fixture(
        generation: PlayerEventGeneration,
        gate: Arc<SessionGate>,
    ) -> WorkerGstreamerFixture {
        use crate::architecture::media::ResolvedHttpRequest;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("url"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");
        let ticket = prepared.ticket().expect("protected media ticket");
        let cancel = OpenCancel::new();
        let registration = proxy.register_in_flight_cancel(1, prepared.generation(), &cancel);
        let (tx, events) = async_channel::unbounded();
        let ctx = SenderOpenContext {
            target: SenderTarget::new("Test", "127.0.0.1", 7000, None),
            prepared_uri: prepared.uri().to_string(),
            event_tx: tx,
            generation,
            media_proxy: Arc::clone(&proxy),
            media_ticket: prepared.ticket(),
            volume: 1.0,
            cancel,
            session_gate: gate,
            open_id: 1,
        };
        WorkerGstreamerFixture {
            proxy,
            ticket,
            ctx,
            registration,
            generation,
            events,
            _prepared: prepared,
        }
    }

    /// Drain the events a worker published.
    fn drain_events(events: &async_channel::Receiver<PlayerEvent>) -> Vec<PlayerEvent> {
        let mut drained = Vec::new();
        while let Ok(event) = events.try_recv() {
            drained.push(event);
        }
        drained
    }

    /// Bound-poll the worker's own coarse cache until it reports `expected`.
    /// The pipeline transition itself is observed through its real effects
    /// (buffers, gate in-flight state, bus messages); this only waits for the
    /// worker thread's cache write to become visible.
    fn wait_for_cache(cache: &Arc<AtomicU8>, expected: PlayerState) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while state_from_u8(cache.load(Ordering::SeqCst)) != expected {
            assert!(
                Instant::now() < deadline,
                "the worker cache never reached {expected:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Wait until the shared gate reports an authorized start effect in-flight
    /// (running its real transition). Polls the gate's own drain condvar so it
    /// observes real arrival rather than sleeping as evidence.
    fn wait_for_start_effect_in_flight(gate: &SessionGate) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while gate.wait_effects_drained(Duration::from_millis(1)) {
            assert!(
                Instant::now() < deadline,
                "the start effect never entered the transition"
            );
        }
    }

    fn buffer_count(counter: &BufferCounter) -> usize {
        *counter.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn wait_for_buffers(counter: &BufferCounter, minimum: usize) {
        let (lock, cvar) = &**counter;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut count = lock.lock().unwrap_or_else(|p| p.into_inner());
        while *count < minimum {
            let now = Instant::now();
            assert!(
                now < deadline,
                "the pipeline consumed no buffers (count={})",
                *count
            );
            let (next, _) = cvar
                .wait_timeout(count, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            count = next;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn test_session_ctx(
        proxy: &Arc<GstreamerMediaProxy>,
        ticket: Option<Arc<GstreamerMediaTicket>>,
        event_tx: async_channel::Sender<PlayerEvent>,
        gate: Arc<SessionGate>,
        generation: PlayerEventGeneration,
    ) -> SenderOpenContext {
        SenderOpenContext {
            target: SenderTarget::new("Test", "127.0.0.1", 7000, None),
            prepared_uri: "http://127.0.0.1:1/media".to_string(),
            event_tx,
            generation,
            media_proxy: Arc::clone(proxy),
            media_ticket: ticket,
            volume: 1.0,
            cancel: OpenCancel::new(),
            session_gate: gate,
            open_id: 1,
        }
    }

    /// Z1: the real [`GstreamerSenderSession`] start/Stop interposition. A
    /// `resume` is authorized and starts the injected pipeline, whose sink
    /// observably consumes buffers; a `Stop` taken through the session's shared
    /// gate then refuses a later start, and `close` releases the protected
    /// route by identity. This drives the production session's own
    /// `resume`/`close` paths rather than the seam in isolation.
    #[test]
    fn a_gstreamer_session_start_consumes_buffers_and_close_releases_the_route() {
        gst::init().expect("GStreamer init");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("url"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");
        let ticket = prepared.ticket().expect("protected media ticket");
        assert!(proxy.has_active_lease());
        assert_eq!(ticket.route_count(), 1);

        let (tx, _rx) = async_channel::unbounded();
        let gate = Arc::new(SessionGate::new());
        let generation = PlayerEventGeneration::from_raw(11);
        let ctx = test_session_ctx(
            &proxy,
            Some(Arc::clone(&ticket)),
            tx,
            Arc::clone(&gate),
            generation,
        );

        let (pipeline, counter) = test_pipeline_with_buffer_counter();
        let session = GstreamerSenderSession::for_test_session(pipeline, &ctx).expect("session");
        let mut session = Box::new(session);

        assert!(
            session.resume(),
            "the authorized start must run the injected pipeline"
        );
        wait_for_buffers(&counter, 1);
        assert!(
            buffer_count(&counter) >= 1,
            "the started pipeline must consume buffers"
        );

        // The production close takes the shared Stop boundary and tears the
        // pipeline down, releasing the protected route by identity.
        session.close();
        assert!(
            !proxy.has_active_lease() && !proxy.has_custody_entries(),
            "close must release the session's route by identity"
        );
        assert_eq!(
            ticket.route_count(),
            0,
            "the closed session's route must be shut down"
        );
    }

    /// Z1: a Stop taken before any start refuses the start effect. The real
    /// session's `resume` reports no start, the injected pipeline never plays
    /// (no buffers consumed, cached state stopped) and nothing is published.
    #[test]
    fn a_stop_before_start_refuses_the_gstreamer_start_and_consumes_no_buffers() {
        gst::init().expect("GStreamer init");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let proxy = Arc::new(GstreamerMediaProxy::new(Some(runtime.handle().clone())));
        let request = ResolvedHttpRequest::new(
            url::Url::parse("https://music.test/stream.flac").expect("url"),
        )
        .expect("resolved request");
        let prepared = proxy.prepare_resolved(request).expect("prepared media");
        let ticket = prepared.ticket().expect("protected media ticket");

        let (tx, rx) = async_channel::unbounded();
        let gate = Arc::new(SessionGate::new());
        let generation = PlayerEventGeneration::from_raw(13);
        let ctx = test_session_ctx(
            &proxy,
            Some(Arc::clone(&ticket)),
            tx,
            Arc::clone(&gate),
            generation,
        );

        let (pipeline, counter) = test_pipeline_with_buffer_counter();
        let session = GstreamerSenderSession::for_test_session(pipeline, &ctx).expect("session");
        let mut session = Box::new(session);

        // The Stop wins the boundary before any start is authorized.
        gate.stop();
        assert!(
            !session.resume(),
            "a start after Stop must be refused (no start after Stop)"
        );
        assert_ne!(
            session.state(),
            PlayerState::Playing,
            "the pipeline must never report playing after a refused start"
        );
        assert_eq!(
            buffer_count(&counter),
            0,
            "no PCM may be consumed after a refused start"
        );
        assert!(
            !rx_has_playing(&rx),
            "a refused start must not publish Playing"
        );

        session.close();
        assert!(
            !proxy.has_active_lease() && !proxy.has_custody_entries(),
            "close must release the session's route by identity"
        );
        assert_eq!(ticket.route_count(), 0);
    }

    /// Z1: a **real** failed GStreamer state transition, driven through the
    /// real [`run_session_worker`]. The injected pipeline's `resume` genuinely
    /// calls `pipeline.set_state(Playing)`; `filesrc` cannot open its source, so
    /// that transition fails. The bus proves the transition was attempted
    /// (state-changed) and that it failed (error); the worker reports `Stopped`,
    /// no PCM or `Playing` is ever produced, and the protected route is released
    /// by identity. The previous fixture substituted a `false` return for the
    /// transition; this exercises the production effect end to end.
    #[test]
    fn a_real_failed_gstreamer_transition_releases_the_route_without_pcm_or_playing() {
        gst::init().expect("GStreamer init");

        let generation = PlayerEventGeneration::from_raw(12);
        let gate = Arc::new(SessionGate::new());
        let WorkerGstreamerFixture {
            proxy,
            ticket,
            ctx,
            registration,
            generation,
            events,
            _prepared,
        } = worker_gstreamer_fixture(generation, gate);
        assert!(proxy.has_active_lease());
        assert_eq!(ticket.route_count(), 1);

        let (pipeline, counter) = failing_transition_pipeline();
        let recorder = TransitionRecorder::install(&pipeline);
        let sender = Arc::new(InjectedPipelineSender::new(pipeline));

        let state_cache = Arc::new(AtomicU8::new(PlayerState::Buffering as u8));
        let position_cache = Arc::new(Mutex::new(SenderPosition::unknown(generation)));
        let event_generation = Arc::new(AtomicU64::new(generation.as_raw()));
        let (_commands, command_rx) = mpsc::channel();

        run_session_worker(
            sender,
            ctx,
            registration,
            command_rx,
            Arc::clone(&state_cache),
            position_cache,
            event_generation,
        );

        assert!(
            recorder.state_changes() > 0,
            "the real start must be attempted on the injected pipeline"
        );
        assert!(
            recorder.errors() > 0,
            "the attempted transition must post a real failure"
        );
        assert_eq!(
            buffer_count(&counter),
            0,
            "no PCM may be consumed after a failed start"
        );
        assert_eq!(
            state_from_u8(state_cache.load(Ordering::SeqCst)),
            PlayerState::Stopped,
            "the worker must report the refused start, not a live one"
        );

        let events = drain_events(&events);
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    generation: g,
                    state: PlayerState::Stopped,
                } if *g == generation
            )),
            "the failed start must publish Stopped: {events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing | PlayerState::Paused,
                    ..
                }
            )),
            "a failed start must not publish Playing/Paused: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PlayerEvent::TrackEnded { .. })),
            "a failed start must not publish completion: {events:?}"
        );
        assert!(
            !proxy.has_active_lease() && !proxy.has_custody_entries(),
            "a failed start must release the protected route by identity"
        );
        assert_eq!(
            ticket.route_count(),
            0,
            "the released route must be shut down"
        );
    }

    /// Z1: start wins the real transition, driven through the worker with the
    /// production `GstreamerSenderSession`. The authorized start runs the
    /// injected pipeline, whose sink observably consumes buffers, the worker
    /// publishes `Playing`, and the worker's own teardown releases the protected
    /// route by identity.
    #[test]
    fn a_worker_start_through_the_real_gstreamer_adapter_consumes_buffers() {
        gst::init().expect("GStreamer init");

        let generation = PlayerEventGeneration::from_raw(21);
        let gate = Arc::new(SessionGate::new());
        let WorkerGstreamerFixture {
            proxy,
            ticket,
            ctx,
            registration,
            generation,
            events,
            _prepared,
        } = worker_gstreamer_fixture(generation, Arc::clone(&gate));

        let (pipeline, counter) = test_pipeline_with_buffer_counter();
        let sender = Arc::new(InjectedPipelineSender::new(pipeline));
        let state_cache = Arc::new(AtomicU8::new(PlayerState::Buffering as u8));
        let position_cache = Arc::new(Mutex::new(SenderPosition::unknown(generation)));
        let event_generation = Arc::new(AtomicU64::new(generation.as_raw()));
        let mut worker = spawn_test_session_worker(
            sender,
            ctx,
            registration,
            Arc::clone(&state_cache),
            position_cache,
            event_generation,
        );

        // The pipeline's sink consuming buffers is the real accepted-start
        // effect; the cache write is only waited for, never used as evidence.
        wait_for_buffers(&counter, 1);
        wait_for_cache(&state_cache, PlayerState::Playing);
        assert!(
            buffer_count(&counter) >= 1,
            "the started pipeline must consume buffers"
        );

        worker.stop_and_join();
        let events = drain_events(&events);
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    generation: g,
                    state: PlayerState::Playing,
                } if *g == generation
            )),
            "the accepted start must publish Playing: {events:?}"
        );
        assert!(
            !proxy.has_active_lease() && !proxy.has_custody_entries(),
            "closing the worker must release the session's route by identity"
        );
        assert_eq!(ticket.route_count(), 0);
    }

    /// Z1: Stop wins the real transition. `set_state(Playing)` is parked inside
    /// `filesrc`'s open of a writer-less FIFO — the actual transition boundary —
    /// the gate observes the authorized effect in-flight, and a Stop then takes
    /// the boundary. Releasing the parked transition lets the effect return a
    /// genuinely started pipeline, which the gate suppresses because the Stop
    /// won: the worker reports `Stopped`, consumes no PCM, publishes no
    /// `Playing`, and releases the route by identity.
    #[cfg(unix)]
    #[test]
    fn a_stop_interposed_at_the_real_gstreamer_transition_refuses_the_start() {
        gst::init().expect("GStreamer init");

        let directory = tempfile::tempdir().expect("tempdir");
        let fifo = directory.path().join("parked-source.fifo");
        let generation = PlayerEventGeneration::from_raw(22);
        let gate = Arc::new(SessionGate::new());
        let WorkerGstreamerFixture {
            proxy,
            ticket,
            ctx,
            registration,
            generation,
            events,
            _prepared,
        } = worker_gstreamer_fixture(generation, Arc::clone(&gate));

        let (pipeline, counter) = parked_transition_pipeline(&fifo);
        let recorder = TransitionRecorder::install(&pipeline);
        let sender = Arc::new(InjectedPipelineSender::new(pipeline));
        let state_cache = Arc::new(AtomicU8::new(PlayerState::Buffering as u8));
        let position_cache = Arc::new(Mutex::new(SenderPosition::unknown(generation)));
        let event_generation = Arc::new(AtomicU64::new(generation.as_raw()));
        let mut worker = spawn_test_session_worker(
            sender,
            ctx,
            registration,
            Arc::clone(&state_cache),
            position_cache,
            event_generation,
        );

        // The real `set_state(Playing)` is now parked inside the FIFO open; the
        // gate reports the authorized effect in-flight.
        wait_for_start_effect_in_flight(&gate);

        // Stop wins the boundary while the effect is parked in the transition.
        gate.stop();
        // Release the parked transition: `filesrc` opens the FIFO, the pipeline
        // really starts, and the gate suppresses the accepted result.
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo)
            .expect("open the parked FIFO's writer");
        drop(writer);

        wait_for_cache(&state_cache, PlayerState::Stopped);
        worker.stop_and_join();

        assert!(
            recorder.state_changes() > 0,
            "the start must have attempted the real transition"
        );
        assert_eq!(
            buffer_count(&counter),
            0,
            "no PCM may be consumed after a Stop won the boundary"
        );
        let events = drain_events(&events);
        assert!(
            !events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing | PlayerState::Paused,
                    ..
                }
            )),
            "a suppressed start must publish no Playing/Paused: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayerEvent::StateChanged {
                    generation: g,
                    state: PlayerState::Stopped,
                } if *g == generation
            )),
            "the suppressed start must publish Stopped: {events:?}"
        );
        assert!(
            !proxy.has_active_lease() && !proxy.has_custody_entries(),
            "the refused start must release the route by identity"
        );
        assert_eq!(ticket.route_count(), 0);
    }

    fn rx_has_playing(rx: &async_channel::Receiver<PlayerEvent>) -> bool {
        let mut saw_playing = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                PlayerEvent::StateChanged {
                    state: PlayerState::Playing,
                    ..
                }
            ) {
                saw_playing = true;
            }
        }
        saw_playing
    }
}
