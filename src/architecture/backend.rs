//! Unified `MediaBackend` trait.
//!
//! Every media source — whether a local SQLite-backed library, a Subsonic
//! server, a DAAP share, or a Jellyfin instance — implements this single
//! async trait. The UI layer programs exclusively against this interface,
//! making data-source swapping and multi-source aggregation trivial.

use async_trait::async_trait;
use url::Url;
use uuid::Uuid;

use super::error::BackendError;
use super::models::{Album, Artist, LibraryStats, SearchResults, SortField, SortOrder, Track};

/// The result type used throughout backend operations.
pub type BackendResult<T> = Result<T, BackendError>;

/// A unified, async-safe interface to any media data source.
///
/// # Design Notes
///
/// * Every method is `async` — implementations are free to hit the network,
///   query a database, or scan the filesystem without blocking the GTK
///   main thread.
/// * The trait is `Send + Sync` so that backend handles can be shared
///   across async tasks and GLib signal handlers.
/// * Errors are returned as structured [`BackendError`] variants rather
///   than opaque `anyhow::Error`, giving the UI enough context to show
///   meaningful messages (e.g., "Authentication failed" vs. "Server
///   unreachable").
#[async_trait]
pub trait MediaBackend: Send + Sync {
    // -------------------------------------------------------------------
    // Identity & Lifecycle
    // -------------------------------------------------------------------

    /// Human-readable display name for this backend.
    ///
    /// Examples: `"Local Library"`, `"Navidrome (home)"`, `"Living Room DAAP"`.
    fn name(&self) -> &str;

    /// A short, machine-readable identifier for the backend type.
    ///
    /// Examples: `"local"`, `"subsonic"`, `"daap"`, `"jellyfin"`.
    fn backend_type(&self) -> &str;

    /// Test connectivity and/or availability of the backend.
    ///
    /// For a local backend this might verify the database is accessible;
    /// for a remote backend it issues a lightweight health-check request.
    async fn ping(&self) -> BackendResult<()>;

    // -------------------------------------------------------------------
    // Search
    // -------------------------------------------------------------------

    /// Full-text search across tracks, albums, and artists.
    ///
    /// The `limit` parameter caps the number of results per entity type.
    async fn search(&self, query: &str, limit: usize) -> BackendResult<SearchResults>;

    // -------------------------------------------------------------------
    // Browsing
    // -------------------------------------------------------------------

    /// Retrieve all albums, optionally sorted.
    async fn list_albums(&self, sort: SortField, order: SortOrder) -> BackendResult<Vec<Album>>;

    /// Retrieve all artists.
    async fn list_artists(&self) -> BackendResult<Vec<Artist>>;

    /// Retrieve every track belonging to a specific album.
    async fn get_album_tracks(&self, album_id: &Uuid) -> BackendResult<Vec<Track>>;

    /// Retrieve every track belonging to a specific artist.
    async fn get_artist_tracks(&self, artist_id: &Uuid) -> BackendResult<Vec<Track>>;

    // -------------------------------------------------------------------
    // Playback
    // -------------------------------------------------------------------

    /// Obtain a playable URL for a track.
    ///
    /// * **Local backend:** returns a `file:///` URI.
    /// * **Remote backends:** returns an authenticated streaming URL
    ///   (possibly time-limited).
    async fn get_stream_url(&self, track_id: &Uuid) -> BackendResult<Url>;

    // -------------------------------------------------------------------
    // Artwork
    // -------------------------------------------------------------------

    /// Retrieve cover art for an album, if available.
    async fn get_cover_art(&self, album_id: &Uuid) -> BackendResult<Option<Url>>;

    // -------------------------------------------------------------------
    // Statistics
    // -------------------------------------------------------------------

    /// Aggregate library statistics for this backend.
    async fn get_stats(&self) -> BackendResult<LibraryStats>;
}
