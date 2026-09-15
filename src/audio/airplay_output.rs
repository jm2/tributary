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
    SenderPosition, SenderSession, SenderTarget, SenderWriteOutcome,
};
use super::gstreamer_media::{GstreamerMediaProxy, GstreamerMediaTicket, PreparedGstreamerMedia};
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
}

impl SenderSession for GstreamerSenderSession {
    fn write_pcm(&mut self, samples: &[u8]) -> SenderWriteOutcome {
        // This session owns its decode pipeline and never consumes pushed
        // audio. Report the buffer accepted so a pump would not stall.
        SenderWriteOutcome::Accepted(samples.len())
    }

    fn set_volume(&mut self, level: f64) {
        if let Some(sink) = self.pipeline.by_name("raop") {
            sink.set_property("volume", AirPlayOutput::volume_to_db(level));
        }
    }

    fn pause(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Paused);
    }

    fn resume(&mut self) -> bool {
        self.pipeline.set_state(gst::State::Playing).is_ok()
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
        // fetch error while GStreamer is still winding down.
        let this = *self;
        let _ = this.pipeline.set_state(gst::State::Null);
        if let Some(ticket) = this.media_ticket.as_ref() {
            this.media_proxy.revoke_if_current(ticket);
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

        OpenOutcome::Opened(Box::new(GstreamerSenderSession {
            pipeline,
            media_ticket: ctx.media_ticket.clone(),
            _bus_watch: bus_watch,
            generation: ctx.generation,
            media_proxy: Arc::clone(&ctx.media_proxy),
        }))
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
                    media_proxy.revoke_if_current(ticket);
                }
                let _ = tx.try_send(PlayerEvent::ended(generation));
            }
            MessageView::Error(pipeline_error) => {
                if let Some(ticket) = media_ticket.as_ref() {
                    media_proxy.revoke_if_current(ticket);
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
        // Retain a handle for the failure path; the context owns its own clone.
        let failure_ticket = prepared.ticket();
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
                Err(message) => self.report_load_failure(generation, &message),
            },
            Err(error) => self.report_load_failure(generation, error.message()),
        }
        true
    }

    /// Tear down the active load without blocking the caller: signal the
    /// worker, then detach it so teardown (a blocking daemon RPC or a pump
    /// join) runs on the worker, not the UI thread (review F2).
    fn close_session(&self) {
        let controller = self.controller_guard().take();
        if let Some(mut controller) = controller {
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

/// Worker for one load: registers its cancellation under the load key, runs
/// the blocking `open_session` off the UI thread, then owns the live session
/// and services control commands. Publishes coarse state/position into caches
/// the UI reads without touching the session (review F2).
fn run_session_worker(
    sender: Arc<dyn AirplaySender>,
    ctx: SenderOpenContext,
    commands: std::sync::mpsc::Receiver<SessionCommand>,
    state_cache: Arc<AtomicU8>,
    position_cache: Arc<Mutex<SenderPosition>>,
    event_generation: Arc<AtomicU64>,
) {
    let proxy = Arc::clone(&ctx.media_proxy);
    proxy.begin_open(ctx.open_id);
    let registration = proxy.register_in_flight_cancel(ctx.open_id, &ctx.cancel);
    if registration.is_superseded() {
        // Lost the prepare-to-open race: no negotiation and no events. The
        // load path releases the ticket.
        release_ticket(&proxy, &ctx);
        return;
    }

    let generation = ctx.generation;
    let outcome = sender.open_session(&ctx);
    // Registration covers only the open call itself; it must not outlive it.
    drop(registration);

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
            state_cache.store(PlayerState::Playing as u8, Ordering::SeqCst);
            let _ = ctx
                .event_tx
                .try_send(PlayerEvent::state(generation, PlayerState::Playing));
            loop {
                state_cache.store(session.state() as u8, Ordering::SeqCst);
                *position_cache.lock().unwrap_or_else(|p| p.into_inner()) = session.observe();
                if ctx.cancel.is_cancelled() {
                    break;
                }
                match commands.recv_timeout(Duration::from_millis(200)) {
                    Ok(SessionCommand::Pause) => session.pause(),
                    Ok(SessionCommand::Resume) => {
                        let _ = session.resume();
                    }
                    Ok(SessionCommand::SetVolume(level)) => session.set_volume(level),
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
                if event_generation.load(Ordering::SeqCst) == generation.as_raw() {
                    let _ = ctx
                        .event_tx
                        .try_send(PlayerEvent::error(generation, error.message()));
                    let _ = ctx
                        .event_tx
                        .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
                }
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
                if event_generation.load(Ordering::SeqCst) == generation.as_raw() {
                    let _ = ctx
                        .event_tx
                        .try_send(PlayerEvent::error(generation, error.message()));
                    let _ = ctx
                        .event_tx
                        .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
                }
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

    /// R1: the availability gate runs on the GTK caller, so a dedicated
    /// daemon that accepts the connection and then never answers must not
    /// freeze the load path. The blocking handshake lives on the load worker;
    /// `load_uri` and a following `stop` both return promptly against a
    /// stalled endpoint.
    ///
    /// Unix-only: the fixture builds the dedicated-instance adapter, which
    /// owns a FIFO, an advisory `flock` and a `/proc`-verified process
    /// binding. On other targets the unsupported shim is compiled instead and
    /// this test would not link (review S1).
    #[cfg(unix)]
    #[test]
    fn a_stalled_owntone_endpoint_does_not_block_load_or_stop() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled endpoint");
        let addr = listener.local_addr().expect("local addr");
        let api_base = format!("http://{addr}");
        // Accept the connection and hold it open without ever answering: any
        // blocking probe on this endpoint would sit until its timeout.
        std::thread::spawn(move || {
            if listener.accept().is_ok() {
                std::thread::sleep(Duration::from_secs(10));
            }
        });

        let directory = tempfile::tempdir().expect("tempdir");
        let binary = directory.path().join("owntone");
        std::fs::write(&binary, b"#!/bin/true\n").expect("dummy binary");
        let sender =
            crate::audio::airplay_owntone::test_owned_sender(&api_base, directory.path(), &binary);

        let (tx, _rx) = async_channel::unbounded();
        let mut output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        output.sender = Arc::new(sender);
        let generation = PlayerEventGeneration::from_raw(64);
        output.set_event_generation(generation);

        let started = Instant::now();
        output.load_uri("http://127.0.0.1:1/media");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "load blocked on the stalled OwnTone endpoint: {:?}",
            started.elapsed()
        );

        let stopped = Instant::now();
        output.stop();
        assert!(
            stopped.elapsed() < Duration::from_millis(500),
            "stop blocked on the stalled OwnTone endpoint: {:?}",
            stopped.elapsed()
        );
    }

    /// S1: on a target with no documented OwnTone acquisition path, the
    /// unsupported shim is compiled instead of the Unix adapter. Selection is
    /// still recognized and the sender still refuses explicitly, so a load can
    /// never be misreported as an OwnTone session — and the Unix-only stalled
    /// endpoint regression above is never compiled here.
    #[cfg(not(unix))]
    #[test]
    fn an_unsupported_platform_owntone_sender_refuses_the_load() {
        let sender = crate::audio::airplay_owntone::OwnToneSender::from_env();
        let error = sender
            .probe()
            .expect_err("the unsupported shim must refuse every load");
        assert!(error.message().contains("OwnTone"), "{}", error.message());
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
}
