//! Bounded Radio-Browser and IP-geolocation HTTP clients.
//!
//! Construction is deliberately local and nonblocking. Radio-Browser's old
//! mirror-discovery recommendation resolves a DNS name to IP addresses, but
//! those addresses cannot be used as HTTPS authorities and the result did not
//! identify a usable certificate-covered hostname. Tributary instead starts
//! with one known HTTPS mirror and performs all network work inside lifecycle-
//! owned, cancellable refresh tasks.

use std::time::Duration;

use tracing::{debug, info, warn};
use url::Url;

use crate::http_body::ResponseBodyError;

use super::api::{FreeIpApiResponse, GeoLocation, IpApiCoResponse, IpWhoIsResponse, RadioStation};

/// Default and maximum station counts accepted in one request.
const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;

/// A certificate-covered Radio-Browser mirror known to support the JSON API.
const RADIO_BROWSER_API_BASE: &str = "https://de1.api.radio-browser.info";
const USER_AGENT: &str = concat!("Tributary/", env!("CARGO_PKG_VERSION"));

/// End-to-end deadline for headers and each finite response body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum buffered finite responses.
const MAX_STATION_BODY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GEOLOCATION_BODY_BYTES: u64 = 256 * 1024;

/// Bounds for untrusted values copied into query parameters or retained as a
/// public stream locator.
const MAX_FILTER_BYTES: usize = 256;
const MAX_PUBLIC_STREAM_URL_BYTES: usize = 16 * 1024;

/// Closed, detail-free Radio-Browser failure categories.
///
/// The value cannot retain a URL, status, response body, native station ID,
/// provider payload, or reqwest error chain. This makes it safe to cross the
/// adapter/lifecycle boundary and to persist in a snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum RadioClientError {
    #[error("radio HTTP client construction failed")]
    ClientConstruction,
    #[error("radio request timed out")]
    Timeout,
    #[error("radio transport failed")]
    Transport,
    #[error("radio service returned an HTTP error")]
    HttpStatus,
    #[error("radio response exceeded its size policy")]
    BodyLimit,
    #[error("radio response could not be parsed")]
    Parse,
    #[error("radio response contained invalid data")]
    InvalidResponse,
}

impl RadioClientError {
    /// Prefer the most operationally useful category when every independent
    /// geolocation provider (or Near Me tier) fails.
    pub(super) const fn priority(self) -> u8 {
        match self {
            Self::Timeout => 7,
            Self::Transport => 6,
            Self::BodyLimit => 5,
            Self::HttpStatus => 4,
            Self::Parse => 3,
            Self::InvalidResponse => 2,
            Self::ClientConstruction => 1,
        }
    }

    pub(super) const fn prefer(self, other: Self) -> Self {
        if other.priority() > self.priority() {
            other
        } else {
            self
        }
    }
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

/// Stateless client for finite Radio-Browser API requests.
pub(super) struct RadioBrowserClient {
    base_url: Url,
    client: reqwest::Client,
    policy: RequestPolicy,
}

impl RadioBrowserClient {
    /// Construct a client without DNS or any other network operation.
    pub(super) fn new() -> Result<Self, RadioClientError> {
        let base_url =
            Url::parse(RADIO_BROWSER_API_BASE).map_err(|_| RadioClientError::ClientConstruction)?;
        let client = public_http_client()?;
        info!(
            host = base_url.host_str(),
            "Radio-Browser API client initialized"
        );
        Ok(Self {
            base_url,
            client,
            policy: RequestPolicy::PRODUCTION,
        })
    }

    #[cfg(test)]
    fn with_http_client(base_url: String, client: reqwest::Client) -> Self {
        Self::with_test_policy(base_url, client, RequestPolicy::PRODUCTION)
    }

    #[cfg(test)]
    fn with_test_policy(base_url: String, client: reqwest::Client, policy: RequestPolicy) -> Self {
        Self {
            base_url: Url::parse(&base_url).expect("fixture base URL"),
            client,
            policy,
        }
    }

    pub(super) async fn fetch_top_click(
        &self,
        limit: Option<u32>,
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        let url = self.station_url("json/stations/topclick", limit, &[])?;
        self.fetch_stations(url).await
    }

