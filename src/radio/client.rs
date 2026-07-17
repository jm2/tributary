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

/// Maximum buffered Radio-Browser station-list response.
const MAX_STATION_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum buffered response from an IP geolocation provider.
const MAX_GEOLOCATION_BODY_BYTES: u64 = 256 * 1024;

/// Radio-Browser API client.
pub struct RadioBrowserClient {
    /// Base URL for API requests (e.g. `https://de1.api.radio-browser.info`).
    base_url: String,
    client: reqwest::Client,
    policy: RequestPolicy,
}

#[derive(Clone, Copy)]
struct RequestPolicy {
    timeout: Duration,
    max_station_body_bytes: u64,
    max_geolocation_body_bytes: u64,
}

impl RequestPolicy {
    const PRODUCTION: Self = Self {
        timeout: REQUEST_TIMEOUT,
        max_station_body_bytes: MAX_STATION_BODY_BYTES,
        max_geolocation_body_bytes: MAX_GEOLOCATION_BODY_BYTES,
    };
}

impl RadioBrowserClient {
    /// Create a new client, resolving the best API mirror via DNS.
    ///
    /// Fails only when no HTTP client can be built at all (a TLS backend that
    /// will not initialise). There is deliberately no degraded fallback: an
    /// unconfigured client would carry neither the request timeout nor the
    /// redirect policy that keeps these requests off plaintext HTTP.
    pub fn new() -> Result<Self, reqwest::Error> {
        let host = resolve_api_host().unwrap_or_else(|| FALLBACK_API_HOST.to_string());
        let base_url = format!("https://{host}");
        info!(base_url = %base_url, "Radio-Browser API client initialized");

        let client = crate::http_security::public_client_builder()
            .user_agent("Tributary/0.2")
            .timeout(REQUEST_TIMEOUT)
            .build()?;

        Ok(Self {
            base_url,
            client,
            policy: RequestPolicy::PRODUCTION,
        })
    }

    #[cfg(test)]
    fn with_http_client(base_url: String, client: reqwest::Client) -> Self {
        Self {
            base_url,
            client,
            policy: RequestPolicy::PRODUCTION,
        }
    }

