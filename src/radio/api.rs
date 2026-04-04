//! Radio-Browser API response types.
//!
//! Only the subset of fields Tributary uses are deserialized;
//! unknown fields are silently ignored via `serde(default)`.

use serde::{Deserialize, Serialize};

/// A radio station from the Radio-Browser API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioStation {
    /// Unique station identifier.
    pub stationuuid: String,

    /// Human-readable station name.
    pub name: String,

    /// Resolved stream URL (may differ from the original `url`).
    #[serde(default)]
    pub url_resolved: String,

    /// Country name (e.g. "United States").
    #[serde(default)]
    pub country: String,

    /// ISO 3166-1 alpha-2 country code (e.g. "US").
    #[serde(default)]
    pub countrycode: String,

    /// Audio codec (e.g. "MP3", "AAC", "OGG").
    #[serde(default)]
    pub codec: String,

    /// Stream bitrate in kbps.
    #[serde(default)]
    pub bitrate: u32,

    /// Comma-separated tags (e.g. "rock,alternative,indie").
    #[serde(default)]
    pub tags: String,

    /// URL to the station's favicon/logo.
    #[serde(default)]
    pub favicon: String,

    /// Geographic latitude of the station.
    #[serde(default)]
    pub geo_lat: Option<f64>,

    /// Geographic longitude of the station.
    #[serde(default)]
    pub geo_long: Option<f64>,
}

/// Response from the ipapi.co geolocation API (HTTPS, free tier).
#[derive(Debug, Deserialize)]
pub struct GeoLocation {
    #[serde(default)]
    pub latitude: f64,
    #[serde(default)]
    pub longitude: f64,
    /// Present and `true` when the API returns an error.
    #[serde(default)]
    pub error: bool,
}