    pub(super) async fn fetch_top_vote(
        &self,
        limit: Option<u32>,
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        let url = self.station_url("json/stations/topvote", limit, &[])?;
        self.fetch_stations(url).await
    }

    /// Fetch the coordinate tier used by Near Me.
    pub(super) async fn fetch_near_me(
        &self,
        latitude: f64,
        longitude: f64,
        limit: Option<u32>,
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        validate_coordinates(latitude, longitude)?;
        let latitude = latitude.to_string();
        let longitude = longitude.to_string();
        let filters = [
            ("geo_lat", latitude.as_str()),
            ("geo_long", longitude.as_str()),
            ("order", "geo_distance"),
            ("has_geo_info", "true"),
        ];
        let url = self.station_url("json/stations/search", limit, &filters)?;
        self.fetch_stations(url).await
    }

    /// Fetch the coordinate tier constrained to a country.
    pub(super) async fn fetch_near_me_with_country(
        &self,
        latitude: f64,
        longitude: f64,
        country_code: &str,
        limit: Option<u32>,
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        validate_coordinates(latitude, longitude)?;
        validate_filter(country_code)?;
        let latitude = latitude.to_string();
        let longitude = longitude.to_string();
        let filters = [
            ("geo_lat", latitude.as_str()),
            ("geo_long", longitude.as_str()),
            ("order", "geo_distance"),
            ("has_geo_info", "true"),
            ("countrycode", country_code),
        ];
        let url = self.station_url("json/stations/search", limit, &filters)?;
        self.fetch_stations(url).await
    }

    /// Fetch the state/province tier, including stations without coordinates.
    pub(super) async fn fetch_near_me_with_state(
        &self,
        country_code: &str,
        state: &str,
        limit: Option<u32>,
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        validate_filter(country_code)?;
        validate_filter(state)?;
        let filters = [
            ("countrycode", country_code),
            ("state", state),
            ("order", "votes"),
            ("reverse", "true"),
        ];
        let url = self.station_url("json/stations/search", limit, &filters)?;
        self.fetch_stations(url).await
    }

    /// Fetch the country fallback tier, including stations without location
    /// metadata more precise than their country.
    pub(super) async fn fetch_near_me_country_only(
        &self,
        country_code: &str,
        limit: Option<u32>,
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        validate_filter(country_code)?;
        let filters = [
            ("countrycode", country_code),
            ("order", "votes"),
            ("reverse", "true"),
        ];
        let url = self.station_url("json/stations/search", limit, &filters)?;
        self.fetch_stations(url).await
    }

    fn station_url(
        &self,
        path: &str,
        limit: Option<u32>,
        filters: &[(&str, &str)],
    ) -> Result<Url, RadioClientError> {
        let mut url = self
            .base_url
            .join(path)
            .map_err(|_| RadioClientError::ClientConstruction)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in filters {
                query.append_pair(key, value);
            }
            query
                .append_pair("limit", &limit.to_string())
                .append_pair("hidebroken", "true");
        }
        Ok(url)
    }

    async fn fetch_stations(&self, url: Url) -> Result<Vec<RadioStation>, RadioClientError> {
        debug!("Fetching a Radio-Browser station view");
        let response = send_bounded(&self.client, url, self.policy.timeout).await?;
        let body = read_bounded(
            response,
            self.policy.max_station_body_bytes,
            self.policy.timeout,
        )
        .await?;
        let stations: Vec<RadioStation> =
            serde_json::from_slice(&body).map_err(|_| RadioClientError::Parse)?;

        // Individual malformed rows do not turn an otherwise valid list into
        // a failed refresh. The adapter performs identity validation; this
        // boundary admits only bounded, authority-free HTTP(S) stream URLs.
        let stations: Vec<_> = stations
            .into_iter()
            .filter(|station| validated_public_stream_url(&station.url_resolved).is_ok())
            .collect();
        info!(count = stations.len(), "Radio-Browser station view fetched");
        Ok(stations)
    }
}

fn public_http_client() -> Result<reqwest::Client, RadioClientError> {
    crate::http_security::public_client_builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| RadioClientError::ClientConstruction)
}

