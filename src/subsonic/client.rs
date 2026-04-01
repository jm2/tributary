//! Low-level Subsonic HTTP client — authentication, request building,
//! and JSON deserialization.

use md5::{Digest, Md5};
use reqwest::Client;
use tracing::{debug, info};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;

use super::api::SubsonicEnvelope;

/// Subsonic API protocol version we advertise.
const API_VERSION: &str = "1.16.1";

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Holds credentials and a reusable `reqwest::Client`.
pub struct SubsonicClient {
    base_url: Url,
    username: String,
    /// Pre-computed: md5(password + salt)
    token: String,
    salt: String,
    http: Client,
}

impl SubsonicClient {
    /// Build a new client.  Generates a random salt and computes the
    /// authentication token immediately (no network call).
    pub fn new(server_url: &str, username: &str, password: &str) -> BackendResult<Self> {
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid server URL: {e}"),
            source: Some(Box::new(e)),
        })?;

        let salt: String = (0..12).map(|_| fastrand::alphanumeric()).collect();
        let token = {
            let mut hasher = Md5::new();
            hasher.update(password.as_bytes());
            hasher.update(salt.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        let http = Client::builder()
            .user_agent(CLIENT_NAME)
            .build()
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to build HTTP client: {e}"),
                source: Some(Box::new(e)),
            })?;

        info!(server = %base_url, user = %username, "Subsonic client created");

        Ok(Self {
            base_url,
            username: username.to_string(),
            token,
            salt,
            http,
        })
    }

    /// Build a full API URL with authentication query parameters.
    pub fn api_url(&self, endpoint: &str) -> Url {
        let mut url = self.base_url.clone();
        {
            let mut segments = url.path_segments_mut().expect("base URL cannot-be-a-base");
            segments.push("rest");
            segments.push(endpoint);
        }
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("u", &self.username);
            q.append_pair("t", &self.token);
            q.append_pair("s", &self.salt);
            q.append_pair("v", API_VERSION);
            q.append_pair("c", CLIENT_NAME);
            q.append_pair("f", "json");
        }
        url
    }

    /// Build a `stream.view` URL for the given Subsonic song ID.
    /// This URL can be passed directly to GStreamer — it includes all
    /// authentication parameters.
    pub fn stream_url(&self, song_id: &str) -> Url {
        let mut url = self.api_url("stream.view");
        url.query_pairs_mut().append_pair("id", song_id);
        url
    }

    /// Build a `getCoverArt.view` URL for the given Subsonic cover art ID.
    pub fn cover_art_url(&self, cover_art_id: &str) -> Url {
        let mut url = self.api_url("getCoverArt.view");
        url.query_pairs_mut().append_pair("id", cover_art_id);
        url
    }

    /// Issue a GET request to a Subsonic endpoint and deserialize the
    /// JSON envelope.  Returns a `BackendError` on HTTP or API errors.
    pub async fn get(&self, endpoint: &str) -> BackendResult<SubsonicEnvelope> {
        self.get_with_params(endpoint, &[]).await
    }

    /// Issue a GET with extra query parameters.
    pub async fn get_with_params(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> BackendResult<SubsonicEnvelope> {
        let mut url = self.api_url(endpoint);
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in params {
                q.append_pair(k, v);
            }
        }

        debug!(url = %url, "Subsonic request");

        let resp = self.http.get(url.as_str()).send().await.map_err(|e| {
            BackendError::ConnectionFailed {
                message: format!("HTTP request failed: {e}"),
                source: Some(Box::new(e)),
            }
        })?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("HTTP {}", resp.status()),
                source: None,
            });
        }

        let envelope: SubsonicEnvelope =
            resp.json().await.map_err(|e| BackendError::ParseError {
                message: format!("Failed to parse Subsonic JSON: {e}"),
                source: Some(Box::new(e)),
            })?;

        if envelope.response.status != "ok" {
            let msg = envelope
                .response
                .error
                .as_ref()
                .map(|e| format!("Subsonic error {}: {}", e.code, e.message))
                .unwrap_or_else(|| "Unknown Subsonic error".into());

            // Code 40 = wrong credentials
            if envelope
                .response
                .error
                .as_ref()
                .is_some_and(|e| e.code == 40)
            {
                return Err(BackendError::AuthenticationFailed { message: msg });
            }

            return Err(BackendError::ConnectionFailed {
                message: msg,
                source: None,
            });
        }

        Ok(envelope)
    }
}
