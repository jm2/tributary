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
pub mod equalizer;
mod gstreamer_media;
pub mod local_output;
#[cfg(any(target_os = "macos", test))]
mod macos_audio;
pub mod mpd_output;
pub mod output;
#[cfg(target_os = "windows")]
mod runtime_probe;
#[cfg(target_os = "windows")]
#[allow(clippy::redundant_pub_crate)]
pub(crate) use runtime_probe::run_packaged_windows_runtime_probe;
#[cfg(test)]
pub mod test_support;
#[cfg(any(target_os = "windows", test))]
mod windows_audio;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use tracing::{debug, error, info, warn};
use url::{Host, Url};

use self::equalizer::{EqSettings, Preset};
use self::gstreamer_media::{GstreamerMediaProxy, GstreamerMediaTicket};
use crate::architecture::media::ResolvedHttpRequest;
use crate::local::resolver::ResolvedLocalMedia;

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

/// Live equalizer engine state owned by the local player.
///
/// `settings` mirrors the persisted contract state; `chain` is present
/// exactly while an equalizer bin is installed at `playbin3.audio-filter`
/// (i.e. only when the equalizer is enabled on the local output).
/// `save_generation` implements the trailing-edge 750 ms debounce: every
/// change re-arms the timer and only the newest generation writes.
#[derive(Default)]
struct EqEngineState {
    settings: EqSettings,
    chain: Option<equalizer::EqChain>,
    save_generation: u64,
}

/// GStreamer playback engine.
///
/// Wraps a `playbin3` (with `playbin` fallback) and exposes a safe,
/// main-thread-only control surface.  State updates are pushed through
/// the [`async_channel::Receiver`] returned by [`Player::new`].
pub struct Player {
    /// Retains the CoreAudio default-output listener and the explicitly
    /// configured `osxaudiosink`. `Player::drop` retires it before taking the
    /// pipeline to `NULL`.
    #[cfg(target_os = "macos")]
    macos_audio_route: Option<macos_audio::MacosAudioRoute>,
    playbin: gst::Element,
    volume: Rc<Cell<f64>>,
    /// Allows at most one warning-triggered sink reconnect until a new load
    /// or an observed system-device change establishes a fresh boundary.
    sink_recovery_claimed: Rc<Cell<bool>>,
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
    /// Equalizer contract state: persisted settings mirror plus the
    /// installed filter-bin handles and the debounce generation.
    eq_state: Rc<RefCell<EqEngineState>>,
    /// The watch is replaced on every URI load. Each watch captures that
    /// load's generation, so even an already-queued message from the previous
    /// pipeline incarnation remains identifiable as stale.
    bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,
    /// Retains the Windows device monitor and its bus watch for the lifetime
    /// of the local playback pipeline.
    #[cfg(target_os = "windows")]
    _windows_audio_route: Option<windows_audio::WindowsAudioRoute>,
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

        #[cfg(target_os = "macos")]
        let macos_audio_route = macos_audio::MacosAudioRoute::install(&playbin);

        let volume = Rc::new(Cell::new(load_saved_volume().unwrap_or(1.0)));
        let sink_recovery_claimed = Rc::new(Cell::new(false));

        // Load the persisted equalizer state. A malformed file has already
        // been replaced with the default state (atomic replace) and reports
        // a single bounded diagnostic here.
        let eq_settings = match equalizer::load_equalizer_settings_from_disk() {
            equalizer::EqLoadOutcome::Loaded(settings) => settings,
            equalizer::EqLoadOutcome::ReplacedWithDefaults {
                settings,
                diagnostic,
            } => {
                warn!(
                    path = %diagnostic.path,
                    byte_count = diagnostic.byte_count,
                    bad_key = %diagnostic.bad_key,
                    "Malformed equalizer.cfg replaced with default state"
                );
                settings
            }
        };
        let eq_state = Rc::new(RefCell::new(EqEngineState {
            settings: eq_settings,
            chain: None,
            save_generation: 0,
        }));

