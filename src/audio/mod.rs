//! Audio playback engine and output abstraction.
//!
//! This module provides:
//!
//! - A non-blocking GStreamer [`Player`] that wraps a `playbin3` pipeline.
//! - An [`AudioOutput`](output::AudioOutput) trait for abstracting over
//!   different playback destinations (local speakers, MPD, AirPlay, etc.).
//! - [`LocalOutput`](local_output::LocalOutput) — wraps [`Player`] for
//!   local speaker output.
//! - [`MpdOutput`](mpd_output::MpdOutput) — sends commands to an MPD
//!   server over TCP.
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

pub mod airplay_output;
pub mod cast_http_server;
pub mod chromecast_output;
mod gstreamer_media;
pub mod local_output;
pub mod mpd_output;
pub mod output;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use tracing::{debug, error, info, warn};
use url::{Host, Url};

use self::gstreamer_media::{GstreamerMediaProxy, GstreamerMediaTicket};
use crate::architecture::media::ResolvedHttpRequest;

/// `souphttpsrc`'s default blocking-I/O deadline is 15 seconds. Protected
/// playback gives the app-owned proxy a shorter upstream startup budget, then
/// leaves this larger downstream budget for the proxy's deterministic 502/504
/// response to reach GStreamer.
const PROTECTED_LOOPBACK_TIMEOUT_SECONDS: u32 = 30;

/// GLib's proxy-resolver sentinel for an explicitly direct connection.
///
/// An empty `souphttpsrc` proxy is not sufficient: with libsoup3 it restores
/// the system resolver and can send even a 127.0.0.1 request to an ambient
/// proxy. `direct://` installs a dedicated resolver that never leaves the
/// machine for this one validated Tributary ticket.
const DIRECT_PROXY_SENTINEL: &str = "direct://";

// ── Events ──────────────────────────────────────────────────────────────

/// Monotonic identity of the playback load that owns a [`PlayerEvent`].
///
/// Outputs capture this value when a URI is loaded (or an asynchronous command
/// is started). The UI accepts an event only while the corresponding playback
/// session generation is still current, so delayed EOS/state/error events from
/// a superseded track or output cannot mutate the new session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PlayerEventGeneration(u64);

impl PlayerEventGeneration {
    pub(crate) fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    pub(crate) fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.0
    }
}

/// Events emitted by an output, delivered on the GTK main thread.
#[derive(Debug, Clone)]
pub enum PlayerEvent {
    /// The pipeline transitioned to a new coarse state.
    StateChanged {
        generation: PlayerEventGeneration,
        state: PlayerState,
    },
    /// Periodic position tick (values in milliseconds).
    PositionChanged {
        generation: PlayerEventGeneration,
        position_ms: u64,
        duration_ms: u64,
    },
    /// The current stream reached its natural end.
    TrackEnded { generation: PlayerEventGeneration },
    /// A pipeline error occurred.
    Error {
        generation: PlayerEventGeneration,
        message: String,
    },
}

impl PlayerEvent {
    pub fn state(generation: PlayerEventGeneration, state: PlayerState) -> Self {
        Self::StateChanged { generation, state }
    }

    pub fn position(generation: PlayerEventGeneration, position_ms: u64, duration_ms: u64) -> Self {
        Self::PositionChanged {
            generation,
            position_ms,
            duration_ms,
        }
    }

    pub fn ended(generation: PlayerEventGeneration) -> Self {
        Self::TrackEnded { generation }
    }

    pub fn error(generation: PlayerEventGeneration, message: impl Into<String>) -> Self {
        Self::Error {
            generation,
            message: message.into(),
        }
    }

    pub fn generation(&self) -> PlayerEventGeneration {
        match self {
            Self::StateChanged { generation, .. }
            | Self::PositionChanged { generation, .. }
            | Self::TrackEnded { generation }
            | Self::Error { generation, .. } => *generation,
        }
    }
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
    /// App-owned exact-origin fetch boundary for authenticated media. The
    /// pipeline receives only a dedicated loopback ticket, never the backend
    /// URL carrying the user's credential.
    media_proxy: Arc<GstreamerMediaProxy>,
    /// Generation assigned by the playback session before each URI load.
    event_generation: Rc<Cell<PlayerEventGeneration>>,
    /// Holds the latest volume awaiting a debounced disk write, or `None`
    /// when no write is scheduled.  Keeps slider-drag volume changes off
    /// the main-thread hot path (see [`Player::save_volume_debounced`]).
    volume_save_pending: Rc<Cell<Option<f64>>>,
    /// The watch is replaced on every URI load. Each watch captures that
    /// load's generation, so even an already-queued message from the previous
    /// pipeline incarnation remains identifiable as stale.
    bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,
}

