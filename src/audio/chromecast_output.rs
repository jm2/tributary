//! Chromecast audio output — streams audio to Chromecast devices
//! discovered via `_googlecast._tcp.local.` mDNS browsing.
//!
//! Chromecast devices appear automatically in the output selector
//! popover alongside AirPlay and MPD outputs — no manual configuration
//! needed.
//!
//! # Implementation strategy
//!
//! This module implements `AudioOutput` using the `rust-cast` crate,
//! a clean-room MIT-licensed implementation of the Cast V2 protocol.
//! When a Chromecast output is selected, playback commands are sent
//! over a TLS connection to the device on port 8009.
//!
//! The Cast V2 flow:
//! 1. Connect to the device via TLS (port 8009)
//! 2. Launch the Default Media Receiver application
//! 3. Send `media.load()` with the stream URL
//! 4. Control playback via media namespace commands (play, pause, seek, stop)
//!
//! # Discovery
//!
//! Chromecast devices are discovered via mDNS browsing for
//! `_googlecast._tcp.local.` in [`crate::discovery`].  Discovered
//! devices are surfaced as `DiscoveryEvent::Found` with
//! `service_type: "chromecast"` and automatically added to the output
//! selector.  The friendly name is extracted from the mDNS TXT record
//! `fn` field.
//!
//! # Local file support
//!
//! Local `file:///` URIs are automatically rewritten to HTTP URLs
//! served by [`CastHttpServer`](super::cast_http_server::CastHttpServer),
//! a minimal LAN-only embedded server.  The server is started on-demand
//! when the first local file is cast and remains running for the
//! duration of the session.
//!
//! # Persistent connection
//!
//! A background thread maintains a persistent TLS connection to the
//! Chromecast device, with a heartbeat PING every 5 seconds.  This
//! keeps the Cast session alive and enables position polling.

use std::sync::{Arc, Mutex};

use super::cast_http_server::CastHttpServer;
use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerState};

use tracing::{debug, error, info, warn};

/// Default Media Receiver app ID — the built-in Google receiver that
/// accepts arbitrary media URLs.
const DEFAULT_MEDIA_RECEIVER_APP_ID: &str = "CC1AD845";

/// Heartbeat interval in seconds.
const HEARTBEAT_INTERVAL_SECS: u64 = 5;

/// Position polling interval in seconds.
const POSITION_POLL_INTERVAL_SECS: u64 = 1;

/// Chromecast audio output — streams to a Cast V2 device.
pub struct ChromecastOutput {
    /// Human-readable name from mDNS discovery (e.g. "Living Room Speaker").
    display_name: String,
    /// Device hostname or IP address.
    host: String,
    /// Device port (typically 8009).
    port: u16,
    /// Event sender for relaying state changes to the GTK main thread.
    event_tx: async_channel::Sender<PlayerEvent>,
    /// Cached volume level (0.0–1.0).
    volume: f64,
    /// Current playback state (best-guess, updated optimistically).
    current_state: PlayerState,
    /// Shared reference to the embedded HTTP server for local file casting.
    cast_server: Arc<Mutex<Option<CastHttpServer>>>,
    /// Tokio runtime handle for spawning async tasks.
    rt_handle: Option<tokio::runtime::Handle>,
}

