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
//! independently of the main `playbin3`.  When an AirPlay output is
//! selected and a URI is loaded:
//!
//!   1. Try a `raopsink` element if one is registered.  Pipeline:
//!      `uridecodebin ! audioconvert ! avenc_alac ! raopsink`.
//!   2. If `raopsink` isn't in the registry, fall back to spawning
//!      `shairport-sync` in pipe mode and shovelling raw S16LE PCM at
//!      its stdin via `fdsink`.
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
//! - **Requires a working `raopsink` element or `shairport-sync` on
//!   PATH.**  The exact packages that provide `raopsink` vary by
//!   distro and aren't asserted here; the code probes the GStreamer
//!   registry at runtime and falls back accordingly.
//! - **Seeking is not supported** for live RAOP streams.

use std::process::Child;
use std::sync::{Arc, Mutex};

use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerState};

use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use tracing::{debug, error, info, warn};

/// Active AirPlay session: the dedicated GStreamer pipeline plus, if we
/// fell back to it, the `shairport-sync` child process.
struct Session {
    pipeline: gst::Pipeline,
    /// Only `Some` when the shairport-sync fallback path is in use —
    /// the child must outlive the pipeline and be killed on `stop()`.
    sps_child: Option<Child>,
    /// Bus watch guard — dropping it removes the watch.
    _bus_watch: gst::bus::BusWatchGuard,
}

/// AirPlay audio output — streams to a RAOP receiver.
pub struct AirPlayOutput {
    /// Human-readable name from mDNS discovery (e.g. "Living Room").
    display_name: String,
    /// Receiver hostname or IP address.
    host: String,
    /// Receiver port (typically 7000 for AirPlay, varies for RAOP).
    port: u16,
    /// Event sender for relaying state changes to the GTK main thread.
    event_tx: async_channel::Sender<PlayerEvent>,
    /// Cached volume level (0.0–1.0).
    volume: f64,
    /// Active session, if any.  `Mutex` (not `RefCell`) because the
    /// bus watch may run on a worker thread.
    session: Arc<Mutex<Option<Session>>>,
}

impl AirPlayOutput {
    /// Create a new AirPlay output targeting the given receiver.
    ///
    /// Does **not** establish a connection — that happens lazily on the
    /// first playback command.
    #[allow(dead_code)]
    pub fn new(
        display_name: &str,
        host: &str,
        port: u16,
        event_tx: async_channel::Sender<PlayerEvent>,
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
            volume: 1.0,
            session: Arc::new(Mutex::new(None)),
        }
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

        let host = &self.host;
        let port = self.port;
        let volume = self.volume;

