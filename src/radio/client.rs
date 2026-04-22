//! Radio-Browser API client with DNS-based mirror resolution.
//!
//! The Radio-Browser project recommends resolving `all.api.radio-browser.info`
//! to discover available API servers, then picking one at random.
//! We fall back to `de1.api.radio-browser.info` if DNS resolution fails.

use std::net::ToSocketAddrs;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::api::{FreeIpApiResponse, GeoLocation, IpApiCoResponse, IpWhoIsResponse, RadioStation};

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
    ///
    /// Adds `has_geo_info=true` to ensure only stations with actual
    /// coordinates are returned and properly distance-sorted.
    /// If a `country_code` is provided, results are further filtered
    /// to the user's country for better relevance.
    pub async fn fetch_near_me(&self, lat: f64, lon: f64, limit: Option<u32>) -> Vec<RadioStation> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        let url = format!(
            "{}/json/stations/search?geo_lat={lat}&geo_long={lon}&order=geo_distance&has_geo_info=true&limit={limit}&hidebroken=true",
            self.base_url
        );
        self.fetch_stations(&url).await
    }

    /// Fetch stations near the given coordinates, filtered by country code.
    pub async fn fetch_near_me_with_country(
        &self,
        lat: f64,
        lon: f64,
        country_code: &str,
        limit: Option<u32>,
    ) -> Vec<RadioStation> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        let url = format!(
            "{}/json/stations/search?geo_lat={lat}&geo_long={lon}&order=geo_distance&has_geo_info=true&countrycode={country_code}&limit={limit}&hidebroken=true",
            self.base_url
        );
        self.fetch_stations(&url).await
    }

    /// Fetch stations in a specific state/province, sorted by votes.
    ///
    /// This catches stations that have state metadata but no geo coordinates
    /// (e.g. WBAA in Indiana). No `has_geo_info` filter is applied.
    pub async fn fetch_near_me_with_state(
        &self,
        country_code: &str,
        state: &str,
        limit: Option<u32>,
    ) -> Vec<RadioStation> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        let encoded_state = urlencoding::encode(state);
        let url = format!(
            "{}/json/stations/search?countrycode={country_code}&state={encoded_state}&order=votes&reverse=true&limit={limit}&hidebroken=true",
            self.base_url
        );
        self.fetch_stations(&url).await
    }

    /// Fetch stations by country only (no state/geo), sorted by votes.
    ///
    /// Fallback tier for stations with neither geo coordinates nor state.
    pub async fn fetch_near_me_country_only(
        &self,
        country_code: &str,
        limit: Option<u32>,
    ) -> Vec<RadioStation> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        let url = format!(
            "{}/json/stations/search?countrycode={country_code}&order=votes&reverse=true&limit={limit}&hidebroken=true",
            self.base_url
        );
        self.fetch_stations(&url).await
    }

    /// Internal: fetch and deserialize a list of stations from a URL.
    /// Filters out stations with non-HTTP(S) stream URLs for safety.
    async fn fetch_stations(&self, url: &str) -> Vec<RadioStation> {
        debug!(url = %url, "Fetching radio stations");
        match self.client.get(url).send().await { // lgtm[rs/cleartext-transmission] Base URL is always HTTPS; station stream URLs may be HTTP but carry no sensitive data.
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
/// Uses a multi-provider cascade of reputable HTTPS geolocation APIs:
/// 1. `ipapi.co` — HTTPS, global, 1000 req/day free
/// 2. `ipwho.is` — HTTPS, global, no documented rate limit
/// 3. `freeipapi.com` — HTTPS, global, 60 req/min free
///
/// Returns the first successful result with valid coordinates.
/// Returns `None` if all providers fail.
pub async fn fetch_geolocation() -> Option<GeoLocation> {
    let client = reqwest::Client::builder()
        .user_agent("Tributary/0.2")
        .timeout(REQUEST_TIMEOUT)
        .build()
        .ok()?;

    // ── Provider 1: ipapi.co ────────────────────────────────────────
    info!("Geolocation: trying ipapi.co (HTTPS)");
    if let Some(geo) = try_ipapi_co(&client).await {
        info!(
            lat = geo.latitude,
            lon = geo.longitude,
            cc = %geo.country_code,
            "Geolocation resolved via ipapi.co"
        );
        return Some(geo);
    }

    // ── Provider 2: ipwho.is ────────────────────────────────────────
    info!("Geolocation: trying ipwho.is (HTTPS)");
    if let Some(geo) = try_ipwhois(&client).await {
        info!(
            lat = geo.latitude,
            lon = geo.longitude,
            cc = %geo.country_code,
            "Geolocation resolved via ipwho.is"
        );
        return Some(geo);
    }

    // ── Provider 3: freeipapi.com ───────────────────────────────────
    info!("Geolocation: trying freeipapi.com (HTTPS)");
    if let Some(geo) = try_freeipapi(&client).await {
        info!(
            lat = geo.latitude,
            lon = geo.longitude,
            cc = %geo.country_code,
            "Geolocation resolved via freeipapi.com"
        );
        return Some(geo);
    }

    warn!("All geolocation providers failed");
    None
}

/// Try ipapi.co geolocation.
async fn try_ipapi_co(client: &reqwest::Client) -> Option<GeoLocation> {
    let resp = client.get("https://ipapi.co/json/").send().await.ok()?;
    let data: IpApiCoResponse = resp.json().await.ok()?;
    if !data.error && (data.latitude != 0.0 || data.longitude != 0.0) {
        Some(GeoLocation {
            latitude: data.latitude,
            longitude: data.longitude,
            country_code: data.country_code,
            region: data.region,
        })
    } else {
        None
    }
}

/// Try ipwho.is geolocation.
async fn try_ipwhois(client: &reqwest::Client) -> Option<GeoLocation> {
    let resp = client.get("https://ipwho.is/").send().await.ok()?;
    let data: IpWhoIsResponse = resp.json().await.ok()?;
    if data.success && (data.latitude != 0.0 || data.longitude != 0.0) {
        Some(GeoLocation {
            latitude: data.latitude,
            longitude: data.longitude,
            country_code: data.country_code,
            region: data.region,
        })
    } else {
        None
    }
}

/// Try freeipapi.com geolocation.
async fn try_freeipapi(client: &reqwest::Client) -> Option<GeoLocation> {
    let resp = client
        .get("https://freeipapi.com/api/json")
        .send()
        .await
        .ok()?;
    let data: FreeIpApiResponse = resp.json().await.ok()?;
    if data.latitude != 0.0 || data.longitude != 0.0 {
        Some(GeoLocation {
            latitude: data.latitude,
            longitude: data.longitude,
            country_code: data.country_code,
            region: data.region,
        })
    } else {
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
    let addrs: Vec<_> = "all.api.radio-browser.info:443"
        .to_socket_addrs()
        .ok()?
        .collect();

    if addrs.is_empty() {
        return None;
    }

    info!(
        resolved_count = addrs.len(),
        host = FALLBACK_API_HOST,
        "DNS resolution succeeded, using known mirror"
    );
    Some(FALLBACK_API_HOST.to_string())
}
