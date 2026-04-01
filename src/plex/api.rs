//! Plex REST API JSON response types.
//!
//! Plex natively returns XML, but responds with JSON when the request
//! includes `Accept: application/json`.  Only the subset of fields
//! Tributary actually uses are deserialized; unknown fields are silently
//! ignored via `serde(default)`.

#![allow(dead_code)]

use serde::Deserialize;

// ── POST https://plex.tv/users/sign_in.json ─────────────────────────────

/// Response from `POST https://plex.tv/users/sign_in.json`.
#[derive(Debug, Deserialize)]
pub struct PlexSignInResponse {
    pub user: PlexSignInUser,
}

/// The `user` object inside the Plex sign-in response.
#[derive(Debug, Deserialize)]
pub struct PlexSignInUser {
    #[serde(rename = "authToken")]
    pub auth_token: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

// ── GET /library/sections ───────────────────────────────────────────────

/// Top-level response from `/library/sections`.
///
/// Contains all library sections configured on the Plex server.
/// Tributary filters this to keep only sections where `type == "artist"`
/// (Plex's designation for music libraries).
#[derive(Debug, Deserialize)]
pub struct PlexSectionsResponse {
    #[serde(rename = "MediaContainer")]
    pub media_container: PlexSectionsContainer,
}

/// The `MediaContainer` wrapper inside the sections response.
#[derive(Debug, Deserialize)]
pub struct PlexSectionsContainer {
    /// Total number of library sections on the server.
    #[serde(default)]
    pub size: u32,

    /// The individual library section directories.
    #[serde(rename = "Directory", default)]
    pub directory: Vec<PlexDirectory>,
}

/// A single library section (directory) on the Plex server.
#[derive(Debug, Deserialize)]
pub struct PlexDirectory {
    /// Human-readable library name (e.g. "Music", "Vinyl Rips").
    pub title: String,

    /// Numeric key used to reference this section in other API calls.
    pub key: String,

    /// The section type.
    ///
    /// Plex music libraries report `type = "artist"`.
    /// Movie libraries report `"movie"`, TV shows report `"show"`,
    /// photo libraries report `"photo"`.
    #[serde(rename = "type")]
    pub section_type: String,

    /// Unique identifier for this library section.
    #[serde(default)]
    pub uuid: Option<String>,

    /// The scanner agent used for this library.
    #[serde(default)]
    pub agent: Option<String>,
}

// ── GET /library/sections/{key}/all?type=10 (tracks) ────────────────────

/// Response from `/library/sections/{key}/all?type=10`.
#[derive(Debug, Deserialize)]
pub struct PlexTracksResponse {
    #[serde(rename = "MediaContainer")]
    pub media_container: PlexTracksContainer,
}

/// The `MediaContainer` wrapper for track listings.
#[derive(Debug, Deserialize)]
pub struct PlexTracksContainer {
    #[serde(default)]
    pub size: u32,

    #[serde(rename = "Metadata", default)]
    pub metadata: Vec<PlexTrack>,
}

/// A single track from the Plex library.
#[derive(Debug, Deserialize)]
pub struct PlexTrack {
    /// Unique rating key (Plex's internal ID for this track).
    #[serde(rename = "ratingKey")]
    pub rating_key: String,

    /// Track title.
    #[serde(default)]
    pub title: Option<String>,

    /// Artist name (grandparent in Plex's hierarchy).
    #[serde(rename = "grandparentTitle", default)]
    pub grandparent_title: Option<String>,

    /// Artist rating key.
    #[serde(rename = "grandparentRatingKey", default)]
    pub grandparent_rating_key: Option<String>,

    /// Album name (parent in Plex's hierarchy).
    #[serde(rename = "parentTitle", default)]
    pub parent_title: Option<String>,

    /// Album rating key.
    #[serde(rename = "parentRatingKey", default)]
    pub parent_rating_key: Option<String>,

    /// Track number within the disc.
    #[serde(default)]
    pub index: Option<u32>,

    /// Disc number.
    #[serde(rename = "parentIndex", default)]
    pub parent_index: Option<u32>,

    /// Duration in milliseconds.
    #[serde(default)]
    pub duration: Option<u64>,

    /// Year of release.
    #[serde(default)]
    pub year: Option<i32>,

    /// Thumbnail path (relative to server base URL).
    #[serde(default)]
    pub thumb: Option<String>,

    /// Media info (bitrate, audio channels, stream parts).
    #[serde(rename = "Media", default)]
    pub media: Vec<PlexMedia>,

