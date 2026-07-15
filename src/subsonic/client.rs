//! Low-level Subsonic HTTP client — authentication, request building,
//! and JSON deserialization.

use std::time::Duration;

use md5::{Digest, Md5};
use reqwest::Client;
use tracing::{debug, info, warn};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::ResolvedHttpRequest;
use crate::http_body::{read_limited, ResponseBodyError};
use crate::http_security::{
    authenticated_client_builder, redact_url_secrets, strip_request_url, validate_base_url,
};

use super::api::SubsonicEnvelope;

/// Subsonic API protocol version we advertise.
const API_VERSION: &str = "1.16.1";

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Connection-establishment timeout for API requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle read timeout.  Guards against a server that accepts the
/// connection but then stalls without sending (or only trickles) data,
/// while still allowing a large-but-healthy library transfer to complete
/// (the timeout resets after each successful read).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum response body we are willing to buffer into memory for API JSON.
const MAX_API_BODY_BYTES: u64 = 256 * 1024 * 1024;

/// End-to-end and body-phase deadline for finite API requests.
const API_RESPONSE_DEADLINE: Duration = Duration::from_secs(120);

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
#[derive(Clone)]
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

        // Reject non-hierarchical, wrong-scheme, and credential-bearing URLs
        // up front. `Url::parse`
        // happily accepts opaque inputs like `localhost:4533` (a scheme-less
        // host:port the user most likely meant as `http://localhost:4533`),
        // but building request paths from such a URL would panic later in
        // `api_url` via `path_segments_mut`. Embedded userinfo would also be
        // copied into every authenticated URL, so fail cleanly without echoing
        // the rejected URL.
        validate_base_url(&base_url).map_err(|message| BackendError::ConnectionFailed {
            message: message.to_string(),
            source: None,
        })?;

        // Token auth still transmits (username, salt, md5(password+salt)) in
        // the URL query string.  Over plain HTTP those can be captured by an
        // on-path attacker and replayed, or used for offline password
        // cracking (the salt is known and MD5 is fast).  Warn loudly — the
        // plaintext fallback is already HTTPS-gated, token auth was not.
        if base_url.scheme() != "https" {
            warn!(
                server = %redact_url_secrets(base_url.as_str()),
                "Subsonic token auth over an insecure (non-HTTPS) connection: the username, \
                 salt and token are sent in cleartext and can be captured and brute-forced. \
                 Use HTTPS where possible."
            );
        }

        let auth = Self::make_token_auth(password);

        let http = authenticated_client_builder()
            .user_agent(CLIENT_NAME)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to build HTTP client: {e}"),
                source: Some(Box::new(e)),
            })?;

        info!(
            server = %redact_url_secrets(base_url.as_str()),
            user = %username,
            "Subsonic client created"
        );

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
            server = %redact_url_secrets(self.base_url.as_str()),
            "Switching to hex-encoded plaintext auth (server does not support token auth)"
        );

        self.auth = AuthMode::Plaintext { hex_password };
        Ok(())
    }

    /// Build a full API URL with authentication query parameters.
    pub fn api_url(&self, endpoint: &str) -> Url {
        let mut url = self.base_url.clone();
        url.set_query(None);
        url.set_fragment(None);
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

    /// Resolve a stream request while keeping Subsonic authentication out of
    /// the inspectable endpoint.
    pub(crate) fn resolved_stream_request(
        &self,
        song_id: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        self.resolved_media_request("stream.view", song_id)
    }

    /// Resolve an artwork request with the same credential isolation.
    pub(crate) fn resolved_artwork_request(
        &self,
        cover_art_id: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        self.resolved_media_request("getCoverArt.view", cover_art_id)
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

        debug!(url = %redact_url_secrets(url.as_str()), "Subsonic request");

        let resp = self
            .http
            .get(url.as_str())
            .timeout(API_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|e| {
                // Strip the request URL from the error before it is formatted or
                // boxed: it carries the auth token + salt (or the hex-encoded
                // plaintext password) in its query string, and reqwest's Display
                // appends the full URL — which would leak the credential into
                // always-on error-level logs on routine transport failures.
                let e = strip_request_url(e);
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

        let body = read_limited(resp, MAX_API_BODY_BYTES, API_RESPONSE_DEADLINE)
            .await
            .map_err(|error| response_body_error("Failed to parse Subsonic JSON", error))?;

        let envelope: SubsonicEnvelope =
            serde_json::from_slice(&body).map_err(|e| BackendError::ParseError {
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

    fn resolved_media_request(
        &self,
        endpoint: &str,
        media_id: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url.path_segments_mut().expect("base URL cannot-be-a-base");
            segments.push("rest");
            segments.push(endpoint);
        }
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("id", media_id)
                .append_pair("v", API_VERSION)
                .append_pair("c", CLIENT_NAME)
                .append_pair("f", "json");
        }

        let mut request =
            ResolvedHttpRequest::new(url)?.with_private_query_pair("u", &self.username)?;
        request = match &self.auth {
            AuthMode::Token { token, salt } => request
                .with_private_query_pair("t", token)?
                .with_private_query_pair("s", salt)?,
            AuthMode::Plaintext { hex_password } => {
                request.with_private_query_pair("p", format!("enc:{hex_password}"))?
            }
        };
        Ok(request)
    }
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
        let api_password = uuid::Uuid::new_v4().to_string();
        let error = SubsonicClient::new(
            &format!("https://embedded-user:{secret}@music.example.test"),
            "api-user",
            &api_password,
        )
        .err()
        .expect("embedded URL credentials must be rejected");

        let rendered = error.to_string();
        assert!(!rendered.contains("embedded-user"));
        assert!(!rendered.contains(&secret));
    }

    #[test]
    fn resolved_token_request_separates_endpoint_and_private_auth() {
        let username = uuid::Uuid::new_v4().to_string();
        let password = uuid::Uuid::new_v4().to_string();
        let client = SubsonicClient::new("https://music.example.test", &username, &password)
            .expect("client");

        let request = client
            .resolved_stream_request("song-id")
            .expect("resolved request");
        let endpoint = request.endpoint().as_str();
        assert!(!endpoint.contains(&username));
        assert!(!endpoint.contains(&password));
        let mut public_keys: Vec<_> = request
            .endpoint()
            .query_pairs()
            .map(|(key, _)| key.into_owned())
            .collect();
        public_keys.sort();
        assert_eq!(public_keys, ["c", "f", "id", "v"]);
        assert!(request.sensitive_headers().is_empty());
        assert!(request
            .private_query_pairs()
            .iter()
            .any(|(key, value)| key == "u" && value == &username));
        assert!(request
            .private_query_pairs()
            .iter()
            .any(|(key, _)| key == "t"));
        assert!(request
            .private_query_pairs()
            .iter()
            .any(|(key, _)| key == "s"));
    }

    #[test]
    fn resolved_plaintext_request_keeps_password_state_private() {
        let username = uuid::Uuid::new_v4().to_string();
        let password = uuid::Uuid::new_v4().to_string();
        let mut client = SubsonicClient::new("https://music.example.test", &username, &password)
            .expect("client");
        client.switch_to_plaintext_auth().expect("HTTPS fallback");

        let request = client
            .resolved_stream_request("song-id")
            .expect("resolved request");
        assert!(!request.endpoint().as_str().contains(&password));
        assert!(request
            .private_query_pairs()
            .iter()
            .any(|(key, value)| key == "p" && value.starts_with("enc:")));
        assert!(!request
            .private_query_pairs()
            .iter()
            .any(|(key, _)| matches!(key.as_str(), "t" | "s")));
    }
}
