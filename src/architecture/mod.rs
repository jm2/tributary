//! Core architecture module for Tributary.
//!
//! This module defines the unified data model and backend traits that allow
//! the UI to work transparently with local libraries (SQLite), Subsonic,
//! DAAP, Jellyfin, and any future media source.

pub mod backend;
pub mod error;
pub mod identity;
pub mod media;
pub mod models;
pub mod server_playlist;

pub use backend::{load_track_catalog, MediaBackend};
pub use identity::{MediaKey, NativePlaylistId, SourceId, TrackId, ViewOrigin};
pub use media::{AdvertisedHttpRoute, RemoteMediaResolver, ResolvedHttpRequest};
pub use server_playlist::{
    ServerPlaylistSnapshot, ServerPlaylistSummary, MAX_SERVER_PLAYLISTS_PER_LIST,
    MAX_SERVER_PLAYLIST_ENTRIES,
};
