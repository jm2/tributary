//! Low-level Jellyfin HTTP client — authentication header injection,
//! request building, and JSON deserialization.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};
use reqwest::Client;
use tracing::{debug, info};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::{AdvertisedHttpRoute, ResolvedHttpRequest};
use crate::http_body::{read_limited, ResponseBodyError};
use crate::http_security::{
    append_base_path_segments, apply_advertised_http_route, authenticated_client_builder,
    redact_url_secrets, strip_request_url, validate_base_url,
};

use super::api::{JellyfinAuthRequest, JellyfinAuthResponse};

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Client version advertised to the Jellyfin server.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Connection-establishment timeout for API requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle read timeout.  Guards against a server that accepts the
/// connection but then stalls without sending (or only trickles) data,
/// while still allowing a large-but-healthy library transfer to complete
/// (the timeout resets after each successful read).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum response bodies for authentication, API JSON, and small text
/// endpoints, respectively.
const MAX_AUTH_BODY_BYTES: u64 = 1024 * 1024;
const MAX_API_BODY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TEXT_BODY_BYTES: u64 = 64 * 1024;

/// End-to-end and body-phase deadlines for each finite request class.
const AUTH_RESPONSE_DEADLINE: Duration = Duration::from_secs(30);
const API_RESPONSE_DEADLINE: Duration = Duration::from_mins(2);
const TEXT_RESPONSE_DEADLINE: Duration = Duration::from_secs(15);

/// Holds credentials and a reusable `reqwest::Client` with the
/// `X-Emby-Authorization` header pre-configured on every request.
pub struct JellyfinClient {
    base_url: Url,
    advertised_route: Option<AdvertisedHttpRoute>,
    user_id: String,
    /// The raw access token, kept for building stream/image URLs.
    api_key: String,
    http: Client,
}

impl JellyfinClient {
    /// Build a new Jellyfin client from a pre-existing API key and user ID.
    ///
    /// The `X-Emby-Authorization` header is injected as a default header
    /// on the inner `reqwest::Client`, so every outgoing request is
    /// automatically authenticated.
    ///
    /// # Arguments
    /// * `server_url` — Base URL of the Jellyfin server (e.g. `https://jellyfin.example.com`)
    /// * `api_key` — API key or authentication token
    /// * `user_id` — The Jellyfin user ID (required for user-scoped endpoints)
    pub fn new(server_url: &str, api_key: &str, user_id: &str) -> BackendResult<Self> {
        Self::new_with_route(server_url, api_key, user_id, None)
    }