async fn send_bounded(
    client: &reqwest::Client,
    url: Url,
    timeout: Duration,
) -> Result<reqwest::Response, RadioClientError> {
    // lgtm[rs/cleartext-transmission] Production endpoints are HTTPS. Fixture
    // and public radio stream URLs can be HTTP and carry no credentials.
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(map_reqwest_error)?;
    if !response.status().is_success() {
        return Err(RadioClientError::HttpStatus);
    }
    Ok(response)
}

async fn read_bounded(
    response: reqwest::Response,
    maximum: u64,
    timeout: Duration,
) -> Result<Vec<u8>, RadioClientError> {
    crate::http_body::read_limited(response, maximum, timeout)
        .await
        .map_err(map_body_error)
}

fn map_reqwest_error(error: reqwest::Error) -> RadioClientError {
    if error.is_timeout() {
        RadioClientError::Timeout
    } else {
        RadioClientError::Transport
    }
}

fn map_body_error(error: ResponseBodyError) -> RadioClientError {
    match error {
        ResponseBodyError::DeadlineExceeded { .. } => RadioClientError::Timeout,
        ResponseBodyError::Transport(error) if error.is_timeout() => RadioClientError::Timeout,
        ResponseBodyError::Transport(_) | ResponseBodyError::BlockingTransport { .. } => {
            RadioClientError::Transport
        }
        ResponseBodyError::TooLarge { .. }
        | ResponseBodyError::InvalidLimit { .. }
        | ResponseBodyError::AllocationFailed { .. } => RadioClientError::BodyLimit,
    }
}

pub(super) fn validated_public_stream_url(value: &str) -> Result<Url, RadioClientError> {
    if value.is_empty() || value.len() > MAX_PUBLIC_STREAM_URL_BYTES {
        return Err(RadioClientError::InvalidResponse);
    }
    let url = Url::parse(value).map_err(|_| RadioClientError::InvalidResponse)?;
    if !url.cannot_be_a_base()
        && matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
    {
        Ok(url)
    } else {
        Err(RadioClientError::InvalidResponse)
    }
}

fn validate_coordinates(latitude: f64, longitude: f64) -> Result<(), RadioClientError> {
    if latitude.is_finite()
        && longitude.is_finite()
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude)
    {
        Ok(())
    } else {
        Err(RadioClientError::InvalidResponse)
    }
}

fn validate_filter(value: &str) -> Result<(), RadioClientError> {
    if !value.is_empty() && value.len() <= MAX_FILTER_BYTES && !value.chars().any(char::is_control)
    {
        Ok(())
    } else {
        Err(RadioClientError::InvalidResponse)
    }
}

/// Fetch the user's approximate geographic coordinates via a bounded cascade
/// of three HTTPS providers.
pub(super) async fn fetch_geolocation() -> Result<GeoLocation, RadioClientError> {
    let client = public_http_client()?;
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
) -> Result<GeoLocation, RadioClientError> {
    fetch_geolocation_with_policy(client, endpoints, RequestPolicy::PRODUCTION).await
}

async fn fetch_geolocation_with_policy(
    client: &reqwest::Client,
    endpoints: &GeolocationEndpoints<'_>,
    policy: RequestPolicy,
) -> Result<GeoLocation, RadioClientError> {
    let mut preferred = RadioClientError::ClientConstruction;

    info!("Geolocation: trying first HTTPS provider");
    match try_ipapi_co(client, endpoints.ipapi_co, policy).await {
        Ok(location) => return Ok(location),
        Err(error) => preferred = preferred.prefer(error),
    }

    info!("Geolocation: trying second HTTPS provider");
    match try_ipwhois(client, endpoints.ipwhois, policy).await {
        Ok(location) => return Ok(location),
        Err(error) => preferred = preferred.prefer(error),
    }

    info!("Geolocation: trying third HTTPS provider");
    match try_freeipapi(client, endpoints.freeipapi, policy).await {
        Ok(location) => return Ok(location),
        Err(error) => preferred = preferred.prefer(error),
    }

    warn!(category = ?preferred, "All geolocation providers failed");
    Err(preferred)
}

async fn fetch_provider_body(
    client: &reqwest::Client,
    endpoint: &str,
    policy: RequestPolicy,
) -> Result<Vec<u8>, RadioClientError> {
    let endpoint = Url::parse(endpoint).map_err(|_| RadioClientError::ClientConstruction)?;
    let response = send_bounded(client, endpoint, policy.timeout).await?;
    read_bounded(response, policy.max_geolocation_body_bytes, policy.timeout).await
}

