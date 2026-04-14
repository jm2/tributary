//! GStreamer audio playback engine.
//!
//! Provides a non-blocking [`Player`] that wraps a GStreamer `playbin3`
//! pipeline and communicates state changes to the GTK main thread via
//! an [`async_channel`].
//!
//! # Threading model
//!
//! The GStreamer pipeline runs its own internal threads for decoding and
//! output.  All public [`Player`] methods are designed to be called from
//! the **GTK main thread**.  Pipeline bus messages and the position
//! polling timer are dispatched through `glib` main-loop callbacks, so
//! they also execute on the main thread without blocking it.
//!
//! The caller receives events by consuming the [`async_channel::Receiver`]
//! inside a `glib::MainContext::default().spawn_local()` loop, identical
//! to the pattern used by [`LibraryEngine`](crate::local::engine::LibraryEngine).

use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use tracing::{debug, error, info, warn};

// ── Events ──────────────────────────────────────────────────────────────

/// Events emitted by the player, delivered on the GTK main thread.
#[derive(Debug, Clone)]
pub enum PlayerEvent {
    /// The pipeline transitioned to a new coarse state.
    StateChanged(PlayerState),
    /// Periodic position tick (values in milliseconds).
    PositionChanged { position_ms: u64, duration_ms: u64 },
    /// The current stream reached its natural end.
    TrackEnded,
    /// A pipeline error occurred.
    Error(String),
}

/// Coarse playback state visible to the rest of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Stopped,
    Buffering,
    Playing,
    Paused,
}

// ── Player ──────────────────────────────────────────────────────────────

/// GStreamer playback engine.
///
/// Wraps a `playbin3` (with `playbin` fallback) and exposes a safe,
/// main-thread-only control surface.  State updates are pushed through
/// the [`async_channel::Receiver`] returned by [`Player::new`].
pub struct Player {
    playbin: gst::Element,
    volume: f64,
    event_tx: async_channel::Sender<PlayerEvent>,
    /// Dropping this guard removes the bus watch — must stay alive.
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Player {
    /// Initialise GStreamer, build the pipeline, and start the bus watch
    /// and position polling timer.
    ///
    /// Returns the player and a receiver.  The caller must consume the
    /// receiver on the GTK main thread via:
    /// ```ignore
    /// glib::MainContext::default().spawn_local(async move {
    ///     while let Ok(event) = player_rx.recv().await {
    ///         // handle PlayerEvent …
    ///     }
    /// });
    /// ```
    pub fn new() -> anyhow::Result<(Self, async_channel::Receiver<PlayerEvent>)> {
        // On Windows, point GStreamer at bundled plugins next to the executable
        // before init() scans the plugin registry.
        #[cfg(target_os = "windows")]
        Self::set_bundled_plugin_path();

        gst::init()?;
        info!("GStreamer {}", gst::version_string());

        // Prefer playbin3 (auto-plugging, modern); fall back to playbin.
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| {
                warn!("playbin3 unavailable, falling back to playbin");
                gst::ElementFactory::make("playbin").build()
            })
            .map_err(|e| anyhow::anyhow!("Failed to create playbin element: {e}"))?;

        let volume = load_saved_volume().unwrap_or(1.0);
        playbin.set_property("volume", slider_to_pipeline(volume));

        let (event_tx, event_rx) = async_channel::unbounded();

        let bus_watch = Self::attach_bus_watch(&playbin, &event_tx)?;
        Self::start_position_timer(&playbin, &event_tx);

        let player = Self {
            playbin,
            volume,
            event_tx,
            _bus_watch: bus_watch,
        };

        Ok((player, event_rx))
    }

    // ── Playback controls ───────────────────────────────────────────

