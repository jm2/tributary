//! Jellyfin REST API JSON response types.
//!
//! Only the subset of fields Tributary actually uses are deserialized;
//! unknown fields are silently ignored via `serde(default)`.

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
}

/// A single item from the Jellyfin library.
///
/// Tributary requests only `Audio` items; absent optional fields are
/// `None` / default.
#[derive(Debug, Deserialize)]
pub struct JellyfinItem {
    #[serde(rename = "Id")]
    pub id: String,

    #[serde(rename = "Name", default)]
    pub name: Option<String>,

    /// Album name.
    #[serde(rename = "Album", default)]
    pub album: Option<String>,

    /// Album ID.
    #[serde(rename = "AlbumId", default)]
    pub album_id: Option<String>,

    /// Album artist display name.
    #[serde(rename = "AlbumArtist", default)]
    pub album_artist: Option<String>,

    /// Artist items array (the first entry is the primary artist).
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

    /// Date the item was created on the server (ISO 8601).
    #[serde(rename = "DateCreated", default)]
    pub date_created: Option<String>,

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
}

/// User-specific data (play count, etc.).
#[derive(Debug, Deserialize)]
pub struct JellyfinUserData {
    #[serde(rename = "PlayCount", default)]
    pub play_count: Option<u32>,
    /// User rating on Jellyfin's nullable decimal zero-through-ten scale.
    #[serde(
        rename = "Rating",
        default,
        deserialize_with = "crate::remote_rating_wire::optional_f64"
    )]
    pub rating: Option<f64>,
}

// ── UDP Discovery ───────────────────────────────────────────────────────

/// Response from Jellyfin UDP broadcast discovery on port 7359.
#[derive(Debug, Deserialize)]
pub struct JellyfinDiscoveryResponse {
    #[serde(rename = "Address")]
    pub address: String,
    #[serde(rename = "Name")]
    pub name: String,
}

// ── GET /System/Ping ────────────────────────────────────────────────────

// The `/System/Ping` endpoint returns a plain string `"Jellyfin Server"`
// with HTTP 200 — no JSON body to deserialize.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_rating_wire_field_is_tolerant() {
        let missing: JellyfinItem =
            serde_json::from_value(serde_json::json!({"Id": "track"})).unwrap();
        assert!(missing.user_data.is_none());

        for (wire, expected) in [
            (serde_json::Value::Null, None),
            (serde_json::json!(7.5), Some(7.5)),
            (serde_json::json!(7), Some(7.0)),
            (serde_json::json!("7.5"), None),
            (serde_json::json!(false), None),
            (serde_json::json!([7.5]), None),
            (serde_json::json!({"value": 7.5}), None),
        ] {
            let item: JellyfinItem = serde_json::from_value(serde_json::json!({
                "Id": "track",
                "UserData": {"Rating": wire},
            }))
            .expect("malformed optional rating must not reject the item");
            assert_eq!(item.user_data.and_then(|data| data.rating), expected);
        }
    }
}
