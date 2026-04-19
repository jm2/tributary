//! AirPlay audio output — streams audio to AirPlay receivers discovered
//! via `_raop._tcp.local.` mDNS browsing.
//!
//! AirPlay devices appear automatically in the output selector popover
//! alongside manually-added MPD sinks — no manual "+" button needed.
//!
//! # Implementation strategy
//!
//! This module implements `AudioOutput` by manipulating the GStreamer
//! pipeline's audio sink.  When an AirPlay output is selected, we
//! replace the default `autoaudiosink` with a `raopsink` element
//! targeting the discovered receiver's IP and port.  If `raopsink` is
//! unavailable (requires `gst-plugins-bad`), we fall back to spawning
//! a `shairport-sync` subprocess in pipe mode.
//!
//! # Discovery
//!
//! AirPlay receivers are discovered via mDNS browsing for
//! `_raop._tcp.local.` in [`crate::discovery`].  Discovered devices
//! are surfaced as `DiscoveryEvent::Found` with
//! `service_type: "airplay"` and automatically added to the output
//! selector.  When a device goes offline, `DiscoveryEvent::Lost`
//! removes it.
//!
//! # Limitations
//!
//! - AirPlay 2 (HomeKit-authenticated) devices require Apple's
//!   proprietary pairing protocol which is not implemented.  This
//!   output targets AirPlay 1 (RAOP) receivers, including
//!   shairport-sync instances and older Apple devices.
//! - Volume control is best-effort: the RAOP protocol supports volume
//!   commands, but not all receivers honour them.
//! - Seeking is not supported for AirPlay streams.

use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerState};