    /// Load a URI (e.g. `file:///path/to/song.flac`) and start playback.
    ///
    /// Immediately emits [`PlayerState::Buffering`] so the UI can show a
    /// spinner while the pipeline transitions to `Playing`.
    pub fn load_uri(&self, uri: &str) {
        tracing::debug!("Loading track");
        let _ = self.playbin.set_state(gst::State::Null);
        self.playbin.set_property("uri", uri);
        // Re-apply volume — the NULL transition resets it to 1.0.
        self.playbin
            .set_property("volume", slider_to_pipeline(self.volume));

        // Signal buffering immediately — the bus watch will send
        // `Playing` once the pipeline actually reaches that state.
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Buffering));

        let _ = self.playbin.set_state(gst::State::Playing);
    }

    /// Resume playback from a paused state.
    pub fn play(&self) {
        debug!("play");
        let _ = self.playbin.set_state(gst::State::Playing);
    }

    /// Pause playback.
    pub fn pause(&self) {
        debug!("pause");
        let _ = self.playbin.set_state(gst::State::Paused);
    }

    /// Stop playback and reset the pipeline to NULL.
    pub fn stop(&self) {
        debug!("stop");
        let _ = self.playbin.set_state(gst::State::Null);
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Stopped));
    }

    /// Toggle between Playing ↔ Paused.
    pub fn toggle_play_pause(&self) {
        // Non-blocking state query (zero timeout).
        let (_, current, _) = self.playbin.state(gst::ClockTime::ZERO);
        match current {
            gst::State::Playing => self.pause(),
            gst::State::Paused => self.play(),
            _ => {}
        }
    }

    /// Seek to an absolute position (milliseconds from start).
    pub fn seek_to(&self, position_ms: u64) {
        debug!(position_ms, "seek");
        let _ = self.playbin.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_mseconds(position_ms),
        );
    }

    // ── Volume ──────────────────────────────────────────────────────

    /// Set pipeline volume (clamped to 0.0 – 1.0, linear).
    /// Set volume from a linear slider position (0.0 – 1.0).
    /// Internally applies a cubic curve for perceptually linear loudness.
    pub fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
        self.playbin
            .set_property("volume", slider_to_pipeline(self.volume));
        save_volume(self.volume);
        debug!(volume = self.volume, "Volume set");
    }

    /// Current pipeline volume (0.0 – 1.0).
    #[allow(dead_code)]
    pub fn volume(&self) -> f64 {
        self.volume
    }

    // ── State / position queries ────────────────────────────────────

    /// Non-blocking query of the current playback state.
    #[allow(dead_code)]
    pub fn state(&self) -> PlayerState {
        let (_, current, _) = self.playbin.state(gst::ClockTime::ZERO);
        match current {
            gst::State::Playing => PlayerState::Playing,
            gst::State::Paused => PlayerState::Paused,
            _ => PlayerState::Stopped,
        }
    }

    /// Current playback position in milliseconds, or `None` if
    /// the pipeline is not in a queryable state.
    pub fn position_ms(&self) -> Option<u64> {
        self.playbin
            .query_position::<gst::ClockTime>()
            .map(|t| t.mseconds())
    }

    // ── Internal: bus watch ─────────────────────────────────────────

    /// Watch the pipeline bus for EOS, Error, and StateChanged messages.
    ///
    /// The watch callback runs on the glib main loop (main thread).
    fn attach_bus_watch(
        playbin: &gst::Element,
        event_tx: &async_channel::Sender<PlayerEvent>,
    ) -> anyhow::Result<gst::bus::BusWatchGuard> {
        let bus = playbin
            .bus()
            .ok_or_else(|| anyhow::anyhow!("playbin has no bus"))?;

        let tx = event_tx.clone();
        let playbin_name = playbin.name();

        bus.add_watch_local(move |_bus, msg| {
            use gst::MessageView;

            match msg.view() {
                MessageView::Eos(_) => {
                    info!("End of stream");
                    let _ = tx.try_send(PlayerEvent::TrackEnded);
                }

                MessageView::Error(err) => {
                    error!(
                        src = ?msg.src().map(|s| s.path_string()),
                        error = %err.error(),
                        debug = ?err.debug(),
                        "Pipeline error"
                    );
                    let _ = tx.try_send(PlayerEvent::Error(err.error().to_string()));
                }

                MessageView::StateChanged(sc) => {
                    // Only react to the playbin's own transitions,
                    // not those of child elements (decoders, sinks, …).
                    let is_playbin = msg.src().is_some_and(|src| src.name() == playbin_name);

                    if is_playbin {
                        let new_state = match sc.current() {
                            gst::State::Playing => PlayerState::Playing,
                            gst::State::Paused => PlayerState::Paused,
                            _ => PlayerState::Stopped,
                        };
                        debug!(
                            old = ?sc.old(),
                            new = ?sc.current(),
                            pending = ?sc.pending(),
                            "Pipeline state changed"
                        );
                        let _ = tx.try_send(PlayerEvent::StateChanged(new_state));
                    }
                }

                MessageView::Buffering(buffering) => {
                    let percent = buffering.percent();
                    debug!(percent, "Buffering");
                    if percent < 100 {
                        let _ = tx.try_send(PlayerEvent::StateChanged(PlayerState::Buffering));
                    }
                    // When buffering reaches 100%, GStreamer will emit a
                    // StateChanged → Playing message, so we don't need to
                    // send Playing here.
                }

                _ => {}
            }

            glib::ControlFlow::Continue
        })
        .map_err(|e| anyhow::anyhow!("Failed to add bus watch: {e}"))
    }

    // ── Internal: position polling ──────────────────────────────────

    /// Start a 500 ms timer that queries the pipeline position while
    /// playing and sends [`PlayerEvent::PositionChanged`].
    ///
    /// The timer self-cancels when the playbin is dropped (weak ref).
    fn start_position_timer(playbin: &gst::Element, event_tx: &async_channel::Sender<PlayerEvent>) {
        let playbin_weak = playbin.downgrade();
        let tx = event_tx.clone();

        glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(playbin) = playbin_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            // Non-blocking check — only query when actually playing.
            let (_, state, _) = playbin.state(gst::ClockTime::ZERO);
            if state == gst::State::Playing {
                if let Some(pos) = playbin.query_position::<gst::ClockTime>() {
                    // Duration may be unknown for live streams (radio).
                    // Send 0 for duration_ms so the UI can still update
                    // the elapsed time label and clear the buffering spinner.
                    let dur = playbin
                        .query_duration::<gst::ClockTime>()
                        .map(|d| d.mseconds())
                        .unwrap_or(0);
                    let _ = tx.try_send(PlayerEvent::PositionChanged {
                        position_ms: pos.mseconds(),
                        duration_ms: dur,
                    });
                }
            }

            glib::ControlFlow::Continue
        });
    }

    // ── Internal: Windows plugin path ───────────────────────────────

    /// On Windows, set `GST_PLUGIN_PATH` to the bundled `lib/gstreamer-1.0`
    /// directory next to the executable, so GStreamer can find codec plugins
    /// in a self-contained deployment.
    ///
    /// Does nothing if the variable is already set (user override) or if
    /// the bundled directory does not exist (dev/MSYS2 environment).
    #[cfg(target_os = "windows")]
    fn set_bundled_plugin_path() {
        use std::env;

        if env::var_os("GST_PLUGIN_PATH").is_some() {
            info!("GST_PLUGIN_PATH already set — skipping bundled plugin detection");
            return;
        }

        let exe = match env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                warn!("Could not determine exe path: {e}");
                return;
            }
        };
        let Some(dir) = exe.parent() else { return };
        let plugin_dir = dir.join("lib").join("gstreamer-1.0");

        if plugin_dir.is_dir() {
            let count = std::fs::read_dir(&plugin_dir)
                .map(|rd| {
                    rd.filter(|e| {
                        e.as_ref()
                            .is_ok_and(|e| e.path().extension().is_some_and(|ext| ext == "dll"))
                    })
                    .count()
                })
                .unwrap_or(0);

            env::set_var("GST_PLUGIN_PATH", &plugin_dir);
            // Force a fresh registry scan so stale system paths don't win.
            let registry = dir.join("gst-registry.bin");
            env::set_var("GST_REGISTRY", &registry);

            info!(
                path = %plugin_dir.display(),
                plugins = count,
                "Bundled GStreamer plugins detected"
            );
        } else {
            info!(
                path = %plugin_dir.display(),
                "No bundled GStreamer plugin directory found — using system plugins"
            );
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        info!("Shutting down GStreamer pipeline");
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

// ── Volume curve ────────────────────────────────────────────────────────

/// Convert a linear slider position (0.0–1.0) to a GStreamer pipeline
/// volume using a cubic curve.  This makes the quiet half of the slider
/// far more usable — without it, most of the perceptible range is
/// crammed into the top 20% of travel.
fn slider_to_pipeline(slider: f64) -> f64 {
    slider * slider * slider
}

// ── Volume persistence ──────────────────────────────────────────────────

/// Path to the volume state file: `<data_dir>/tributary/volume`
fn volume_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join("volume"))
}

