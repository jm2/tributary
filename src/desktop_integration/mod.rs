//! OS-level media transport controls via the `souvlaki` crate.
//!
//! Provides a [`MediaController`] that bridges the host OS media overlay
//! (MPRIS on Linux, SMTC on Windows, Now Playing on macOS) to the
//! application through an [`async_channel`].
//!
//! # Threading model
//!
//! `souvlaki` invokes its event callback from an internal thread.  The
//! callback forwards [`MediaAction`]s through an [`async_channel::Sender`]
//! (which is `Send`) so that the GTK main thread receives them safely
//! via the [`async_channel::Receiver`] returned by [`MediaController::new`].
//!
//! [`MediaController::update_metadata`] and [`MediaController::update_playback`]
//! must be called from the GTK main thread.

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use tracing::{debug, info, warn};

// ── Actions ─────────────────────────────────────────────────────────────

/// Actions received from the operating system's media transport controls.
///
/// These arrive on the GTK main thread via the [`async_channel::Receiver`]
/// returned by [`MediaController::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaAction {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
}

// ── Controller ──────────────────────────────────────────────────────────

/// Wraps `souvlaki::MediaControls` and routes OS media-key events to
/// the GTK main thread.
pub struct MediaController {
    controls: MediaControls,
}

impl MediaController {
    /// Register with the host OS and return the controller + a receiver
    /// for incoming [`MediaAction`]s.
    ///
    /// On Linux this creates an MPRIS D-Bus service named `tributary`.
    /// On Windows, pass the window HWND via [`PlatformConfig`] (TODO).
    ///
    /// The caller must consume the receiver on the GTK main thread via:
    /// ```ignore
    /// glib::MainContext::default().spawn_local(async move {
    ///     while let Ok(action) = media_rx.recv().await {
    ///         // handle MediaAction …
    ///     }
    /// });
    /// ```
    pub fn new() -> anyhow::Result<(Self, async_channel::Receiver<MediaAction>)> {
        let config = PlatformConfig {
            dbus_name: "tributary",
            display_name: "Tributary",
            hwnd: None, // Windows: requires real HWND for SMTC
        };

        let mut controls = MediaControls::new(config)
            .map_err(|e| anyhow::anyhow!("Failed to create media controls: {e:?}"))?;

        info!("OS media transport controls initialised");

        // ── Event channel: souvlaki callback thread → GTK main thread ──
        let (action_tx, action_rx) = async_channel::unbounded();

        controls
            .attach(move |event: MediaControlEvent| {
                let action = match event {
                    MediaControlEvent::Play => Some(MediaAction::Play),
                    MediaControlEvent::Pause => Some(MediaAction::Pause),
                    MediaControlEvent::Toggle => Some(MediaAction::Toggle),
                    MediaControlEvent::Next => Some(MediaAction::Next),
                    MediaControlEvent::Previous => Some(MediaAction::Previous),
                    MediaControlEvent::Stop => Some(MediaAction::Stop),
                    other => {
                        debug!("Unhandled media control event: {other:?}");
                        None
                    }
                };

                if let Some(a) = action {
                    debug!(?a, "OS media key received");
                    let _ = action_tx.try_send(a);
                }
            })
            .map_err(|e| anyhow::anyhow!("Failed to attach media controls handler: {e:?}"))?;

        // Publish initial (idle) state so the OS overlay is registered.
        controls
            .set_metadata(MediaMetadata {
                title: Some("Tributary"),
                artist: Some("No track loaded"),
                album: Some(""),
                ..Default::default()
            })
            .map_err(|e| anyhow::anyhow!("Failed to set initial metadata: {e:?}"))?;

        controls
            .set_playback(MediaPlayback::Stopped)
            .map_err(|e| anyhow::anyhow!("Failed to set initial playback state: {e:?}"))?;

        Ok((Self { controls }, action_rx))
    }

    // ── Outbound: app → OS overlay ──────────────────────────────────

    /// Push the current track's text metadata to the OS overlay.
    ///
    /// Per MVP scope this sends title, artist, and album only —
    /// no cover art URI.
    pub fn update_metadata(&mut self, title: &str, artist: &str, album: &str) {
        if let Err(e) = self.controls.set_metadata(MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album: Some(album),
            ..Default::default()
        }) {
            warn!("Failed to update media metadata: {e:?}");
        }
    }

    /// Inform the OS whether we are currently playing or paused.
    pub fn update_playback(&mut self, playing: bool) {
        let state = if playing {
            MediaPlayback::Playing { progress: None }
        } else {
            MediaPlayback::Paused { progress: None }
        };

        if let Err(e) = self.controls.set_playback(state) {
            warn!("Failed to update playback state: {e:?}");
        }
    }

    /// Tell the OS that playback has stopped entirely.
    pub fn set_stopped(&mut self) {
        if let Err(e) = self.controls.set_playback(MediaPlayback::Stopped) {
            warn!("Failed to set stopped state: {e:?}");
        }
    }
}
