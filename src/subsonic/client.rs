//! Low-level Subsonic HTTP client — authentication, request building,
//! and JSON deserialization.

use std::time::Duration;

use md5::{Digest, Md5};
use reqwest::Client;
use tracing::{debug, info, warn};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::http_security::{authenticated_client_builder, redact_url_secrets, strip_request_url};

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

/// Maximum response body we are willing to buffer into memory.  A generous
/// cap that still rules out a malicious or misbehaving server trying to
/// exhaust memory with an unbounded body.  Enforced from the
/// `Content-Length` header before the body is read (see [`check_body_size`]).
const MAX_BODY_BYTES: u64 = 256 * 1024 * 1024;

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
        if base_url.cannot_be_a_base()
            || !matches!(base_url.scheme(), "http" | "https")
            || !base_url.username().is_empty()
            || base_url.password().is_some()
        {
            return Err(BackendError::ConnectionFailed {
                message: "Invalid server URL: use an http:// or https:// URL without embedded credentials"
                    .into(),
                source: None,
            });
        }

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

        debug!(url = %redact_url_secrets(url.as_str()), "Subsonic request");

        let resp = self.http.get(url.as_str()).send().await.map_err(|e| {
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

        // Refuse to buffer an oversized body (DoS guard) before reading it.
        check_body_size(&resp)?;

        let envelope: SubsonicEnvelope = resp.json().await.map_err(|e| {
            // A body-read failure here can also carry the credential-bearing
            // request URL; strip it before formatting/boxing into the error.
            let e = strip_request_url(e);
            BackendError::ParseError {
                message: format!("Failed to parse Subsonic JSON: {e}"),
                source: Some(Box::new(e)),
            }
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

/// Reject a response whose declared body exceeds [`MAX_BODY_BYTES`].
///
/// A lightweight, best-effort DoS guard: it inspects only the
/// `Content-Length` header, so a chunked response sent without a length is
/// not covered here — the client's `read_timeout` still bounds a stalled or
/// slow-trickling transfer in that case.
fn check_body_size(resp: &reqwest::Response) -> BackendResult<()> {
    if let Some(len) = resp.content_length() {
        if len > MAX_BODY_BYTES {
            return Err(BackendError::ConnectionFailed {
                message: format!(
                    "Response body too large: {len} bytes exceeds the {MAX_BODY_BYTES}-byte cap"
                ),
                source: None,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_embedded_url_credentials_without_echoing_them() {
        let secret = "userinfo-secret";
        let error = SubsonicClient::new(
            &format!("https://embedded-user:{secret}@music.example.test"),
            "api-user",
            "api-password",
        )
        .err()
        .expect("embedded URL credentials must be rejected");

        let rendered = error.to_string();
        assert!(!rendered.contains("embedded-user"));
        assert!(!rendered.contains(secret));
    }
}
