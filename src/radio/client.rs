//! Bounded Radio-Browser and IP-geolocation HTTP clients.
//!
//! Construction is local and nonblocking. The first station request fetches
//! Radio-Browser's published mirror list (`/json/servers`), keeps only plain
//! hostnames inside the `api.radio-browser.info` zone, shuffles them as the
//! API documentation recommends, and appends a small fixed fallback list. If
//! the mirror list cannot be fetched, the fallback list is used alone. The
//! candidate list is fixed for the client's lifetime.
//!
//! Each station request tries at most [`MAX_MIRROR_ATTEMPTS`] mirrors, starting
//! from the one that last succeeded, and fails over only on transport, timeout,
//! HTTP-status, or parse failures. Every attempt keeps the per-request deadline
//! and body bounds. All network work runs inside lifecycle-owned, cancellable
//! refresh tasks.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};
use url::Url;

use crate::http_body::ResponseBodyError;

use super::api::{FreeIpApiResponse, GeoLocation, IpApiCoResponse, IpWhoIsResponse, RadioStation};

/// Default and maximum station counts accepted in one request.
const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;

/// Radio-Browser's documented mirror-list endpoint. Each entry's `name` is a
/// certificate-covered HTTPS authority serving the full JSON API.
const RADIO_BROWSER_SERVERS_URL: &str = "https://all.api.radio-browser.info/json/servers";
/// Mirrors tried after the discovered ones, or alone when discovery fails.
const FALLBACK_MIRROR_HOSTS: [&str; 2] =
    ["de1.api.radio-browser.info", "de2.api.radio-browser.info"];
/// DNS zone every accepted mirror hostname must belong to, so a mirror list
/// can never direct requests to an unrelated host.
const MIRROR_HOST_SUFFIX: &str = ".api.radio-browser.info";
/// Bounds on the retained mirror list, its response body, and the mirrors
/// tried for one station request.
const MAX_MIRRORS: usize = 16;
const MAX_MIRROR_LIST_BODY_BYTES: u64 = 64 * 1024;
const MAX_MIRROR_ATTEMPTS: usize = 3;
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

    /// Whether another mirror could plausibly serve the same request.
    /// Size-policy and local validation failures would repeat on every mirror.
    const fn allows_mirror_failover(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Transport | Self::HttpStatus | Self::Parse
        )
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

/// One entry of the `/json/servers` mirror list. The `ip` field is ignored:
/// an address cannot serve as the HTTPS authority.
#[derive(Deserialize)]
struct MirrorEntry {
    #[serde(default)]
    name: String,
}

/// Client for finite Radio-Browser API requests with mirror failover.
pub(super) struct RadioBrowserClient {
    /// Mirror-list endpoint consulted once to populate `mirrors`; `None` when
    /// the list was supplied at construction.
    servers_url: Option<Url>,
    /// Ordered, non-empty candidate API bases.
    mirrors: OnceCell<Vec<Url>>,
    /// Index into `mirrors` of the mirror that last served a request.
    preferred_mirror: AtomicUsize,
    client: reqwest::Client,
    policy: RequestPolicy,
}