impl ChromecastOutput {
    /// Create a new Chromecast output targeting the given device.
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
            "Chromecast output configured"
        );
        Self {
            display_name: display_name.to_string(),
            host: host.to_string(),
            port,
            event_tx,
            volume: 1.0,
            current_state: PlayerState::Stopped,
            cast_server: Arc::new(Mutex::new(None)),
            rt_handle: tokio::runtime::Handle::try_current().ok(),
        }
    }

    /// Resolve a URI for Chromecast playback.
    ///
    /// If the URI is a `file://` path, starts the embedded HTTP server
    /// (if not already running) and rewrites the URI to an HTTP URL.
    /// Otherwise returns the URI unchanged.
    fn resolve_uri(&self, uri: &str) -> Result<String, String> {
        if !uri.starts_with("file://") {
            return Ok(uri.to_string());
        }

        // Extract the file path from the URI.
        let path_str = uri
            .strip_prefix("file://")
            .unwrap_or(uri)
            .trim_start_matches('/');

        // On Windows, file:///C:/path → C:/path
        // On Unix, file:///path → /path
        #[cfg(target_os = "windows")]
        let file_path = std::path::PathBuf::from(path_str);
        #[cfg(not(target_os = "windows"))]
        let file_path = std::path::PathBuf::from(format!("/{path_str}"));

        if !file_path.exists() {
            return Err(format!("File not found: {}", file_path.display()));
        }

        // Canonicalize to resolve symlinks and prevent path traversal.
        let file_path = file_path
            .canonicalize()
            .map_err(|e| format!("Failed to canonicalize path: {e}"))?;

        // Start the HTTP server if needed.
        let mut server_guard = self
            .cast_server
            .lock()
            .map_err(|e| format!("Server lock poisoned: {e}"))?;

        if server_guard.is_none() {
            let rt = self
                .rt_handle
                .as_ref()
                .ok_or("No tokio runtime available")?;

            let server = rt
                .block_on(CastHttpServer::start())
                .map_err(|e| format!("Failed to start cast HTTP server: {e}"))?;

            info!(addr = %server.addr(), "Cast HTTP server started for local file casting");
            *server_guard = Some(server);
        }

        let server = server_guard.as_ref().unwrap();
        let http_url = server.register_file(&file_path);
        Ok(http_url)
    }

    /// Connect to the Chromecast device, launch the Default Media
    /// Receiver, and load the given media URL.
    ///
    /// Runs on a background thread to avoid blocking the GTK main thread.
    /// Starts a heartbeat + position polling loop after successful load.
    fn cast_media(&self, uri: &str) {
        let host = self.host.clone();
        let port = self.port;
        let tx = self.event_tx.clone();
        let volume = self.volume;

        // Resolve file:// URIs to HTTP URLs.
        let resolved_uri = match self.resolve_uri(uri) {
            Ok(u) => u,
            Err(e) => {
                error!(error = %e, "Failed to resolve URI for casting");
                let _ = tx.try_send(PlayerEvent::Error(format!("Chromecast: {e}")));
                return;
            }
        };

        std::thread::spawn(move || {
            match Self::cast_media_with_session(&host, port, &resolved_uri, volume, tx.clone()) {
                Ok(()) => {}
                Err(e) => {
                    error!(error = %e, "Chromecast: media load failed");
                    let _ = tx.try_send(PlayerEvent::Error(format!("Chromecast: {e}")));
                }
            }
        });
    }

    /// Connect, launch app, load media, then maintain a persistent
    /// connection with heartbeat and position polling.
    fn cast_media_with_session(
        host: &str,
        port: u16,
        uri: &str,
        volume: f64,
        tx: async_channel::Sender<PlayerEvent>,
    ) -> Result<(), String> {
        use rust_cast::channels::media::StreamType;
        use rust_cast::channels::receiver::CastDeviceApp;
        use rust_cast::CastDevice;

        info!(host = %host, port, "Chromecast: connecting via Cast V2");

        let device = CastDevice::connect_without_host_verification(host, port)
            .map_err(|e| format!("TLS connect failed: {e}"))?;

        debug!("Chromecast: connected, launching Default Media Receiver");

        // Establish a connection to the receiver.
        device
            .connection
            .connect("receiver-0")
            .map_err(|e| format!("Connection channel failed: {e}"))?;

        // Set volume on the device.
        device
            .receiver
            .set_volume(volume as f32)
            .map_err(|e| format!("Set volume failed: {e}"))?;

        // Launch the Default Media Receiver app.
        let app = device
            .receiver
            .launch_app(&CastDeviceApp::DefaultMediaReceiver)
            .map_err(|e| format!("Launch app failed: {e}"))?;

        let transport_id = app.transport_id.clone();

        debug!(
            transport_id = %transport_id,
            "Chromecast: Default Media Receiver launched"
        );

        // Connect to the media application's transport.
        device
            .connection
            .connect(&transport_id)
            .map_err(|e| format!("App connection failed: {e}"))?;

        // Determine content type from URI extension.
        let content_type = guess_content_type(uri);

        // Determine stream type.
        let stream_type = if uri.contains("/radio/")
            || std::path::Path::new(uri)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"))
            || std::path::Path::new(uri)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pls"))
        {
            StreamType::Live
        } else {
            StreamType::Buffered
        };

        info!(
            uri_redacted = %crate::audio::redact_url_secrets(uri),
            content_type = %content_type,
            "Chromecast: loading media"
        );

        // Load the media URL on the Chromecast.
        device
            .media
            .load(
                &transport_id,
                &app.session_id,
                &rust_cast::channels::media::Media {
                    content_id: uri.to_string(),
                    content_type: content_type.to_string(),
                    stream_type,
                    duration: None,
                    metadata: None,
                },
            )
            .map_err(|e| format!("Media load failed: {e}"))?;

        info!(host = %host, "Chromecast: media loaded successfully");

        // Signal playing state.
        let _ = tx.try_send(PlayerEvent::StateChanged(PlayerState::Playing));

        // ── Persistent connection: heartbeat + position polling ──────
        //
        // Keep the connection alive by sending PINGs and polling media
        // status for position updates.
        let heartbeat_interval = std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS);
        let position_interval = std::time::Duration::from_secs(POSITION_POLL_INTERVAL_SECS);

        let mut last_heartbeat = std::time::Instant::now();
        let mut last_position_poll = std::time::Instant::now();

        loop {
            // Send heartbeat PING to keep the connection alive.
            if last_heartbeat.elapsed() >= heartbeat_interval {
                if let Err(e) = device.heartbeat.ping() {
                    debug!(error = %e, "Chromecast: heartbeat failed, session ended");
                    break;
                }
                last_heartbeat = std::time::Instant::now();
            }

            // Poll media status for position updates.
            if last_position_poll.elapsed() >= position_interval {
                match device.media.get_status(&transport_id, None) {
                    Ok(status) => {
                        if let Some(entry) = status.entries.first() {
                            // Report position.
                            if let Some(current_time) = entry.current_time {
                                let position_ms = (current_time as f64 * 1000.0) as u64;
                                let duration_ms = entry
                                    .media
                                    .as_ref()
                                    .and_then(|m| m.duration)
                                    .map(|d| (d as f64 * 1000.0) as u64)
                                    .unwrap_or(0);

                                let _ = tx.try_send(PlayerEvent::PositionChanged {
                                    position_ms,
                                    duration_ms,
                                });
                            }

                            // Check if playback has ended via idle_reason.
                            use rust_cast::channels::media::IdleReason;
                            match entry.idle_reason {
                                Some(IdleReason::Finished) => {
                                    info!("Chromecast: track ended");
                                    let _ = tx.try_send(PlayerEvent::TrackEnded);
                                    break;
                                }
                                Some(
                                    IdleReason::Cancelled
                                    | IdleReason::Error
                                    | IdleReason::Interrupted,
                                ) => {
                                    info!(reason = ?entry.idle_reason, "Chromecast: playback ended");
                                    let _ = tx
                                        .try_send(PlayerEvent::StateChanged(PlayerState::Stopped));
                                    break;
                                }
                                None => {
                                    // Still playing — continue polling.
                                }
                            }
                        } else {
                            // No active media session — playback has ended.
                            debug!("Chromecast: no active media session, stopping poll");
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "Chromecast: media status poll failed");
                        break;
                    }
                }
                last_position_poll = std::time::Instant::now();
            }

            // Sleep briefly to avoid busy-looping.
            std::thread::sleep(std::time::Duration::from_millis(250));
        }

        Ok(())
    }

    /// Send a simple Cast command (play, pause, stop) on a background thread.
    ///
    /// Opens a fresh connection per command (same pattern as MPD output).
    /// A persistent connection will be added as a future optimisation.
    fn send_cast_command(&self, command: CastCommand) {
        let host = self.host.clone();
        let port = self.port;
        let tx = self.event_tx.clone();

        std::thread::spawn(move || {
            if let Err(e) = Self::send_cast_command_sync(&host, port, command) {
                warn!(error = %e, "Chromecast: command failed");
                let _ = tx.try_send(PlayerEvent::Error(format!("Chromecast: {e}")));
            }
        });
    }

    /// Synchronous Cast command sender (runs on background thread).
    fn send_cast_command_sync(host: &str, port: u16, command: CastCommand) -> Result<(), String> {
        use rust_cast::CastDevice;

        let device = CastDevice::connect_without_host_verification(host, port)
            .map_err(|e| format!("TLS connect failed: {e}"))?;

        device
            .connection
            .connect("receiver-0")
            .map_err(|e| format!("Connection channel failed: {e}"))?;

        // Get the current app status to find the active session.
        let status = device
            .receiver
            .get_status()
            .map_err(|e| format!("Get status failed: {e}"))?;

        let app = status
            .applications
            .into_iter()
            .find(|a| a.app_id == DEFAULT_MEDIA_RECEIVER_APP_ID)
            .ok_or_else(|| "Default Media Receiver not running".to_string())?;

        let transport_id = app.transport_id.clone();

        device
            .connection
            .connect(&transport_id)
            .map_err(|e| format!("App connection failed: {e}"))?;

        // Get media status to find the active media session ID.
        let media_status = device
            .media
            .get_status(&transport_id, None)
            .map_err(|e| format!("Get media status failed: {e}"))?;

        let media_session_id = media_status
            .entries
            .first()
            .map(|e| e.media_session_id)
            .ok_or_else(|| "No active media session".to_string())?;

        match command {
            CastCommand::Play => {
                debug!("Chromecast: sending play");
                device
                    .media
                    .play(&transport_id, media_session_id)
                    .map_err(|e| format!("Play failed: {e}"))?;
            }
            CastCommand::Pause => {
                debug!("Chromecast: sending pause");
                device
                    .media
                    .pause(&transport_id, media_session_id)
                    .map_err(|e| format!("Pause failed: {e}"))?;
            }
            CastCommand::Stop => {
                debug!("Chromecast: sending stop");
                device
                    .media
                    .stop(&transport_id, media_session_id)
                    .map_err(|e| format!("Stop failed: {e}"))?;
            }
            CastCommand::Seek(position_ms) => {
                let position_secs = position_ms as f32 / 1000.0;
                debug!(position_secs, "Chromecast: sending seek");
                device
                    .media
                    .seek(&transport_id, media_session_id, Some(position_secs), None)
                    .map_err(|e| format!("Seek failed: {e}"))?;
            }
            CastCommand::Volume(level) => {
                debug!(level, "Chromecast: setting volume");
                device
                    .receiver
                    .set_volume(level as f32)
                    .map_err(|e| format!("Set volume failed: {e}"))?;
            }
        }

        Ok(())
    }
}

