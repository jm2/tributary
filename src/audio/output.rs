//! Audio output abstraction layer.
//!
//! Defines the [`AudioOutput`] trait that all playback backends implement,
//! and the [`OutputTarget`] enum for identifying output types.
//!
//! # Current implementations
//!
//! - [`LocalOutput`](super::local_output::LocalOutput) — wraps the existing
//!   GStreamer `playbin3` pipeline for local speaker output.
//! - [`MpdOutput`](super::mpd_output::MpdOutput) — sends MPD protocol
//!   commands over TCP to a remote (or local) MPD server.
//!
//! # Planned implementations
//!
//! - **AirPlay 2 output** — stream to AirPlay receivers discovered via
//!   `_raop._tcp.local.` mDNS browsing (shairport-sync compatible).
//!   Discovered devices will appear automatically in the output selector
//!   popover alongside manually-added MPD sinks.
//!
//! # Architecture
//!
//! The output layer is strictly a **sink** abstraction.  It controls
//! *where audio plays*, not *where music comes from*.  Library sources
//! (local, Subsonic, Jellyfin, Plex, DAAP, radio) are managed by the
//! sidebar and are completely independent of the active output.

use super::PlayerState;

/// Identifies the type of an audio output for UI purposes (icon, label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OutputType {
    /// Local GStreamer pipeline (system speakers / headphones).
    Local,
    /// MPD server (Music Player Daemon) over TCP.
    Mpd,
    // Future: AirPlay, PulseAudio, PipeWire, JACK, …
}

/// Trait that all audio output backends implement.
///
/// All methods are designed to be called from the **GTK main thread**.
/// Implementations that perform network I/O (e.g. MPD) must handle
/// that internally without blocking the main thread.
#[allow(dead_code)]
pub trait AudioOutput {
    /// Human-readable display name for the output selector UI.
    ///
    /// Examples: "My Computer", "Living Room MPD".
    fn name(&self) -> &str;

    /// The output type, used for icon selection in the popover.
    fn output_type(&self) -> OutputType;

    /// Whether this output supports application-controlled volume.
    ///
    /// When `false`, the header bar volume slider should be disabled
    /// (greyed out).  MPD manages its own volume independently.
    fn supports_volume(&self) -> bool;

    // ── Playback controls ───────────────────────────────────────────

    /// Load a URI and start playback.
    ///
    /// `uri` may be a `file:///…` path or an `http(s)://…` stream URL.
    fn load_uri(&self, uri: &str);

    /// Resume playback from a paused state.
    fn play(&self);

    /// Pause playback.
    fn pause(&self);

    /// Stop playback and reset to idle.
    fn stop(&self);

    /// Toggle between playing and paused states.
    fn toggle_play_pause(&self);

    /// Seek to an absolute position (milliseconds from start).
    fn seek_to(&self, position_ms: u64);

    // ── Volume ──────────────────────────────────────────────────────

    /// Set volume from a linear slider position (0.0–1.0).
    ///
    /// No-op if [`supports_volume`](Self::supports_volume) returns `false`.
    fn set_volume(&mut self, level: f64);

    /// Current volume (0.0–1.0).
    fn volume(&self) -> f64;

    // ── State queries ───────────────────────────────────────────────

    /// Non-blocking query of the current playback state.
    fn state(&self) -> PlayerState;

    /// Current playback position in milliseconds, or `None` if unknown.
    fn position_ms(&self) -> Option<u64>;
}
