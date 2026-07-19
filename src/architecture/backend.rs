//! Unified `MediaBackend` trait.
//!
//! Every shipping media backend — local SQLite, Subsonic, Jellyfin, Plex, and
//! DAAP — implements this single async trait. Complete catalogue publication
//! now uses its dynamic-dispatch boundary for all five. Authentication, source
//! lifecycle, and some browsing paths still require backend-specific
//! integration.

use async_trait::async_trait;
use uuid::Uuid;

use super::error::BackendError;
use super::identity::TrackId;
use super::models::{
    Album, Artist, LibraryStats, Rating, RatingCapability, SearchResults, SortField, SortOrder,
    Track,
};

/// The result type used throughout backend operations.
pub type BackendResult<T> = Result<T, BackendError>;

/// Reject a catalogue whose per-track rating capability drifts from the
/// source-wide contract advertised by its backend.
///
/// Most backends reach this guard through [`load_track_catalog`]. Adapters
/// with a specialized lifecycle loader must call it before publication too.
pub fn validate_catalogue_rating_capability(
    tracks: &[Track],
    capability: RatingCapability,
) -> BackendResult<()> {
    if tracks
        .iter()
        .any(|track| track.rating.capability() != capability)
    {
        return Err(BackendError::Internal(anyhow::anyhow!(
            "backend published a track rating capability inconsistent with its source capability"
        )));
    }
    Ok(())
}

/// Read the complete track catalogue through the application backend seam.
///
/// Keeping this adapter typed as `&dyn MediaBackend` is intentional: source
/// setup may still need a concrete backend for authentication or protected
/// media resolution, but catalogue publication must not grow a parallel
/// backend-specific path. Local and remote sources both enter the UI through
/// this dynamic-dispatch boundary.
pub async fn load_track_catalog(backend: &dyn MediaBackend) -> BackendResult<Vec<Track>> {
    let tracks = backend.list_tracks().await?;
    validate_catalogue_rating_capability(&tracks, backend.rating_capability())?;
    Ok(tracks)
}

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

    /// Retrieve the complete track catalogue for publication to the UI.
    ///
    /// Implementations backed by a remote in-memory cache return its current
    /// snapshot. The local implementation queries its SQLite catalogue.
    async fn list_tracks(&self) -> BackendResult<Vec<Track>>;

    // -------------------------------------------------------------------
    // Ratings
    // -------------------------------------------------------------------

    /// Source-wide rating capability for tracks published by this backend.
    ///
    /// The fail-closed default is unsupported. An adapter may advertise
    /// read-only ratings only when its current API data has an unambiguous
    /// canonical conversion.
    fn rating_capability(&self) -> RatingCapability {
        RatingCapability::Unsupported
    }

    /// Persist an app-owned rating for one exact source-native track ID.
    ///
    /// `None` clears the rating. The fail-closed default prevents remote or
    /// otherwise incapable backends from accidentally receiving a write.
    async fn set_track_rating(
        &self,
        _track_id: &TrackId,
        _rating: Option<Rating>,
    ) -> BackendResult<Option<Track>> {
        Err(BackendError::Unsupported {
            operation: "write track ratings".to_string(),
        })
    }

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
    // Statistics
    // -------------------------------------------------------------------

    /// Aggregate library statistics for this backend.
    async fn get_stats(&self) -> BackendResult<LibraryStats>;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    struct CatalogSpy {
        calls: AtomicUsize,
        tracks: Vec<Track>,
        rating_capability: RatingCapability,
    }

    #[async_trait]
    impl MediaBackend for CatalogSpy {
        fn name(&self) -> &str {
            "catalog-spy"
        }

        fn backend_type(&self) -> &str {
            "test"
        }

        async fn ping(&self) -> BackendResult<()> {
            unreachable!("catalog publication must not ping the backend")
        }

        async fn search(&self, _query: &str, _limit: usize) -> BackendResult<SearchResults> {
            unreachable!("catalog publication must not search the backend")
        }

        async fn list_tracks(&self) -> BackendResult<Vec<Track>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.tracks.clone())
        }

        fn rating_capability(&self) -> RatingCapability {
            self.rating_capability
        }

        async fn list_albums(
            &self,
            _sort: SortField,
            _order: SortOrder,
        ) -> BackendResult<Vec<Album>> {
            unreachable!("catalog publication must not list albums")
        }

        async fn list_artists(&self) -> BackendResult<Vec<Artist>> {
            unreachable!("catalog publication must not list artists")
        }

        async fn get_album_tracks(&self, _album_id: &Uuid) -> BackendResult<Vec<Track>> {
            unreachable!("catalog publication must not resolve an album")
        }

        async fn get_artist_tracks(&self, _artist_id: &Uuid) -> BackendResult<Vec<Track>> {
            unreachable!("catalog publication must not resolve an artist")
        }

        async fn get_stats(&self) -> BackendResult<LibraryStats> {
            unreachable!("catalog publication must not query statistics")
        }
    }

    #[tokio::test]
    async fn track_catalog_uses_the_object_safe_backend_boundary() {
        let spy = Arc::new(CatalogSpy {
            calls: AtomicUsize::new(0),
            tracks: Vec::new(),
            rating_capability: RatingCapability::Unsupported,
        });
        let backend: Arc<dyn MediaBackend> = spy.clone();

        let tracks = load_track_catalog(backend.as_ref())
            .await
            .expect("load catalogue through trait object");

        assert!(tracks.is_empty());
        assert_eq!(spy.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn unknown_backends_fail_closed_for_rating_writes() {
        let backend = CatalogSpy {
            calls: AtomicUsize::new(0),
            tracks: Vec::new(),
            rating_capability: RatingCapability::Unsupported,
        };
        assert_eq!(backend.rating_capability(), RatingCapability::Unsupported);
        let track_id = TrackId::remote("remote-track").expect("track ID");
        let error = backend
            .set_track_rating(&track_id, Some(Rating::new(50).unwrap()))
            .await
            .expect_err("default rating mutation must fail closed");
        assert!(matches!(error, BackendError::Unsupported { .. }));
    }

    fn fixture_track(rating: super::super::models::TrackRating) -> Track {
        Track {
            id: Uuid::new_v4(),
            native_track_id: Some(TrackId::new("fixture").unwrap()),
            title: "Fixture".to_string(),
            artist_name: "Artist".to_string(),
            album_artist_name: None,
            artist_id: None,
            album_title: "Album".to_string(),
            album_id: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            composer: None,
            genre: None,
            year: None,
            file_path: None,
            stream_url: None,
            cover_art_url: None,
            date_added: None,
            date_modified: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: None,
            rating,
            last_played: None,
        }
    }

    #[tokio::test]
    async fn catalogue_boundary_rejects_per_track_capability_drift() {
        let backend = CatalogSpy {
            calls: AtomicUsize::new(0),
            tracks: vec![fixture_track(super::super::models::TrackRating::writable(
                None,
            ))],
            rating_capability: RatingCapability::Unsupported,
        };

        let error = load_track_catalog(&backend)
            .await
            .expect_err("mismatched track capability must fail closed");
        assert!(matches!(error, BackendError::Internal(_)));
    }
}