    /// Build a client with an immutable address route supplied by discovery.
    pub fn new_with_route(
        server_url: &str,
        api_key: &str,
        user_id: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid server URL: {e}"),
            source: Some(Box::new(e)),
        })?;
        validate_base_url(&base_url).map_err(|message| BackendError::ConnectionFailed {
            message: message.to_string(),
            source: None,
        })?;

        let http = build_http_client(api_key, &base_url, advertised_route.as_ref())?;

        info!(
            server = %redact_url_secrets(base_url.as_str()),
            user_id = %user_id,
            "Jellyfin client created (API key)"
        );

        Ok(Self {
            base_url,
            advertised_route,
            user_id: user_id.to_string(),
            api_key: api_key.to_string(),
            http,
        })
    }

    /// Authenticate with a Jellyfin server using username and password.
    ///
    /// Posts to `/Users/AuthenticateByName`, extracts the `AccessToken`
    /// and `User.Id` from the response, and returns a fully authenticated
    /// client.
    pub async fn authenticate(
        server_url: &str,
        username: &str,
        password: &str,
    ) -> BackendResult<Self> {
        Self::authenticate_with_route(server_url, username, password, None).await
    }

    /// Authenticate with an immutable address route supplied by discovery.
    pub async fn authenticate_with_route(
        server_url: &str,
        username: &str,
        password: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid server URL: {e}"),
            source: Some(Box::new(e)),
        })?;
        validate_base_url(&base_url).map_err(|message| BackendError::ConnectionFailed {
            message: message.to_string(),
            source: None,
        })?;

        // Build a temporary client WITHOUT a token for the auth request.
        let pre_auth_header = format!(
            r#"MediaBrowser Client="{CLIENT_NAME}", Device="{CLIENT_NAME}", DeviceId="{CLIENT_NAME}", Version="{CLIENT_VERSION}""#,
        );

        let mut pre_auth_headers = HeaderMap::new();
        pre_auth_headers.insert(
            "X-Emby-Authorization",
            HeaderValue::from_str(&pre_auth_header).map_err(|e| {
                BackendError::ConnectionFailed {
                    message: format!("Invalid auth header value: {e}"),
                    source: Some(Box::new(e)),
                }
            })?,
        );
        pre_auth_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        let pre_auth_builder = authenticated_client_builder()
            .user_agent(CLIENT_NAME)
            .default_headers(pre_auth_headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT);
        let pre_auth_http =
            apply_advertised_http_route(pre_auth_builder, &base_url, advertised_route.as_ref())
                .map_err(|message| BackendError::ConnectionFailed {
                    message: message.to_string(),
                    source: None,
                })?
                .build()
                .map_err(|e| BackendError::ConnectionFailed {
                    message: format!("Failed to build HTTP client: {e}"),
                    source: Some(Box::new(e)),
                })?;

        // POST /Users/AuthenticateByName
        let mut auth_url = base_url.clone();
        append_base_path_segments(&mut auth_url, ["Users", "AuthenticateByName"]);

        let body = JellyfinAuthRequest {
            username: username.to_string(),
            pw: password.to_string(),
        };

        debug!(url = %redact_url_secrets(auth_url.as_str()), "Jellyfin auth request");

        let resp = pre_auth_http
            .post(auth_url.as_str())
            .json(&body)
            .timeout(AUTH_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|e| {
                let e = strip_request_url(e);
                BackendError::ConnectionFailed {
                    message: format!("Auth request failed: {e}"),
                    source: Some(Box::new(e)),
                }
            })?;

        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::BAD_REQUEST
        {
            return Err(BackendError::AuthenticationFailed {
                message: format!("Jellyfin authentication failed (HTTP {status})"),
            });
        }

        if !status.is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("HTTP {status}"),
                source: None,
            });
        }

        let body = read_limited(resp, MAX_AUTH_BODY_BYTES, AUTH_RESPONSE_DEADLINE)
            .await
            .map_err(|error| response_body_error("Failed to parse auth response", error))?;

        let auth_resp: JellyfinAuthResponse =
            serde_json::from_slice(&body).map_err(|e| BackendError::ParseError {
                message: format!("Failed to parse auth response: {e}"),
                source: Some(Box::new(e)),
            })?;

        let api_key = auth_resp.access_token;
        let user_id = auth_resp.user.id;

        info!(
            server = %redact_url_secrets(base_url.as_str()),
            user = %auth_resp.user.name,
            user_id = %user_id,
            "Jellyfin authentication successful"
        );

        // Build the real client with the acquired token.
        let http = build_http_client(&api_key, &base_url, advertised_route.as_ref())?;

        Ok(Self {
            base_url,
            advertised_route,
            user_id,
            api_key,
            http,
        })
    }

    /// The Jellyfin user ID this client is configured for.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The raw API key / access token.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The base URL of the Jellyfin server.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Build a full API URL for the given endpoint path.
    ///
    /// The `endpoint` should be a relative path like `System/Ping` or
    /// `Users/{id}/Views`. It will be appended to the base URL.
    pub fn api_url(&self, endpoint: &str) -> Url {
        let mut url = self.base_url.clone();
        append_base_path_segments(
            &mut url,
            endpoint.split('/').filter(|part| !part.is_empty()),
        );
        url
    }

    /// Resolve a direct-stream request with authentication kept in a
    /// sensitive header rather than the URL.
    pub(crate) fn resolved_stream_request(
        &self,
        item_id: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        let mut url = self.api_url(&format!("Audio/{item_id}/stream"));
        url.set_query(None);
        url.set_fragment(None);
        url.query_pairs_mut().append_pair("static", "true");
        let request = ResolvedHttpRequest::new(url)?.with_sensitive_header(
            HeaderName::from_static("x-emby-authorization"),
            jellyfin_auth_header(&self.api_key)?,
        )?;
        match &self.advertised_route {
            Some(route) => request.with_advertised_route(route.clone()),
            None => Ok(request),
        }
    }

    /// Resolve a cover-art request with authentication isolated likewise.
    pub(crate) fn resolved_artwork_request(
        &self,
        item_id: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        let mut url = self.api_url(&format!("Items/{item_id}/Images/Primary"));
        url.set_query(None);
        url.set_fragment(None);
        let request = ResolvedHttpRequest::new(url)?.with_sensitive_header(
            HeaderName::from_static("x-emby-authorization"),
            jellyfin_auth_header(&self.api_key)?,
        )?;
        match &self.advertised_route {
            Some(route) => request.with_advertised_route(route.clone()),
            None => Ok(request),
        }
    }

    /// Issue a GET request to a Jellyfin endpoint and deserialize the
    /// JSON response into the requested type.
    pub async fn get<T: serde::de::DeserializeOwned>(&self, endpoint: &str) -> BackendResult<T> {
        self.get_with_params(endpoint, &[]).await
    }

    /// Issue a GET request with extra query parameters.
    pub async fn get_with_params<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> BackendResult<T> {
        let mut url = self.api_url(endpoint);
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in params {
                q.append_pair(k, v);
            }
        }

        debug!(url = %redact_url_secrets(url.as_str()), "Jellyfin request");

        let resp = self
            .http
            .get(url.as_str())
            .timeout(API_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|e| {
                let e = strip_request_url(e);
                BackendError::ConnectionFailed {
                    message: format!("HTTP request failed: {e}"),
                    source: Some(Box::new(e)),
                }
            })?;

        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(BackendError::AuthenticationFailed {
                message: "Jellyfin returned 401 Unauthorized".into(),
            });
        }

        if !status.is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("HTTP {status}"),
                source: None,
            });
        }

        let body = read_limited(resp, MAX_API_BODY_BYTES, API_RESPONSE_DEADLINE)
            .await
            .map_err(|error| response_body_error("Failed to parse Jellyfin JSON", error))?;

        let body = serde_json::from_slice::<T>(&body).map_err(|e| BackendError::ParseError {
            message: format!("Failed to parse Jellyfin JSON: {e}"),
            source: Some(Box::new(e)),
        })?;

        Ok(body)
    }

    /// Issue a GET request and return the raw response text.
    ///
    /// Used for endpoints that return plain text (e.g. `/System/Ping`).
    pub async fn get_text(&self, endpoint: &str) -> BackendResult<String> {
        let url = self.api_url(endpoint);
        debug!(url = %redact_url_secrets(url.as_str()), "Jellyfin text request");

        let resp = self
            .http
            .get(url.as_str())
            .timeout(TEXT_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|e| {
                let e = strip_request_url(e);
                BackendError::ConnectionFailed {
                    message: format!("HTTP request failed: {e}"),
                    source: Some(Box::new(e)),
                }
            })?;

        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(BackendError::AuthenticationFailed {
                message: "Jellyfin returned 401 Unauthorized".into(),
            });
        }

        if !status.is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("HTTP {status}"),
                source: None,
            });
        }

        let body = read_limited(resp, MAX_TEXT_BODY_BYTES, TEXT_RESPONSE_DEADLINE)
            .await
            .map_err(|error| response_body_error("Failed to read Jellyfin response body", error))?;
        let text = String::from_utf8_lossy(&body).into_owned();

        Ok(text)
    }
}