        #[cfg(target_os = "windows")]
        let windows_audio_route = windows_audio::WindowsAudioRoute::install(
            &playbin,
            Rc::clone(&volume),
            Rc::clone(&sink_recovery_claimed),
        );

        playbin.set_property("volume", slider_to_pipeline(volume.get()));

        let (event_tx, event_rx) = async_channel::unbounded();

        let event_generation = Rc::new(Cell::new(PlayerEventGeneration::default()));
        Self::start_position_timer(&playbin, &event_tx, Rc::clone(&event_generation));

        let player = Self {
            #[cfg(target_os = "macos")]
            macos_audio_route,
            playbin,
            volume,
            sink_recovery_claimed,
            event_tx,
            media_proxy: Arc::new(GstreamerMediaProxy::new(Some(rt_handle))),
            event_generation,
            volume_save_pending: Rc::new(Cell::new(None)),
            eq_state,
            bus_watch: RefCell::new(None),
            #[cfg(target_os = "windows")]
            _windows_audio_route: windows_audio_route,
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

    /// Load an exact local-library file through an app-owned handle-backed
    /// loopback ticket. The GStreamer source never reopens the database path.
    pub fn load_local(&self, media: ResolvedLocalMedia) {
        tracing::debug!("Loading authorized local track");
        let generation = self.begin_load();
        let prepared = self.media_proxy.prepare_local(media);
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
            .set_property("volume", slider_to_pipeline(self.volume.get()));
        // The equalizer bin persists across URI transitions; only a
        // missing chain (first enable, or retired after an error) is
        // rebuilt here, before the pipeline leaves NULL.
        self.ensure_equalizer_installed();
        self.sink_recovery_claimed.set(false);
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
            Rc::clone(&self.volume),
            Rc::clone(&self.sink_recovery_claimed),
            Rc::clone(&self.eq_state),
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
    pub fn set_volume(&self, level: f64) {
        self.volume.set(level.clamp(0.0, 1.0));
        self.playbin
            .set_property("volume", slider_to_pipeline(self.volume.get()));
        self.save_volume_debounced();
        debug!(volume = self.volume.get(), "Volume set");
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
        self.volume_save_pending.set(Some(self.volume.get()));
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
        self.volume.get()
    }

    // ── Equalizer (docs/equalizer.md contract) ──────────────────────

    /// Current in-memory equalizer state (mirrors the persisted file).
    pub fn equalizer_settings(&self) -> EqSettings {
        self.eq_state.borrow().settings
    }

    /// Force a re-read of `equalizer.cfg` — the settings UI's only
    /// escape hatch from a malformed on-disk file. Returns the state now
    /// in effect and, if the pipeline is live, re-applies it.
    pub fn reload_equalizer_settings(&self) -> EqSettings {
        let settings = match equalizer::load_equalizer_settings_from_disk() {
            equalizer::EqLoadOutcome::Loaded(settings) => settings,
            equalizer::EqLoadOutcome::ReplacedWithDefaults {
                settings,
                diagnostic,
            } => {
                warn!(
                    path = %diagnostic.path,
                    byte_count = diagnostic.byte_count,
                    bad_key = %diagnostic.bad_key,
                    "Malformed equalizer.cfg replaced with default state"
                );
                settings
            }
        };
        self.apply_equalizer_settings(settings);
        settings
    }

    /// Apply a new equalizer state per the live-reconfiguration boundary:
    ///
    /// - `Enabled` and `Clip protection` toggles change the bin topology,
    ///   so they run through the pause → surgery → resume seam.
    /// - Band, preamp, and preset changes are buffer-boundary
    ///   property-write transactions (freeze/thaw) on the running bin.
    /// - The new state is persisted through the trailing-edge debounce,
    ///   suppressed entirely when the state is the fresh-install default.
    ///
    /// A configuration update never emits a `Buffering` event: GObject
    /// property writes produce no pipeline event, and any `Buffering`
    /// observed on the bus originates from the upstream decoder.
    pub fn apply_equalizer_settings(&self, next: EqSettings) {
        let current = self.eq_state.borrow().settings;
        let enabled_changed = current.enabled != next.enabled;
        let clip_changed = next.enabled && current.clip_protection != next.clip_protection;

        if enabled_changed || clip_changed {
            if next.enabled && !enabled_changed && clip_changed {
                // Limiter-only topology change inside the installed bin.
                self.with_pipeline_suspended(|| {
                    if let Some(chain) = self.eq_state.borrow_mut().chain.as_mut() {
                        let installed = chain.set_clip_protection(next.clip_protection);
                        info!(
                            clip_protection = ?next.clip_protection,
                            installed,
                            "Clip protection element toggled in local pipeline"
                        );
                        installed
                    } else {
                        false
                    }
                });
            } else if next.enabled {
                self.install_equalizer_bin(&next);
            } else {
                self.uninstall_equalizer_bin();
            }
        }

        if next.enabled {
            let state = self.eq_state.borrow();
            if let Some(chain) = state.chain.as_ref() {
                chain.apply_band_transaction(&next);
            }
        }

        self.eq_state.borrow_mut().settings = next;
        self.schedule_equalizer_save();
    }

    /// Install a freshly built equalizer bin via the pause/relink seam,
    /// or directly when the pipeline is not running. Construction or
    /// negotiation failure degrades to the passthrough layout with a
    /// single informational diagnostic — never a half-inserted chain.
    fn install_equalizer_bin(&self, settings: &EqSettings) {
        match equalizer::EqChain::build(settings) {
            Ok(chain) => {
                let bin = chain.bin.clone();
                let installed = self.with_pipeline_suspended(|| {
                    self.playbin.set_property("audio-filter", Some(&bin));
                    true
                });
                if installed {
                    self.eq_state.borrow_mut().chain = Some(chain);
                    info!(
                        enabled = settings.enabled,
                        preset = Preset::key(settings.preset),
                        "Equalizer chain installed into local pipeline"
                    );
                }
            }
            Err(error) => {
                self.eq_state.borrow_mut().chain = None;
                self.playbin
                    .set_property("audio-filter", Option::<&gst::Element>::None);
                info!(
                    error = %error,
                    "Equalizer unavailable; local output remains passthrough"
                );
            }
        }
    }

    /// Remove the installed equalizer bin via the pause/relink seam.
    /// Persisted settings stay on disk untouched.
    fn uninstall_equalizer_bin(&self) {
        let had_chain = self.eq_state.borrow_mut().chain.take().is_some();
        if !had_chain {
            return;
        }
        self.with_pipeline_suspended(|| {
            self.playbin
                .set_property("audio-filter", Option::<&gst::Element>::None);
            true
        });
        info!(
            enabled = false,
            preset = Preset::key(self.eq_state.borrow().settings.preset),
            "Equalizer chain removed from local pipeline"
        );
    }

    /// Guarantee an installed bin before a fresh pipeline spins up (URI
    /// load). The bin persists across URI transitions — gapless album
    /// navigation keeps the same chain, and its property state is *not*
    /// re-applied automatically on each new URI.
    fn ensure_equalizer_installed(&self) {
        let (settings, missing) = {
            let state = self.eq_state.borrow();
            (state.settings, state.chain.is_none())
        };
        if settings.enabled && missing {
            self.install_equalizer_bin(&settings);
        }
    }

    /// Run one pipeline-topology edit through the pause → edit → resume
    /// seam. A running pipeline is paused and given a bounded window to
    /// settle; a NULL pipeline (idle player) skips straight to the edit.
    fn with_pipeline_suspended<F: FnOnce() -> bool>(&self, edit: F) -> bool {
        let (_, current, _) = self.playbin.state(gst::ClockTime::ZERO);
        let was_playing = current == gst::State::Playing;
        if was_playing {
            let _ = self.playbin.set_state(gst::State::Paused);
            // Bounded settle — a topology edit must never wedge the UI.
            let _ = self.playbin.state(gst::ClockTime::from_seconds(1));
        }
        let result = edit();
        if was_playing {
            let _ = self.playbin.set_state(gst::State::Playing);
        }
        result
    }

    /// Persist the equalizer state on the trailing edge of a change
    /// spell: every change re-arms the 750 ms timer, and only the newest
    /// generation actually writes. Suppressed entirely while the state
    /// matches the fresh-install default.
    fn schedule_equalizer_save(&self) {
        let next_generation = self.eq_state.borrow().save_generation.wrapping_add(1);
        self.eq_state.borrow_mut().save_generation = next_generation;
        if self.eq_state.borrow().settings.is_fresh_default() {
            return;
        }
        let state = Rc::clone(&self.eq_state);
        glib::timeout_add_local_once(
            Duration::from_millis(equalizer::SAVE_DEBOUNCE_MS),
            move || {
                let state = state.borrow();
                if state.save_generation == next_generation && !state.settings.is_fresh_default() {
                    equalizer::save_equalizer_settings_to_disk(&state.settings);
                }
            },
        );
    }

    /// Shutdown flush: synchronously write the current state (unless it
    /// is the fresh-install default) before the pipeline is retired, so
    /// quitting with a pending debounce never loses the last change.
    fn flush_equalizer_for_shutdown(&self) {
        let state = self.eq_state.borrow();
        if !state.settings.is_fresh_default() {
            equalizer::save_equalizer_settings_to_disk(&state.settings);
        }
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
    #[allow(clippy::too_many_arguments)] // mirrors the Player field set the watch captures
    fn attach_bus_watch(
        playbin: &gst::Element,
        event_tx: &async_channel::Sender<PlayerEvent>,
        generation: PlayerEventGeneration,
        media_proxy: Arc<GstreamerMediaProxy>,
        media_ticket: Option<Arc<GstreamerMediaTicket>>,
        volume: Rc<Cell<f64>>,
        sink_recovery_claimed: Rc<Cell<bool>>,
        eq_state: Rc<RefCell<EqEngineState>>,
    ) -> anyhow::Result<gst::bus::BusWatchGuard> {
        let bus = playbin
            .bus()
            .ok_or_else(|| anyhow::anyhow!("playbin has no bus"))?;

        let tx = event_tx.clone();
        let playbin_name = playbin.name();
        let started_at = Instant::now();
        let playbin_for_eq = playbin.downgrade();
        #[cfg(any(target_os = "windows", test))]
        let playbin_for_recovery = playbin.downgrade();
        #[cfg(not(any(target_os = "windows", test)))]
        let _ = (&volume, &sink_recovery_claimed);

        bus.add_watch_local(move |_bus, msg| {
            use gst::MessageView;

            #[cfg(any(target_os = "windows", test))]
            if playbin_for_recovery.upgrade().is_some_and(|playbin| {
                windows_audio::recover_warning(msg, &playbin, volume.get(), &sink_recovery_claimed)
            }) {
                return glib::ControlFlow::Continue;
            }

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
                    // The equalizer chain may itself be the failure (e.g.
                    // a non-PCM source that cannot deliver the pinned
                    // F32LE stereo caps); roll back to the passthrough
                    // layout for all subsequent loads.
                    if let Some(playbin) = playbin_for_eq.upgrade() {
                        let had_chain = eq_state.borrow_mut().chain.take().is_some();
                        if had_chain {
                            playbin.set_property("audio-filter", Option::<&gst::Element>::None);
                            info!(
                                "Equalizer chain retired after pipeline error; passthrough restored"
                            );
                        }
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
        // Shutdown flush: persist the current equalizer state (unless it
        // is the fresh-install default) even if the debounce timer is
        // still armed, so quitting never loses the last change.
        self.flush_equalizer_for_shutdown();
        #[cfg(target_os = "macos")]
        drop(self.macos_audio_route.take());
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
