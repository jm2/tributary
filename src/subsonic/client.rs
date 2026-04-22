//! Low-level Subsonic HTTP client — authentication, request building,
//! and JSON deserialization.

use md5::{Digest, Md5};
use reqwest::Client;
use tracing::{debug, info, warn};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;

use super::api::SubsonicEnvelope;

/// Subsonic API protocol version we advertise.
const API_VERSION: &str = "1.16.1";

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Authentication mode used for API requests.
#[derive(Clone)]
enum AuthMode {
    /// Modern token/salt authentication (Subsonic API ≥ 1.13.0).
    /// Sends `t=md5(password+salt)` and `s=salt`.
    Token { token: String, salt: String },

    /// Legacy plaintext authentication for servers that do not support
    /// token auth (e.g. Nextcloud Music).
    /// Sends `p=enc:<hex-encoded password>`.
    /// Only permitted over HTTPS.
    Plaintext { hex_password: String },
}

impl std::fmt::Debug for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token { .. } => f.debug_struct("Token").finish_non_exhaustive(),
            Self::Plaintext { .. } => f.debug_struct("Plaintext").finish_non_exhaustive(),
        }
    }
}

/// Holds credentials and a reusable `reqwest::Client`.
pub struct SubsonicClient {
    base_url: Url,
    username: String,
    /// Raw password retained so we can switch auth modes on fallback.
    password: String,
    auth: AuthMode,
    http: Client,
}

impl SubsonicClient {
    /// Build a new client.  Generates a random salt and computes the
    /// authentication token immediately (no network call).
    ///
    /// Starts in **token auth** mode.  Call
    /// [`switch_to_plaintext_auth`] to fall back to hex-encoded
    /// plaintext (only allowed over HTTPS).
    pub fn new(server_url: &str, username: &str, password: &str) -> BackendResult<Self> {
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid server URL: {e}"),
            source: Some(Box::new(e)),
        })?;

        let auth = Self::make_token_auth(password);

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
            password: password.to_string(),
            auth,
            http,
        })
    }

    /// Switch to hex-encoded plaintext authentication.
    ///
    /// Returns an error if the server URL is not HTTPS — we refuse
    /// to send even hex-encoded passwords over unencrypted connections.
    pub fn switch_to_plaintext_auth(&mut self) -> BackendResult<()> {
        if self.base_url.scheme() != "https" {
            return Err(BackendError::ConnectionFailed {
                message: format!(
                    "Refusing to use plaintext authentication over insecure connection ({}). \
                     This server requires legacy auth which sends the password in the URL. \
                     Please use HTTPS.",
                    self.base_url.scheme(),
                ),
                source: None,
            });
        }

        let hex_password = self
            .password
            .as_bytes()
            .iter()
            .fold(String::new(), |mut acc, b| {
                use std::fmt::Write;
                let _ = write!(acc, "{b:02x}");
                acc
            });

        warn!(
            server = %self.base_url,
            "Switching to hex-encoded plaintext auth (server does not support token auth)"
        );

        self.auth = AuthMode::Plaintext { hex_password };
        Ok(())
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

            match &self.auth {
                AuthMode::Token { token, salt } => {
                    q.append_pair("t", token);
                    q.append_pair("s", salt);
                }
                AuthMode::Plaintext { hex_password } => {
                    q.append_pair("p", &format!("enc:{hex_password}"));
                }
            }

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

        debug!(url = %crate::audio::redact_url_secrets(url.as_str()), "Subsonic request");

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

            if let Some(err) = &envelope.response.error {
                match err.code {
                    // Code 40 = wrong credentials
                    40 => {
                        return Err(BackendError::AuthenticationFailed { message: msg });
                    }
                    // Code 41 = token auth not supported
                    41 => {
                        return Err(BackendError::TokenAuthNotSupported { message: msg });
                    }
                    _ => {}
                }
            }

            return Err(BackendError::ConnectionFailed {
                message: msg,
                source: None,
            });
        }

        Ok(envelope)
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Compute token auth params from a password.
    fn make_token_auth(password: &str) -> AuthMode {
        let salt: String = (0..12).map(|_| fastrand::alphanumeric()).collect();
        let token = {
            let mut hasher = Md5::new();
            hasher.update(password.as_bytes());
            hasher.update(salt.as_bytes());
            hasher.finalize().iter().fold(String::new(), |mut acc, b| {
                use std::fmt::Write;
                let _ = write!(acc, "{b:02x}");
                acc
            })
        };
        AuthMode::Token { token, salt }
    }
}
