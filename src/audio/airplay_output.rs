//! AirPlay 1 (RAOP) audio output.
//!
//! Streams audio to legacy AirPlay (RAOP) receivers discovered via
//! `_raop._tcp.local.` mDNS browsing.  Discovered devices surface as
//! `DiscoveryEvent::Found` with `service_type: "airplay"` and appear
//! automatically in the output selector popover.
//!
//! # Implementation strategy
//!
//! We build a dedicated GStreamer pipeline per session and operate it
//! independently of the main `playbin3`:
//! `uridecodebin ! audioconvert ! avenc_alac ! raopsink`.
//!
//! `raopsink` is the only transmitter.  There is deliberately no
//! fallback: the one this module used to have piped decoded PCM into a
//! spawned `shairport-sync`, which is an AirPlay *receiver* — it
//! ignored the device the user selected and could never reach it
//! (review finding M3, tracker item P2.9).  A missing `raopsink` now
//! fails the load with a localized error naming the package to
//! install instead of silently spawning a subprocess that cannot work.
//!
//! A bus watch on the dedicated pipeline forwards EOS / errors / state
//! changes into the same `PlayerEvent` channel the rest of the app
//! consumes, so the header bar reflects what's actually happening on
//! the receiver instead of optimistic guesses.
//!
//! # Scope
//!
//! - **AirPlay 1 (RAOP) only.**  AirPlay 2 receivers (HomePod, recent
//!   Apple TVs, AirPlay-2-certified speakers) advertise via
//!   `_airplay._tcp.local.` and speak a different protocol stack that
//!   this output does not implement.  Such devices are filtered out
//!   of the output selector by [`crate::ui::discovery_handler`] until
//!   a sender-side AirPlay 2 implementation lands.
//! - **Requires a working `raopsink` element.**  It ships in the
//!   GStreamer "bad" plugins set (`gst-plugins-bad`); the exact
//!   package name varies by distro.  The code probes the GStreamer
//!   registry at runtime and surfaces an actionable error when the
//!   element is absent.
//! - **Seeking is not supported** for live RAOP streams.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::gstreamer_media::{GstreamerMediaProxy, GstreamerMediaTicket, PreparedGstreamerMedia};
use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerEventGeneration, PlayerState};

use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use tracing::{debug, error, info, warn};

use crate::architecture::media::ResolvedHttpRequest;
use crate::local::resolver::ResolvedLocalMedia;

/// Active AirPlay session: the dedicated GStreamer RAOP pipeline.
struct Session {
    pipeline: gst::Pipeline,
    /// Exact protected-media ticket owned by this session. Credential-free
    /// media has no ticket and retains its existing direct-URI behavior.
    media_ticket: Option<Arc<GstreamerMediaTicket>>,
    /// Bus watch guard — dropping it removes the watch.
    _bus_watch: gst::bus::BusWatchGuard,
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
    /// Event sender for relaying state changes to the GTK main thread.
    event_tx: async_channel::Sender<PlayerEvent>,
    event_generation: AtomicU64,
    /// Cached volume level (0.0–1.0).
    volume: f64,
    /// App-owned exact-origin fetch boundary for authenticated media. The
    /// GStreamer pipelines receive only its opaque loopback ticket.
    media_proxy: Arc<GstreamerMediaProxy>,
    /// Active session, if any.  `Mutex` (not `RefCell`) because the
    /// bus watch may run on a worker thread.
    session: Arc<Mutex<Option<Session>>>,
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
            event_tx,
            event_generation: AtomicU64::new(0),
            // Seed from the current slider value so switching to this device
            // doesn't reset the effective volume to maximum (0 dB) on the
            // first track load.
            volume: initial_volume.clamp(0.0, 1.0),
            media_proxy: Arc::new(GstreamerMediaProxy::new(None)),
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// Supply the application runtime used to host exact-route media tickets.
    #[must_use]
    pub fn with_runtime(self, handle: tokio::runtime::Handle) -> Self {
        self.media_proxy.set_runtime(handle);
        self
    }

    /// Lock the session, recovering transparently from poisoning.
    ///
    /// A poisoned `Mutex` here means a previous holder panicked. The bus
    /// watch runs on the GLib main loop, so a panic in any of its
    /// branches (or in `close_session`) would otherwise propagate as an
    /// app-wide crash on the next lock — even though we don't actually
    /// rely on any invariant the panicking thread might have left
    /// half-built. `into_inner()` returns the underlying value either
    /// way, which is the behaviour we want.
    fn session_lock(&self) -> std::sync::MutexGuard<'_, Option<Session>> {
        self.session.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn event_generation(&self) -> PlayerEventGeneration {
        PlayerEventGeneration::from_raw(self.event_generation.load(Ordering::SeqCst))
    }