impl Player {
    /// Return a clone of the event sender.
    ///
    /// Used to give `MpdOutput` (or other non-GStreamer outputs) a sender
    /// that feeds into the **same** `player_rx` event loop, so position
    /// ticks, state changes, and errors from any output are handled
    /// uniformly by the single `PlayerEvent` consumer in `window.rs`.
    pub fn event_sender(&self) -> async_channel::Sender<PlayerEvent> {
        self.event_tx.clone()
    }

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
    pub fn new(
        rt_handle: tokio::runtime::Handle,
    ) -> anyhow::Result<(Self, async_channel::Receiver<PlayerEvent>)> {
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

        // Protected remote media is deliberately handed to GStreamer as an
        // opaque loopback ticket. Configure the HTTP source before it opens so
        // an ambient system proxy can never receive that ticket.
        Self::install_loopback_http_source_policy(&playbin);

        // ── macOS: work around GStreamer ≤1.28 channel-negotiation bug ──
        // On multi-channel audio devices (e.g. monitors with spatial audio
        // speakers reporting 8 channels to Core Audio), osxaudiosink
        // advertises channels=[1,N] in its caps.  audioconvert's
        // fixate_caps then picks the maximum channel count and sets
        // channel-mask=0x0 (no positions).  The resulting 2→N channel
        // conversion fails with "Failed to make converter", surfacing as
        // "not-negotiated" on every file.
        //
        // Workaround: use playbin's "element-setup" signal to intercept
        // osxaudiosink when it is created, and install a pad probe on its
        // sink pad that rewrites CAPS query results to cap channels at 2.
        // This makes audioconvert preserve the source channel count.
        //
        // TODO: remove once GStreamer ships a fix (likely ≥1.28.3 or 1.30).
        #[cfg(target_os = "macos")]
        Self::install_macos_channel_cap(&playbin);

        let volume = load_saved_volume().unwrap_or(1.0);
        playbin.set_property("volume", slider_to_pipeline(volume));

        let (event_tx, event_rx) = async_channel::unbounded();

        let event_generation = Rc::new(Cell::new(PlayerEventGeneration::default()));
        Self::start_position_timer(&playbin, &event_tx, Rc::clone(&event_generation));

        let player = Self {
            playbin,
            volume,
            event_tx,
            media_proxy: Arc::new(GstreamerMediaProxy::new(Some(rt_handle))),
            event_generation,
            volume_save_pending: Rc::new(Cell::new(None)),
            bus_watch: RefCell::new(None),
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
        let generation = self.begin_load();
        let prepared = self.media_proxy.prepare(uri);
        self.finish_load(generation, prepared);
    }

    /// Load one backend-resolved authenticated request through an app-owned
    /// loopback ticket. The typed request is never eligible for direct
    /// GStreamer playback.
    pub fn load_resolved(&self, request: ResolvedHttpRequest) {
        tracing::debug!("Loading resolved track");
        let generation = self.begin_load();
        let prepared = self.media_proxy.prepare_resolved(request);
        self.finish_load(generation, prepared);
    }

    fn begin_load(&self) -> PlayerEventGeneration {
        // Remove the previous generation's watch before driving that pipeline
        // to NULL. Flush the bus during teardown as well: otherwise a queued
        // EOS from the old URI could be consumed by the newly attached watch
        // and inherit the new generation despite originating from the old
        // pipeline incarnation.
        self.bus_watch.borrow_mut().take();
        if let Some(bus) = self.playbin.bus() {
            bus.set_flushing(true);
        }
        let _ = self.playbin.set_state(gst::State::Null);
        // Retiring the pipeline state does not clear playbin's URI property.
        // If preparation of the replacement media then fails, a later Play
        // must not be able to restart the previous track under the new queue
        // item's metadata.
        self.playbin.set_property("uri", "");

        self.event_generation.get()
    }

    fn finish_load(
        &self,
        generation: PlayerEventGeneration,
        prepared: Result<gstreamer_media::PreparedGstreamerMedia, &'static str>,
    ) {
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(failure) => {
                error!(error = %failure, "Audio media preparation failed");
                self.emit_load_failure(generation, failure.to_string());
                return;
            }
        };
        self.playbin.set_property("uri", prepared.uri());
        // Re-apply volume — the NULL transition resets it to 1.0.
        self.playbin
            .set_property("volume", slider_to_pipeline(self.volume));
        if let Some(bus) = self.playbin.bus() {
            bus.set_flushing(false);
        }

        // Signal buffering immediately — the bus watch will send
        // `Playing` once the pipeline actually reaches that state.
        let ticket = prepared.ticket();
        match Self::attach_bus_watch(
            &self.playbin,
            &self.event_tx,
            generation,
            Arc::clone(&self.media_proxy),
            ticket.clone(),
        ) {
            Ok(watch) => *self.bus_watch.borrow_mut() = Some(watch),
            Err(error) => {
                if let Some(ticket) = ticket.as_ref() {
                    self.media_proxy.revoke_if_current(ticket);
                }
                if let Some(bus) = self.playbin.bus() {
                    bus.set_flushing(true);
                }
                let _ = self
                    .event_tx
                    .try_send(PlayerEvent::error(generation, error.to_string()));
                let _ = self
                    .event_tx
                    .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
                return;
            }
        }

        if let Err(e) = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering))
        {
            warn!(error = %e, "dropped Buffering event — UI consumer may be stalled");
        }

        if self.playbin.set_state(gst::State::Playing).is_err() {
            self.bus_watch.borrow_mut().take();
            if let Some(bus) = self.playbin.bus() {
                bus.set_flushing(true);
            }
            let _ = self.playbin.set_state(gst::State::Null);
            if let Some(ticket) = ticket.as_ref() {
                self.media_proxy.revoke_if_current(ticket);
            }
            error!("Audio pipeline failed to start");
            let _ = self.event_tx.try_send(PlayerEvent::error(
                generation,
                "Audio playback failed to start",
            ));
            let _ = self
                .event_tx
                .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
        }
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
        self.bus_watch.borrow_mut().take();
        if let Some(bus) = self.playbin.bus() {
            // Leave the idle bus flushing until the next load; the explicit
            // scoped Stopped event below is the only stop notification needed.
            bus.set_flushing(true);
        }
        let _ = self.playbin.set_state(gst::State::Null);
        self.media_proxy.revoke();
        let generation = self.event_generation.get();
        if let Err(e) = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Stopped))
        {
            warn!(error = %e, "dropped Stopped event — UI consumer may be stalled");
        }
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

    /// Associate subsequently emitted events with a playback-session load.
    pub fn set_event_generation(&self, generation: PlayerEventGeneration) {
        self.event_generation.set(generation);
    }

    // ── Volume ──────────────────────────────────────────────────────

    /// Set pipeline volume (clamped to 0.0 – 1.0, linear).
    /// Set volume from a linear slider position (0.0 – 1.0).
    /// Internally applies a cubic curve for perceptually linear loudness.
    pub fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
        self.playbin
            .set_property("volume", slider_to_pipeline(self.volume));
        self.save_volume_debounced();
        debug!(volume = self.volume, "Volume set");
    }

    /// Persist the current volume off the GTK main-thread hot path.
    ///
    /// The volume adjustment fires `set_volume` on every tick of a slider
    /// drag; writing the volume file synchronously on each tick would do
    /// many redundant blocking disk writes on the main thread.  Instead we
    /// coalesce them: record the latest value and, if no write is already
    /// scheduled, queue a single delayed flush that persists whatever value
    /// the slider has settled on.
    fn save_volume_debounced(&self) {
        let already_scheduled = self.volume_save_pending.get().is_some();
        self.volume_save_pending.set(Some(self.volume));
        if already_scheduled {
            return;
        }
        let pending = Rc::clone(&self.volume_save_pending);
        glib::timeout_add_local_once(Duration::from_millis(750), move || {
            if let Some(level) = pending.take() {
                save_volume(level);
            }
        });
    }

    /// Current pipeline volume (0.0 – 1.0).
    pub fn volume(&self) -> f64 {
        self.volume
    }

    // ── State / position queries ────────────────────────────────────

    /// Non-blocking query of the current playback state.
    ///
    /// Reachable only through `LocalOutput::state` (the trait impl),
    /// which itself currently has no production caller — the UI
    /// follows state via `PlayerEvent::StateChanged` instead. Keeping
    /// the method as part of `Player`'s API surface for future
    /// on-demand queries.
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
        generation: PlayerEventGeneration,
        media_proxy: Arc<GstreamerMediaProxy>,
        media_ticket: Option<Arc<GstreamerMediaTicket>>,
    ) -> anyhow::Result<gst::bus::BusWatchGuard> {
        let bus = playbin
            .bus()
            .ok_or_else(|| anyhow::anyhow!("playbin has no bus"))?;

        let tx = event_tx.clone();
        let playbin_name = playbin.name();
        let started_at = Instant::now();

        bus.add_watch_local(move |_bus, msg| {
            use gst::MessageView;

            match msg.view() {
                MessageView::Eos(_) => {
                    if let Some(ticket) = media_ticket.as_ref() {
                        media_proxy.revoke_if_current(ticket);
                    }
                    info!("End of stream");
                    if let Err(e) = tx.try_send(PlayerEvent::ended(generation)) {
                        warn!(error = %e, "dropped TrackEnded event — UI consumer may be stalled");
                    }
                }

                MessageView::Error(pipeline_error) => {
                    if let Some(ticket) = media_ticket.as_ref() {
                        media_proxy.revoke_if_current(ticket);
                    }
                    // GStreamer error/debug strings can retain the complete
                    // authenticated source URI. Record only closed categories
                    // and numeric codes; never inspect message/debug/details.
                    let error_value = pipeline_error.error();
                    let source_category = pipeline_error_source_category(msg);
                    let elapsed_ms =
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                    error!(
                        protected = media_ticket.is_some(),
                        domain = pipeline_error_domain(&error_value),
                        code = error_value.code(),
                        source_category = source_category.as_str(),
                        elapsed_ms,
                        "Audio pipeline error"
                    );
                    if let Err(e) =
                        tx.try_send(PlayerEvent::error(generation, source_category.ui_message()))
                    {
                        warn!(error = %e, "dropped Error event — UI consumer may be stalled");
                    }
                    return glib::ControlFlow::Break;
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
                        let _ = tx.try_send(PlayerEvent::state(generation, new_state));
                    }
                }

                MessageView::Buffering(buffering) => {
                    let percent = buffering.percent();
                    debug!(percent, "Buffering");
                    if percent < 100 {
                        let _ = tx.try_send(PlayerEvent::state(generation, PlayerState::Buffering));
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

    /// Publish one coherent terminal sequence for a URI rejected before it can
    /// reach GStreamer. The supplied message is already fixed and URL-free.
    fn emit_load_failure(&self, generation: PlayerEventGeneration, message: String) {
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering));
        let _ = self
            .event_tx
            .try_send(PlayerEvent::error(generation, message));
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
    }

    // ── Internal: position polling ──────────────────────────────────

    /// Start a 500 ms timer that queries the pipeline position while
    /// playing and sends [`PlayerEvent::PositionChanged`].
    ///
    /// The timer self-cancels when the playbin is dropped (weak ref).
    fn start_position_timer(
        playbin: &gst::Element,
        event_tx: &async_channel::Sender<PlayerEvent>,
        event_generation: Rc<Cell<PlayerEventGeneration>>,
    ) {
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
                    let _ = tx.try_send(PlayerEvent::position(
                        event_generation.get(),
                        pos.mseconds(),
                        dur,
                    ));
                }
            }

            glib::ControlFlow::Continue
        });
    }

    // ── Internal: Windows plugin path ───────────────────────────────

    /// Force Tributary's own loopback media tickets to stay off ambient HTTP
    /// proxies. The callback is emitted on a GStreamer streaming thread, so it
    /// intentionally captures no GTK/Rc state.
    pub(super) fn install_loopback_http_source_policy(playbin: &gst::Element) {
        playbin.connect("source-setup", false, |args| {
            let source = args.get(1)?.get::<gst::Element>().ok()?;
            let location = source
                .find_property("location")
                .and_then(|_| source.property_value("location").get::<String>().ok());

            if !location
                .as_deref()
                .is_some_and(is_protected_loopback_ticket_uri)
            {
                return None;
            }

            if configure_protected_loopback_source(&source) {
                debug!("Protected loopback HTTP source forced to direct routing");
            } else {
                // A protected ticket must never fall back to a system proxy.
                // Publish a fixed bus error, then lock the source in NULL so
                // its parent cannot open the URI or wait indefinitely.
                gst::element_error!(
                    source,
                    gst::ResourceError::Settings,
                    ("Protected loopback routing unavailable")
                );
                source.set_locked_state(true);
                let _ = source.set_state(gst::State::Null);
                error!("Protected loopback HTTP source could not enforce direct routing");
            }

            None
        });
    }

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

    /// Install a pad probe on `osxaudiosink` that caps the negotiated
    /// channel count to stereo.
    ///
    /// On multi-channel Core Audio devices (e.g. monitors reporting 8
    /// channels), `audioconvert` fixates to the device maximum and then
    /// fails to build a channel converter because the fixated caps have
    /// `channel-mask=0x0` (no positions).
    ///
    /// This method connects to playbin's `element-setup` signal and,
    /// when `osxaudiosink` is created, installs a `QUERY_DOWNSTREAM`
    /// pad probe on its sink pad.  The probe intercepts `CAPS` queries
    /// and rewrites the `channels` field from `[1, N]` to `[1, 2]`,
    /// causing `audioconvert` to preserve the source channel count.
    #[cfg(target_os = "macos")]
    fn install_macos_channel_cap(playbin: &gst::Element) {
        use gst::prelude::*;

        playbin.connect("element-setup", false, |args| {
            let element = args[1].get::<gst::Element>().ok()?;
            let factory = element.factory()?;
            if factory.name() != "osxaudiosink" {
                return None;
            }

            let pad = element.static_pad("sink")?;
            pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |pad, info| {
                let Some(query) = info.query_mut() else {
                    return gst::PadProbeReturn::Ok;
                };
                if query.type_() != gst::QueryType::Caps {
                    return gst::PadProbeReturn::Ok;
                }

                // Let the original handler run first so we can rewrite
                // its result.
                let parent = pad.parent_element();
                let handled = if let Some(ref el) = parent {
                    gst::Pad::query_default(pad, Some(el), query)
                } else {
                    false
                };
                if !handled {
                    return gst::PadProbeReturn::Ok;
                }

                // Rewrite every structure's channels field to [1, 2].
                if let gst::QueryViewMut::Caps(ref mut q) = query.view_mut() {
                    if let Some(result) = q.result_owned() {
                        let mut capped = gst::Caps::new_empty();
                        {
                            let capped_mut = capped.make_mut();
                            for i in 0..result.size() {
                                if let Some(s) = result.structure(i) {
                                    let mut s = s.to_owned();
                                    if s.name().as_str() == "audio/x-raw" && s.has_field("channels")
                                    {
                                        s.set("channels", gst::IntRange::new(1, 2));
                                    }
                                    capped_mut.append_structure(s);
                                }
                            }
                        }
                        q.set_result(&capped);
                    }
                }

                gst::PadProbeReturn::Handled
            });

            info!(
                "macOS: installed channel-cap probe on osxaudiosink \
                 (GStreamer ≤1.28 workaround)"
            );
            None
        });
    }
}

