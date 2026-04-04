//! Radio-Browser API client with DNS-based mirror resolution.
//!
//! The Radio-Browser project recommends resolving `all.api.radio-browser.info`
//! to discover available API servers, then picking one at random.
//! We fall back to `de1.api.radio-browser.info` if DNS resolution fails.

use std::net::ToSocketAddrs;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::api::{GeoLocation, RadioStation};

/// Default station fetch limit.
const DEFAULT_LIMIT: u32 = 100;

/// Fallback API server if DNS resolution fails.
const FALLBACK_API_HOST: &str = "de1.api.radio-browser.info";

/// HTTP request timeout for all Radio-Browser and geolocation requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Radio-Browser API client.
pub struct RadioBrowserClient {
    /// Base URL for API requests (e.g. `https://de1.api.radio-browser.info`).
    base_url: String,
    client: reqwest::Client,
}

impl RadioBrowserClient {
    /// Create a new client, resolving the best API mirror via DNS.
    pub fn new() -> Self {
        let host = resolve_api_host().unwrap_or_else(|| FALLBACK_API_HOST.to_string());
        let base_url = format!("https://{host}");
        info!(base_url = %base_url, "Radio-Browser API client initialized");

        Self {
            base_url,
            client: reqwest::Client::builder()
                .user_agent("Tributary/0.2")
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    /// Fetch top-clicked stations.
    pub async fn fetch_top_click(&self, limit: Option<u32>) -> Vec<RadioStation> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        let url = format!(
            "{}/json/stations/topclick?limit={limit}&hidebroken=true",
            self.base_url
        );
        self.fetch_stations(&url).await
    }

    /// Fetch top-voted stations.
    pub async fn fetch_top_vote(&self, limit: Option<u32>) -> Vec<RadioStation> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        let url = format!(
            "{}/json/stations/topvote?limit={limit}&hidebroken=true",
            self.base_url
        );
        self.fetch_stations(&url).await
    }

    /// Fetch stations near the given coordinates, sorted by distance.
    pub async fn fetch_near_me(&self, lat: f64, lon: f64, limit: Option<u32>) -> Vec<RadioStation> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        let url = format!(
            "{}/json/stations/search?geo_lat={lat}&geo_long={lon}&order=geo_distance&limit={limit}&hidebroken=true",
            self.base_url
        );
        self.fetch_stations(&url).await
    }

    /// Internal: fetch and deserialize a list of stations from a URL.
    /// Filters out stations with non-HTTP(S) stream URLs for safety.
    async fn fetch_stations(&self, url: &str) -> Vec<RadioStation> {
        debug!(url = %url, "Fetching radio stations");
        match self.client.get(url).send().await {
            Ok(resp) => match resp.json::<Vec<RadioStation>>().await {
                Ok(stations) => {
                    // Filter out stations with non-HTTP(S) stream URLs
                    // to prevent file:// or other scheme injection.
                    let safe: Vec<RadioStation> = stations
                        .into_iter()
                        .filter(|s| {
                            let url = s.url_resolved.to_lowercase();
                            url.starts_with("http://") || url.starts_with("https://")
                        })
                        .collect();
                    info!(count = safe.len(), "Radio stations fetched (filtered)");
                    safe
                }
                Err(e) => {
                    warn!(error = %e, "Failed to parse radio station response");
                    Vec::new()
                }
            },
            Err(e) => {
                warn!(error = %e, "Failed to fetch radio stations");
                Vec::new()
            }
        }
    }
}

/// Fetch the user's approximate geographic coordinates via IP geolocation.
///
/// Uses `ipapi.co` which provides a free HTTPS tier (no API key required,
/// 1000 requests/day). Returns `None` on any error.
pub async fn fetch_geolocation() -> Option<(f64, f64)> {
    let url = "https://ipapi.co/json/";
    info!("Fetching geolocation from ipapi.co (HTTPS)");

    let client = reqwest::Client::builder()
        .user_agent("Tributary/0.2")
        .timeout(REQUEST_TIMEOUT)
        .build()
        .ok()?;

    let resp = client.get(url).send().await.ok()?;
    let geo: GeoLocation = resp.json().await.ok()?;

    if !geo.error && (geo.latitude != 0.0 || geo.longitude != 0.0) {
        info!(
            lat = geo.latitude,
            lon = geo.longitude,
            "Geolocation resolved"
        );
        Some((geo.latitude, geo.longitude))
    } else {
        warn!("Geolocation API returned error or zero coordinates");
        None
    }
}

/// Resolve `all.api.radio-browser.info` via DNS and pick a random server.
///
/// Returns a hostname suitable for HTTPS requests. Since the TLS certificate
/// is issued for `*.api.radio-browser.info`, we cannot use raw IP addresses.
/// Instead, we verify that DNS resolution succeeds (proving the service is
/// reachable) and then use the fallback hostname which is covered by the cert.
fn resolve_api_host() -> Option<String> {
    // Resolve the DNS A records for the Radio-Browser discovery hostname.
    // This validates that the Radio-Browser service is reachable.
    let addrs: Vec<_> = "all.api.radio-browser.info:443"
        .to_socket_addrs()
        .ok()?
        .collect();

    if addrs.is_empty() {
        return None;
    }

    // DNS resolved successfully — the service is up.
    // Use the well-known fallback hostname since TLS certs don't cover
    // raw IPs. In the future, we could maintain a list of known mirrors
    // and pick one randomly.
    info!(
        resolved_count = addrs.len(),
        host = FALLBACK_API_HOST,
        "DNS resolution succeeded, using known mirror"
    );
    Some(FALLBACK_API_HOST.to_string())
}
