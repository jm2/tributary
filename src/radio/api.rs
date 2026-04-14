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

    /// State/province name (e.g. "Indiana").
    #[serde(default)]
    pub state: String,

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

/// Geolocation result from the multi-provider cascade.
#[derive(Debug)]
pub struct GeoLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub country_code: String,
    /// State/region name (e.g. "Indiana", "California").
    pub region: String,
}

// ── Provider-specific response types (internal) ─────────────────────

/// Response from ipapi.co (HTTPS, free tier).
#[derive(Debug, Deserialize)]
pub struct IpApiCoResponse {
    #[serde(default)]
    pub latitude: f64,
    #[serde(default)]
    pub longitude: f64,
    #[serde(default)]
    pub country_code: String,
    /// State/region name (e.g. "California").
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub error: bool,
}

/// Response from ipwho.is (HTTPS, free tier).
#[derive(Debug, Deserialize)]
pub struct IpWhoIsResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub latitude: f64,
    #[serde(default)]
    pub longitude: f64,
    #[serde(default)]
    pub country_code: String,
    /// State/region name.
    #[serde(default)]
    pub region: String,
}

/// Response from freeipapi.com (HTTPS, free tier).
#[derive(Debug, Deserialize)]
pub struct FreeIpApiResponse {
    #[serde(default)]
    pub latitude: f64,
    #[serde(default)]
    pub longitude: f64,
    #[serde(default, rename = "countryCode")]
    pub country_code: String,
    /// State/region name.
    #[serde(default, rename = "regionName")]
    pub region: String,
}
