//! Subsonic/Navidrome/Airsonic REST API backend.
//!
//! All library metadata is held strictly in memory — nothing is written
//! to the local SQLite database. Catalogue rows retain backend-native song
//! IDs; playback and artwork resolve protected requests only at use, and
//! Tributary's app-owned proxy keeps Subsonic authentication out of GStreamer.

mod api;
mod backend;
mod client;

pub use backend::SubsonicBackend;

#[cfg(test)]
pub use client::SubsonicClient;