/// Build a `reqwest::Client` with the full `X-Emby-Authorization` header.
fn build_http_client(
    api_key: &str,
    base_url: &Url,
    advertised_route: Option<&AdvertisedHttpRoute>,
) -> BackendResult<Client> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert("X-Emby-Authorization", jellyfin_auth_header(api_key)?);
    default_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

    let builder = authenticated_client_builder()
        .user_agent(CLIENT_NAME)
        .default_headers(default_headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT);
    apply_advertised_http_route(builder, base_url, advertised_route)
        .map_err(|message| BackendError::ConnectionFailed {
            message: message.to_string(),
            source: None,
        })?
        .build()
        .map_err(|e| BackendError::ConnectionFailed {
            message: format!("Failed to build HTTP client: {e}"),
            source: Some(Box::new(e)),
        })
}

fn jellyfin_auth_header(api_key: &str) -> BackendResult<HeaderValue> {
    let auth_value = format!(
        r#"MediaBrowser Client="{CLIENT_NAME}", Device="{CLIENT_NAME}", DeviceId="{CLIENT_NAME}", Version="{CLIENT_VERSION}", Token="{api_key}""#,
    );
    let mut value =
        HeaderValue::from_str(&auth_value).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid auth header value: {e}"),
            source: Some(Box::new(e)),
        })?;
    value.set_sensitive(true);
    Ok(value)
}

