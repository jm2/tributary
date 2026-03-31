//! Platform-specific media controls abstraction.
//!
//! Provides a unified interface for OS-level media key integration:
//! - **Linux:** MPRIS over D-Bus (`mpris-server`)
//! - **Windows:** System Media Transport Controls (`windows` crate)
//! - **macOS:** `MPNowPlayingInfoCenter` / `MPRemoteCommandCenter` (`objc2`)
//!
//! The implementation files are compiled conditionally via `#[cfg(target_os)]`.
//! During Phase 1, all backends are stubs that log calls and return `Ok(())`.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shared Types
// ---------------------------------------------------------------------------

/// Events dispatched from the operating system to the application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsMediaEvent {
    Play,
    Pause,
    TogglePlayPause,
    Next,
    Previous,
    Stop,
    SeekForward,
    SeekBackward,
}

/// Playback state reported *to* the operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

/// Track metadata broadcast to the OS lock screen / control centre.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowPlayingMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    /// OS APIs generally prefer a local `file://` URI for artwork.
    pub cover_art_uri: Option<String>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Contract fulfilled by each platform-specific media controls backend.
pub trait MediaControlsBackend: Send + Sync {
    /// Push current track metadata to the OS.
    fn update_metadata(&self, metadata: &NowPlayingMetadata) -> anyhow::Result<()>;

    /// Inform the OS of the current playback state.
    fn update_playback_state(&self, state: PlaybackState) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Stub Backends (Phase 1 — no-op implementations)
// ---------------------------------------------------------------------------

/// Stub backend that logs calls but does nothing.
struct StubMediaControls {
    platform: &'static str,
}

impl StubMediaControls {
    fn new(platform: &'static str) -> Self {
        tracing::info!("Initialising stub media controls for {platform}");
        Self { platform }
    }
}

impl MediaControlsBackend for StubMediaControls {
    fn update_metadata(&self, metadata: &NowPlayingMetadata) -> anyhow::Result<()> {
        tracing::debug!(
            platform = self.platform,
            title = %metadata.title,
            "stub: update_metadata"
        );
        Ok(())
    }

    fn update_playback_state(&self, state: PlaybackState) -> anyhow::Result<()> {
        tracing::debug!(
            platform = self.platform,
            ?state,
            "stub: update_playback_state"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Initialise the platform-appropriate media controls backend.
///
/// Returns the backend handle (to push metadata outward) and a channel
/// receiver for incoming OS media-key events.
///
/// In Phase 1 all backends are stubs; the channel will never yield events.
pub fn init_system_media_controls(
) -> anyhow::Result<(
    Box<dyn MediaControlsBackend>,
    tokio::sync::mpsc::Receiver<OsMediaEvent>,
)> {
    let (tx, rx) = tokio::sync::mpsc::channel::<OsMediaEvent>(32);

    // Keep `tx` alive (leak it) so the receiver doesn't immediately close.
    // Real implementations will move `tx` into their event-listener loops.
    let _tx = tx;

    #[cfg(target_os = "linux")]
    let backend: Box<dyn MediaControlsBackend> =
        Box::new(StubMediaControls::new("linux/mpris"));

    #[cfg(target_os = "windows")]
    let backend: Box<dyn MediaControlsBackend> =
        Box::new(StubMediaControls::new("windows/smtc"));

    #[cfg(target_os = "macos")]
    let backend: Box<dyn MediaControlsBackend> =
        Box::new(StubMediaControls::new("macos/nowplaying"));

    // Fallback for other/unsupported platforms (e.g., FreeBSD).
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let backend: Box<dyn MediaControlsBackend> =
        Box::new(StubMediaControls::new("unknown"));

    Ok((backend, rx))
}
