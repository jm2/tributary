//! Low-level Plex HTTP client — authentication header injection,
//! request building, and JSON deserialization.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use tracing::{debug, info};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::ResolvedHttpRequest;
use crate::http_body::{read_limited, ResponseBodyError};
use crate::http_security::{
    authenticated_client_builder, redact_url_secrets, strip_request_url, validate_base_url,
};

use super::api::PlexSignInResponse;

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Client version advertised to the Plex server.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Plex.tv sign-in endpoint.
const PLEX_TV_SIGN_IN: &str = "https://plex.tv/users/sign_in.json";

/// Connection-establishment timeout for API requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle read timeout.  Guards against a server that accepts the
/// connection but then stalls without sending (or only trickles) data,
/// while still allowing a large-but-healthy library transfer to complete
/// (the timeout resets after each successful read).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum response bodies for authentication and API JSON, respectively.
const MAX_AUTH_BODY_BYTES: u64 = 1024 * 1024;
const MAX_API_BODY_BYTES: u64 = 256 * 1024 * 1024;

/// End-to-end and body-phase deadlines for each finite request class.
const AUTH_RESPONSE_DEADLINE: Duration = Duration::from_secs(30);
const API_RESPONSE_DEADLINE: Duration = Duration::from_secs(120);

/// Holds credentials and a reusable `reqwest::Client` with the
/// `X-Plex-Token` header pre-configured on every request.
pub struct PlexClient {
    base_url: Url,
    /// The raw auth token, kept for building stream/thumbnail URLs.
    auth_token: String,
    http: Client,
}

impl PlexClient {
    /// Build a new Plex client from a pre-existing auth token.
    ///
    /// The `X-Plex-Token` and Plex identification headers are injected
    /// as default headers on the inner `reqwest::Client`, so every
    /// outgoing request is automatically authenticated and identified.
    ///
    /// # Arguments
    /// * `server_url` — Base URL of the Plex server (e.g. `https://plex.example.com:32400`)
    /// * `auth_token` — Plex authentication token (`X-Plex-Token`)
    pub fn new(server_url: &str, auth_token: &str) -> BackendResult<Self> {
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid server URL: {e}"),
            source: Some(Box::new(e)),
        })?;
        validate_base_url(&base_url).map_err(|message| BackendError::ConnectionFailed {
            message: message.to_string(),
            source: None,
        })?;

        let http = build_http_client(auth_token)?;

        info!(
            server = %redact_url_secrets(base_url.as_str()),
            "Plex client created (token)"
        );

        Ok(Self {
            base_url,
            auth_token: auth_token.to_string(),
            http,
        })
    }

    /// Authenticate with Plex using username and password via plex.tv.
    ///
    /// Posts to `https://plex.tv/users/sign_in.json`, extracts the
    /// `authToken`, and returns a client configured for the given
    /// local server URL.
    pub async fn authenticate(
        server_url: &str,
        username: &str,
        password: &str,
    ) -> BackendResult<Self> {
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid server URL: {e}"),
            source: Some(Box::new(e)),
        })?;
        validate_base_url(&base_url).map_err(|message| BackendError::ConnectionFailed {
            message: message.to_string(),
            source: None,
        })?;

        // Build Plex identification headers (no token yet).
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Plex-Client-Identifier",
            HeaderValue::from_static(CLIENT_NAME),
        );
        headers.insert("X-Plex-Product", HeaderValue::from_static(CLIENT_NAME));
        headers.insert(
            "X-Plex-Version",
            HeaderValue::from_str(CLIENT_VERSION)
                .unwrap_or_else(|_| HeaderValue::from_static("0.1.0")),
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );

        let pre_auth_http = authenticated_client_builder()
            .user_agent(CLIENT_NAME)
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to build HTTP client: {e}"),
                source: Some(Box::new(e)),
            })?;

        let form_body = format!(
            "user[login]={}&user[password]={}",
            urlencoding::encode(username),
            urlencoding::encode(password)
        );

        debug!("Plex sign-in request to plex.tv");

        let resp = pre_auth_http
            .post(PLEX_TV_SIGN_IN)
            .body(form_body)
            .timeout(AUTH_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|e| {
                let e = strip_request_url(e);
                BackendError::ConnectionFailed {
                    message: format!("Plex sign-in request failed: {e}"),
                    source: Some(Box::new(e)),
                }
            })?;

        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
        {
            return Err(BackendError::AuthenticationFailed {
                message: format!("Plex authentication failed (HTTP {status})"),
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
            .map_err(|error| response_body_error("Failed to parse Plex sign-in response", error))?;

        let sign_in: PlexSignInResponse =
            serde_json::from_slice(&body).map_err(|e| BackendError::ParseError {
                message: format!("Failed to parse Plex sign-in response: {e}"),
                source: Some(Box::new(e)),
            })?;

        let auth_token = sign_in.user.auth_token;

        info!(
            server = %redact_url_secrets(base_url.as_str()),
            user = ?sign_in.user.username,
            "Plex authentication successful"
        );

        let http = build_http_client(&auth_token)?;

        Ok(Self {
            base_url,
            auth_token,
            http,
        })
    }

    /// The raw auth token.
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    /// The base URL of the Plex server.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Build a full API URL for the given endpoint path.
    ///
    /// The `endpoint` should be a relative path like `identity` or
    /// `library/sections`. It will be appended to the base URL.
    pub fn api_url(&self, endpoint: &str) -> Url {
        let mut url = self.base_url.clone();
        {
            let mut segments = url.path_segments_mut().expect("base URL cannot-be-a-base");
            for part in endpoint.split('/') {
                if !part.is_empty() {
                    segments.push(part);
                }
            }
        }
        url
    }

    /// Resolve a stream request for a track part.
    ///
    /// The `part_key` is a relative path like `/library/parts/12345/file.flac`.
    /// Authentication is retained as a sensitive header, never appended to
    /// the URL copied into generic models.
    pub(crate) fn resolved_stream_request(
        &self,
        part_key: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        let mut url = self.base_url.clone();
        url.set_path(part_key);
        url.set_query(None);
        url.set_fragment(None);
        ResolvedHttpRequest::new(url)?.with_sensitive_header(
            HeaderName::from_static("x-plex-token"),
            plex_auth_header(&self.auth_token)?,
        )
    }

    /// Resolve a thumbnail request with the same credential isolation.
    ///
    /// The `thumb_path` is a relative path like `/library/metadata/12345/thumb/1234567890`.
    pub(crate) fn resolved_artwork_request(
        &self,
        thumb_path: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        let mut url = self.base_url.clone();
        url.set_path(thumb_path);
        url.set_query(None);
        url.set_fragment(None);
        ResolvedHttpRequest::new(url)?.with_sensitive_header(
            HeaderName::from_static("x-plex-token"),
            plex_auth_header(&self.auth_token)?,
        )
    }

    /// Issue a GET request to a Plex endpoint and deserialize the
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

        debug!(url = %redact_url_secrets(url.as_str()), "Plex request");

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
                message: "Plex returned 401 Unauthorized".into(),
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
            .map_err(|error| response_body_error("Failed to parse Plex JSON", error))?;

        let body = serde_json::from_slice::<T>(&body).map_err(|e| BackendError::ParseError {
            message: format!("Failed to parse Plex JSON: {e}"),
            source: Some(Box::new(e)),
        })?;

        Ok(body)
    }
}

