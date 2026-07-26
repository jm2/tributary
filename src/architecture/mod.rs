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
pub mod offline;
pub mod server_playlist;

pub use backend::{load_track_catalog, MediaBackend};
pub use identity::{MediaKey, NativePlaylistId, SourceId, TrackId, ViewOrigin};
pub use media::{AdvertisedHttpRoute, RemoteMediaResolver, ResolvedHttpRequest};
// The offline and server_playlist re-exports below are intentional public
// surface for the bounded download/cache engine, the offline catalogue
// resolver, and the GTK storage panel. The follow-up slices that consume
// them land as separate implementation records; the binary is not yet
// wired to them, so silence the unused-import lint at the bin root while
// keeping the lib-level surface unchanged.
#[allow(unused_imports)]
pub use offline::{
    licence_labels, CommittedSnapshot, JobRecord, JobState, LeaseId, OfflineCapability,
    OfflineCatalogueEntry, OfflineError, OfflineSnapshot, OperationalLicence,
    MAX_OFFLINE_BYTE_HINT, MAX_OFFLINE_SNAPSHOT_PATH_BYTES,
};
#[allow(unused_imports)]
pub use server_playlist::{
    ServerPlaylistSnapshot, ServerPlaylistSummary, MAX_SERVER_PLAYLISTS_PER_LIST,
    MAX_SERVER_PLAYLIST_ENTRIES,
};