    #[cfg(test)]
    fn with_test_policy(base_url: String, client: reqwest::Client, policy: RequestPolicy) -> Self {
        Self {
            base_url,
            client,
            policy,
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
        // Percent-encode the externally-sourced country code (it comes
        // verbatim from a third-party geolocation provider) so a value
        // containing `&`/`=`/`#` cannot inject extra query parameters.
        let encoded_cc = urlencoding::encode(country_code);
        let url = format!(
            "{}/json/stations/search?geo_lat={lat}&geo_long={lon}&order=geo_distance&has_geo_info=true&countrycode={encoded_cc}&limit={limit}&hidebroken=true",
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
        // Percent-encode the externally-sourced country code (see above).
        let encoded_cc = urlencoding::encode(country_code);
        let encoded_state = urlencoding::encode(state);
        let url = format!(
            "{}/json/stations/search?countrycode={encoded_cc}&state={encoded_state}&order=votes&reverse=true&limit={limit}&hidebroken=true",
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
        // Percent-encode the externally-sourced country code (see above).
        let encoded_cc = urlencoding::encode(country_code);
        let url = format!(
            "{}/json/stations/search?countrycode={encoded_cc}&order=votes&reverse=true&limit={limit}&hidebroken=true",
            self.base_url
        );
        self.fetch_stations(&url).await
    }

    /// Internal: fetch and deserialize a list of stations from a URL.
    /// Filters out stations with non-HTTP(S) stream URLs for safety.
    async fn fetch_stations(&self, url: &str) -> Vec<RadioStation> {
        debug!(url = %url, "Fetching radio stations");
        match self
            .client
            .get(url)
            .timeout(self.policy.timeout)
            .send()
            .await
        {
            // lgtm[rs/cleartext-transmission] Base URL is always HTTPS; station stream URLs may be HTTP but carry no sensitive data.
            Ok(resp) if resp.status().is_success() => {
                match crate::http_body::read_limited(
                    resp,
                    self.policy.max_station_body_bytes,
                    self.policy.timeout,
                )
                .await
                {
                    Ok(body) => match serde_json::from_slice::<Vec<RadioStation>>(&body) {
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
                        warn!(error = %e, "Failed to read radio station response");
                        Vec::new()
                    }
                }
            }
            Ok(resp) => {
                warn!(status = %resp.status(), "Radio-Browser request returned an HTTP error");
                Vec::new()
            }
            Err(e) => {
                let e = crate::http_security::strip_request_url(e);
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
    let client = crate::http_security::public_client_builder()
        .user_agent("Tributary/0.2")
        .timeout(REQUEST_TIMEOUT)
        .build()
        .ok()?;

    fetch_geolocation_with(&client, &GeolocationEndpoints::production()).await
}

struct GeolocationEndpoints<'a> {
    ipapi_co: &'a str,
    ipwhois: &'a str,
    freeipapi: &'a str,
}

impl GeolocationEndpoints<'static> {
    const fn production() -> Self {
        Self {
            ipapi_co: "https://ipapi.co/json/",
            ipwhois: "https://ipwho.is/",
            freeipapi: "https://freeipapi.com/api/json",
        }
    }
}

async fn fetch_geolocation_with(
    client: &reqwest::Client,
    endpoints: &GeolocationEndpoints<'_>,
) -> Option<GeoLocation> {
    fetch_geolocation_with_policy(client, endpoints, RequestPolicy::PRODUCTION).await
}

async fn fetch_geolocation_with_policy(
    client: &reqwest::Client,
    endpoints: &GeolocationEndpoints<'_>,
    policy: RequestPolicy,
) -> Option<GeoLocation> {
    // ── Provider 1: ipapi.co ────────────────────────────────────────
    info!("Geolocation: trying ipapi.co (HTTPS)");
    if let Some(geo) = try_ipapi_co(client, endpoints.ipapi_co, policy).await {
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
    if let Some(geo) = try_ipwhois(client, endpoints.ipwhois, policy).await {
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
    if let Some(geo) = try_freeipapi(client, endpoints.freeipapi, policy).await {
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
async fn try_ipapi_co(
    client: &reqwest::Client,
    endpoint: &str,
    policy: RequestPolicy,
) -> Option<GeoLocation> {
    let resp = client
        .get(endpoint)
        .timeout(policy.timeout)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body =
        crate::http_body::read_limited(resp, policy.max_geolocation_body_bytes, policy.timeout)
            .await
            .ok()?;
    let data: IpApiCoResponse = serde_json::from_slice(&body).ok()?;
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
async fn try_ipwhois(
    client: &reqwest::Client,
    endpoint: &str,
    policy: RequestPolicy,
) -> Option<GeoLocation> {
    let resp = client
        .get(endpoint)
        .timeout(policy.timeout)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body =
        crate::http_body::read_limited(resp, policy.max_geolocation_body_bytes, policy.timeout)
            .await
            .ok()?;
    let data: IpWhoIsResponse = serde_json::from_slice(&body).ok()?;
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
async fn try_freeipapi(
    client: &reqwest::Client,
    endpoint: &str,
    policy: RequestPolicy,
) -> Option<GeoLocation> {
    let resp = client
        .get(endpoint)
        .timeout(policy.timeout)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body =
        crate::http_body::read_limited(resp, policy.max_geolocation_body_bytes, policy.timeout)
            .await
            .ok()?;
    let data: FreeIpApiResponse = serde_json::from_slice(&body).ok()?;
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

#[cfg(test)]
mod tests {
    use axum::http::{header::LOCATION, HeaderValue, StatusCode};

    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};

    use super::*;

    fn fixture_client() -> reqwest::Client {
        crate::http_security::public_client_builder()
            .user_agent("Tributary/0.2")
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build public fixture client")
    }

    #[tokio::test]
    async fn radio_browser_fixture_returns_only_http_streams() {
        let service = MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
            .with_query("limit", "3")
            .with_query("hidebroken", "true")
            .reply(MockResponse::json(serde_json::json!([
                {
                    "stationuuid": "https-station",
                    "name": "HTTPS Station",
                    "url_resolved": "https://stream.example.test/live"
                },
                {
                    "stationuuid": "http-station",
                    "name": "HTTP Station",
                    "url_resolved": "http://stream.example.test/live"
                },
                {
                    "stationuuid": "file-station",
                    "name": "File Station",
                    "url_resolved": "file:///tmp/not-a-radio-stream"
                }
            ])))])
        .await;
        let client = RadioBrowserClient::with_http_client(service.base_url(), fixture_client());

        let stations = client.fetch_top_click(Some(3)).await;

        assert_eq!(stations.len(), 2);
        assert_eq!(stations[0].stationuuid, "https-station");
        assert_eq!(stations[1].stationuuid, "http-station");
        let requests = service.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].body.is_empty());
        service.finish().await;
    }

    #[tokio::test]
    async fn geolocation_fixture_stops_after_first_valid_provider() {
        let service = MockHttpService::start(vec![MockRoute::get("/ipapi").reply(
            MockResponse::json(serde_json::json!({
                "latitude": 39.7684,
                "longitude": -86.1581,
                "country_code": "US",
                "region": "Indiana",
                "error": false
            })),
        )])
        .await;
        let base_url = service.base_url();
        let client = fixture_client();
        let endpoints = GeolocationEndpoints {
            ipapi_co: &format!("{base_url}/ipapi"),
            ipwhois: &format!("{base_url}/ipwhois"),
            freeipapi: &format!("{base_url}/freeipapi"),
        };

        let location = fetch_geolocation_with(&client, &endpoints)
            .await
            .expect("fixture geolocation");

        assert!((location.latitude - 39.7684).abs() < 1e-9);
        assert!((location.longitude + 86.1581).abs() < 1e-9);
        assert_eq!(location.country_code, "US");
        assert_eq!(location.region, "Indiana");
        assert_eq!(service.requests().len(), 1);
        service.finish().await;
    }

    #[tokio::test]
    async fn radio_browser_follows_public_redirect_but_rejects_non_success_json() {
        let redirected = MockHttpService::start(vec![
            MockRoute::get("/json/stations/topclick")
                .with_query("limit", "1")
                .reply(
                    MockResponse::status(StatusCode::TEMPORARY_REDIRECT)
                        .with_header(LOCATION, HeaderValue::from_static("/mirror/topclick")),
                ),
            MockRoute::get("/mirror/topclick").reply(MockResponse::json(serde_json::json!([{
                "stationuuid": "redirected",
                "name": "Redirected Station",
                "url_resolved": "https://stream.example.test/live"
            }]))),
        ])
        .await;
        let client = RadioBrowserClient::with_http_client(redirected.base_url(), fixture_client());

        let stations = client.fetch_top_click(Some(1)).await;
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].stationuuid, "redirected");
        redirected.finish().await;

        let failed = MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
            .with_query("limit", "1")
            .reply(
                MockResponse::json(serde_json::json!([{
                    "stationuuid": "must-not-publish",
                    "name": "Error Body Station",
                    "url_resolved": "https://stream.example.test/live"
                }]))
                .with_status(StatusCode::SERVICE_UNAVAILABLE),
            )])
        .await;
        let client = RadioBrowserClient::with_http_client(failed.base_url(), fixture_client());

        assert!(client.fetch_top_click(Some(1)).await.is_empty());
        failed.finish().await;
    }

    #[tokio::test]
    async fn radio_browser_deadline_and_body_cap_fail_closed() {
        let policy = RequestPolicy {
            timeout: Duration::from_millis(25),
            max_station_body_bytes: 128,
            max_geolocation_body_bytes: 128,
        };
        let delayed = MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
            .with_query("limit", "1")
            .reply(
                MockResponse::json(serde_json::json!([{
                    "stationuuid": "too-late",
                    "name": "Too Late",
                    "url_resolved": "https://stream.example.test/live"
                }]))
                .with_delay(Duration::from_millis(100)),
            )])
        .await;
        let client =
            RadioBrowserClient::with_test_policy(delayed.base_url(), fixture_client(), policy);
        assert!(client.fetch_top_click(Some(1)).await.is_empty());
        delayed.finish().await;

        let oversized = MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
            .with_query("limit", "1")
            .reply(MockResponse::text(format!(
                r#"[{{"stationuuid":"oversized","name":"{}","url_resolved":"https://stream.example.test/live"}}]"#,
                "x".repeat(256)
            )))])
        .await;
        let client =
            RadioBrowserClient::with_test_policy(oversized.base_url(), fixture_client(), policy);
        assert!(client.fetch_top_click(Some(1)).await.is_empty());
        oversized.finish().await;
    }

    #[tokio::test]
    async fn geolocation_skips_http_errors_oversized_bodies_and_timeouts() {
        let service = MockHttpService::start(vec![
            MockRoute::get("/ipapi").reply(
                MockResponse::json(serde_json::json!({
                    "latitude": 1.0,
                    "longitude": 2.0,
                    "country_code": "BAD",
                    "region": "HTTP error",
                    "error": false
                }))
                .with_status(StatusCode::SERVICE_UNAVAILABLE),
            ),
            MockRoute::get("/ipwhois").reply(MockResponse::text(format!(
                r#"{{"success":true,"latitude":3.0,"longitude":4.0,"country_code":"BAD","region":"oversized","padding":"{}"}}"#,
                "x".repeat(256)
            ))),
            MockRoute::get("/freeipapi").reply(
                MockResponse::json(serde_json::json!({
                    "latitude": 39.7684,
                    "longitude": -86.1581,
                    "countryCode": "US",
                    "regionName": "Indiana"
                }))
                .with_delay(Duration::from_millis(100)),
            ),
        ])
        .await;
        let base_url = service.base_url();
        let endpoints = GeolocationEndpoints {
            ipapi_co: &format!("{base_url}/ipapi"),
            ipwhois: &format!("{base_url}/ipwhois"),
            freeipapi: &format!("{base_url}/freeipapi"),
        };
        let policy = RequestPolicy {
            timeout: Duration::from_millis(25),
            max_station_body_bytes: 128,
            max_geolocation_body_bytes: 128,
        };

        assert!(
            fetch_geolocation_with_policy(&fixture_client(), &endpoints, policy)
                .await
                .is_none()
        );
        service.finish().await;
    }
}