async fn try_ipapi_co(
    client: &reqwest::Client,
    endpoint: &str,
    policy: RequestPolicy,
) -> Result<GeoLocation, RadioClientError> {
    let body = fetch_provider_body(client, endpoint, policy).await?;
    let data: IpApiCoResponse =
        serde_json::from_slice(&body).map_err(|_| RadioClientError::Parse)?;
    if data.error {
        return Err(RadioClientError::InvalidResponse);
    }
    validated_location(
        data.latitude,
        data.longitude,
        data.country_code,
        data.region,
    )
}

async fn try_ipwhois(
    client: &reqwest::Client,
    endpoint: &str,
    policy: RequestPolicy,
) -> Result<GeoLocation, RadioClientError> {
    let body = fetch_provider_body(client, endpoint, policy).await?;
    let data: IpWhoIsResponse =
        serde_json::from_slice(&body).map_err(|_| RadioClientError::Parse)?;
    if !data.success {
        return Err(RadioClientError::InvalidResponse);
    }
    validated_location(
        data.latitude,
        data.longitude,
        data.country_code,
        data.region,
    )
}

async fn try_freeipapi(
    client: &reqwest::Client,
    endpoint: &str,
    policy: RequestPolicy,
) -> Result<GeoLocation, RadioClientError> {
    let body = fetch_provider_body(client, endpoint, policy).await?;
    let data: FreeIpApiResponse =
        serde_json::from_slice(&body).map_err(|_| RadioClientError::Parse)?;
    validated_location(
        data.latitude,
        data.longitude,
        data.country_code,
        data.region,
    )
}

fn validated_location(
    latitude: f64,
    longitude: f64,
    country_code: String,
    region: String,
) -> Result<GeoLocation, RadioClientError> {
    validate_coordinates(latitude, longitude)?;
    if (latitude == 0.0 && longitude == 0.0)
        || country_code.len() > MAX_FILTER_BYTES
        || region.len() > MAX_FILTER_BYTES
        || country_code.chars().any(char::is_control)
        || region.chars().any(char::is_control)
    {
        return Err(RadioClientError::InvalidResponse);
    }
    Ok(GeoLocation {
        latitude,
        longitude,
        country_code,
        region,
    })
}