impl RadioBrowserClient {
    /// Construct a client without DNS or any other network operation.
    pub(super) fn new() -> Result<Self, RadioClientError> {
        let servers_url = Url::parse(RADIO_BROWSER_SERVERS_URL)
            .map_err(|_| RadioClientError::ClientConstruction)?;
        let client = public_http_client()?;
        info!("Radio-Browser API client initialized");
        Ok(Self {
            servers_url: Some(servers_url),
            mirrors: OnceCell::new(),
            preferred_mirror: AtomicUsize::new(0),
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
        Self::with_mirrors(&[base_url], client, policy)
    }

    /// Use exactly `base_urls`, in order, without mirror discovery.
    #[cfg(test)]
    fn with_mirrors(base_urls: &[String], client: reqwest::Client, policy: RequestPolicy) -> Self {
        let mirrors: Vec<Url> = base_urls
            .iter()
            .map(|base_url| Url::parse(base_url).expect("fixture base URL"))
            .collect();
        Self {
            servers_url: None,
            mirrors: OnceCell::from(mirrors),
            preferred_mirror: AtomicUsize::new(0),
            client,
            policy,
        }
    }

    /// Discover mirrors through `servers_url` instead of the production list.
    #[cfg(test)]
    fn with_mirror_discovery(servers_url: &str, client: reqwest::Client) -> Self {
        Self {
            servers_url: Some(Url::parse(servers_url).expect("fixture servers URL")),
            mirrors: OnceCell::new(),
            preferred_mirror: AtomicUsize::new(0),
            client,
            policy: RequestPolicy::PRODUCTION,
        }
    }

    pub(super) async fn fetch_top_click(
        &self,
        limit: Option<u32>,
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        self.fetch_stations("json/stations/topclick", limit, &[])
            .await
    }

    pub(super) async fn fetch_top_vote(
        &self,
        limit: Option<u32>,
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        self.fetch_stations("json/stations/topvote", limit, &[])
            .await
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
        self.fetch_stations("json/stations/search", limit, &filters)
            .await
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
        self.fetch_stations("json/stations/search", limit, &filters)
            .await
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
        self.fetch_stations("json/stations/search", limit, &filters)
            .await
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
        self.fetch_stations("json/stations/search", limit, &filters)
            .await
    }

    /// The candidate mirrors, fetching the published list on first use.
    async fn mirrors(&self) -> &[Url] {
        self.mirrors
            .get_or_init(|| async {
                let discovered = match &self.servers_url {
                    Some(servers_url) => {
                        // Boxed so every station-view future stays small.
                        match Box::pin(fetch_mirror_hosts(
                            &self.client,
                            servers_url.clone(),
                            self.policy,
                        ))
                        .await
                        {
                            Ok(hosts) => hosts,
                            Err(error) => {
                                warn!(
                                    category = ?error,
                                    "Radio-Browser mirror list unavailable; using fallback mirrors"
                                );
                                Vec::new()
                            }
                        }
                    }
                    None => Vec::new(),
                };
                let mirrors = mirror_candidates(discovered);
                info!(mirrors = mirrors.len(), "Radio-Browser mirror list ready");
                mirrors
            })
            .await
    }

    /// Fetch one station view, failing over across a bounded number of
    /// mirrors and remembering the mirror that served it.
    async fn fetch_stations(
        &self,
        path: &str,
        limit: Option<u32>,
        filters: &[(&str, &str)],
    ) -> Result<Vec<RadioStation>, RadioClientError> {
        let mirrors = self.mirrors().await;
        if mirrors.is_empty() {
            return Err(RadioClientError::ClientConstruction);
        }
        let start = self.preferred_mirror.load(Ordering::Relaxed) % mirrors.len();
        let mut failure: Option<RadioClientError> = None;
        for offset in 0..mirrors.len().min(MAX_MIRROR_ATTEMPTS) {
            let index = (start + offset) % mirrors.len();
            let url = station_url(&mirrors[index], path, limit, filters)?;
            // Boxed so the retry loop does not inline one request future per
            // caller into every station-view future.
            match Box::pin(self.fetch_stations_from(url)).await {
                Ok(stations) => {
                    self.preferred_mirror.store(index, Ordering::Relaxed);
                    return Ok(stations);
                }
                Err(error) if error.allows_mirror_failover() => {
                    debug!(category = ?error, "Radio-Browser mirror failed");
                    failure = Some(failure.map_or(error, |previous| previous.prefer(error)));
                }
                Err(error) => return Err(error),
            }
        }
        Err(failure.unwrap_or(RadioClientError::Transport))
    }

    async fn fetch_stations_from(&self, url: Url) -> Result<Vec<RadioStation>, RadioClientError> {
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

fn station_url(
    base_url: &Url,
    path: &str,
    limit: Option<u32>,
    filters: &[(&str, &str)],
) -> Result<Url, RadioClientError> {
    let mut url = base_url
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

/// Fetch and validate the published mirror hostnames, in response order.
async fn fetch_mirror_hosts(
    client: &reqwest::Client,
    servers_url: Url,
    policy: RequestPolicy,
) -> Result<Vec<String>, RadioClientError> {
    let response = send_bounded(client, servers_url, policy.timeout).await?;
    let body = read_bounded(response, MAX_MIRROR_LIST_BODY_BYTES, policy.timeout).await?;
    parse_mirror_hosts(&body)
}

/// Keep at most [`MAX_MIRRORS`] distinct, valid mirror hostnames. The list
/// carries one entry per address, so the same name normally repeats.
fn parse_mirror_hosts(body: &[u8]) -> Result<Vec<String>, RadioClientError> {
    let entries: Vec<MirrorEntry> =
        serde_json::from_slice(body).map_err(|_| RadioClientError::Parse)?;
    let mut hosts = Vec::new();
    for entry in entries {
        let host = entry.name.to_ascii_lowercase();
        if is_mirror_host(&host) && !hosts.contains(&host) {
            hosts.push(host);
            if hosts.len() == MAX_MIRRORS {
                break;
            }
        }
    }
    Ok(hosts)
}

/// Accept exactly one lowercase DNS label directly under
/// [`MIRROR_HOST_SUFFIX`]. This rejects addresses, user-info, ports, paths,
/// and every host outside the Radio-Browser API zone.
fn is_mirror_host(host: &str) -> bool {
    host.strip_suffix(MIRROR_HOST_SUFFIX).is_some_and(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

/// Shuffle the discovered mirrors, then append the fallback mirrors not
/// already present. The result is never empty.
fn mirror_candidates(mut discovered: Vec<String>) -> Vec<Url> {
    fastrand::shuffle(&mut discovered);
    for fallback in FALLBACK_MIRROR_HOSTS {
        if !discovered.iter().any(|host| host == fallback) {
            discovered.push(fallback.to_string());
        }
    }
    discovered
        .iter()
        .filter_map(|host| Url::parse(&format!("https://{host}/")).ok())
        .collect()
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
    fn construction_is_local_and_defers_mirror_discovery() {
        let client = RadioBrowserClient::new().expect("construct client without network");
        assert!(client.mirrors.get().is_none());
        assert_eq!(
            client.servers_url.as_ref().map(Url::as_str),
            Some("https://all.api.radio-browser.info/json/servers")
        );
    }

    #[test]
    fn mirror_list_keeps_only_distinct_hosts_in_the_api_zone() {
        let body = serde_json::json!([
            {"ip": "192.0.2.1", "name": "de1.api.radio-browser.info"},
            {"ip": "2001:db8::1", "name": "de1.api.radio-browser.info"},
            {"ip": "192.0.2.2", "name": "NL1.API.Radio-Browser.Info"},
            {"ip": "192.0.2.3", "name": "192.0.2.3"},
            {"ip": "192.0.2.4", "name": "mirror.example.test"},
            {"ip": "192.0.2.5", "name": "de1.api.radio-browser.info.example.test"},
            {"ip": "192.0.2.6", "name": "user@de2.api.radio-browser.info"},
            {"ip": "192.0.2.7", "name": "de2.api.radio-browser.info:8443"},
            {"ip": "192.0.2.8", "name": "a.b.api.radio-browser.info"},
            {"ip": "192.0.2.9", "name": "-x.api.radio-browser.info"},
            {"ip": "192.0.2.10", "name": ".api.radio-browser.info"},
            {"ip": "192.0.2.11"}
        ]);
        let hosts = parse_mirror_hosts(body.to_string().as_bytes()).expect("mirror list");
        assert_eq!(
            hosts,
            ["de1.api.radio-browser.info", "nl1.api.radio-browser.info"]
        );

        let many: Vec<_> = (0..MAX_MIRRORS + 4)
            .map(|index| serde_json::json!({"name": format!("m{index}.api.radio-browser.info")}))
            .collect();
        let hosts = parse_mirror_hosts(serde_json::Value::from(many).to_string().as_bytes())
            .expect("long mirror list");
        assert_eq!(hosts.len(), MAX_MIRRORS);

        assert_eq!(
            parse_mirror_hosts(b"not JSON"),
            Err(RadioClientError::Parse)
        );
    }

    #[test]
    fn mirror_candidates_append_missing_fallbacks_after_discovered_hosts() {
        let candidates = mirror_candidates(vec![
            "nl1.api.radio-browser.info".to_string(),
            "de1.api.radio-browser.info".to_string(),
        ]);
        let hosts: Vec<_> = candidates.iter().filter_map(Url::host_str).collect();
        assert_eq!(hosts.len(), 3);
        assert!(hosts[..2].contains(&"nl1.api.radio-browser.info"));
        assert!(hosts[..2].contains(&"de1.api.radio-browser.info"));
        assert_eq!(hosts[2], "de2.api.radio-browser.info");
        assert!(candidates.iter().all(|url| url.scheme() == "https"));

        let fallback: Vec<_> = mirror_candidates(Vec::new())
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            fallback,
            [
                "https://de1.api.radio-browser.info/",
                "https://de2.api.radio-browser.info/"
            ]
        );
    }

    #[tokio::test]
    async fn mirror_discovery_runs_once_and_falls_back_when_unavailable() {
        let listed = MockHttpService::start(vec![MockRoute::get("/json/servers").reply(
            MockResponse::json(serde_json::json!([
                {"ip": "192.0.2.1", "name": "fi1.api.radio-browser.info"},
                {"ip": "192.0.2.2", "name": "mirror.example.test"}
            ])),
        )])
        .await;
        let client = RadioBrowserClient::with_mirror_discovery(
            &format!("{}/json/servers", listed.base_url()),
            fixture_client(),
        );
        let hosts: Vec<_> = client
            .mirrors()
            .await
            .iter()
            .filter_map(Url::host_str)
            .map(str::to_string)
            .collect();
        assert_eq!(
            hosts,
            [
                "fi1.api.radio-browser.info",
                "de1.api.radio-browser.info",
                "de2.api.radio-browser.info"
            ]
        );
        // The candidate list is cached: a second lookup makes no request.
        assert_eq!(client.mirrors().await.len(), 3);
        listed.finish().await;

        let unavailable = MockHttpService::start(vec![MockRoute::get("/json/servers")
            .reply(MockResponse::status(StatusCode::SERVICE_UNAVAILABLE))])
        .await;
        let client = RadioBrowserClient::with_mirror_discovery(
            &format!("{}/json/servers", unavailable.base_url()),
            fixture_client(),
        );
        let hosts: Vec<_> = client
            .mirrors()
            .await
            .iter()
            .filter_map(Url::host_str)
            .collect();
        assert_eq!(
            hosts,
            ["de1.api.radio-browser.info", "de2.api.radio-browser.info"]
        );
        unavailable.finish().await;
    }

    #[tokio::test]
    async fn station_requests_fail_over_and_remember_the_working_mirror() {
        let failing = MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
            .with_query("limit", "1")
            .reply(MockResponse::status(StatusCode::SERVICE_UNAVAILABLE))])
        .await;
        let working = MockHttpService::start(vec![
            MockRoute::get("/json/stations/topclick")
                .with_query("limit", "1")
                .reply(MockResponse::json(serde_json::json!([station(
                    "second-mirror",
                    "https://stream.example.test/live"
                )]))),
            MockRoute::get("/json/stations/topvote")
                .with_query("limit", "1")
                .reply(MockResponse::json(serde_json::json!([]))),
        ])
        .await;
        let client = RadioBrowserClient::with_mirrors(
            &[failing.base_url(), working.base_url()],
            fixture_client(),
            RequestPolicy::PRODUCTION,
        );

        let stations = client
            .fetch_top_click(Some(1))
            .await
            .expect("second mirror serves the view");
        assert_eq!(stations[0].stationuuid, "second-mirror");
        // The next request starts at the mirror that last succeeded, so the
        // failing mirror sees exactly one request.
        assert!(client
            .fetch_top_vote(Some(1))
            .await
            .expect("remembered mirror serves the next view")
            .is_empty());
        failing.finish().await;
        working.finish().await;
    }

    #[tokio::test]
    async fn failover_is_bounded_and_skips_non_transient_failures() {
        let oversized = MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
            .with_query("limit", "1")
            .reply(MockResponse::text("x".repeat(256)))])
        .await;
        let unused = MockHttpService::start(Vec::new()).await;
        let policy = RequestPolicy {
            max_station_body_bytes: 128,
            ..RequestPolicy::PRODUCTION
        };
        let client = RadioBrowserClient::with_mirrors(
            &[oversized.base_url(), unused.base_url()],
            fixture_client(),
            policy,
        );
        assert_eq!(
            client.fetch_top_click(Some(1)).await.map(|_| ()),
            Err(RadioClientError::BodyLimit)
        );
        assert!(unused.requests().is_empty());
        oversized.finish().await;
        unused.finish().await;

        let mut mirrors = Vec::new();
        for _ in 0..MAX_MIRROR_ATTEMPTS {
            mirrors.push(
                MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
                    .with_query("limit", "1")
                    .reply(MockResponse::status(StatusCode::BAD_GATEWAY))])
                .await,
            );
        }
        // One mirror beyond the attempt bound must never be contacted.
        mirrors.push(MockHttpService::start(Vec::new()).await);
        let client = RadioBrowserClient::with_mirrors(
            &mirrors
                .iter()
                .map(MockHttpService::base_url)
                .collect::<Vec<_>>(),
            fixture_client(),
            RequestPolicy::PRODUCTION,
        );
        assert_eq!(
            client.fetch_top_click(Some(1)).await.map(|_| ()),
            Err(RadioClientError::HttpStatus)
        );
        assert!(mirrors[MAX_MIRROR_ATTEMPTS].requests().is_empty());
        for mirror in mirrors {
            mirror.finish().await;
        }
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

    /// The deadline gap must survive full-suite parallelism: under heavy CPU
    /// contention a current-thread tokio runtime can be starved for tens of
    /// milliseconds, which previously let the 25 ms request deadline abort the
    /// request before the mock dispatched it, failing `finish()` with an
    /// unmet route. Warm the connection pool with an immediate reply so the
    /// timed request reuses an established connection (no connect handshake
    /// inside its race window), then apply a 250 ms deadline against a
    /// 1500 ms body delay: the deadline still fires while the body is
    /// streaming, the oversized reply still caps out, and the route observes
    /// both of its expected calls.
    #[tokio::test]
    async fn deadline_and_streaming_body_cap_have_distinct_categories() {
        let policy = RequestPolicy {
            timeout: Duration::from_millis(250),
            max_station_body_bytes: 128,
            max_geolocation_body_bytes: 128,
        };
        let http = fixture_client();
        let delayed = MockHttpService::start(vec![MockRoute::get("/json/stations/topclick")
            .with_query("limit", "1")
            .replies([
                MockResponse::json(serde_json::json!([station(
                    "warm",
                    "https://stream.example.test/live"
                )])),
                MockResponse::json(serde_json::json!([station(
                    "late",
                    "https://stream.example.test/live"
                )]))
                .with_delay(Duration::from_millis(1500)),
            ])])
        .await;
        let base_url = delayed.base_url();
        let warm = RadioBrowserClient::with_test_policy(
            base_url.clone(),
            http.clone(),
            RequestPolicy::PRODUCTION,
        );
        assert!(
            warm.fetch_top_click(Some(1)).await.is_ok(),
            "pool warm-up request must succeed"
        );
        let client = RadioBrowserClient::with_test_policy(base_url, http, policy);
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