/// Recognize only opaque HTTP tickets created by Tributary's dedicated local
/// media proxy. Ordinary loopback web/radio URLs keep their normal source
/// behavior, and non-loopback media may continue to use the user's proxy.
fn is_protected_loopback_ticket_uri(candidate: &str) -> bool {
    let Ok(url) = Url::parse(candidate) else {
        return false;
    };
    let loopback = matches!(
        url.host(),
        Some(Host::Ipv4(address)) if address.is_loopback()
    ) || matches!(
        url.host(),
        Some(Host::Ipv6(address)) if address.is_loopback()
    );
    let Some(route) = url.path().strip_prefix("/cast/") else {
        return false;
    };
    let (ticket_id, valid_extension) = match route.split_once('.') {
        Some((id, extension)) => (
            id,
            !extension.contains('.')
                && cast_http_server::PROTECTED_TICKET_AUDIO_EXTENSIONS.contains(&extension),
        ),
        None => (route, true),
    };
    let canonical_ticket_id = uuid::Uuid::parse_str(ticket_id)
        .is_ok_and(|ticket| ticket.hyphenated().to_string() == ticket_id);

    url.scheme() == "http"
        && loopback
        && url.port().is_some_and(|port| port != 0)
        && url.username().is_empty()
        && url.password().is_none()
        && !route.is_empty()
        && !route.contains('/')
        && canonical_ticket_id
        && valid_extension
        && url.query().is_none()
        && url.fragment().is_none()
}

