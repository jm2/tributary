//! Core data models for Tributary.
//!
//! These structs represent the universal vocabulary of the application.
//! Every backend (Local, Subsonic, DAAP, Jellyfin) must map its native
//! data into these types before they reach the UI layer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use super::identity::TrackId;

// ---------------------------------------------------------------------------
// Ratings
// ---------------------------------------------------------------------------

/// Tributary's canonical track-rating value.
///
/// Ratings are whole integers from 1 through 100. `None` at the owning
/// [`TrackRating`] boundary means unrated; zero is never a second spelling for
/// that state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Rating(u8);

impl Rating {
    pub const MIN: u8 = 1;
    pub const MAX: u8 = 100;

    /// Validate one canonical rating value.
    pub fn new(value: u8) -> Result<Self, InvalidRating> {
        if (Self::MIN..=Self::MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidRating {
                value: i64::from(value),
            })
        }
    }

    pub const fn value(self) -> u8 {
        self.0
    }

    /// Convert an exact Subsonic-style one-through-five star value.
    /// Invalid native values are not guessed or clamped.
    pub fn from_five_star_scale(value: i32) -> Option<Self> {
        if !(1..=5).contains(&value) {
            return None;
        }
        let canonical = value.checked_mul(20)?;
        u8::try_from(canonical)
            .ok()
            .and_then(|value| Self::new(value).ok())
    }

    /// Convert a Jellyfin/Plex-style zero-through-ten decimal value.
    ///
    /// Only finite values inside the documented native range are accepted;
    /// malformed server data is not clamped into a rating. Accepted values
    /// are scaled and rounded to the nearest whole canonical point. Native
    /// zero remains rated and maps to the canonical minimum of one; absence
    /// is the only unrated representation.
    pub fn from_ten_point_scale(value: f64) -> Option<Self> {
        if !value.is_finite() || !(0.0..=10.0).contains(&value) {
            return None;
        }
        let scaled = (value * 10.0).round();
        let canonical = (scaled as u8).clamp(Self::MIN, Self::MAX);
        Self::new(canonical).ok()
    }
}

impl TryFrom<u8> for Rating {
    type Error = InvalidRating;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<i32> for Rating {
    type Error = InvalidRating;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        u8::try_from(value)
            .map_err(|_| InvalidRating {
                value: i64::from(value),
            })
            .and_then(Self::new)
    }
}

impl From<Rating> for u8 {
    fn from(value: Rating) -> Self {
        value.value()
    }
}

/// A rejected canonical rating value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRating {
    value: i64,
}

impl std::fmt::Display for InvalidRating {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "rating {} is outside the inclusive 1..=100 range",
            self.value
        )
    }
}

impl std::error::Error for InvalidRating {}

/// The rating operations supported by the source which published a track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatingCapability {
    /// The source did not publish a trustworthy rating and cannot accept one.
    #[default]
    Unsupported,
    /// The source published a trustworthy rating, but Tributary cannot write it.
    ReadOnly,
    /// Tributary owns the rating and can persist updates.
    Writable,
}

impl RatingCapability {
    pub const fn is_readable(self) -> bool {
        !matches!(self, Self::Unsupported)
    }

    pub const fn is_writable(self) -> bool {
        matches!(self, Self::Writable)
    }
}

/// One track's rating together with its source capability.
///
/// Keeping these values in one enum makes inconsistent states such as an
/// unsupported source carrying a rating impossible. DAAP, radio, removable
/// media, and external files currently use `Unsupported`;
/// a future adapter may use `ReadOnly` only for an unambiguous native value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "capability", rename_all = "snake_case")]
pub enum TrackRating {
    #[default]
    Unsupported,
    ReadOnly {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Rating>,
    },
    Writable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Rating>,
    },
}

impl TrackRating {
    pub const fn unsupported() -> Self {
        Self::Unsupported
    }

    pub const fn read_only(value: Option<Rating>) -> Self {
        Self::ReadOnly { value }
    }

    pub const fn writable(value: Option<Rating>) -> Self {
        Self::Writable { value }
    }

    pub const fn value(self) -> Option<Rating> {
        match self {
            Self::Unsupported => None,
            Self::ReadOnly { value } | Self::Writable { value } => value,
        }
    }

    pub const fn capability(self) -> RatingCapability {
        match self {
            Self::Unsupported => RatingCapability::Unsupported,
            Self::ReadOnly { .. } => RatingCapability::ReadOnly,
            Self::Writable { .. } => RatingCapability::Writable,
        }
    }
}

// ---------------------------------------------------------------------------
// Primary Entities
// ---------------------------------------------------------------------------