        // Try GStreamer raopsink first.
        match Self::build_raop_pipeline(host, port, uri, volume) {
            Ok(pipeline) => {
                let bus_watch = self.attach_bus_watch(&pipeline)?;
                pipeline
                    .set_state(gst::State::Paused)
                    .map_err(|e| format!("RAOP pipeline preroll failed: {e}"))?;
                info!(host = %host, port, "AirPlay: session opened via raopsink");
                *self.session.lock().expect("session mutex poisoned") = Some(Session {
                    pipeline,
                    sps_child: None,
                    _bus_watch: bus_watch,
                });
                Ok(())
            }
            Err(e1) => {
                warn!(error = %e1, "raopsink unavailable, trying shairport-sync");
                let (pipeline, sps_child) = Self::build_shairport_pipeline(host, port, uri)
                    .map_err(|e2| format!("Both AirPlay paths failed: {e1}; {e2}"))?;
                let bus_watch = self.attach_bus_watch(&pipeline)?;
                pipeline
                    .set_state(gst::State::Paused)
                    .map_err(|e| format!("shairport-sync pipeline preroll failed: {e}"))?;
                info!(host = %host, port, "AirPlay: session opened via shairport-sync");
                *self.session.lock().expect("session mutex poisoned") = Some(Session {
                    pipeline,
                    sps_child: Some(sps_child),
                    _bus_watch: bus_watch,
                });
                Ok(())
            }
        }
    }

    /// Tear down the active session — pipeline → Null, kill child if any.
    fn close_session(&self) {
        let mut guard = self.session.lock().expect("session mutex poisoned");
        if let Some(mut sess) = guard.take() {
            let _ = sess.pipeline.set_state(gst::State::Null);
            if let Some(ref mut child) = sess.sps_child {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Construct an attached bus watch that forwards EOS / Error / state
    /// changes to the shared `PlayerEvent` channel.
    fn attach_bus_watch(
        &self,
        pipeline: &gst::Pipeline,
    ) -> Result<gst::bus::BusWatchGuard, String> {
        let bus = pipeline
            .bus()
            .ok_or_else(|| "Pipeline has no bus".to_string())?;
        let tx = self.event_tx.clone();
        let pipeline_weak = pipeline.downgrade();
        bus.add_watch(move |_, msg| {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => {
                    let _ = tx.try_send(PlayerEvent::TrackEnded);
                }
                MessageView::Error(err) => {
                    error!(
                        error = %err.error(),
                        debug = ?err.debug(),
                        "AirPlay pipeline error"
                    );
                    let _ = tx.try_send(PlayerEvent::Error(format!(
                        "AirPlay: {}",
                        err.error()
                    )));
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
                                gst::State::Ready | gst::State::Null => {
                                    Some(PlayerState::Stopped)
                                }
                                gst::State::VoidPending => None,
                            };
                            if let Some(state) = mapped {
                                let _ = tx.try_send(PlayerEvent::StateChanged(state));
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

    /// Build a pipeline using GStreamer's `raopsink`, if it is registered.
    fn build_raop_pipeline(
        host: &str,
        port: u16,
        uri: &str,
        volume: f64,
    ) -> Result<gst::Pipeline, String> {
        let registry = gst::Registry::get();
        if registry
            .find_feature("raopsink", gst::ElementFactory::static_type())
            .is_none()
        {
            return Err("raopsink not found in GStreamer registry".to_string());
        }

        let pipeline_str = format!(
            "uridecodebin uri=\"{}\" ! audioconvert ! avenc_alac ! raopsink name=raop host={} port={}",
            uri.replace('"', "\\\""),
            host,
            port,
        );

        let element = gst::parse::launch(&pipeline_str)
            .map_err(|e| format!("Failed to build RAOP pipeline: {e}"))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| "RAOP launch did not yield a Pipeline".to_string())?;

        if let Some(sink) = pipeline.by_name("raop") {
            sink.set_property("volume", Self::volume_to_db(volume));
        }

        Ok(pipeline)
    }

    /// Fallback: pipe raw S16LE PCM into a `shairport-sync` subprocess.
    #[cfg(unix)]
    fn build_shairport_pipeline(
        host: &str,
        port: u16,
        uri: &str,
    ) -> Result<(gst::Pipeline, Child), String> {
        let _ = (host, port); // shairport-sync uses its own discovery.

        // Locate shairport-sync.
        let check = std::process::Command::new("which")
            .arg("shairport-sync")
            .output()
            .map_err(|e| format!("Failed to invoke 'which': {e}"))?;
        if !check.status.success() {
            return Err("shairport-sync not found on PATH".to_string());
        }

        // Spawn shairport-sync reading PCM from stdin.
        let mut child = std::process::Command::new("shairport-sync")
            .args(["-o", "pipe", "--", "/dev/stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to spawn shairport-sync: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture shairport-sync stdin".to_string())?;

        let fd = {
            use std::os::unix::io::IntoRawFd;
            stdin.into_raw_fd()
        };

        let pipeline_str = format!(
            "uridecodebin uri=\"{}\" ! audioconvert ! audio/x-raw,format=S16LE,rate=44100,channels=2 ! fdsink fd={}",
            uri.replace('"', "\\\""),
            fd,
        );

        let element = gst::parse::launch(&pipeline_str).map_err(|e| {
            let _ = child.kill();
            let _ = child.wait();
            format!("Failed to build shairport-sync pipeline: {e}")
        })?;
        let pipeline = element.downcast::<gst::Pipeline>().map_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
            "shairport-sync launch did not yield a Pipeline".to_string()
        })?;

        Ok((pipeline, child))
    }

    /// Windows fallback: shairport-sync pipe mode is Unix-only.
    #[cfg(not(unix))]
    fn build_shairport_pipeline(
        _host: &str,
        _port: u16,
        _uri: &str,
    ) -> Result<(gst::Pipeline, Child), String> {
        Err("shairport-sync pipe mode requires Unix".to_string())
    }

    /// Apply a state transition to the active pipeline, if any.
    fn set_pipeline_state(&self, target: gst::State) {
        let guard = self.session.lock().expect("session mutex poisoned");
        if let Some(ref sess) = *guard {
            if let Err(e) = sess.pipeline.set_state(target) {
                warn!(error = %e, ?target, "AirPlay: state transition failed");
            }
        } else {
            debug!(?target, "AirPlay: no active session for state change");
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
        // The raopsink path forwards volume to the receiver; the
        // shairport-sync fallback does not (it uses its own mixer).
        // We always advertise support — the slider is still useful
        // when raopsink is in use, and the receiver's hardware volume
        // remains as a backstop in either case.
        true
    }

    fn load_uri(&self, uri: &str) {
        info!("AirPlay: loading URI");
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Buffering));

        if let Err(e) = self.open_session(uri) {
            error!(error = %e, "AirPlay: failed to open session");
            let _ = self.event_tx.try_send(PlayerEvent::Error(e));
            let _ = self
                .event_tx
                .try_send(PlayerEvent::StateChanged(PlayerState::Stopped));
        }
    }

    fn play(&self) {
        debug!("AirPlay: play");
        self.set_pipeline_state(gst::State::Playing);
    }

    fn pause(&self) {
        debug!("AirPlay: pause");
        self.set_pipeline_state(gst::State::Paused);
    }

    fn stop(&self) {
        debug!("AirPlay: stop");
        self.close_session();
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Stopped));
    }

    fn toggle_play_pause(&self) {
        let target = {
            let guard = self.session.lock().expect("session mutex poisoned");
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
            self.set_pipeline_state(state);
        }
    }

    fn seek_to(&self, _position_ms: u64) {
        // Live RAOP streams don't expose seekable timelines; skip.
        debug!("AirPlay: seek not supported on live streams");
    }

    fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
        let guard = self.session.lock().expect("session mutex poisoned");
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
        let guard = self.session.lock().expect("session mutex poisoned");
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
        let guard = self.session.lock().expect("session mutex poisoned");
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
        let output = AirPlayOutput::new("Living Room", "192.168.1.100", 7000, tx);
        assert_eq!(output.name(), "Living Room");
    }

    #[test]
    fn test_airplay_output_type() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx);
        assert_eq!(output.output_type(), OutputType::AirPlay);
    }

    #[test]
    fn test_airplay_supports_volume() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx);
        assert!(output.supports_volume());
    }

    #[test]
    fn test_airplay_volume_clamp() {
        let (tx, _rx) = async_channel::unbounded();
        let mut output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx);
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
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx);
        assert!(output.position_ms().is_none());
    }

    #[test]
    fn test_airplay_initial_state() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx);
        assert_eq!(output.state(), PlayerState::Stopped);
    }
}
