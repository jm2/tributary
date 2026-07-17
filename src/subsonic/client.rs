//! Low-level Subsonic HTTP client — authentication, request building,
//! and JSON deserialization.

use std::time::Duration;

use md5::{Digest, Md5};
use reqwest::Client;
use tracing::{debug, info, warn};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::{AdvertisedHttpRoute, ResolvedHttpRequest};
use crate::http_body::{read_limited, ResponseBodyError};
use crate::http_security::{
    append_base_path_segments, apply_advertised_http_route, authenticated_client_builder,
    redact_url_secrets, strip_request_url, validate_base_url,
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
const API_RESPONSE_DEADLINE: Duration = Duration::from_mins(2);

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
    advertised_route: Option<AdvertisedHttpRoute>,
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
        Self::new_with_route(server_url, username, password, None)
    }

    /// Build a client with an immutable address route supplied by discovery.
    ///
    /// The route changes only connection establishment. Request URLs, HTTP
    /// `Host`, TLS identity, redirects, and proxy selection continue to use
    /// the validated server URL.
    pub fn new_with_route(
        server_url: &str,
        username: &str,
        password: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
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

        let http_builder = authenticated_client_builder()
            .user_agent(CLIENT_NAME)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT);
        let http = apply_advertised_http_route(http_builder, &base_url, advertised_route.as_ref())
            .map_err(|message| BackendError::ConnectionFailed {
                message: message.to_string(),
                source: None,
            })?
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
            advertised_route,
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
        append_base_path_segments(&mut url, ["rest", endpoint]);
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
            if let Some(err) = &envelope.response.error {
                let message = format!("Subsonic API error {}", err.code);
                match err.code {
                    // Code 40 = wrong credentials
                    40 => {
                        return Err(BackendError::AuthenticationFailed { message });
                    }
                    // Code 41 = token auth not supported
                    41 => {
                        return Err(BackendError::TokenAuthNotSupported { message });
                    }
                    _ => {}
                }

                return Err(BackendError::ConnectionFailed {
                    message,
                    source: None,
                });
            }

            return Err(BackendError::ConnectionFailed {
                message: "Subsonic API returned a failed response".into(),
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
        append_base_path_segments(&mut url, ["rest", endpoint]);
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
        match &self.advertised_route {
            Some(route) => request.with_advertised_route(route.clone()),
            None => Ok(request),
        }
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
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};
    use std::thread;

    use super::*;

    fn advertised_route(origin: &str) -> AdvertisedHttpRoute {
        let origin = Url::parse(origin).expect("route origin");
        AdvertisedHttpRoute::new(&origin, [SocketAddr::from((Ipv4Addr::LOCALHOST, 45_321))])
            .expect("domain route")
    }

    fn spawn_failed_envelope(code: i32) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fixture server");
        listener
            .set_nonblocking(true)
            .expect("set fixture listener nonblocking");
        let address = listener.local_addr().expect("fixture address");
        let body = format!(
            r#"{{"subsonic-response":{{"status":"failed","version":"1.16.1","error":{{"code":{code},"message":"fixture failure"}}}}}}"#
        );
        let server = thread::spawn(move || {
            let accept_deadline = std::time::Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < accept_deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept fixture request: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("set fixture stream blocking");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set fixture read timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).expect("read fixture request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write fixture response");
        });
        (format!("http://{address}"), server)
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
    fn api_and_media_paths_preserve_reverse_proxy_prefixes_exactly() {
        let password = uuid::Uuid::new_v4().to_string();
        for (base, prefix) in [
            ("https://music.example.test", ""),
            ("https://music.example.test/share", "/share"),
            ("https://music.example.test/share/", "/share"),
            (
                "https://music.example.test/tenant%2Fmusic/",
                "/tenant%2Fmusic",
            ),
        ] {
            let mut client = SubsonicClient::new(base, "user", &password).expect("client");
            client.auth = AuthMode::Token {
                token: "fixed-token".to_string(),
                salt: "fixed-salt".to_string(),
            };
            assert_eq!(
                client.api_url("ping.view").as_str(),
                format!(
                    "https://music.example.test{prefix}/rest/ping.view?u=user&t=fixed-token&s=fixed-salt&v=1.16.1&c=Tributary&f=json"
                ),
                "API base URL: {base}"
            );
            assert_eq!(
                client
                    .resolved_stream_request("song-id")
                    .expect("stream request")
                    .endpoint()
                    .as_str(),
                format!(
                    "https://music.example.test{prefix}/rest/stream.view?id=song-id&v=1.16.1&c=Tributary&f=json"
                ),
                "stream base URL: {base}"
            );
            assert_eq!(
                client
                    .resolved_artwork_request("cover-id")
                    .expect("artwork request")
                    .endpoint()
                    .as_str(),
                format!(
                    "https://music.example.test{prefix}/rest/getCoverArt.view?id=cover-id&v=1.16.1&c=Tributary&f=json"
                ),
                "artwork base URL: {base}"
            );
            assert!(!client.api_url("ping.view").as_str().contains("%252F"));
        }
    }

    #[tokio::test]
    async fn maps_http_200_failed_envelopes_to_typed_subsonic_errors() {
        let password = uuid::Uuid::new_v4().to_string();
        for code in [40, 41, 70] {
            let (base_url, server) = spawn_failed_envelope(code);
            let client = SubsonicClient::new(&base_url, "user", &password).expect("client");
            let error = client
                .get("ping.view")
                .await
                .expect_err("failed envelope must not be accepted");
            server.join().expect("join fixture server");

            let expected = format!("Subsonic API error {code}");
            match (code, error) {
                (40, BackendError::AuthenticationFailed { message }) => {
                    assert_eq!(message, expected);
                }
                (41, BackendError::TokenAuthNotSupported { message }) => {
                    assert_eq!(message, expected);
                }
                (
                    70,
                    BackendError::ConnectionFailed {
                        message,
                        source: None,
                    },
                ) => assert_eq!(message, expected),
                (_, other) => panic!("unexpected Subsonic error mapping: {other}"),
            }
        }
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

    #[test]
    fn advertised_route_reaches_stream_and_artwork_requests() {
        let origin = "https://music.example.test";
        let route = advertised_route(origin);
        let username = uuid::Uuid::new_v4().to_string();
        let password = uuid::Uuid::new_v4().to_string();
        let client =
            SubsonicClient::new_with_route(origin, &username, &password, Some(route.clone()))
                .expect("routed client");

        for request in [
            client.resolved_stream_request("song-id").unwrap(),
            client.resolved_artwork_request("cover-id").unwrap(),
        ] {
            assert_eq!(request.advertised_route(), Some(&route));
            assert_eq!(request.endpoint().host_str(), Some("music.example.test"));
        }

        let ordinary = SubsonicClient::new(origin, &username, &password).expect("ordinary client");
        assert!(ordinary
            .resolved_stream_request("song-id")
            .unwrap()
            .advertised_route()
            .is_none());
    }

    #[test]
    fn mismatched_advertised_route_fails_without_exposing_credentials() {
        let username = uuid::Uuid::new_v4().to_string();
        let password = uuid::Uuid::new_v4().to_string();
        let Err(error) = SubsonicClient::new_with_route(
            "https://music.example.test",
            &username,
            &password,
            Some(advertised_route("https://other.example.test")),
        ) else {
            panic!("mismatched route must fail");
        };

        let rendered = error.to_string();
        assert!(!rendered.contains(&username));
        assert!(!rendered.contains(&password));
    }
}