    /// Linear 0.0–1.0 volume → RAOP dB scale (-30.0 = quiet, 0.0 = max).
    fn volume_to_db(linear: f64) -> f64 {
        if linear <= 0.0 {
            -144.0
        } else {
            (linear - 1.0) * 30.0
        }
    }

    /// Build a fresh session for `uri`, replacing any existing one.
    fn open_session(&self, uri: &str) -> Result<(), String> {
        // Tear down any previous session before starting a new one.
        self.close_session();

        // Refuse a missing transmitter before any per-track proxy work:
        // otherwise a protected load would start a loopback route and mint
        // a ticket only to revoke it, and a preparation failure would mask
        // the actionable install guidance with its generic message.
        Self::ensure_raopsink(Self::raopsink_available())?;

        // Prepare exactly once so the pipeline receives the credential-safe
        // URI and owns the ticket.
        let prepared = self
            .media_proxy
            .prepare(uri)
            .map_err(|_| "AirPlay media preparation failed".to_string())?;
        self.open_prepared_media(prepared)
    }

    fn open_resolved_session(&self, request: ResolvedHttpRequest) -> Result<(), String> {
        self.close_session();
        // Same order as `open_session`: transmitter first, proxy work second.
        Self::ensure_raopsink(Self::raopsink_available())?;
        let prepared = self
            .media_proxy
            .prepare_resolved(request)
            .map_err(|_| "AirPlay media preparation failed".to_string())?;
        self.open_prepared_media(prepared)
    }

    fn open_local_session(&self, media: ResolvedLocalMedia) -> Result<(), String> {
        self.close_session();
        Self::ensure_raopsink(Self::raopsink_available())?;
        let prepared = self
            .media_proxy
            .prepare_local(media)
            .map_err(|_| "AirPlay media preparation failed".to_string())?;
        self.open_prepared_media(prepared)
    }

    fn open_prepared_media(&self, prepared: PreparedGstreamerMedia) -> Result<(), String> {
        let media_ticket = prepared.ticket();
        let result = self.open_prepared_session(prepared.uri(), media_ticket.clone());
        if result.is_err() {
            if let Some(ticket) = media_ticket.as_ref() {
                self.media_proxy.revoke_if_current(ticket);
            }
        }
        result
    }

    fn finish_load(&self, generation: PlayerEventGeneration, result: Result<(), String>) {
        if let Err(e) = result {
            error!(error = %e, "AirPlay: failed to open session");
            let _ = self.event_tx.try_send(PlayerEvent::error(generation, e));
            let _ = self
                .event_tx
                .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
        } else if !self.set_pipeline_state(gst::State::Playing) {
            // `open_session` only prerolls the pipeline to Paused; like every
            // other output, a load must actually start playback.
            self.close_session();
            let _ = self.event_tx.try_send(PlayerEvent::error(
                generation,
                "AirPlay playback failed to start",
            ));
            let _ = self
                .event_tx
                .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
        }
    }

