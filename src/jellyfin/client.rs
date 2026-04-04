//! Low-level Jellyfin HTTP client — authentication header injection,
//! request building, and JSON deserialization.

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
use reqwest::Client;
use tracing::{debug, info};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;

use super::api::{JellyfinAuthRequest, JellyfinAuthResponse};

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Client version advertised to the Jellyfin server.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Holds credentials and a reusable `reqwest::Client` with the
/// `X-Emby-Authorization` header pre-configured on every request.
pub struct JellyfinClient {
    base_url: Url,
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
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid server URL: {e}"),
            source: Some(Box::new(e)),
        })?;

        let http = build_http_client(api_key)?;

        info!(server = %base_url, user_id = %user_id, "Jellyfin client created (API key)");

        Ok(Self {
            base_url,
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
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid server URL: {e}"),
            source: Some(Box::new(e)),
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

        let pre_auth_http = Client::builder()
            .user_agent(CLIENT_NAME)
            .default_headers(pre_auth_headers)
            .build()
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to build HTTP client: {e}"),
                source: Some(Box::new(e)),
            })?;

        // POST /Users/AuthenticateByName
        let mut auth_url = base_url.clone();
        {
            let mut segments = auth_url
                .path_segments_mut()
                .expect("base URL cannot-be-a-base");
            segments.push("Users");
            segments.push("AuthenticateByName");
        }

        let body = JellyfinAuthRequest {
            username: username.to_string(),
            pw: password.to_string(),
        };

        debug!(url = %crate::audio::redact_url_secrets(auth_url.as_str()), "Jellyfin auth request");

        let resp = pre_auth_http
            .post(auth_url.as_str())
            .json(&body)
            .send()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Auth request failed: {e}"),
                source: Some(Box::new(e)),
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

        let auth_resp: JellyfinAuthResponse =
            resp.json().await.map_err(|e| BackendError::ParseError {
                message: format!("Failed to parse auth response: {e}"),
                source: Some(Box::new(e)),
            })?;

        let api_key = auth_resp.access_token;
        let user_id = auth_resp.user.id;

        info!(
            server = %base_url,
            user = %auth_resp.user.name,
            user_id = %user_id,
            "Jellyfin authentication successful"
        );

        // Build the real client with the acquired token.
        let http = build_http_client(&api_key)?;

        Ok(Self {
            base_url,
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

    /// Build a direct-stream URL for a track.
    ///
    /// Uses `api_key` as a query parameter so GStreamer's `playbin3`
    /// can fetch the audio without needing custom HTTP headers.
    pub fn stream_url(&self, item_id: &str) -> Url {
        let mut url = self.api_url(&format!("Audio/{item_id}/stream"));
        url.query_pairs_mut()
            .append_pair("static", "true")
            .append_pair("api_key", &self.api_key);
        url
    }

    /// Build a cover art URL for an item.
    pub fn image_url(&self, item_id: &str) -> Url {
        let mut url = self.api_url(&format!("Items/{item_id}/Images/Primary"));
        url.query_pairs_mut().append_pair("api_key", &self.api_key);
        url
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

        debug!(url = %crate::audio::redact_url_secrets(url.as_str()), "Jellyfin request");

        let resp = self.http.get(url.as_str()).send().await.map_err(|e| {
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

        let body = resp
            .json::<T>()
            .await
            .map_err(|e| BackendError::ParseError {
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
        debug!(url = %crate::audio::redact_url_secrets(url.as_str()), "Jellyfin text request");

        let resp = self.http.get(url.as_str()).send().await.map_err(|e| {
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

        let text = resp.text().await.map_err(|e| BackendError::ParseError {
            message: format!("Failed to read Jellyfin response body: {e}"),
            source: Some(Box::new(e)),
        })?;

        Ok(text)
    }
}

/// Build a `reqwest::Client` with the full `X-Emby-Authorization` header.
fn build_http_client(api_key: &str) -> BackendResult<Client> {
    let auth_value = format!(
        r#"MediaBrowser Client="{CLIENT_NAME}", Device="{CLIENT_NAME}", DeviceId="{CLIENT_NAME}", Version="{CLIENT_VERSION}", Token="{api_key}""#,
    );

    let mut default_headers = HeaderMap::new();
    default_headers.insert(
        "X-Emby-Authorization",
        HeaderValue::from_str(&auth_value).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid auth header value: {e}"),
            source: Some(Box::new(e)),
        })?,
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
