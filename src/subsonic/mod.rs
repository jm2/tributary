//! Subsonic/Navidrome/Airsonic REST API backend.
//!
//! All library metadata is held strictly in memory — nothing is written
//! to the local SQLite database.  Streaming URLs include full Subsonic
//! token authentication so GStreamer can fetch audio directly.

mod api;
mod backend;
mod client;

pub use backend::SubsonicBackend;