    fn open_prepared_session(
        &self,
        prepared_uri: &str,
        media_ticket: Option<Arc<GstreamerMediaTicket>>,
    ) -> Result<(), String> {
        let host = &self.host;
        let port = self.port;
        let volume = self.volume;
        let generation = self.event_generation();

        let pipeline = Self::build_raop_pipeline(host, port, prepared_uri, volume)?;
        let bus_watch = match self.attach_bus_watch(&pipeline, generation, media_ticket.clone()) {
            Ok(watch) => watch,
            Err(failure) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(failure);
            }
        };
        let session = Session {
            pipeline,
            media_ticket,
            _bus_watch: bus_watch,
        };
        if session.pipeline.set_state(gst::State::Paused).is_err() {
            let _ = session.pipeline.set_state(gst::State::Null);
            return Err("RAOP pipeline preroll failed".to_string());
        }
        info!(host = %host, port, "AirPlay: session opened via raopsink");
        *self.session_lock() = Some(session);
        Ok(())
    }

    /// True when GStreamer's registry has a `raopsink` element to transmit
    /// with. Requires an initialised GStreamer.
    fn raopsink_available() -> bool {
        gst::Registry::get()
            .find_feature("raopsink", gst::ElementFactory::static_type())
            .is_some()
    }

    /// Gate every load on the transmitter actually existing.
    ///
    /// There is deliberately no fallback here: the one this module used to
    /// have piped PCM into `shairport-sync`, an AirPlay *receiver*, which
    /// ignored the device the user selected and could never reach it. A
    /// missing `raopsink` is a hard error that tells the user what to
    /// install instead.
    fn ensure_raopsink(available: bool) -> Result<(), String> {
        if available {
            Ok(())
        } else {
            Err(Self::raopsink_missing_message(&rust_i18n::locale()))
        }
    }

    /// Localized "install gst-plugins-bad" guidance. `raopsink` and
    /// `gst-plugins-bad` are technical identifiers and stay untranslated
    /// inside every catalog entry.
    fn raopsink_missing_message(locale: &str) -> String {
        rust_i18n::t!("errors.playback.airplay_raopsink_missing", locale = locale).into_owned()
    }

    /// Tear down the active session — pipeline → Null.
    fn close_session(&self) {
        let mut guard = self.session_lock();
        if let Some(sess) = guard.take() {
            // Stop the pipeline before invalidating its loopback route. Doing
            // this in the opposite order can turn an intentional close into
            // a transient fetch error while GStreamer is still winding down.
            let _ = sess.pipeline.set_state(gst::State::Null);
            if let Some(ticket) = sess.media_ticket.as_ref() {
                self.media_proxy.revoke_if_current(ticket);
            }
        }
    }

    /// Construct an attached bus watch that forwards EOS / Error / state
    /// changes to the shared `PlayerEvent` channel.
    fn attach_bus_watch(
        &self,
        pipeline: &gst::Pipeline,
        generation: PlayerEventGeneration,
        media_ticket: Option<Arc<GstreamerMediaTicket>>,
    ) -> Result<gst::bus::BusWatchGuard, String> {
        let bus = pipeline
            .bus()
            .ok_or_else(|| "Pipeline has no bus".to_string())?;
        let tx = self.event_tx.clone();
        let pipeline_weak = pipeline.downgrade();
        let media_proxy = Arc::clone(&self.media_proxy);
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

    /// Build a pipeline using GStreamer's `raopsink`. The caller has
    /// already verified via [`Self::ensure_raopsink`] that the element
    /// is registered.
    fn build_raop_pipeline(
        host: &str,
        port: u16,
        uri: &str,
        volume: f64,
    ) -> Result<gst::Pipeline, String> {
        let pipeline_str = format!(
            "uridecodebin name=decoder uri=\"{}\" ! audioconvert ! avenc_alac ! raopsink name=raop host={} port={}",
            uri.replace('"', "\\\""),
            host,
            port,
        );

        let element = gst::parse::launch(&pipeline_str)
            .map_err(|_| "Failed to build RAOP pipeline".to_string())?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| "RAOP launch did not yield a Pipeline".to_string())?;

        if let Some(sink) = pipeline.by_name("raop") {
            sink.set_property("volume", Self::volume_to_db(volume));
        }
        let decoder = pipeline
            .by_name("decoder")
            .ok_or_else(|| "RAOP pipeline has no URI decoder".to_string())?;
        super::Player::install_loopback_http_source_policy(&decoder);

        Ok(pipeline)
    }

    /// Apply a state transition to the active pipeline, if any.
    fn set_pipeline_state(&self, target: gst::State) -> bool {
        let guard = self.session_lock();
        if let Some(ref sess) = *guard {
            if let Err(e) = sess.pipeline.set_state(target) {
                warn!(error = %e, ?target, "AirPlay: state transition failed");
                return false;
            }
            true
        } else {
            debug!(?target, "AirPlay: no active session for state change");
            false
        }
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

        self.finish_load(generation, self.open_session(uri));
        true
    }

    fn load_resolved(&self, request: ResolvedHttpRequest) -> bool {
        info!("AirPlay: loading resolved media");
        let generation = self.event_generation();
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering));
        self.finish_load(generation, self.open_resolved_session(request));
        true
    }

    fn load_local(&self, media: ResolvedLocalMedia) -> bool {
        info!("AirPlay: loading authorized local media");
        let generation = self.event_generation();
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering));
        self.finish_load(generation, self.open_local_session(media));
        true
    }

    fn set_event_generation(&self, generation: PlayerEventGeneration) {
        self.event_generation
            .store(generation.as_raw(), Ordering::SeqCst);
    }

    fn play(&self) {
        debug!("AirPlay: play");
        let _ = self.set_pipeline_state(gst::State::Playing);
    }

    fn pause(&self) {
        debug!("AirPlay: pause");
        let _ = self.set_pipeline_state(gst::State::Paused);
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
            let guard = self.session_lock();
            guard.as_ref().and_then(|sess| {
                let (_, current, _) = sess.pipeline.state(Some(gst::ClockTime::ZERO));
                match current {
                    gst::State::Playing => Some(gst::State::Paused),
                    gst::State::Paused | gst::State::Ready => Some(gst::State::Playing),
                    _ => None,
                }
            })
        };
        if let Some(state) = target {
            let _ = self.set_pipeline_state(state);
        }
    }

    fn seek_to(&self, _position_ms: u64) {
        // Live RAOP streams don't expose seekable timelines; skip.
        debug!("AirPlay: seek not supported on live streams");
    }

    fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
        let guard = self.session_lock();
        if let Some(ref sess) = *guard {
            if let Some(sink) = sess.pipeline.by_name("raop") {
                sink.set_property("volume", Self::volume_to_db(self.volume));
            }
        }
    }

    fn volume(&self) -> f64 {
        self.volume
    }

    fn state(&self) -> PlayerState {
        let guard = self.session_lock();
        guard.as_ref().map_or(PlayerState::Stopped, |sess| {
            let (_, current, _) = sess.pipeline.state(Some(gst::ClockTime::ZERO));
            match current {
                gst::State::Playing => PlayerState::Playing,
                gst::State::Paused => PlayerState::Paused,
                _ => PlayerState::Stopped,
            }
        })
    }

    fn position_ms(&self) -> Option<u64> {
        let guard = self.session_lock();
        let sess = guard.as_ref()?;
        sess.pipeline
            .query_position::<gst::ClockTime>()
            .map(|t| t.mseconds())
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

    /// P2.9's core guarantee: a missing `raopsink` is refused with guidance
    /// naming both the element and the package that provides it — never
    /// routed to a fallback that cannot transmit.
    #[test]
    fn a_missing_raopsink_is_refused_with_install_guidance() {
        assert!(AirPlayOutput::ensure_raopsink(true).is_ok());

        let error = AirPlayOutput::ensure_raopsink(false).unwrap_err();
        assert!(error.contains("raopsink"), "{error}");
        assert!(error.contains("gst-plugins-bad"), "{error}");
    }

    /// The guidance must be real in every catalog — present, mentioning the
    /// exact technical identifiers, and not silently falling back to English.
    #[test]
    fn raopsink_guidance_is_localized_for_every_catalog() {
        let english = AirPlayOutput::raopsink_missing_message("en");
        assert!(!english.is_empty());

        for locale in rust_i18n::available_locales!() {
            let localized = AirPlayOutput::raopsink_missing_message(&locale);
            assert!(localized.contains("raopsink"), "{locale}: {localized}");
            assert!(
                localized.contains("gst-plugins-bad"),
                "{locale}: {localized}"
            );
            if locale != "en" {
                assert_ne!(localized, english, "{locale} must not fall back to English");
            }
        }
    }

    /// A load without a transmitter must fail *loudly* — an `Error` event
    /// carrying the actionable message followed by `Stopped` — never a
    /// silent no-op stream.
    #[test]
    fn a_missing_raopsink_load_fails_loudly_not_silently() {
        let (tx, rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        let generation = PlayerEventGeneration::from_raw(7);
        output.set_event_generation(generation);

        output.finish_load(generation, AirPlayOutput::ensure_raopsink(false));

        match rx.try_recv() {
            Ok(PlayerEvent::Error {
                generation: event_generation,
                message,
            }) => {
                assert_eq!(event_generation, generation);
                assert!(message.contains("raopsink"), "{message}");
                assert!(message.contains("gst-plugins-bad"), "{message}");
            }
            event => panic!("expected actionable error, got {event:?}"),
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

    #[test]
    fn protected_load_fails_closed_before_any_pipeline_sees_the_secret() {
        const SECRET: &str = "airplay-secret-must-not-leak";

        gst::init().expect("GStreamer init");

        let (tx, rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx, 1.0);
        let generation = PlayerEventGeneration::from_raw(42);
        output.set_event_generation(generation);

        // The transmitter gate runs before any per-track proxy work, so on
        // a machine without `raopsink` the actionable install guidance wins.
        // With `raopsink` registered, preparation is reached next and must
        // fail — a protected URI requires the app runtime to mint its
        // loopback ticket, and none is configured. Either way the failure
        // is a fixed message and no pipeline is ever constructed around the
        // credential-bearing URI.
        let expected = if AirPlayOutput::raopsink_available() {
            "AirPlay media preparation failed".to_string()
        } else {
            AirPlayOutput::raopsink_missing_message(&rust_i18n::locale())
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
