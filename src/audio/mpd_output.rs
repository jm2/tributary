//! MPD audio output — sends playback commands to a Music Player Daemon
//! server over TCP.
//!
//! MPD acts as a **sink** only: it controls *where audio plays*, not
//! where music comes from.  The track list in Tributary remains driven
//! by the active source (local, Subsonic, Jellyfin, etc.).
//!
//! # Security
//!
//! - **Command injection prevention**: All strings sent to MPD (file
//!   paths, URIs) are sanitised to strip newline characters (`\n`, `\r`)
//!   before being included in protocol commands.  The MPD protocol is
//!   newline-delimited, so an unescaped newline could inject arbitrary
//!   commands.
//! - **Connection timeout**: TCP connections use a 5-second timeout to
//!   prevent UI hangs on unreachable hosts.
//! - **No credentials logged**: Host and port are logged at info level;
//!   if MPD password support is added in the future, passwords must
//!   never appear in logs.
//! - **Plaintext protocol**: MPD uses unencrypted TCP.  Users connecting
//!   to remote MPD servers should be aware that commands travel in
//!   cleartext.  Localhost connections (the default) are not affected.
//!
//! # Threading model
//!
//! All public methods are called from the GTK main thread.  Network I/O
//! is performed synchronously on a background thread spawned per-command
//! to avoid blocking the UI.  A persistent connection could be added
//! later as an optimisation.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use tracing::{debug, error, info, warn};

use super::output::{AudioOutput, OutputType};
use super::{PlayerEvent, PlayerState};

/// Timeout for TCP connect and individual read/write operations.
#[allow(dead_code)]
const TCP_TIMEOUT: Duration = Duration::from_secs(5);

/// MPD audio output — sends commands to an MPD server.
#[allow(dead_code)]
pub struct MpdOutput {
    /// Human-readable name shown in the output selector.
    display_name: String,
    /// MPD server hostname or IP.
    host: String,
    /// MPD server port (typically 6600).
    port: u16,
    /// Event sender for relaying state changes to the GTK main thread.
    event_tx: async_channel::Sender<PlayerEvent>,
    /// Cached volume level (0.0–1.0).  MPD manages its own volume,
    /// so this is only used to satisfy the `AudioOutput::volume()` query.
    volume: f64,
}

impl MpdOutput {
    /// Create a new MPD output.
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
        info!(host = %host, port, name = %display_name, "MPD output configured");
        Self {
            display_name: display_name.to_string(),
            host: host.to_string(),
            port,
            event_tx,
            volume: 1.0,
        }
    }

    /// Attempt a TCP connection to the MPD server and verify the
    /// greeting line (`OK MPD x.y.z`).
    ///
    /// Returns `Ok(version_string)` on success.
    #[allow(dead_code)]
    pub fn probe(host: &str, port: u16) -> Result<String, String> {
        let addr = format!("{host}:{port}");
        let addrs: Vec<_> = addr
            .to_socket_addrs()
            .map_err(|e| format!("DNS resolution failed: {e}"))?
            .collect();

        if addrs.is_empty() {
            return Err("No addresses resolved".to_string());
        }

        let stream = TcpStream::connect_timeout(&addrs[0], TCP_TIMEOUT)
            .map_err(|e| format!("Connection failed: {e}"))?;
        stream
            .set_read_timeout(Some(TCP_TIMEOUT))
            .map_err(|e| format!("Set read timeout failed: {e}"))?;

        let mut reader = BufReader::new(&stream);
        let mut greeting = String::new();
        reader
            .read_line(&mut greeting)
            .map_err(|e| format!("Read greeting failed: {e}"))?;

        if greeting.starts_with("OK MPD") {
            let version = greeting.trim().to_string();
            info!(version = %version, "MPD probe successful");
            Ok(version)
        } else {
            Err(format!("Unexpected greeting: {}", greeting.trim()))
        }
    }

    /// Send one or more MPD commands on a background thread.
    ///
    /// Commands are newline-separated.  The connection is opened fresh
    /// each time (simple but reliable).
    fn send_commands(&self, commands: &str) {
        let host = self.host.clone();
        let port = self.port;
        let cmds = commands.to_string();
        let tx = self.event_tx.clone();

        std::thread::spawn(move || {
            if let Err(e) = Self::send_commands_sync(&host, port, &cmds) {
                error!(error = %e, "MPD command failed");
                let _ = tx.try_send(PlayerEvent::Error(format!("MPD: {e}")));
            }
        });
    }

    /// Synchronous command sender (runs on background thread).
    fn send_commands_sync(host: &str, port: u16, commands: &str) -> Result<(), String> {
        let addr = format!("{host}:{port}");
        let addrs: Vec<_> = addr
            .to_socket_addrs()
            .map_err(|e| format!("DNS: {e}"))?
            .collect();

        if addrs.is_empty() {
            return Err("No addresses resolved".to_string());
        }

        let mut stream = TcpStream::connect_timeout(&addrs[0], TCP_TIMEOUT)
            .map_err(|e| format!("Connect: {e}"))?;
        stream
            .set_read_timeout(Some(TCP_TIMEOUT))
            .map_err(|e| format!("Timeout: {e}"))?;
        stream
            .set_write_timeout(Some(TCP_TIMEOUT))
            .map_err(|e| format!("Timeout: {e}"))?;

        let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("Clone: {e}"))?);

        // Read and verify greeting.
        let mut greeting = String::new();
        reader
            .read_line(&mut greeting)
            .map_err(|e| format!("Greeting: {e}"))?;
        if !greeting.starts_with("OK MPD") {
            return Err(format!("Bad greeting: {}", greeting.trim()));
        }

        // Send commands.
        for line in commands.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            debug!(cmd = %line, "MPD ←");
            stream
                .write_all(format!("{line}\n").as_bytes())
                .map_err(|e| format!("Write: {e}"))?;

            // Read response until "OK" or "ACK".
            loop {
                let mut resp = String::new();
                reader
                    .read_line(&mut resp)
                    .map_err(|e| format!("Read: {e}"))?;
                let resp = resp.trim();
                debug!(resp = %resp, "MPD →");
                if resp.starts_with("OK") {
                    break;
                }
                if resp.starts_with("ACK") {
                    warn!(response = %resp, "MPD error");
                    // Don't fail the whole batch — log and continue.
                    break;
                }
                // Otherwise it's a data line (e.g. from `status`) — skip.
            }
        }

        Ok(())
    }
}