    /// View count (play count).
    #[serde(rename = "viewCount", default)]
    pub view_count: Option<u32>,
}

/// Media container info for a track.
#[derive(Debug, Deserialize)]
pub struct PlexMedia {
    /// Bitrate in kbps.
    #[serde(default)]
    pub bitrate: Option<u32>,

    /// Number of audio channels.
    #[serde(rename = "audioChannels", default)]
    pub audio_channels: Option<u32>,

    /// Audio codec (e.g. "flac", "mp3", "aac").
    #[serde(rename = "audioCodec", default)]
    pub audio_codec: Option<String>,

    /// Container format.
    #[serde(default)]
    pub container: Option<String>,

    /// Stream parts (contains the actual file/stream key).
    #[serde(rename = "Part", default)]
    pub part: Vec<PlexPart>,
}

/// A single stream part — contains the key used to build the stream URL.
#[derive(Debug, Deserialize)]
pub struct PlexPart {
    /// Relative path to the stream (e.g. `/library/parts/12345/file.flac`).
    #[serde(default)]
    pub key: Option<String>,

    /// File size in bytes.
    #[serde(default)]
    pub size: Option<u64>,

    /// Container format.
    #[serde(default)]
    pub container: Option<String>,
}

// ── GET /library/sections/{key}/all?type=9 (albums) ─────────────────────

/// Response from `/library/sections/{key}/all?type=9`.
#[derive(Debug, Deserialize)]
pub struct PlexAlbumsResponse {
    #[serde(rename = "MediaContainer")]
    pub media_container: PlexAlbumsContainer,
}

/// The `MediaContainer` wrapper for album listings.
#[derive(Debug, Deserialize)]
pub struct PlexAlbumsContainer {
    #[serde(default)]
    pub size: u32,

    #[serde(rename = "Metadata", default)]
    pub metadata: Vec<PlexAlbum>,
}

/// A single album from the Plex library.
#[derive(Debug, Deserialize)]
pub struct PlexAlbum {
    /// Unique rating key.
    #[serde(rename = "ratingKey")]
    pub rating_key: String,

    /// Album title.
    #[serde(default)]
    pub title: Option<String>,

    /// Artist name (parent in Plex's hierarchy).
    #[serde(rename = "parentTitle", default)]
    pub parent_title: Option<String>,

    /// Artist rating key.
    #[serde(rename = "parentRatingKey", default)]
    pub parent_rating_key: Option<String>,

    /// Release year.
    #[serde(default)]
    pub year: Option<i32>,

    /// Number of tracks in this album.
    #[serde(rename = "leafCount", default)]
    pub leaf_count: Option<u32>,

    /// Total duration in milliseconds.
    #[serde(default)]
    pub duration: Option<u64>,

    /// Thumbnail path.
    #[serde(default)]
    pub thumb: Option<String>,

    /// Genre tags.
    #[serde(rename = "Genre", default)]
    pub genre: Vec<PlexTag>,
}

/// A tag object (used for genres, etc.).
#[derive(Debug, Deserialize)]
pub struct PlexTag {
    #[serde(default)]
    pub tag: Option<String>,
}

// ── GET /library/sections/{key}/all?type=8 (artists) ────────────────────

/// Response from `/library/sections/{key}/all?type=8`.
#[derive(Debug, Deserialize)]
pub struct PlexArtistsResponse {
    #[serde(rename = "MediaContainer")]
    pub media_container: PlexArtistsContainer,
}

/// The `MediaContainer` wrapper for artist listings.
#[derive(Debug, Deserialize)]
pub struct PlexArtistsContainer {
    #[serde(default)]
    pub size: u32,

    #[serde(rename = "Metadata", default)]
    pub metadata: Vec<PlexArtist>,
}

/// A single artist from the Plex library.
#[derive(Debug, Deserialize)]
pub struct PlexArtist {
    /// Unique rating key.
    #[serde(rename = "ratingKey")]
    pub rating_key: String,

    /// Artist name.
    #[serde(default)]
    pub title: Option<String>,

    /// Thumbnail path.
    #[serde(default)]
    pub thumb: Option<String>,
}

// ── GET /identity ───────────────────────────────────────────────────────

/// Response from `/identity` — used as a lightweight health check.
#[derive(Debug, Deserialize)]
pub struct PlexIdentityResponse {
    #[serde(rename = "MediaContainer")]
    pub media_container: PlexIdentityContainer,
}

/// The `MediaContainer` wrapper inside the identity response.
#[derive(Debug, Deserialize)]
pub struct PlexIdentityContainer {
    /// The Plex server's machine identifier.
    #[serde(rename = "machineIdentifier", default)]
    pub machine_identifier: Option<String>,

    /// The Plex server's version string.
    #[serde(default)]
    pub version: Option<String>,
}