/// Build a `reqwest::Client` with Plex auth and identification headers.
fn build_http_client(auth_token: &str) -> BackendResult<Client> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert("X-Plex-Token", plex_auth_header(auth_token)?);
    default_headers.insert(
        "X-Plex-Client-Identifier",
        HeaderValue::from_static(CLIENT_NAME),
    );
    default_headers.insert("X-Plex-Product", HeaderValue::from_static(CLIENT_NAME));
    default_headers.insert(
        "X-Plex-Version",
        HeaderValue::from_str(CLIENT_VERSION).unwrap_or_else(|_| HeaderValue::from_static("0.1.0")),
    );
    default_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

    authenticated_client_builder()
        .user_agent(CLIENT_NAME)
        .default_headers(default_headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| BackendError::ConnectionFailed {
            message: format!("Failed to build HTTP client: {e}"),
            source: Some(Box::new(e)),
        })
}

fn invalid_auth_header(error: reqwest::header::InvalidHeaderValue) -> BackendError {
    BackendError::ConnectionFailed {
        message: format!("Invalid auth token value: {error}"),
        source: Some(Box::new(error)),
    }
}

fn plex_auth_header(auth_token: &str) -> BackendResult<HeaderValue> {
    let mut value = HeaderValue::from_str(auth_token).map_err(invalid_auth_header)?;
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
    use super::*;

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
        let auth_token = uuid::Uuid::new_v4().to_string();
        let error = PlexClient::new(
            &format!("https://embedded-user:{secret}@plex.example.test"),
            &auth_token,
        )
        .err()
        .expect("embedded URL credentials must be rejected");

        let rendered = error.to_string();
        assert!(!rendered.contains("embedded-user"));
        assert!(!rendered.contains(&secret));
    }

    #[test]
    fn resolved_media_requests_keep_token_out_of_urls() {
        let auth_token = uuid::Uuid::new_v4().to_string();
        let client = PlexClient::new("https://plex.example.test", &auth_token).expect("client");

        for request in [
            client
                .resolved_stream_request("/library/parts/1/file.flac")
                .unwrap(),
            client
                .resolved_artwork_request("/library/metadata/1/thumb/2")
                .unwrap(),
        ] {
            assert!(!request.endpoint().as_str().contains(&auth_token));
            assert!(request.endpoint().query().is_none());
            assert!(request.private_query_pairs().is_empty());
            let value = request
                .sensitive_headers()
                .get("x-plex-token")
                .expect("auth header");
            assert!(value.is_sensitive());
        }
    }
}