/// Apply and verify the source properties that keep a protected ticket local.
/// The round-trip check makes an older or alternate HTTP plugin fail closed
/// instead of silently accepting a property value it cannot enforce.
fn configure_protected_loopback_source(source: &gst::Element) -> bool {
    let is_soup_http = source
        .factory()
        .is_some_and(|factory| factory.name() == "souphttpsrc");
    let required = ["proxy", "retries", "timeout"];
    if !is_soup_http
        || required
            .iter()
            .any(|property| source.find_property(property).is_none())
    {
        return false;
    }

    source.set_property("proxy", DIRECT_PROXY_SENTINEL);
    source.set_property("retries", 0_i32);
    source.set_property("timeout", PROTECTED_LOOPBACK_TIMEOUT_SECONDS);

    source
        .property_value("proxy")
        .get::<String>()
        .is_ok_and(|proxy| proxy.starts_with("direct:"))
        && source.property::<i32>("retries") == 0
        && source.property::<u32>("timeout") == PROTECTED_LOOPBACK_TIMEOUT_SECONDS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipelineErrorSourceCategory {
    Network,
    Decoder,
    AudioOutput,
    Pipeline,
}

impl PipelineErrorSourceCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network-source",
            Self::Decoder => "decoder",
            Self::AudioOutput => "audio-sink",
            Self::Pipeline => "pipeline",
        }
    }

    fn ui_message(self) -> String {
        let locale = rust_i18n::locale();
        self.ui_message_for_locale(&locale)
    }

    fn ui_message_for_locale(self, locale: &str) -> String {
        match self {
            Self::Network => {
                rust_i18n::t!("errors.playback.network_request_failed", locale = locale)
            }
            Self::Decoder => rust_i18n::t!("errors.playback.decoder_failed", locale = locale),
            Self::AudioOutput => {
                rust_i18n::t!("errors.playback.audio_output_failed", locale = locale)
            }
            Self::Pipeline => rust_i18n::t!("errors.playback.playback_failed", locale = locale),
        }
        .into_owned()
    }
}