/// A single audio track, the fundamental unit of the library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    /// Unique identifier (assigned by the originating backend).
    ///
    /// This UUID remains a compatibility key for album/search APIs. Playback
    /// and queue ownership use the exact `native_track_id` below.
    pub id: Uuid,

    /// Exact backend-native track identifier, when the adapter can preserve
    /// it independently from the compatibility UUID above.
    ///
    /// Local-library rows use their SQLite `tracks.id` value byte-for-byte so
    /// legacy non-UUID keys remain stable across reads. The value is typed and
    /// bounded before an adapter publishes the row; remote resolvers consume
    /// it directly without round-tripping through a derived UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_track_id: Option<TrackId>,

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

    /// Composer credit.
    pub composer: Option<String>,

    /// Genre tag.
    pub genre: Option<String>,

    /// Release year.
    pub year: Option<i32>,

    /// Local file path (only for the local backend).
    pub file_path: Option<String>,

    /// Credential-free playable reference, when one is intrinsic to the
    /// model. Authenticated remote backends leave this empty and resolve their
    /// native track ID through a retained source session at playback time.
    pub stream_url: Option<Url>,

    /// Credential-free cover-art URL or local path. Authenticated remote
    /// artwork is resolved through its retained source session instead.
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

    /// Canonical rating plus the publishing source's read/write capability.
    /// Local-library ratings are app-owned; absent legacy values are unrated.
    #[serde(default)]
    pub rating: TrackRating,

    /// UTC instant when this track most recently crossed Tributary's
    /// counted-play threshold. Legacy and unplayed tracks leave this unset;
    /// file metadata timestamps are never substituted for listening history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<DateTime<Utc>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_accepts_only_the_inclusive_whole_integer_range() {
        assert!(Rating::try_from(-1).is_err());
        assert!(Rating::try_from(0).is_err());
        assert_eq!(Rating::try_from(1).unwrap().value(), 1);
        assert_eq!(Rating::try_from(100).unwrap().value(), 100);
        assert!(Rating::try_from(101).is_err());
        assert!(Rating::try_from(i32::MAX).is_err());
    }

    #[test]
    fn rating_serde_rejects_noncanonical_numbers() {
        assert_eq!(serde_json::from_str::<Rating>("1").unwrap().value(), 1);
        assert_eq!(serde_json::from_str::<Rating>("100").unwrap().value(), 100);
        assert!(serde_json::from_str::<Rating>("0").is_err());
        assert!(serde_json::from_str::<Rating>("101").is_err());
        assert!(serde_json::from_str::<Rating>("50.5").is_err());
        assert!(serde_json::from_str::<Rating>("\"50\"").is_err());
        assert_eq!(
            serde_json::to_string(&Rating::new(73).unwrap()).unwrap(),
            "73"
        );
    }

    #[test]
    fn track_rating_keeps_capability_and_value_coherent() {
        let value = Rating::new(80).unwrap();
        assert_eq!(TrackRating::unsupported().value(), None);
        assert_eq!(
            TrackRating::read_only(Some(value)).capability(),
            RatingCapability::ReadOnly
        );
        assert_eq!(TrackRating::read_only(Some(value)).value(), Some(value));
        assert_eq!(
            TrackRating::writable(None).capability(),
            RatingCapability::Writable
        );
        assert!(RatingCapability::ReadOnly.is_readable());
        assert!(!RatingCapability::ReadOnly.is_writable());
        assert!(RatingCapability::Writable.is_writable());
        assert!(!RatingCapability::Unsupported.is_readable());
    }

    #[test]
    fn track_rating_tagged_serde_pins_every_supported_state() {
        let value = Rating::new(64).unwrap();
        for (rating, expected) in [
            (
                TrackRating::unsupported(),
                serde_json::json!({"capability": "unsupported"}),
            ),
            (
                TrackRating::read_only(None),
                serde_json::json!({"capability": "read_only"}),
            ),
            (
                TrackRating::read_only(Some(value)),
                serde_json::json!({"capability": "read_only", "value": 64}),
            ),
            (
                TrackRating::writable(None),
                serde_json::json!({"capability": "writable"}),
            ),
            (
                TrackRating::writable(Some(value)),
                serde_json::json!({"capability": "writable", "value": 64}),
            ),
        ] {
            assert_eq!(serde_json::to_value(rating).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<TrackRating>(expected).unwrap(),
                rating
            );
        }
    }

    #[test]
    fn legacy_track_json_without_rating_defaults_to_unsupported() {
        let original = Track {
            id: Uuid::new_v4(),
            native_track_id: None,
            title: "Legacy Song".to_string(),
            artist_name: "Legacy Artist".to_string(),
            album_artist_name: None,
            artist_id: None,
            album_title: "Legacy Album".to_string(),
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
            rating: TrackRating::writable(Some(Rating::new(90).unwrap())),
            last_played: None,
        };
        let mut legacy = serde_json::to_value(&original).unwrap();
        legacy
            .as_object_mut()
            .expect("track serializes as an object")
            .remove("rating");

        let restored: Track = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.rating, TrackRating::unsupported());
    }

    #[test]
    fn native_rating_scales_have_pinned_boundary_behavior() {
        assert_eq!(Rating::from_five_star_scale(1).unwrap().value(), 20);
        assert_eq!(Rating::from_five_star_scale(5).unwrap().value(), 100);
        assert_eq!(Rating::from_five_star_scale(0), None);
        assert_eq!(Rating::from_five_star_scale(6), None);

        assert_eq!(Rating::from_ten_point_scale(0.0).unwrap().value(), 1);
        assert_eq!(Rating::from_ten_point_scale(0.04).unwrap().value(), 1);
        assert_eq!(Rating::from_ten_point_scale(0.15).unwrap().value(), 2);
        assert_eq!(Rating::from_ten_point_scale(7.34).unwrap().value(), 73);
        assert_eq!(Rating::from_ten_point_scale(10.0).unwrap().value(), 100);
        assert_eq!(Rating::from_ten_point_scale(-f64::EPSILON), None);
        assert_eq!(Rating::from_ten_point_scale(10.000_001), None);
        assert_eq!(Rating::from_ten_point_scale(f64::NAN), None);
        assert_eq!(Rating::from_ten_point_scale(f64::INFINITY), None);
    }
}
