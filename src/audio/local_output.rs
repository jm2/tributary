//! Local audio output — wraps the existing GStreamer [`Player`] to
//! implement the [`AudioOutput`] trait.
//!
//! This is the default "My Computer" output that plays audio through
//! the system's speakers or headphones via a GStreamer `playbin3` pipeline.

use super::output::{AudioOutput, OutputType};
use super::{Player, PlayerState};

/// Local GStreamer output — delegates to the existing [`Player`].
#[allow(dead_code)]
pub struct LocalOutput {
    player: Player,
}

impl LocalOutput {
    /// Create a new local output wrapping the given [`Player`].
    #[allow(dead_code)]
    pub fn new(player: Player) -> Self {
        Self { player }
    }

    /// Access the underlying [`Player`] for bus-watch or timer setup
    /// that the window bridge needs.
    #[allow(dead_code)]
    pub fn player(&self) -> &Player {
        &self.player
    }
}

impl AudioOutput for LocalOutput {
    fn name(&self) -> &str {
        "My Computer"
    }

    fn output_type(&self) -> OutputType {
        OutputType::Local
    }

    fn supports_volume(&self) -> bool {
        true
    }

    fn load_uri(&self, uri: &str) {
        self.player.load_uri(uri);
    }

    fn play(&self) {
        self.player.play();
    }

    fn pause(&self) {
        self.player.pause();
    }

    fn stop(&self) {
        self.player.stop();
    }

    fn toggle_play_pause(&self) {
        self.player.toggle_play_pause();
    }

    fn seek_to(&self, position_ms: u64) {
        self.player.seek_to(position_ms);
    }

    fn set_volume(&mut self, level: f64) {
        self.player.set_volume(level);
    }

    fn volume(&self) -> f64 {
        self.player.volume()
    }

    fn state(&self) -> PlayerState {
        self.player.state()
    }

    fn position_ms(&self) -> Option<u64> {
        self.player.position_ms()
    }
}
