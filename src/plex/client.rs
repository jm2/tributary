//! Low-level Plex HTTP client — authentication header injection,
//! request building, and JSON deserialization.

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use tracing::{debug, info};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;

use super::api::PlexSignInResponse;

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Client version advertised to the Plex server.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Plex.tv sign-in endpoint.
const PLEX_TV_SIGN_IN: &str = "https://plex.tv/users/sign_in.json";

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

        let http = build_http_client(auth_token)?;

        info!(server = %base_url, "Plex client created (token)");

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

        let pre_auth_http = Client::builder()
            .user_agent(CLIENT_NAME)
            .default_headers(headers)
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
            .send()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Plex sign-in request failed: {e}"),
                source: Some(Box::new(e)),
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

        let sign_in: PlexSignInResponse =
            resp.json().await.map_err(|e| BackendError::ParseError {
                message: format!("Failed to parse Plex sign-in response: {e}"),
                source: Some(Box::new(e)),
            })?;

        let auth_token = sign_in.user.auth_token;

        info!(
            server = %base_url,
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

    /// Build a stream URL for a track part.
    ///
    /// The `part_key` is a relative path like `/library/parts/12345/file.flac`.
    /// The token is appended as a query parameter so GStreamer's `playbin3`
    /// can fetch the audio without needing custom HTTP headers.
    pub fn stream_url(&self, part_key: &str) -> Url {
        let mut url = self.base_url.clone();
        url.set_path(part_key);
        url.query_pairs_mut()
            .append_pair("X-Plex-Token", &self.auth_token);
        url
    }

    /// Build a thumbnail URL.
    ///
    /// The `thumb_path` is a relative path like `/library/metadata/12345/thumb/1234567890`.
    pub fn thumb_url(&self, thumb_path: &str) -> Url {
        let mut url = self.base_url.clone();
        url.set_path(thumb_path);
        url.query_pairs_mut()
            .append_pair("X-Plex-Token", &self.auth_token);
        url
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

        debug!(url = %url, "Plex request");

        let resp = self.http.get(url.as_str()).send().await.map_err(|e| {
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

        let body = resp
            .json::<T>()
            .await
            .map_err(|e| BackendError::ParseError {
                message: format!("Failed to parse Plex JSON: {e}"),
                source: Some(Box::new(e)),
            })?;

        Ok(body)
    }
}

/// Build a `reqwest::Client` with Plex auth and identification headers.
fn build_http_client(auth_token: &str) -> BackendResult<Client> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(
        "X-Plex-Token",
        HeaderValue::from_str(auth_token).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid auth token value: {e}"),
            source: Some(Box::new(e)),
        })?,
    );
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

    Client::builder()
        .user_agent(CLIENT_NAME)
        .default_headers(default_headers)
        .build()
        .map_err(|e| BackendError::ConnectionFailed {
            message: format!("Failed to build HTTP client: {e}"),
            source: Some(Box::new(e)),
        })
}