fn load_saved_volume() -> Option<f64> {
    let path = volume_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: f64 = text.trim().parse().ok()?;
    if (0.0..=1.0).contains(&v) {
        Some(v)
    } else {
        None
    }
}

fn save_volume(level: f64) {
    if let Some(path) = volume_path() {
        // Ensure the parent directory exists (may not on first launch
        // if the DB hasn't been initialised yet).
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, format!("{level:.3}"));
    }
}

// ── URL secret redaction ────────────────────────────────────────────────

/// Mask sensitive query parameters in URLs for safe logging.
///
/// Redacts `X-Plex-Token`, `api_key`, `t` (Subsonic token), and `s`
/// (Subsonic salt) to prevent auth credentials from appearing in logs.
pub fn redact_url_secrets(uri: &str) -> String {
    // Note: "s" is only redacted when "t" is also present (Subsonic salt+token pair).
    // This avoids false positives on unrelated URLs that happen to have an "s" param.
    const SENSITIVE_PARAMS: &[&str] = &["X-Plex-Token", "api_key"];
    const SUBSONIC_TOKEN_PARAMS: &[&str] = &["t", "s"];

    let Ok(mut url) = url::Url::parse(uri) else {
        return uri.to_string();
    };

    // Check if this looks like a Subsonic URL (has both "t" and "s" params).
    let has_subsonic_token = url.query_pairs().any(|(k, _)| k == "t");

    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| {
            let should_redact = SENSITIVE_PARAMS.contains(&k.as_ref())
                || (has_subsonic_token && SUBSONIC_TOKEN_PARAMS.contains(&k.as_ref()));
            let v = if should_redact {
                "REDACTED".to_string()
            } else {
                v.to_string()
            };
            (k.to_string(), v)
        })
        .collect();

    if pairs.is_empty() {
        return uri.to_string();
    }

    url.query_pairs_mut().clear();
    for (k, v) in &pairs {
        url.query_pairs_mut().append_pair(k, v);
    }
    url.to_string()
}