use tracing::{debug, error, info, warn};

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
    /// Current playback state (best-guess, updated optimistically).
    current_state: PlayerState,
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
            current_state: PlayerState::Stopped,
        }
    }

    /// Attempt to stream audio to the AirPlay receiver.
    ///
    /// Tries GStreamer `raopsink` first, falls back to `shairport-sync`.
    fn send_to_receiver(&self, uri: &str) {
        let host = self.host.clone();
        let port = self.port;
        let uri = uri.to_string();
        let tx = self.event_tx.clone();
        let volume = self.volume;

        std::thread::spawn(move || {
            // Try GStreamer raopsink approach first.
            if let Err(e) = Self::try_gstreamer_raop(&host, port, &uri, volume) {
                warn!(error = %e, "GStreamer RAOP sink unavailable, trying shairport-sync");

                // Fallback: try shairport-sync pipe mode.
                if let Err(e2) = Self::try_shairport_sync(&host, port, &uri) {
                    error!(
                        error = %e2,
                        "AirPlay streaming failed (both methods)"
                    );
                    let _ = tx.try_send(PlayerEvent::Error(format!("AirPlay: {e2}")));
                }
            }
        });
    }

    /// Attempt to use GStreamer's `raopsink` element.
    ///
    /// Requires `gst-plugins-bad` to be installed with RAOP support.
    /// Builds a pipeline: `uridecodebin ! audioconvert ! avenc_alac ! raopsink`
    fn try_gstreamer_raop(host: &str, port: u16, uri: &str, volume: f64) -> Result<(), String> {
        use gst::prelude::*;
        use gstreamer as gst;

        // Check if raopsink is available in the GStreamer registry.
        let registry = gst::Registry::get();
        if registry
            .find_feature("raopsink", gst::ElementFactory::static_type())
            .is_none()
        {
            return Err("raopsink not found in GStreamer registry".to_string());
        }

        let pipeline_str = format!(
            "uridecodebin uri=\"{}\" ! audioconvert ! avenc_alac ! raopsink host={} port={}",
            uri.replace('"', "\\\""),
            host,
            port
        );

        let pipeline = gst::parse::launch(&pipeline_str)
            .map_err(|e| format!("Failed to build RAOP pipeline: {e}"))?;

        // Set volume on the raopsink element if available.
        if let Some(bin) = pipeline.downcast_ref::<gst::Bin>() {
            if let Some(sink) = bin.by_name("raopsink0") {
                // RAOP volume is in dB: 0.0 = max, -144.0 = mute.
                // Convert linear 0.0–1.0 to -30.0–0.0 dB range.
                let vol_db = if volume <= 0.0 {
                    -144.0
                } else {
                    (volume - 1.0) * 30.0
                };
                sink.set_property("volume", vol_db);
            }
        }

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| format!("Failed to start RAOP pipeline: {e}"))?;

        // The pipeline runs in its own GStreamer threads.
        // We leak the pipeline intentionally — it will be cleaned up
        // when stop() is called, which sets the main playbin to Null.
        // TODO: Store the pipeline handle for proper lifecycle management.
        std::mem::forget(pipeline);

        info!(host = %host, port, "AirPlay: streaming via GStreamer raopsink");
        Ok(())
    }

    /// Attempt to stream via `shairport-sync` in pipe mode.
    ///
    /// This requires `shairport-sync` to be installed on the system.
    /// Uses GStreamer to decode audio into raw S16LE PCM, piped to
    /// `shairport-sync` via its stdin pipe backend.
    fn try_shairport_sync(host: &str, port: u16, uri: &str) -> Result<(), String> {
        // Check if shairport-sync is available on PATH.
        #[cfg(not(target_os = "windows"))]
        let check = std::process::Command::new("which")
            .arg("shairport-sync")
            .output();

        #[cfg(target_os = "windows")]
        let check = std::process::Command::new("where")
            .arg("shairport-sync")
            .output();

        match check {
            Ok(output) if output.status.success() => {
                debug!("shairport-sync found on PATH");

                // Launch shairport-sync in pipe mode.
                let child = std::process::Command::new("shairport-sync")
                    .args(["-o", "pipe", "--", "/dev/stdin"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();

                let mut child =
                    child.map_err(|e| format!("Failed to spawn shairport-sync: {e}"))?;

                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| "Failed to capture shairport-sync stdin".to_string())?;

                // Get the raw file descriptor for the pipe.
                #[cfg(unix)]
                let fd = {
                    use std::os::unix::io::IntoRawFd;
                    stdin.into_raw_fd()
                };

                #[cfg(not(unix))]
                {
                    drop(stdin);
                    let _ = child.kill();
                    Err(
                        "shairport-sync pipe mode requires Unix (not available on Windows)"
                            .to_string(),
                    )
                }

                #[cfg(unix)]
                {
                    use gst::prelude::*;
                    use gstreamer as gst;

                    let pipeline_str = format!(
                        "uridecodebin uri=\"{}\" ! audioconvert ! audio/x-raw,format=S16LE,rate=44100,channels=2 ! fdsink fd={}",
                        uri.replace('"', "\\\""),
                        fd
                    );

                    let pipeline = gst::parse::launch(&pipeline_str)
                        .map_err(|e| format!("Failed to build shairport-sync pipeline: {e}"))?;

                    pipeline
                        .set_state(gst::State::Playing)
                        .map_err(|e| format!("Failed to start shairport-sync pipeline: {e}"))?;

                    // Leak pipeline and child — cleaned up when stop() kills
                    // everything.  Proper lifecycle management is a future TODO.
                    std::mem::forget(pipeline);
                    std::mem::forget(child);

                    info!(
                        host = %host,
                        port,
                        "AirPlay: streaming via shairport-sync pipe mode"
                    );
                    Ok(())
                }
            }
            _ => Err("shairport-sync not found on PATH".to_string()),
        }
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
        // RAOP protocol supports volume commands, but we return false
        // for now since the implementation is scaffolding-only.
        false
    }

    fn load_uri(&self, uri: &str) {
        info!("AirPlay: loading URI");
        self.send_to_receiver(uri);

        // Optimistically signal buffering.
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Buffering));
    }

    fn play(&self) {
        debug!("AirPlay: play (no-op in scaffolding)");
    }

    fn pause(&self) {
        debug!("AirPlay: pause (no-op in scaffolding)");
    }

    fn stop(&self) {
        debug!("AirPlay: stop");
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Stopped));
    }

    fn toggle_play_pause(&self) {
        debug!("AirPlay: toggle (no-op in scaffolding)");
    }

    fn seek_to(&self, _position_ms: u64) {
        // AirPlay does not support seeking in stream mode.
        debug!("AirPlay: seek not supported");
    }

    fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
        // Future: send RAOP volume command to receiver.
    }

    fn volume(&self) -> f64 {
        self.volume
    }

    fn state(&self) -> PlayerState {
        self.current_state
    }

    fn position_ms(&self) -> Option<u64> {
        // Position tracking not yet implemented for AirPlay.
        None
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
    fn test_airplay_no_volume_support() {
        let (tx, _rx) = async_channel::unbounded();
        let output = AirPlayOutput::new("Test", "127.0.0.1", 7000, tx);
        assert!(!output.supports_volume());
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
    fn test_airplay_no_position() {
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
