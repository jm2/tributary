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
// Same bin-target rationale as the offline block below: the re-export is
// lib surface until the engine slice (tr-4q8) wires it.
#[allow(unused_imports)]
pub use identity::{
    MediaKey, NativePlaylistId, SourceId, SourceIncarnationId, TrackId, ViewOrigin,
};
pub use media::{
    AdvertisedHttpRoute, MediaRepresentation, MediaStreamKind, RemoteMediaResolver,
    ResolvedHttpRequest,
};
// The offline re-export below is intentional public surface for exactly
// two follow-up slices: the bounded download/cache engine (tr-4q8) and
// the GTK offline storage panel (tr-8h4). This binary is not yet wired
// to it — the surface is lib-only until those slices land — so the
// unused-import lint is silenced at the bin root while the lib-level
// surface stays complete.
#[allow(unused_imports)]
pub use offline::{
    check_declared_total, CommittedSnapshot, DigestProvenance, EntityValidator, JobRecord,
    JobState, LeaseId, OfflineCatalogueEntry, OfflineError, OfflineSnapshot, OperationalLicence,
    MAX_OFFLINE_METADATA_BYTES, MAX_OFFLINE_SNAPSHOT_PATH_BYTES,
};
pub use server_playlist::{
    ServerPlaylistSnapshot, ServerPlaylistSummary, MAX_SERVER_PLAYLISTS_PER_LIST,
    MAX_SERVER_PLAYLIST_ENTRIES,
};