#[cfg(test)]
mod tests {
    use axum::http::{header::LOCATION, HeaderValue, StatusCode};

    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};

    use super::*;

    fn fixture_client() -> reqwest::Client {
        crate::http_security::public_client_builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build public fixture client")
    }

    fn station(id: &str, stream: &str) -> serde_json::Value {
        serde_json::json!({
            "stationuuid": id,
            "name": id,
            "url_resolved": stream
        })
    }

    #[test]
    fn construction_is_local_and_selects_a_known_https_authority() {
        let client = RadioBrowserClient::new().expect("construct client without network");
        assert_eq!(
            client.base_url.as_str(),
            "https://de1.api.radio-browser.info/"
        );
    }

    #[tokio::test]
    async fn success_empty_is_distinct_from_failure_and_unsafe_rows_are_skipped() {
        let service = MockHttpService::start(vec![
            MockRoute::get("/json/stations/topclick")
                .with_query("limit", "3")
                .with_query("hidebroken", "true")
                .reply(MockResponse::json(serde_json::json!([
                    station("https", "https://stream.example.test/live?quality=high"),
                    station("http", "http://stream.example.test/live"),
                    station("unsafe", "file:///tmp/not-a-radio-stream")
                ]))),
            MockRoute::get("/json/stations/topvote")
                .with_query("limit", "1")
                .with_query("hidebroken", "true")
                .reply(MockResponse::json(serde_json::json!([]))),
        ])
        .await;
        let client = RadioBrowserClient::with_http_client(service.base_url(), fixture_client());

        let stations = client
            .fetch_top_click(Some(3))
            .await
            .expect("valid list response");
        assert_eq!(stations.len(), 2);
        assert_eq!(stations[0].stationuuid, "https");
        assert_eq!(stations[1].stationuuid, "http");
        assert!(client
            .fetch_top_vote(Some(1))
            .await
            .expect("valid empty response")
            .is_empty());
        service.finish().await;
    }

    #[tokio::test]
    async fn redirect_is_allowed_but_status_and_parse_failures_remain_typed() {
        let service = MockHttpService::start(vec![
            MockRoute::get("/json/stations/topclick")
                .with_query("limit", "1")
                .reply(
                    MockResponse::status(StatusCode::TEMPORARY_REDIRECT)
                        .with_header(LOCATION, HeaderValue::from_static("/mirror/topclick")),
                ),
            MockRoute::get("/mirror/topclick").reply(MockResponse::json(serde_json::json!([
                station("redirected", "https://stream.example.test/live")
            ]))),
            MockRoute::get("/json/stations/topvote")
                .with_query("limit", "1")
                .reply(MockResponse::status(StatusCode::SERVICE_UNAVAILABLE)),
            MockRoute::get("/json/stations/search")
                .with_query("geo_lat", "39")
                .with_query("geo_long", "-86")
                .reply(MockResponse::text("not JSON")),
        ])
        .await;
        let client = RadioBrowserClient::with_http_client(service.base_url(), fixture_client());

        assert_eq!(
            client
                .fetch_top_click(Some(1))
                .await
                .expect("redirected response")[0]
                .stationuuid,
            "redirected"
        );
        assert!(matches!(
            client.fetch_top_vote(Some(1)).await,
            Err(RadioClientError::HttpStatus)
        ));
        assert!(matches!(
            client.fetch_near_me(39.0, -86.0, Some(1)).await,
            Err(RadioClientError::Parse)
        ));
        service.finish().await;
    }

    #[tokio::test]
    async fn deadline_and_streaming_body_cap_have_distinct_categories() {
        let policy = RequestPolicy {
            timeout: Duration::from_millis(25),
            max_station_body_bytes: 128,
            max_geolocation_body_bytes: 128,
        };
        let delayed = MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
            .with_query("limit", "1")
            .reply(
                MockResponse::json(serde_json::json!([station(
                    "late",
                    "https://stream.example.test/live"
                )]))
                .with_delay(Duration::from_millis(100)),
            )])
        .await;
        let client =
            RadioBrowserClient::with_test_policy(delayed.base_url(), fixture_client(), policy);
        assert!(matches!(
            client.fetch_top_click(Some(1)).await,
            Err(RadioClientError::Timeout)
        ));
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
        assert!(matches!(
            client.fetch_top_click(Some(1)).await,
            Err(RadioClientError::BodyLimit)
        ));
        oversized.finish().await;
    }

    #[tokio::test]
    async fn geolocation_stops_at_first_bounded_valid_provider() {
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
        let endpoints = GeolocationEndpoints {
            ipapi_co: &format!("{base_url}/ipapi"),
            ipwhois: &format!("{base_url}/ipwhois"),
            freeipapi: &format!("{base_url}/freeipapi"),
        };

        let location = fetch_geolocation_with(&fixture_client(), &endpoints)
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
    async fn geolocation_rejects_nonfinite_and_out_of_range_coordinates() {
        let service = MockHttpService::start(vec![
            MockRoute::get("/ipapi").reply(MockResponse::json(serde_json::json!({
                "latitude": 91.0,
                "longitude": -86.0,
                "country_code": "US",
                "region": "bad",
                "error": false
            }))),
            MockRoute::get("/ipwhois").reply(MockResponse::json(serde_json::json!({
                "success": true,
                "latitude": 39.0,
                "longitude": -181.0,
                "country_code": "US",
                "region": "bad"
            }))),
            MockRoute::get("/freeipapi").reply(MockResponse::json(serde_json::json!({
                "latitude": 40.0,
                "longitude": -86.0,
                "countryCode": "US",
                "regionName": "Indiana"
            }))),
        ])
        .await;
        let base_url = service.base_url();
        let endpoints = GeolocationEndpoints {
            ipapi_co: &format!("{base_url}/ipapi"),
            ipwhois: &format!("{base_url}/ipwhois"),
            freeipapi: &format!("{base_url}/freeipapi"),
        };

        let location = fetch_geolocation_with(&fixture_client(), &endpoints)
            .await
            .expect("third provider is valid");
        assert!((location.latitude - 40.0).abs() < f64::EPSILON);
        assert_eq!(service.requests().len(), 3);
        service.finish().await;
    }

    #[tokio::test]
    async fn geolocation_rejects_the_all_zero_provider_sentinel() {
        let service = MockHttpService::start(vec![
            MockRoute::get("/ipapi").reply(MockResponse::json(serde_json::json!({
                "latitude": 0.0,
                "longitude": 0.0,
                "country_code": "",
                "region": "",
                "error": false
            }))),
            MockRoute::get("/ipwhois").reply(MockResponse::json(serde_json::json!({
                "success": true,
                "latitude": 39.7684,
                "longitude": -86.1581,
                "country_code": "",
                "region": ""
            }))),
        ])
        .await;
        let base_url = service.base_url();
        let endpoints = GeolocationEndpoints {
            ipapi_co: &format!("{base_url}/ipapi"),
            ipwhois: &format!("{base_url}/ipwhois"),
            freeipapi: &format!("{base_url}/freeipapi"),
        };

        let location = fetch_geolocation_with(&fixture_client(), &endpoints)
            .await
            .expect("second provider is valid without country metadata");
        assert!((location.latitude - 39.7684).abs() < f64::EPSILON);
        assert!(location.country_code.is_empty());
        assert_eq!(service.requests().len(), 2);
        service.finish().await;
    }

    #[tokio::test]
    async fn geolocation_chooses_deterministic_preferred_failure() {
        let service = MockHttpService::start(vec![
            MockRoute::get("/ipapi").reply(MockResponse::status(StatusCode::BAD_GATEWAY)),
            MockRoute::get("/ipwhois").reply(MockResponse::text("not JSON")),
            MockRoute::get("/freeipapi").reply(
                MockResponse::json(serde_json::json!({
                    "latitude": 39.0,
                    "longitude": -86.0,
                    "countryCode": "US",
                    "regionName": "late"
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

        assert!(matches!(
            fetch_geolocation_with_policy(&fixture_client(), &endpoints, policy).await,
            Err(RadioClientError::Timeout)
        ));
        service.finish().await;
    }

    #[tokio::test]
    async fn coordinate_and_filter_validation_happens_before_network_io() {
        let service = MockHttpService::start(Vec::new()).await;
        let client = RadioBrowserClient::with_http_client(service.base_url(), fixture_client());

        assert!(matches!(
            client.fetch_near_me(f64::NAN, 0.0, None).await,
            Err(RadioClientError::InvalidResponse)
        ));
        assert!(matches!(
            client.fetch_near_me_with_state("", "Indiana", None).await,
            Err(RadioClientError::InvalidResponse)
        ));
        assert!(service.requests().is_empty());
        service.finish().await;
    }

    #[tokio::test]
    async fn externally_sourced_filters_are_single_percent_encoded_query_values() {
        let service = MockHttpService::start(vec![MockRoute::get("/json/stations/search")
            .with_query("countrycode", "US&limit=999")
            .with_query("state", "A&B=Somewhere")
            .with_query("order", "votes")
            .with_query("reverse", "true")
            .with_query("limit", "1")
            .with_query("hidebroken", "true")
            .reply(MockResponse::json(serde_json::json!([])))])
        .await;
        let client = RadioBrowserClient::with_http_client(service.base_url(), fixture_client());

        assert!(client
            .fetch_near_me_with_state("US&limit=999", "A&B=Somewhere", Some(1))
            .await
            .expect("encoded query fixture")
            .is_empty());
        assert_eq!(service.requests().len(), 1);
        service.finish().await;
    }

    #[test]
    fn public_stream_validation_is_exact_and_bounded() {
        assert!(
            validated_public_stream_url("https://stream.example.test/Live?token=public").is_ok()
        );
        assert!(validated_public_stream_url("http://stream.example.test/live").is_ok());
        assert!(validated_public_stream_url("file:///tmp/live").is_err());
        assert!(validated_public_stream_url("https://user@stream.example.test/live").is_err());
        assert!(validated_public_stream_url("https://stream.example.test/live#fragment").is_err());
        assert!(validated_public_stream_url(&format!(
            "https://stream.example.test/{}",
            "x".repeat(MAX_PUBLIC_STREAM_URL_BYTES)
        ))
        .is_err());
    }
}
