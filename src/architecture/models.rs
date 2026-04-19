//! Core data models for Tributary.
//!
//! These structs represent the universal vocabulary of the application.
//! Every backend (Local, Subsonic, DAAP, Jellyfin) must map its native
//! data into these types before they reach the UI layer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Primary Entities
// ---------------------------------------------------------------------------

/// A single audio track, the fundamental unit of the library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    /// Unique identifier (assigned by the originating backend).
    pub id: Uuid,

    /// Track title.
    pub title: String,

    /// Display name of the performing artist.
    pub artist_name: String,

    /// Album artist (used for grouping when different from track artist).
    #[serde(default)]
    pub album_artist_name: Option<String>,

    /// Artist unique identifier (if resolvable).
    pub artist_id: Option<Uuid>,

    /// Album title this track belongs to.
    pub album_title: String,

    /// Album unique identifier (if resolvable).
    pub album_id: Option<Uuid>,

    /// Track number within the disc.
    pub track_number: Option<u32>,

    /// Disc number within the album.
    pub disc_number: Option<u32>,

    /// Duration in whole seconds.
    pub duration_secs: Option<u64>,

    /// Genre tag.
    pub genre: Option<String>,

    /// Release year.
    pub year: Option<i32>,

    /// Local file path (only for the local backend).
    pub file_path: Option<String>,

    /// Streamable URL (for remote backends, or local `file://` URIs).
    pub stream_url: Option<Url>,

    /// Cover art URL or local path.
    pub cover_art_url: Option<Url>,

    /// Timestamp when this track was first added to the library.
    pub date_added: Option<DateTime<Utc>>,

    /// Timestamp of the last metadata modification (e.g., FS mtime).
    pub date_modified: Option<DateTime<Utc>>,

    /// Audio bitrate in kbps (if known).
    pub bitrate_kbps: Option<u32>,

    /// Audio sample rate in Hz (if known).
    pub sample_rate_hz: Option<u32>,

    /// File format / codec (e.g., "FLAC", "MP3", "AAC").
    pub format: Option<String>,

    /// Number of times this track has been played.
    pub play_count: Option<u32>,
}

/// An album — a logical grouping of tracks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    /// Unique identifier.
    pub id: Uuid,

    /// Album title.
    pub title: String,

    /// Primary artist display name.
    pub artist_name: String,

    /// Artist unique identifier (if resolvable).
    pub artist_id: Option<Uuid>,

    /// Release year.
    pub year: Option<i32>,

    /// Genre tag.
    pub genre: Option<String>,

    /// Cover art URL.
    pub cover_art_url: Option<Url>,

    /// Number of tracks in this album.
    pub track_count: u32,

    /// Total duration of the album in seconds.
    pub total_duration_secs: Option<u64>,
}

/// An artist entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artist {
    /// Unique identifier.
    pub id: Uuid,

    /// Artist display name.
    pub name: String,

    /// Number of albums by this artist in the library.
    pub album_count: u32,

    /// Number of tracks by this artist in the library.
    pub track_count: u32,

    /// Artist photo / cover art URL.
    pub cover_art_url: Option<Url>,
}

// ---------------------------------------------------------------------------
// Query & Result Types
// ---------------------------------------------------------------------------

/// Aggregated search results across all entity types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
}

/// Fields by which library listings can be sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortField {
    Title,
    Artist,
    Album,
    Year,
    DateAdded,
    DateModified,
    Duration,
    TrackNumber,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortOrder {
    Ascending,
    Descending,
}

// ---------------------------------------------------------------------------
// Aggregate Statistics
// ---------------------------------------------------------------------------

/// High-level statistics for an entire backend / library source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryStats {
    /// Total number of tracks.
    pub total_tracks: u64,

    /// Total number of albums.
    pub total_albums: u64,

    /// Total number of artists.
    pub total_artists: u64,

    /// Total playback duration in seconds.
    pub total_duration_secs: u64,
}