fn response_body_error(context: &str, error: ResponseBodyError) -> BackendError {
    match error {
        error @ (ResponseBodyError::TooLarge { .. }
        | ResponseBodyError::InvalidLimit { .. }
        | ResponseBodyError::AllocationFailed { .. }) => BackendError::ConnectionFailed {
            message: error.to_string(),
            source: None,
        },
        ResponseBodyError::DeadlineExceeded { deadline } => BackendError::Timeout {
            duration_secs: deadline.as_secs(),
        },
        error => BackendError::ParseError {
            message: format!("{context}: {error}"),
            source: Some(Box::new(error)),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use axum::http::{Method, StatusCode};

    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};

    use super::*;

    fn advertised_route(origin: &str) -> AdvertisedHttpRoute {
        let origin = Url::parse(origin).expect("route origin");
        AdvertisedHttpRoute::new(&origin, [SocketAddr::from((Ipv4Addr::LOCALHOST, 45_322))])
            .expect("domain route")
    }

    #[test]
    fn maps_response_body_deadline_to_timeout() {
        let error = response_body_error(
            "body",
            ResponseBodyError::DeadlineExceeded {
                deadline: Duration::from_secs(7),
            },
        );

        assert!(matches!(error, BackendError::Timeout { duration_secs: 7 }));
    }

    #[test]
    fn rejects_embedded_url_credentials_without_echoing_them() {
        let secret = uuid::Uuid::new_v4().to_string();
        let api_key = uuid::Uuid::new_v4().to_string();
        let error = JellyfinClient::new(
            &format!("https://embedded-user:{secret}@media.example.test"),
            &api_key,
            "user-id",
        )
        .err()
        .expect("embedded URL credentials must be rejected");

        let rendered = error.to_string();
        assert!(!rendered.contains("embedded-user"));
        assert!(!rendered.contains(&secret));
    }

    #[test]
    fn resolved_media_requests_keep_token_out_of_urls() {
        let api_key = uuid::Uuid::new_v4().to_string();
        let client =
            JellyfinClient::new("https://media.example.test", &api_key, "user-id").expect("client");

        let stream = client.resolved_stream_request("track-id").unwrap();
        let artwork = client.resolved_artwork_request("album-id").unwrap();
        assert_eq!(stream.endpoint().query(), Some("static=true"));
        assert!(artwork.endpoint().query().is_none());

        for request in [stream, artwork] {
            assert!(!request.endpoint().as_str().contains(&api_key));
            assert!(request.private_query_pairs().is_empty());
            let value = request
                .sensitive_headers()
                .get("x-emby-authorization")
                .expect("auth header");
            assert!(value.is_sensitive());
        }
    }

    #[test]
    fn api_and_media_paths_preserve_reverse_proxy_prefixes_exactly() {
        for (base, prefix) in [
            ("https://media.example.test", ""),
            ("https://media.example.test/share", "/share"),
            ("https://media.example.test/share/", "/share"),
            (
                "https://media.example.test/tenant%2Fmusic/",
                "/tenant%2Fmusic",
            ),
        ] {
            let client = JellyfinClient::new(base, "api-key", "user-id").expect("client");
            assert_eq!(
                client.api_url("System/Ping").as_str(),
                format!("https://media.example.test{prefix}/System/Ping"),
                "base URL: {base}"
            );
            assert_eq!(
                client
                    .resolved_stream_request("track-id")
                    .expect("stream request")
                    .endpoint()
                    .as_str(),
                format!("https://media.example.test{prefix}/Audio/track-id/stream?static=true"),
                "base URL: {base}"
            );
            assert_eq!(
                client
                    .resolved_artwork_request("album-id")
                    .expect("artwork request")
                    .endpoint()
                    .as_str(),
                format!("https://media.example.test{prefix}/Items/album-id/Images/Primary"),
                "base URL: {base}"
            );
        }
    }

    #[tokio::test]
    async fn rejected_auth_uses_prefixed_endpoint_and_returns_typed_redacted_error() {
        let service = MockHttpService::start(vec![MockRoute::new(
            Method::POST,
            "/gateway/Users/AuthenticateByName",
        )
        .reply(MockResponse::status(StatusCode::UNAUTHORIZED))])
        .await;
        let username = uuid::Uuid::new_v4().to_string();
        let password = uuid::Uuid::new_v4().to_string();
        let result = JellyfinClient::authenticate(
            &format!("{}/gateway/", service.base_url()),
            &username,
            &password,
        )
        .await;
        let error = result.err().expect("fixture authentication must fail");

        assert!(matches!(error, BackendError::AuthenticationFailed { .. }));
        let rendered = error.to_string();
        assert!(!rendered.contains(&username));
        assert!(!rendered.contains(&password));
        let requests = service.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].uri.path(), "/gateway/Users/AuthenticateByName");
        service.finish().await;
    }

    #[test]
    fn advertised_route_reaches_stream_and_artwork_requests() {
        let origin = "https://media.example.test";
        let route = advertised_route(origin);
        let client = JellyfinClient::new_with_route(
            origin,
            "route-api-key",
            "route-user-id",
            Some(route.clone()),
        )
        .expect("routed client");

        for request in [
            client.resolved_stream_request("track-id").unwrap(),
            client.resolved_artwork_request("album-id").unwrap(),
        ] {
            assert_eq!(request.advertised_route(), Some(&route));
            assert_eq!(request.endpoint().host_str(), Some("media.example.test"));
        }

        let ordinary = JellyfinClient::new(origin, "api-key", "user-id").expect("ordinary client");
        assert!(ordinary
            .resolved_stream_request("track-id")
            .unwrap()
            .advertised_route()
            .is_none());
    }

    #[test]
    fn mismatched_advertised_route_fails_without_exposing_credentials() {
        let api_key = uuid::Uuid::new_v4().to_string();
        let user_id = uuid::Uuid::new_v4().to_string();
        let Err(error) = JellyfinClient::new_with_route(
            "https://media.example.test",
            &api_key,
            &user_id,
            Some(advertised_route("https://other.example.test")),
        ) else {
            panic!("mismatched route must fail");
        };

        let rendered = error.to_string();
        assert!(!rendered.contains(&api_key));
        assert!(!rendered.contains(&user_id));
    }
}