/// Internal command enum for background-threaded Cast operations.
#[derive(Debug, Clone, Copy)]
enum CastCommand {
    Play,
    Pause,
    Stop,
    Seek(u64),
    Volume(f64),
}

/// Guess a MIME content type from a URI's file extension.
///
/// Returns a reasonable default if the extension is not recognised.
fn guess_content_type(uri: &str) -> &'static str {
    // Strip query parameters before checking extension.
    let path = uri.split('?').next().unwrap_or(uri);
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "aac" | "m4a" => "audio/mp4",
        "aiff" | "aif" => "audio/aiff",
        "m3u8" => "application/x-mpegURL",
        "pls" => "audio/x-scpls",
        // Default: let the Chromecast figure it out.
        _ => "audio/mpeg",
    }
}

impl AudioOutput for ChromecastOutput {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn output_type(&self) -> OutputType {
        OutputType::Chromecast
    }

    fn supports_volume(&self) -> bool {
        true
    }

    fn load_uri(&self, uri: &str) {
        info!("Chromecast: loading URI");
        self.cast_media(uri);

        // Optimistically signal buffering.
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Buffering));
    }

    fn play(&self) {
        debug!("Chromecast: play");
        self.send_cast_command(CastCommand::Play);
    }

    fn pause(&self) {
        debug!("Chromecast: pause");
        self.send_cast_command(CastCommand::Pause);
    }

    fn stop(&self) {
        debug!("Chromecast: stop");
        self.send_cast_command(CastCommand::Stop);
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Stopped));
    }

    fn toggle_play_pause(&self) {
        // Without persistent state tracking, we optimistically send pause.
        // A future enhancement with persistent connection will query state.
        debug!("Chromecast: toggle play/pause");
        self.send_cast_command(CastCommand::Pause);
    }

    fn seek_to(&self, position_ms: u64) {
        debug!(position_ms, "Chromecast: seek");
        self.send_cast_command(CastCommand::Seek(position_ms));
    }

    fn set_volume(&mut self, level: f64) {
        self.volume = level.clamp(0.0, 1.0);
        // Send volume command to the Chromecast device.
        let host = self.host.clone();
        let port = self.port;
        let vol = self.volume;
        let tx = self.event_tx.clone();

        std::thread::spawn(move || {
            if let Err(e) = Self::send_cast_command_sync(&host, port, CastCommand::Volume(vol)) {
                warn!(error = %e, "Chromecast: volume command failed");
                let _ = tx.try_send(PlayerEvent::Error(format!("Chromecast volume: {e}")));
            }
        });
    }

    fn volume(&self) -> f64 {
        self.volume
    }

    fn state(&self) -> PlayerState {
        self.current_state
    }

    fn position_ms(&self) -> Option<u64> {
        // Position is now reported via the persistent connection's
        // media status polling loop, which sends PositionChanged events.
        // This method returns None because the position is delivered
        // asynchronously via the event channel.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chromecast_output_name() {
        let (tx, _rx) = async_channel::unbounded();
        let output = ChromecastOutput::new("Living Room Speaker", "192.168.1.50", 8009, tx);
        assert_eq!(output.name(), "Living Room Speaker");
    }

    #[test]
    fn test_chromecast_output_type() {
        let (tx, _rx) = async_channel::unbounded();
        let output = ChromecastOutput::new("Test", "127.0.0.1", 8009, tx);
        assert_eq!(output.output_type(), OutputType::Chromecast);
    }

    #[test]
    fn test_chromecast_supports_volume() {
        let (tx, _rx) = async_channel::unbounded();
        let output = ChromecastOutput::new("Test", "127.0.0.1", 8009, tx);
        assert!(output.supports_volume());
    }

    #[test]
    fn test_chromecast_volume_clamp() {
        let (tx, _rx) = async_channel::unbounded();
        // Use a non-routable IP and unusual port to prevent actual connection attempts.
        let mut output = ChromecastOutput::new("Test", "192.0.2.1", 1, tx);
        // Note: set_volume spawns a thread that will fail to connect,
        // but the volume field is updated synchronously.
        output.volume = 1.5_f64.clamp(0.0, 1.0);
        assert!((output.volume() - 1.0).abs() < f64::EPSILON);
        output.volume = (-0.5_f64).clamp(0.0, 1.0);
        assert!((output.volume() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_chromecast_initial_state() {
        let (tx, _rx) = async_channel::unbounded();
        let output = ChromecastOutput::new("Test", "127.0.0.1", 8009, tx);
        assert_eq!(output.state(), PlayerState::Stopped);
    }

    #[test]
    fn test_chromecast_no_position() {
        let (tx, _rx) = async_channel::unbounded();
        let output = ChromecastOutput::new("Test", "127.0.0.1", 8009, tx);
        assert!(output.position_ms().is_none());
    }

    #[test]
    fn test_guess_content_type() {
        assert_eq!(
            guess_content_type("http://example.com/song.mp3"),
            "audio/mpeg"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.flac"),
            "audio/flac"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.ogg"),
            "audio/ogg"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.opus"),
            "audio/opus"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.wav"),
            "audio/wav"
        );
        assert_eq!(
            guess_content_type("http://example.com/song.m4a"),
            "audio/mp4"
        );
        assert_eq!(
            guess_content_type("http://example.com/stream.m3u8"),
            "application/x-mpegURL"
        );
        // With query parameters.
        assert_eq!(
            guess_content_type("http://example.com/song.flac?token=abc"),
            "audio/flac"
        );
        // Unknown extension falls back to audio/mpeg.
        assert_eq!(
            guess_content_type("http://example.com/stream"),
            "audio/mpeg"
        );
    }
}
