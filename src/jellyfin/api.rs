//! Jellyfin REST API JSON response types.
//!
//! Only the subset of fields Tributary actually uses are deserialized;
//! unknown fields are silently ignored via `serde(default)`.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ── POST /Users/AuthenticateByName ──────────────────────────────────────

/// Request body for `/Users/AuthenticateByName`.
#[derive(Debug, Serialize)]
pub struct JellyfinAuthRequest {
    #[serde(rename = "Username")]
    pub username: String,
    #[serde(rename = "Pw")]
    pub pw: String,
}

/// Response from `/Users/AuthenticateByName`.
#[derive(Debug, Deserialize)]
pub struct JellyfinAuthResponse {
    #[serde(rename = "User")]
    pub user: JellyfinAuthUser,
    #[serde(rename = "AccessToken")]
    pub access_token: String,
}

/// The `User` object inside the auth response.
#[derive(Debug, Deserialize)]
pub struct JellyfinAuthUser {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
}

// ── GET /Users/{UserId}/Views ───────────────────────────────────────────

/// Top-level response from `/Users/{UserId}/Views`.
///
/// Contains all user-visible library views (music, movies, TV, etc.).
/// Tributary filters this to keep only `CollectionType == "music"`.
#[derive(Debug, Deserialize)]
pub struct JellyfinViewsResponse {
    /// The library view items returned by the server.
    #[serde(rename = "Items", default)]
    pub items: Vec<JellyfinLibraryItem>,

    /// Total number of items (may exceed `items.len()` if paged).
    #[serde(rename = "TotalRecordCount", default)]
    pub total_record_count: u32,
}

/// A single library view / virtual folder on the Jellyfin server.
#[derive(Debug, Deserialize)]
pub struct JellyfinLibraryItem {
    /// Display name of the library (e.g. "Music", "My Albums").
    #[serde(rename = "Name")]
    pub name: String,

    /// Unique identifier for this library on the server.
    #[serde(rename = "Id")]
    pub id: String,

    /// The collection type tag.
    ///
    /// Music libraries have `CollectionType = "music"`.
    /// Video libraries have `"movies"`, `"tvshows"`, etc.
    /// Some custom libraries may have `None`.
    #[serde(rename = "CollectionType", default)]
    pub collection_type: Option<String>,
}

// ── GET /Users/{UserId}/Items (generic paginated response) ──────────────

/// Generic paginated items response from `/Users/{UserId}/Items`.
#[derive(Debug, Deserialize)]
pub struct JellyfinItemsResponse {
    #[serde(rename = "Items", default)]
    pub items: Vec<JellyfinItem>,

    #[serde(rename = "TotalRecordCount", default)]
    pub total_record_count: u32,
}

/// A single item from the Jellyfin library.
///
/// Used for tracks (`Audio`), albums (`MusicAlbum`), and artists
/// (`MusicArtist`). Fields that don't apply to a given type will be
/// `None` / default.
#[derive(Debug, Deserialize)]
pub struct JellyfinItem {
    #[serde(rename = "Id")]
    pub id: String,

    #[serde(rename = "Name", default)]
    pub name: Option<String>,

    /// Item type: `"Audio"`, `"MusicAlbum"`, `"MusicArtist"`, etc.
    #[serde(rename = "Type", default)]
    pub item_type: Option<String>,

    // ── Track fields ────────────────────────────────────────────────
    /// Album name (for tracks).
    #[serde(rename = "Album", default)]
    pub album: Option<String>,

    /// Album ID (for tracks).
    #[serde(rename = "AlbumId", default)]
    pub album_id: Option<String>,

    /// Album artist display name.
    #[serde(rename = "AlbumArtist", default)]
    pub album_artist: Option<String>,

    /// Artist items array (for tracks — first entry is the primary artist).
    #[serde(rename = "ArtistItems", default)]
    pub artist_items: Vec<JellyfinNameId>,

    /// Track number within the disc.
    #[serde(rename = "IndexNumber", default)]
    pub index_number: Option<u32>,

    /// Disc number.
    #[serde(rename = "ParentIndexNumber", default)]
    pub parent_index_number: Option<u32>,

    /// Duration in 100-nanosecond ticks. Divide by 10_000_000 for seconds.
    #[serde(rename = "RunTimeTicks", default)]
    pub run_time_ticks: Option<u64>,

    /// Genre tags.
    #[serde(rename = "Genres", default)]
    pub genres: Vec<String>,

    /// Production year.
    #[serde(rename = "ProductionYear", default)]
    pub production_year: Option<i32>,

    /// Container format (e.g. "flac", "mp3").
    #[serde(rename = "Container", default)]
    pub container: Option<String>,

    /// Media sources (contains bitrate, sample rate info).
    #[serde(rename = "MediaSources", default)]
    pub media_sources: Vec<JellyfinMediaSource>,

    // ── Album fields ────────────────────────────────────────────────
    /// Number of child items (track count for albums).
    #[serde(rename = "ChildCount", default)]
    pub child_count: Option<u32>,

    /// Image tags — presence of `"Primary"` means cover art exists.
    #[serde(rename = "ImageTags", default)]
    pub image_tags: Option<serde_json::Value>,

    /// Date the item was created on the server (ISO 8601).
    #[serde(rename = "DateCreated", default)]
    pub date_created: Option<String>,

    // ── Artist fields ───────────────────────────────────────────────
    /// Number of albums (for artist items).
    #[serde(rename = "AlbumCount", default)]
    pub album_count: Option<u32>,

    /// Play count.
    #[serde(rename = "UserData", default)]
    pub user_data: Option<JellyfinUserData>,
}

/// A name+id pair used in `ArtistItems` and similar arrays.
#[derive(Debug, Deserialize)]
pub struct JellyfinNameId {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Id")]
    pub id: String,
}

/// Media source information (bitrate, sample rate, etc.).
#[derive(Debug, Deserialize)]
pub struct JellyfinMediaSource {
    #[serde(rename = "Bitrate", default)]
    pub bitrate: Option<u32>,

    #[serde(rename = "MediaStreams", default)]
    pub media_streams: Vec<JellyfinMediaStream>,
}

/// A single media stream (audio, video, subtitle).
#[derive(Debug, Deserialize)]
pub struct JellyfinMediaStream {
    #[serde(rename = "Type", default)]
    pub stream_type: Option<String>,

    #[serde(rename = "SampleRate", default)]
    pub sample_rate: Option<u32>,

    #[serde(rename = "BitRate", default)]
    pub bit_rate: Option<u32>,
}

/// User-specific data (play count, etc.).
#[derive(Debug, Deserialize)]
pub struct JellyfinUserData {
    #[serde(rename = "PlayCount", default)]
    pub play_count: Option<u32>,
}

// ── UDP Discovery ───────────────────────────────────────────────────────

/// Response from Jellyfin UDP broadcast discovery on port 7359.
#[derive(Debug, Deserialize)]
pub struct JellyfinDiscoveryResponse {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Address")]
    pub address: String,
    #[serde(rename = "Name")]
    pub name: String,
}

// ── GET /System/Ping ────────────────────────────────────────────────────

// The `/System/Ping` endpoint returns a plain string `"Jellyfin Server"`
// with HTTP 200 — no JSON body to deserialize.