fn pipeline_error_source_category(message: &gst::MessageRef) -> PipelineErrorSourceCategory {
    let Some(element) = message
        .src()
        .and_then(|source| source.downcast_ref::<gst::Element>())
    else {
        return PipelineErrorSourceCategory::Pipeline;
    };
    let Some(klass) = element
        .factory()
        .and_then(|factory| factory.metadata("klass").map(str::to_owned))
    else {
        return PipelineErrorSourceCategory::Pipeline;
    };

    pipeline_error_source_category_from_klass(&klass)
}

fn pipeline_error_source_category_from_klass(klass: &str) -> PipelineErrorSourceCategory {
    if klass.contains("Network") && klass.contains("Source") {
        PipelineErrorSourceCategory::Network
    } else if klass.contains("Decoder") || klass.contains("Demuxer") || klass.contains("Parser") {
        PipelineErrorSourceCategory::Decoder
    } else if klass.contains("Audio") && klass.contains("Sink") {
        PipelineErrorSourceCategory::AudioOutput
    } else {
        PipelineErrorSourceCategory::Pipeline
    }
}

/// Map GStreamer's quark to a closed category. The underlying error message is
/// deliberately never read because it may retain the authenticated URI.
fn pipeline_error_domain(error: &glib::Error) -> &'static str {
    use glib::error::ErrorDomain;

    let domain = error.domain();
    if domain == gst::CoreError::domain() {
        "core"
    } else if domain == gst::LibraryError::domain() {
        "library"
    } else if domain == gst::ResourceError::domain() {
        "resource"
    } else if domain == gst::StreamError::domain() {
        "stream"
    } else {
        "other"
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

#[cfg(test)]
mod tests {
    use super::*;

    const PROXY_BYPASS_CHILD: &str = "TRIBUTARY_PROXY_BYPASS_CHILD";
    const PROXY_BYPASS_CHILD_VALUE: &str = "tributary-proxy-bypass-child-v1";

    fn serve_one_test_request(
        listener: std::net::TcpListener,
        response: &'static [u8],
        observed: std::sync::mpsc::Sender<bool>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        timeout: Option<Duration>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            use std::io::{Read, Write};

            listener
                .set_nonblocking(true)
                .expect("set test listener nonblocking");
            let deadline = timeout.map(|timeout| Instant::now() + timeout);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut request = [0_u8; 4096];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(response);
                        let _ = stream.flush();
                        let _ = observed.send(true);
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop.load(std::sync::atomic::Ordering::Acquire) {
                            break;
                        }
                        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            let _ = observed.send(false);
        })
    }

    fn run_proxy_bypass_child() {
        let target = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind target listener");
        let target_addr = target.local_addr().expect("target listener address");
        let ticket = format!("http://{target_addr}/cast/550e8400-e29b-41d4-a716-446655440000.flac");

        gst::init().expect("GStreamer init");
        let source = gst::ElementFactory::make("souphttpsrc")
            .build()
            .expect("packaged souphttpsrc");
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("GStreamer fakesink");
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("playbin").build())
            .expect("GStreamer playbin");

        source.set_property("location", &ticket);
        Player::install_loopback_http_source_policy(&playbin);
        playbin.emit_by_name::<()>("source-setup", &[&source]);
        assert!(source.property::<String>("proxy").starts_with("direct:"));
        // Keep a broken-policy child bounded independently of the production
        // 30-second downstream budget.
        source.set_property("timeout", 2_u32);

        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([&source, &sink])
            .expect("assemble proxy bypass pipeline");
        source.link(&sink).expect("link proxy bypass pipeline");

        // Start the bounded observation window only after process startup,
        // GStreamer initialization, and plugin discovery have completed.
        // Those operations can exceed several seconds on a cold Windows host.
        let (target_tx, target_rx) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let target_thread = serve_one_test_request(
            target,
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\ntest",
            target_tx,
            std::sync::Arc::clone(&stop),
            Some(Duration::from_secs(8)),
        );

        pipeline
            .set_state(gst::State::Playing)
            .expect("start proxy bypass pipeline");
        let bus = pipeline.bus().expect("proxy bypass pipeline bus");
        // The target observation, not the terminal message kind, proves the
        // request reached the intended fixture. The parent process separately
        // proves that the poisoned proxy was never contacted.
        // Some packaged source/plugin combinations report a downstream error
        // after the complete HTTP body has already reached `fakesink`; treating
        // that as proxy use made this security regression flaky on Windows.
        let _terminal = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("proxy bypass pipeline reaches a terminal state");
        let _ = pipeline.set_state(gst::State::Null);

        // Do not cancel the target listener until it has recorded the route (or
        // exhausted its own deadline). This avoids racing an accepted request
        // during Windows process/thread teardown.
        let target_observed = target_rx
            .recv_timeout(Duration::from_secs(9))
            .expect("target listener result");
        stop.store(true, std::sync::atomic::Ordering::Release);
        target_thread.join().expect("target listener thread");

        assert!(
            target_observed,
            "the loopback media fixture was not reached"
        );
    }

    // ── slider_to_pipeline tests ────────────────────────────────────

    #[test]
    fn test_slider_to_pipeline_zero() {
        assert!((slider_to_pipeline(0.0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_slider_to_pipeline_one() {
        assert!((slider_to_pipeline(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_slider_to_pipeline_half() {
        // 0.5^3 = 0.125
        assert!((slider_to_pipeline(0.5) - 0.125).abs() < 1e-10);
    }

    #[test]
    fn test_slider_to_pipeline_monotonic() {
        // The cubic curve should be monotonically increasing.
        let mut prev = slider_to_pipeline(0.0);
        for i in 1..=100 {
            let val = slider_to_pipeline(i as f64 / 100.0);
            assert!(val >= prev, "slider_to_pipeline should be monotonic");
            prev = val;
        }
    }

    // ── Volume persistence helpers ──────────────────────────────────

    #[test]
    fn test_volume_path_returns_some() {
        // On any system with a data directory, this should return Some.
        // (May fail in extremely minimal CI environments.)
        let path = volume_path();
        if let Some(p) = path {
            assert!(p.to_string_lossy().contains("tributary"));
            assert!(p.to_string_lossy().contains("volume"));
        }
    }

    #[test]
    fn only_opaque_tributary_loopback_tickets_receive_direct_routing() {
        let ticket = "550e8400-e29b-41d4-a716-446655440000.flac";
        assert!(is_protected_loopback_ticket_uri(&format!(
            "http://127.0.0.1:53123/cast/{ticket}"
        )));
        assert!(is_protected_loopback_ticket_uri(&format!(
            "http://[::1]:53123/cast/{ticket}"
        )));

        for rejected in [
            format!("https://127.0.0.1:53123/cast/{ticket}"),
            format!("http://192.168.1.5:53123/cast/{ticket}"),
            format!("http://127.0.0.1:53123/radio/{ticket}"),
            "http://127.0.0.1:53123/cast/not-a-ticket".to_string(),
            format!("http://127.0.0.1:53123/cast/{ticket}.exe"),
            format!("http://127.0.0.1:53123/cast/{ticket}.flac.exe"),
            format!("http://127.0.0.1:53123/cast/{ticket}.FLAC"),
            "http://127.0.0.1:53123/cast/550e8400e29b41d4a716446655440000.flac".to_string(),
            format!("http://user@127.0.0.1:53123/cast/{ticket}"),
            format!("http://127.0.0.1:53123/cast/{ticket}?forward=1"),
            format!("http://127.0.0.1:53123/cast/{ticket}#fragment"),
        ] {
            assert!(
                !is_protected_loopback_ticket_uri(&rejected),
                "non-ticket URI must retain normal proxy policy"
            );
        }
    }

    #[test]
    fn soup_source_policy_installs_and_verifies_a_direct_resolver() {
        gst::init().expect("GStreamer init");
        let Ok(source) = gst::ElementFactory::make("souphttpsrc").build() else {
            // Minimal development hosts may omit gst-plugins-good. Packaged
            // builds require it, and CI's package jobs exercise that contract.
            return;
        };

        source.set_property("proxy", "http://proxy.invalid:8080");
        source.set_property("retries", 3_i32);
        source.set_property("timeout", 15_u32);
        assert!(configure_protected_loopback_source(&source));
        assert!(source.property::<String>("proxy").starts_with("direct:"));
        assert_eq!(source.property::<i32>("retries"), 0);
        assert_eq!(
            source.property::<u32>("timeout"),
            PROTECTED_LOOPBACK_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn source_setup_signal_overrides_a_poisoned_ticket_proxy_before_open() {
        gst::init().expect("GStreamer init");
        let Ok(playbin) = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("playbin").build())
        else {
            return;
        };
        let Ok(source) = gst::ElementFactory::make("souphttpsrc").build() else {
            return;
        };
        Player::install_loopback_http_source_policy(&playbin);
        source.set_property(
            "location",
            "http://127.0.0.1:54321/cast/550e8400-e29b-41d4-a716-446655440000.flac",
        );
        source.set_property("proxy", "http://192.0.2.1:3128");

        playbin.emit_by_name::<()>("source-setup", &[&source]);

        assert!(source.property::<String>("proxy").starts_with("direct:"));
        assert_eq!(source.property::<i32>("retries"), 0);
        assert_eq!(
            source.property::<u32>("timeout"),
            PROTECTED_LOOPBACK_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn protected_loopback_source_bypasses_a_poisoned_ambient_proxy() {
        if std::env::var(PROXY_BYPASS_CHILD).as_deref() == Ok(PROXY_BYPASS_CHILD_VALUE) {
            run_proxy_bypass_child();
            return;
        }

        let poison = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind poison proxy listener");
        let poison_addr = poison.local_addr().expect("poison listener address");
        let proxy = format!("http://{poison_addr}");
        let (poison_tx, poison_rx) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The proxy fixture is stop-driven rather than deadline-driven so cold
        // child startup and plugin discovery cannot make it disappear early.
        let poison_thread = serve_one_test_request(
            poison,
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            poison_tx,
            std::sync::Arc::clone(&stop),
            None,
        );

        let output =
            std::process::Command::new(std::env::current_exe().expect("current test executable"))
                .args([
                    "--exact",
                    "audio::tests::protected_loopback_source_bypasses_a_poisoned_ambient_proxy",
                    "--nocapture",
                ])
                .env(PROXY_BYPASS_CHILD, PROXY_BYPASS_CHILD_VALUE)
                .env("http_proxy", &proxy)
                .env("HTTP_PROXY", &proxy)
                .env_remove("no_proxy")
                .env_remove("NO_PROXY")
                .output()
                .expect("run isolated proxy bypass child");
        stop.store(true, std::sync::atomic::Ordering::Release);
        let poison_observed = poison_rx
            .recv_timeout(Duration::from_secs(9))
            .expect("poison listener result");
        poison_thread.join().expect("poison listener thread");

        assert!(
            !poison_observed,
            "the opaque loopback ticket reached the ambient proxy"
        );
        assert!(
            output.status.success(),
            "isolated GStreamer child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn pipeline_diagnostics_use_closed_categories_and_fixed_ui_text() {
        assert_eq!(
            pipeline_error_source_category_from_klass("Source/Network"),
            PipelineErrorSourceCategory::Network
        );
        assert_eq!(
            pipeline_error_source_category_from_klass("Codec/Decoder/Audio"),
            PipelineErrorSourceCategory::Decoder
        );
        assert_eq!(
            pipeline_error_source_category_from_klass("Sink/Audio"),
            PipelineErrorSourceCategory::AudioOutput
        );
        assert_eq!(
            pipeline_error_source_category_from_klass("Generic/Bin"),
            PipelineErrorSourceCategory::Pipeline
        );

        let secret = "https://music.invalid/stream?token=must-not-escape";
        let error = glib::Error::new(gst::ResourceError::OpenRead, secret);
        assert_eq!(pipeline_error_domain(&error), "resource");
        for category in [
            PipelineErrorSourceCategory::Network,
            PipelineErrorSourceCategory::Decoder,
            PipelineErrorSourceCategory::AudioOutput,
            PipelineErrorSourceCategory::Pipeline,
        ] {
            assert!(!category.as_str().contains(secret));
            assert!(!category.ui_message().contains(secret));
        }
    }

    #[test]
    fn pipeline_error_messages_are_localized_for_every_catalog() {
        for category in [
            PipelineErrorSourceCategory::Network,
            PipelineErrorSourceCategory::Decoder,
            PipelineErrorSourceCategory::AudioOutput,
            PipelineErrorSourceCategory::Pipeline,
        ] {
            let english = category.ui_message_for_locale("en");
            assert!(!english.is_empty());

            for locale in rust_i18n::available_locales!() {
                let localized = category.ui_message_for_locale(&locale);
                assert!(!localized.is_empty(), "{locale} is empty for {category:?}");
                if locale != "en" {
                    assert_ne!(
                        localized, english,
                        "{locale} must not fall back to English for {category:?}"
                    );
                }
            }
        }
    }
}