/// Sanitise a string for inclusion in an MPD command.
///
/// Strips `\n` and `\r` to prevent command injection via crafted
/// filenames or URIs.  The MPD protocol is newline-delimited, so
/// embedded newlines could inject arbitrary commands.
fn sanitise_mpd_arg(s: &str) -> String {
    s.chars().filter(|&c| c != '\n' && c != '\r').collect()
}

impl AudioOutput for MpdOutput {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn output_type(&self) -> OutputType {
        OutputType::Mpd
    }

    fn supports_volume(&self) -> bool {
        // MPD manages its own volume — the app slider shouldn't control it.
        false
    }

    fn load_uri(&self, uri: &str) {
        let safe_uri = sanitise_mpd_arg(uri);
        let cmds = format!("clear\nadd \"{safe_uri}\"\nplay");
        self.send_commands(&cmds);

        // Optimistically signal buffering — the UI will update when
        // position ticks arrive (or an error is reported).
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Buffering));
    }

    fn play(&self) {
        self.send_commands("play");
    }

    fn pause(&self) {
        self.send_commands("pause 1");
    }

    fn stop(&self) {
        self.send_commands("stop");
        let _ = self
            .event_tx
            .try_send(PlayerEvent::StateChanged(PlayerState::Stopped));
    }

    fn toggle_play_pause(&self) {
        // MPD's `pause` without argument toggles.
        self.send_commands("pause");
    }

    fn seek_to(&self, position_ms: u64) {
        let secs = position_ms as f64 / 1000.0;
        self.send_commands(&format!("seekcur {secs:.1}"));
    }

    fn set_volume(&mut self, level: f64) {
        // Store locally but don't send to MPD — volume is MPD-managed.
        self.volume = level.clamp(0.0, 1.0);
    }

    fn volume(&self) -> f64 {
        self.volume
    }

    fn state(&self) -> PlayerState {
        // Non-blocking: return a best-guess.  The real state comes
        // from position polling (future enhancement).
        PlayerState::Stopped
    }

    fn position_ms(&self) -> Option<u64> {
        // Would require a synchronous `status` query — return None
        // for now.  Position ticks for MPD will be added when we
        // implement a persistent connection with async status polling.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitise_strips_newlines() {
        assert_eq!(sanitise_mpd_arg("hello\nworld"), "helloworld");
        assert_eq!(sanitise_mpd_arg("hello\r\nworld"), "helloworld");
        assert_eq!(sanitise_mpd_arg("clean"), "clean");
    }

    #[test]
    fn test_sanitise_injection_attempt() {
        // A crafted filename that tries to inject an MPD command.
        let malicious = "song.flac\ndelete 0\n";
        let safe = sanitise_mpd_arg(malicious);
        assert!(!safe.contains('\n'));
        assert_eq!(safe, "song.flacdelete 0");
    }

    #[test]
    fn test_sanitise_empty_string() {
        assert_eq!(sanitise_mpd_arg(""), "");
    }

    #[test]
    fn test_sanitise_preserves_unicode() {
        assert_eq!(sanitise_mpd_arg("日本語の曲.flac"), "日本語の曲.flac");
    }

    #[test]
    fn test_sanitise_preserves_quotes() {
        // MPD uses double quotes for arguments — they should pass through.
        assert_eq!(sanitise_mpd_arg("it's a \"test\""), "it's a \"test\"");
    }
}
