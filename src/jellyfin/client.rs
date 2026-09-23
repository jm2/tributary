//! Low-level Jellyfin HTTP client — authentication header injection,
//! request building, and JSON deserialization.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};
use reqwest::Client;
use tracing::{debug, info};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::remote_json::parse_remote_json;
use crate::architecture::{AdvertisedHttpRoute, MediaRepresentation, ResolvedHttpRequest};
use crate::http_body::{read_limited, ResponseBodyError};
use crate::http_security::{
    append_base_path_segments, apply_advertised_http_route, authenticated_client_builder,
    redact_url_secrets, strip_request_url, validate_base_url,
};
use crate::install_id::install_id;

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
/// `Authorization` header pre-configured on every request.
pub struct JellyfinClient {
    base_url: Url,
    advertised_route: Option<AdvertisedHttpRoute>,
    user_id: String,
    /// The raw access token, kept for building stream/image URLs.
    api_key: String,
    http: Client,
    /// True only for a token minted by AuthenticateByName in this process.
    /// Pre-existing API keys are durable credentials and must not be revoked
    /// by source disconnect.
    owns_session_token: bool,
}

impl JellyfinClient {
    /// Build a new Jellyfin client from a pre-existing API key and user ID.
    ///
    /// The `Authorization` header is injected as a default header
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
            owns_session_token: false,
        })
    }

    /// Authenticate with a Jellyfin server using username and password.
    ///
    /// Posts to `/Users/AuthenticateByName`, extracts the `AccessToken`
    /// and `User.Id` from the response, and returns a fully authenticated
    /// client.
    pub(crate) async fn authenticate(
        server_url: &str,
        username: &str,
        password: &str,
    ) -> BackendResult<Self> {
        Self::authenticate_with_route(server_url, username, password, None).await
    }

    /// Authenticate with an immutable address route supplied by discovery.
    pub(crate) async fn authenticate_with_route(
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
        let mut pre_auth_headers = HeaderMap::new();
        pre_auth_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&client_authorization()).map_err(|e| {
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
            parse_remote_json("Failed to parse auth response", &body)?;

        let api_key = auth_resp.access_token;
        let user_id = auth_resp.user.id;
        let user_name = auth_resp.user.name;

        // Validate the server-supplied token before it can enter any request.
        // A token containing control bytes cannot be represented as an HTTP
        // header, including on a cleanup request, so that narrow hostile-peer
        // case must fail closed without echoing the value. Once the token has
        // a safe header representation, retain a copy for exact best-effort
        // logout if the final authenticated client cannot be constructed.
        let auth_header = jellyfin_auth_header(&api_key)?;
        let http = build_http_client_with_auth_header(
            auth_header.clone(),
            &base_url,
            advertised_route.as_ref(),
        );

        let client = finish_interactive_authentication(
            &pre_auth_http,
            base_url,
            advertised_route,
            user_id,
            api_key,
            auth_header,
            http,
        )
        .await?;

        info!(
            server = %redact_url_secrets(client.base_url.as_str()),
            user = %user_name,
            user_id = %client.user_id,
            "Jellyfin authentication successful"
        );
        Ok(client)
    }

    /// Revoke the exact interactive session token owned by this client.
    /// Durable API keys are intentionally a no-op: Tributary did not mint
    /// them and has no authority to revoke them on source disconnect.
    pub(crate) async fn logout_owned_session(&self) -> BackendResult<()> {
        if !self.owns_session_token {
            return Ok(());
        }

        let url = self.api_url("Sessions/Logout");
        debug!(url = %redact_url_secrets(url.as_str()), "Jellyfin session logout");
        let response = self
            .http
            .post(url.as_str())
            .timeout(AUTH_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|error| {
                let error = strip_request_url(error);
                BackendError::ConnectionFailed {
                    message: format!("Session logout failed: {error}"),
                    source: Some(Box::new(error)),
                }
            })?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(());
        }
        Err(BackendError::ConnectionFailed {
            message: format!("Session logout returned HTTP {status}"),
            source: None,
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
    ///
    /// The caller supplies the validated representation of the media this
    /// request returns; `static=true` means the original bytes, so the
    /// container is authoritative (see `MediaRepresentation`).
    pub(crate) fn resolved_stream_request(
        &self,
        item_id: &str,
        representation: MediaRepresentation,
    ) -> BackendResult<ResolvedHttpRequest> {
        let mut url = self.api_url(&format!("Audio/{item_id}/stream"));
        url.set_query(None);
        url.set_fragment(None);
        url.query_pairs_mut().append_pair("static", "true");
        let request = ResolvedHttpRequest::new(url)?
            .with_representation(representation)
            .with_sensitive_header(AUTHORIZATION, jellyfin_auth_header(&self.api_key)?)?;
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
        let request = ResolvedHttpRequest::new(url)?
            .with_sensitive_header(AUTHORIZATION, jellyfin_auth_header(&self.api_key)?)?;
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

        let body = parse_remote_json::<T>("Failed to parse Jellyfin JSON", &body)?;

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

/// Build a `reqwest::Client` with the token-bearing `Authorization` header.
fn build_http_client(
    api_key: &str,
    base_url: &Url,
    advertised_route: Option<&AdvertisedHttpRoute>,
) -> BackendResult<Client> {
    build_http_client_with_auth_header(jellyfin_auth_header(api_key)?, base_url, advertised_route)
}

fn build_http_client_with_auth_header(
    auth_header: HeaderValue,
    base_url: &Url,
    advertised_route: Option<&AdvertisedHttpRoute>,
) -> BackendResult<Client> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(AUTHORIZATION, auth_header);
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

/// Complete ownership transfer for a syntactically valid token minted by
/// `AuthenticateByName`.
///
/// The authentication client already carries the exact advertised route and
/// is therefore the only safe fallback transport if constructing the normal
/// token-bearing client fails. The original construction error remains the
/// caller-visible result; logout is bounded and best-effort because there is
/// no close-capable `JellyfinClient` to hand to the lifecycle registry yet.
async fn finish_interactive_authentication(
    pre_auth_http: &Client,
    base_url: Url,
    advertised_route: Option<AdvertisedHttpRoute>,
    user_id: String,
    api_key: String,
    auth_header: HeaderValue,
    http: BackendResult<Client>,
) -> BackendResult<JellyfinClient> {
    let http = match http {
        Ok(http) => http,
        Err(error) => {
            best_effort_logout_minted_session(pre_auth_http, &base_url, auth_header).await;
            return Err(error);
        }
    };

    Ok(JellyfinClient {
        base_url,
        advertised_route,
        user_id,
        api_key,
        http,
        owns_session_token: true,
    })
}

/// Attempt to retire one exact, safely representable token before lifecycle
/// staging is possible. No response or transport error is exposed or logged:
/// the original client-construction failure is authoritative and must remain
/// free of the server-supplied token.
async fn best_effort_logout_minted_session(
    pre_auth_http: &Client,
    base_url: &Url,
    auth_header: HeaderValue,
) {
    let mut url = base_url.clone();
    append_base_path_segments(&mut url, ["Sessions", "Logout"]);
    let _ = pre_auth_http
        .post(url.as_str())
        .header(AUTHORIZATION, auth_header)
        .timeout(AUTH_RESPONSE_DEADLINE)
        .send()
        .await;
}

/// The `MediaBrowser` client identification Jellyfin reads from the standard
/// `Authorization` header.
///
/// Every Jellyfin release accepts this header. 12.0 by default ignores the
/// legacy `X-Emby-Authorization` header, so only the standard one is sent.
/// `DeviceId` must be unique per install: issuing a token logs out the same
/// user's other sessions that share it.
fn client_authorization() -> String {
    format!(
        r#"MediaBrowser Client="{CLIENT_NAME}", Device="{CLIENT_NAME}", DeviceId="{}", Version="{CLIENT_VERSION}""#,
        install_id(),
    )
}

fn jellyfin_auth_header(api_key: &str) -> BackendResult<HeaderValue> {
    let auth_value = format!(r#"{}, Token="{api_key}""#, client_authorization());
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
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Mutex};

    use axum::extract::{Request, State};
    use axum::http::{Method, StatusCode};
    use axum::response::{IntoResponse, Response};

    use crate::architecture::remote_json::rendered_error_chain;
    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};
    use crate::jellyfin::api::JellyfinItemsResponse;

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

        let stream = client
            .resolved_stream_request("track-id", MediaRepresentation::buffered_unknown())
            .unwrap();
        let artwork = client.resolved_artwork_request("album-id").unwrap();
        assert_eq!(stream.endpoint().query(), Some("static=true"));
        assert!(artwork.endpoint().query().is_none());

        for request in [stream, artwork] {
            assert!(!request.endpoint().as_str().contains(&api_key));
            assert!(request.private_query_pairs().is_empty());
            let value = request
                .sensitive_headers()
                .get(AUTHORIZATION)
                .expect("auth header");
            assert!(value.is_sensitive());
            assert_eq!(request.sensitive_headers().len(), 1);
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
                    .resolved_stream_request("track-id", MediaRepresentation::buffered_unknown())
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

    #[tokio::test]
    async fn interactive_token_is_logged_out_once_with_authenticated_post() {
        let token = uuid::Uuid::new_v4().to_string();
        let service = MockHttpService::start(vec![
            MockRoute::new(Method::POST, "/Users/AuthenticateByName").reply(MockResponse::json(
                serde_json::json!({
                    "User": { "Id": "user-id", "Name": "Fixture" },
                    "AccessToken": token
                }),
            )),
            MockRoute::new(Method::POST, "/Sessions/Logout")
                .reply(MockResponse::status(StatusCode::NO_CONTENT)),
        ])
        .await;

        let password = uuid::Uuid::new_v4().to_string();
        let client = JellyfinClient::authenticate(&service.base_url(), "fixture-user", &password)
            .await
            .expect("interactive client");
        client
            .logout_owned_session()
            .await
            .expect("owned session logout");

        let requests = service.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].uri.path(), "/Users/AuthenticateByName");
        assert_eq!(requests[1].uri.path(), "/Sessions/Logout");
        assert_eq!(requests[1].method, Method::POST);
        let authorization = requests[1]
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .expect("logout authorization");
        assert!(authorization.contains(&format!(r#"Token="{token}""#)));
        assert!(!requests[1].uri.to_string().contains(&token));
        service.finish().await;
    }

    #[tokio::test]
    async fn valid_minted_token_is_logged_out_when_final_client_construction_fails() {
        let token = uuid::Uuid::new_v4().to_string();
        let service = MockHttpService::start(vec![MockRoute::new(
            Method::POST,
            "/gateway/Sessions/Logout",
        )
        .reply(MockResponse::status(StatusCode::NO_CONTENT))])
        .await;

        // Match the production pre-auth client closely enough to prove that
        // the per-request minted-token header replaces its tokenless default.
        let mut pre_auth_headers = HeaderMap::new();
        pre_auth_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("MediaBrowser Client=\"pre-auth\""),
        );
        let pre_auth_http = authenticated_client_builder()
            .default_headers(pre_auth_headers)
            .build()
            .expect("pre-auth client");
        let base_url =
            Url::parse(&format!("{}/gateway/", service.base_url())).expect("fixture base URL");
        let auth_header = jellyfin_auth_header(&token).expect("safe minted token");
        let construction_error = BackendError::ConnectionFailed {
            message: "synthetic final client construction failure".to_string(),
            source: None,
        };

        let result = finish_interactive_authentication(
            &pre_auth_http,
            base_url,
            None,
            "fixture-user-id".to_string(),
            token.clone(),
            auth_header,
            Err(construction_error),
        )
        .await;
        let error = result.err().expect("construction must fail");
        assert_eq!(
            error.to_string(),
            "Connection failed: synthetic final client construction failure"
        );
        assert!(!error.to_string().contains(&token));

        let requests = service.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::POST);
        assert_eq!(requests[0].uri.path(), "/gateway/Sessions/Logout");
        let mut authorizations = requests[0].headers.get_all(AUTHORIZATION).iter();
        let authorization = authorizations
            .next()
            .and_then(|value| value.to_str().ok())
            .expect("cleanup authorization");
        assert!(
            authorizations.next().is_none(),
            "tokenless default replaced"
        );
        assert!(authorization.contains(&format!(r#"Token="{token}""#)));
        assert!(!requests[0].uri.to_string().contains(&token));
        service.finish().await;
    }

    #[tokio::test]
    async fn header_invalid_minted_token_fails_closed_without_echo_or_unsafe_logout() {
        let secret = uuid::Uuid::new_v4().to_string();
        let invalid_token = format!("{secret}\r\nX-Injected: rejected");
        let service = MockHttpService::start(vec![MockRoute::new(
            Method::POST,
            "/Users/AuthenticateByName",
        )
        .reply(MockResponse::json(serde_json::json!({
            "User": { "Id": "user-id", "Name": "Fixture" },
            "AccessToken": invalid_token
        })))])
        .await;

        let password = uuid::Uuid::new_v4().to_string();
        let result =
            JellyfinClient::authenticate(&service.base_url(), "fixture-user", &password).await;
        let error = result.err().expect("invalid token header must fail");
        let rendered = error.to_string();
        assert!(rendered.contains("Invalid auth header value"));
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains("X-Injected"));

        // The hostile value cannot safely be represented in the exact header
        // required by Sessions/Logout. Sending a transformed or raw value
        // would either target another session or permit header injection.
        let requests = service.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].uri.path(), "/Users/AuthenticateByName");
        service.finish().await;
    }

    #[tokio::test]
    async fn durable_api_key_is_not_logged_out() {
        let service = MockHttpService::start(Vec::new()).await;
        let client = JellyfinClient::new(&service.base_url(), "durable-key", "user-id")
            .expect("durable API-key client");

        client
            .logout_owned_session()
            .await
            .expect("durable credential close is local-only");

        assert!(service.requests().is_empty());
        service.finish().await;
    }

    const JELLYFIN_12_TOKEN: &str = "jellyfin-12-token";

    /// Parameters of a standard `Authorization: MediaBrowser …` header.
    fn standard_authorization(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
        let Some((scheme, parameters)) = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
        else {
            return HashMap::new();
        };
        if !scheme.eq_ignore_ascii_case("MediaBrowser") {
            return HashMap::new();
        }
        parameters
            .split(',')
            .filter_map(|parameter| parameter.split_once('='))
            .map(|(key, value)| {
                (
                    key.trim().to_owned(),
                    value.trim().trim_matches('"').to_owned(),
                )
            })
            .collect()
    }

    /// Accepted request paths and the `DeviceId` each one carried.
    type Accepted = Arc<Mutex<Vec<(String, String)>>>;

    /// Jellyfin 12.0 with its default `EnableLegacyAuthorization = false`: the
    /// client identification and token are read only from the standard
    /// `Authorization` header, never from `X-Emby-Authorization`.
    async fn jellyfin_12(State(accepted): State<Accepted>, request: Request) -> Response {
        let parameters = standard_authorization(request.headers());
        let path = request.uri().path().to_owned();
        let signing_in = path == "/Users/AuthenticateByName";
        let authorized = if signing_in {
            ["Client", "Device", "DeviceId", "Version"]
                .iter()
                .all(|key| parameters.get(*key).is_some_and(|value| !value.is_empty()))
        } else {
            parameters.get("Token").map(String::as_str) == Some(JELLYFIN_12_TOKEN)
        };
        if !authorized {
            return if signing_in {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::UNAUTHORIZED
            }
            .into_response();
        }

        let device_id = parameters.get("DeviceId").cloned().unwrap_or_default();
        accepted.lock().unwrap().push((path, device_id));
        if signing_in {
            return axum::Json(serde_json::json!({
                "User": { "Id": "user-id", "Name": "Fixture" },
                "AccessToken": JELLYFIN_12_TOKEN
            }))
            .into_response();
        }
        StatusCode::OK.into_response()
    }

    /// Serve [`jellyfin_12`] on an ephemeral loopback port.
    async fn start_jellyfin_12() -> (
        String,
        Accepted,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let accepted = Accepted::default();
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind Jellyfin 12 fixture");
        let base_url = format!("http://{}", listener.local_addr().expect("fixture address"));
        let app = axum::Router::new()
            .fallback(jellyfin_12)
            .with_state(Arc::clone(&accepted));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        (base_url, accepted, server)
    }

    /// Guards the regression test below against a fixture that would also
    /// accept what older Tributary builds sent.
    #[tokio::test]
    async fn jellyfin_12_fixture_rejects_the_legacy_header_alone() {
        let (base_url, accepted, server) = start_jellyfin_12().await;
        let legacy_only = authenticated_client_builder()
            .build()
            .expect("client")
            .get(format!("{base_url}/System/Ping"))
            .header(
                "X-Emby-Authorization",
                jellyfin_auth_header(JELLYFIN_12_TOKEN).expect("legacy header"),
            )
            .send()
            .await
            .expect("legacy-only request");
        server.abort();

        assert_eq!(legacy_only.status(), StatusCode::UNAUTHORIZED);
        assert!(accepted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn jellyfin_12_default_server_accepts_sign_in_api_media_and_logout() {
        let (base_url, accepted, server) = start_jellyfin_12().await;
        let http = authenticated_client_builder()
            .build()
            .expect("media client");

        let client = JellyfinClient::authenticate(&base_url, "fixture-user", "fixture-password")
            .await
            .expect("sign-in with the standard header");
        client
            .get_text("System/Ping")
            .await
            .expect("API request with the standard header");
        // Streams and artwork leave through the relay and artwork worker,
        // which send exactly the resolved request's sensitive headers.
        for request in [
            client
                .resolved_stream_request("track-id", MediaRepresentation::buffered_unknown())
                .expect("stream request"),
            client
                .resolved_artwork_request("album-id")
                .expect("artwork request"),
        ] {
            let response = http
                .get(request.endpoint().clone())
                .headers(request.sensitive_headers().clone())
                .send()
                .await
                .expect("media request");
            assert_eq!(response.status(), StatusCode::OK, "{}", request.endpoint());
        }
        client
            .logout_owned_session()
            .await
            .expect("logout with the standard header");
        server.abort();

        let expected: Vec<_> = [
            "/Users/AuthenticateByName",
            "/System/Ping",
            "/Audio/track-id/stream",
            "/Items/album-id/Images/Primary",
            "/Sessions/Logout",
        ]
        .into_iter()
        .map(|path| (path.to_owned(), install_id().to_owned()))
        .collect();
        assert_eq!(*accepted.lock().unwrap(), expected);
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
            client
                .resolved_stream_request("track-id", MediaRepresentation::buffered_unknown())
                .unwrap(),
            client.resolved_artwork_request("album-id").unwrap(),
        ] {
            assert_eq!(request.advertised_route(), Some(&route));
            assert_eq!(request.endpoint().host_str(), Some("media.example.test"));
        }

        let ordinary = JellyfinClient::new(origin, "api-key", "user-id").expect("ordinary client");
        assert!(ordinary
            .resolved_stream_request("track-id", MediaRepresentation::buffered_unknown())
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

    /// A Jellyfin auth body whose `User` is a string instead of the expected
    /// object makes `serde_json` quote the wrong-type value. The production
    /// authentication path must drop it rather than log or surface it.
    #[tokio::test]
    async fn auth_parse_failures_omit_response_content_from_diagnostics() {
        let sentinel = "JELLYFIN-AUTH-PARSE-SENTINEL-7e02";
        let service = MockHttpService::start(vec![MockRoute::new(
            Method::POST,
            "/Users/AuthenticateByName",
        )
        .reply(MockResponse::json(serde_json::json!({
            "User": sentinel,
            "AccessToken": "fixture-token"
        })))])
        .await;
        let password = uuid::Uuid::new_v4().to_string();
        let error = JellyfinClient::authenticate(&service.base_url(), "fixture-user", &password)
            .await
            .err()
            .expect("wrong-type auth body must fail");
        service.finish().await;

        assert_parse_error_omits(&error, sentinel, &["fixture-token", &password]);
    }

    /// A Jellyfin catalogue body whose `TotalRecordCount` is a string instead
    /// of the expected integer exercises the generic catalogue parser with a
    /// short and a large sentinel value.
    #[tokio::test]
    async fn catalogue_parse_failures_omit_response_content_from_diagnostics() {
        let cases = [
            "JELLYFIN-CATALOGUE-SENTINEL-3c88".to_string(),
            format!(
                "{}{}",
                "w".repeat(48 * 1024),
                "JELLYFIN-LARGE-SENTINEL-a5f1"
            ),
        ];

        for payload in cases {
            let service = MockHttpService::start(vec![MockRoute::get("/Users/user-id/Items")
                .reply(MockResponse::json(serde_json::json!({
                    "Items": [],
                    "TotalRecordCount": payload
                })))])
            .await;
            let client =
                JellyfinClient::new(&service.base_url(), "api-key", "user-id").expect("client");
            let error = client
                .get::<JellyfinItemsResponse>("Users/user-id/Items")
                .await
                .expect_err("wrong-type catalogue body must fail");
            service.finish().await;

            assert_parse_error_omits(&error, &payload, &["api-key"]);
        }
    }

    /// Assert a parse failure retains no response content and keeps the fixed
    /// category in its message.
    fn assert_parse_error_omits(error: &BackendError, payload: &str, secrets: &[&str]) {
        match error {
            BackendError::ParseError { message, source } => {
                assert!(source.is_none(), "content-bearing source retained");
                assert!(
                    message.contains("unexpected type or shape"),
                    "unexpected category: {message}"
                );
            }
            other => panic!("expected ParseError, got {other:?}"),
        }
        let rendered = format!("{error:?}\n{error}\n{}", rendered_error_chain(error));
        assert!(!rendered.contains(payload), "response content leaked");
        for secret in secrets {
            // Do not interpolate `secret` into the failure message: the CodeQL
            // cleartext-logging query treats the panic payload as a log sink,
            // and this assertion exists precisely to keep secrets out of one.
            assert!(!rendered.contains(secret), "credential leaked");
        }
    }
}
