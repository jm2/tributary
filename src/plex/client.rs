//! Low-level Plex HTTP client — authentication header injection,
//! request building, and JSON deserialization.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use tracing::{debug, info, warn};
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

use super::api::{PlexIdentityResponse, PlexResource, PlexSignInResponse};

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Client version advertised to the Plex server.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Plex.tv sign-in endpoint.
const PLEX_TV_SIGN_IN: &str = "https://plex.tv/users/sign_in.json";

/// Plex.tv listing of the servers the signed-in account can reach, with the
/// HTTPS (`plex.direct`) connections each server publishes.
const PLEX_TV_RESOURCES: &str = "https://plex.tv/api/v2/resources?includeHttps=1";

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
const API_RESPONSE_DEADLINE: Duration = Duration::from_mins(2);

/// Holds credentials and a reusable `reqwest::Client` with the
/// `X-Plex-Token` header pre-configured on every request.
pub struct PlexClient {
    base_url: Url,
    advertised_route: Option<AdvertisedHttpRoute>,
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
        Self::new_with_route(server_url, auth_token, None)
    }

    /// Build a client with an immutable address route supplied by discovery.
    pub fn new_with_route(
        server_url: &str,
        auth_token: &str,
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

        let http = build_http_client(auth_token, &base_url, advertised_route.as_ref())?;

        info!(
            server = %redact_url_secrets(base_url.as_str()),
            "Plex client created (token)"
        );

        Ok(Self {
            base_url,
            advertised_route,
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
        Self::authenticate_with_route(server_url, username, password, None).await
    }

    /// Authenticate via plex.tv, then route only the selected Plex server.
    pub async fn authenticate_with_route(
        server_url: &str,
        username: &str,
        password: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
        Self::authenticate_with_route_at(
            server_url,
            username,
            password,
            advertised_route,
            PLEX_TV_SIGN_IN,
        )
        .await
    }

    async fn authenticate_with_route_at(
        server_url: &str,
        username: &str,
        password: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
        sign_in_url: &str,
    ) -> BackendResult<Self> {
        let base_url = routed_server_url(server_url, advertised_route.as_ref())?;
        let auth_token = sign_in(username, password, sign_in_url).await?;
        info!(
            server = %redact_url_secrets(base_url.as_str()),
            "Plex authentication successful"
        );

        // The discovery route belongs to the selected Plex server. The
        // account sign-in above intentionally uses the ordinary plex.tv
        // client and must never inherit this route.
        let http = build_http_client(&auth_token, &base_url, advertised_route.as_ref())?;

        Ok(Self {
            base_url,
            advertised_route,
            auth_token,
            http,
        })
    }

    /// Sign in via plex.tv and connect to a server found by network discovery.
    ///
    /// Discovery is unauthenticated, so the advertised host only names a
    /// candidate. Its `/identity` is read without credentials, and the
    /// account's plex.tv resources must list a server with that machine
    /// identifier. The client then uses that server's own access token over
    /// an HTTPS connection plex.tv publishes for it, so the account token is
    /// only ever sent to plex.tv and no token travels in cleartext. A server
    /// that publishes no HTTPS connection is refused.
    pub async fn authenticate_discovered(
        discovered_url: &str,
        username: &str,
        password: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
        Self::authenticate_discovered_at(
            discovered_url,
            username,
            password,
            advertised_route,
            PLEX_TV_SIGN_IN,
            PLEX_TV_RESOURCES,
        )
        .await
    }

    async fn authenticate_discovered_at(
        discovered_url: &str,
        username: &str,
        password: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
        sign_in_url: &str,
        resources_url: &str,
    ) -> BackendResult<Self> {
        let discovered = routed_server_url(discovered_url, advertised_route.as_ref())?;
        let machine_identifier =
            unauthenticated_machine_identifier(&discovered, advertised_route.as_ref()).await?;
        let account_token = sign_in(username, password, sign_in_url).await?;
        let resources = account_resources(&account_token, resources_url).await?;
        let server = linked_server(resources, &machine_identifier)?;
        info!(
            server = %redact_url_secrets(server.url.as_str()),
            "Discovered Plex server verified through plex.tv"
        );
        Self::new_with_route(server.url.as_str(), &server.access_token, server.route)
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
        append_base_path_segments(
            &mut url,
            endpoint.split('/').filter(|part| !part.is_empty()),
        );
        url
    }

    /// Resolve a stream request for a track part.
    ///
    /// The `part_key` is a relative path like `/library/parts/12345/file.flac`.
    /// Authentication is retained as a sensitive header, never appended to
    /// the URL copied into generic models. `representation` is the validated
    /// descriptor of the media the part returns (see `MediaRepresentation`).
    pub(crate) fn resolved_stream_request(
        &self,
        part_key: &str,
        representation: MediaRepresentation,
    ) -> BackendResult<ResolvedHttpRequest> {
        let url = self.media_url(part_key)?;
        let request = ResolvedHttpRequest::new(url)?
            .with_representation(representation)
            .with_sensitive_header(
                HeaderName::from_static("x-plex-token"),
                plex_auth_header(&self.auth_token)?,
            )?;
        match &self.advertised_route {
            Some(route) => request.with_advertised_route(route.clone()),
            None => Ok(request),
        }
    }

    /// Resolve a thumbnail request with the same credential isolation.
    ///
    /// The `thumb_path` is a relative path like `/library/metadata/12345/thumb/1234567890`.
    pub(crate) fn resolved_artwork_request(
        &self,
        thumb_path: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        let url = self.media_url(thumb_path)?;
        let request = ResolvedHttpRequest::new(url)?.with_sensitive_header(
            HeaderName::from_static("x-plex-token"),
            plex_auth_header(&self.auth_token)?,
        )?;
        match &self.advertised_route {
            Some(route) => request.with_advertised_route(route.clone()),
            None => Ok(request),
        }
    }

    /// Append a server-issued root-relative path beneath the configured Plex
    /// base path. Reverse proxies commonly expose Plex below `/plex`; replacing
    /// the URL path here would silently bypass that prefix for playback and
    /// artwork even though catalogue requests used it correctly.
    fn media_url(&self, server_path: &str) -> BackendResult<Url> {
        let suffix = server_path.trim_start_matches('/');
        if suffix.is_empty() {
            return Err(BackendError::ConnectionFailed {
                message: "Plex returned an empty media path".into(),
                source: None,
            });
        }

        // Match `append_base_path_segments`: remove exactly one trailing
        // empty segment. A deliberately configured `/share//` prefix must
        // remain `/share//` for media just as it does for API requests.
        let base_path = self
            .base_url
            .path()
            .strip_suffix('/')
            .unwrap_or_else(|| self.base_url.path());
        let required_prefix = if base_path.is_empty() || base_path == "/" {
            "/".to_string()
        } else {
            format!("{base_path}/")
        };
        let mut url = self.base_url.clone();
        url.set_path(&format!("{required_prefix}{suffix}"));
        url.set_query(None);
        url.set_fragment(None);

        // `Url::set_path` normalizes dot segments. A peer-supplied path must
        // not use that normalization to escape a configured reverse-proxy
        // prefix; keep the diagnostic fixed so the path cannot reach logs/UI.
        if !url.path().starts_with(&required_prefix) {
            return Err(BackendError::ConnectionFailed {
                message: "Plex returned a media path outside the configured base path".into(),
                source: None,
            });
        }
        Ok(url)
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

        let body = parse_remote_json::<T>("Failed to parse Plex JSON", &body)?;

        Ok(body)
    }
}

/// Parse and validate a Plex server URL, and check that an advertised route
/// belongs to it before any credential-bearing request is built.
fn routed_server_url(
    server_url: &str,
    advertised_route: Option<&AdvertisedHttpRoute>,
) -> BackendResult<Url> {
    let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
        message: format!("Invalid server URL: {e}"),
        source: Some(Box::new(e)),
    })?;
    validate_base_url(&base_url).map_err(|message| BackendError::ConnectionFailed {
        message: message.to_string(),
        source: None,
    })?;
    // The returned builder is deliberately discarded: this route belongs only
    // to the selected Plex server and must never be installed on a plex.tv
    // account client.
    drop(
        apply_advertised_http_route(authenticated_client_builder(), &base_url, advertised_route)
            .map_err(|message| BackendError::ConnectionFailed {
                message: message.to_string(),
                source: None,
            })?,
    );
    Ok(base_url)
}

/// Headers identifying Tributary to Plex, without any token.
fn identification_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Plex-Client-Identifier",
        HeaderValue::from_static(install_id()),
    );
    headers.insert("X-Plex-Product", HeaderValue::from_static(CLIENT_NAME));
    headers.insert(
        "X-Plex-Version",
        HeaderValue::from_str(CLIENT_VERSION).unwrap_or_else(|_| HeaderValue::from_static("0.1.0")),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers
}

/// A client carrying identification headers only, optionally routed to one
/// Plex server.
fn unauthenticated_client(
    origin: &Url,
    advertised_route: Option<&AdvertisedHttpRoute>,
) -> BackendResult<Client> {
    let builder = authenticated_client_builder()
        .user_agent(CLIENT_NAME)
        .default_headers(identification_headers())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT);
    apply_advertised_http_route(builder, origin, advertised_route)
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

/// Exchange a username and password for the account-wide plex.tv token.
async fn sign_in(username: &str, password: &str, sign_in_url: &str) -> BackendResult<String> {
    let pre_auth_http = unauthenticated_client(
        &Url::parse(sign_in_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid plex.tv URL: {e}"),
            source: Some(Box::new(e)),
        })?,
        None,
    )?;

    let form_body = format!(
        "user[login]={}&user[password]={}",
        urlencoding::encode(username),
        urlencoding::encode(password)
    );

    debug!("Plex sign-in request to plex.tv");

    let resp = pre_auth_http
        .post(sign_in_url)
        .header(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        )
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
        parse_remote_json("Failed to parse Plex sign-in response", &body)?;
    info!(user = ?sign_in.user.username, "Plex sign-in successful");
    Ok(sign_in.user.auth_token)
}

/// Read the machine identifier a Plex server reports on its unauthenticated
/// `/identity` endpoint. The answer names a server but proves nothing about
/// it, so no credential is sent.
async fn unauthenticated_machine_identifier(
    base_url: &Url,
    advertised_route: Option<&AdvertisedHttpRoute>,
) -> BackendResult<String> {
    let http = unauthenticated_client(base_url, advertised_route)?;
    let mut url = base_url.clone();
    append_base_path_segments(&mut url, ["identity"]);
    let resp = http
        .get(url.as_str())
        .timeout(AUTH_RESPONSE_DEADLINE)
        .send()
        .await
        .map_err(|e| {
            let e = strip_request_url(e);
            BackendError::ConnectionFailed {
                message: format!("Plex identity request failed: {e}"),
                source: Some(Box::new(e)),
            }
        })?;
    if !resp.status().is_success() {
        return Err(BackendError::ConnectionFailed {
            message: format!("Plex identity HTTP {}", resp.status()),
            source: None,
        });
    }
    let body = read_limited(resp, MAX_AUTH_BODY_BYTES, AUTH_RESPONSE_DEADLINE)
        .await
        .map_err(|error| response_body_error("Failed to parse Plex identity", error))?;
    let identity: PlexIdentityResponse = parse_remote_json("Failed to parse Plex identity", &body)?;
    identity
        .media_container
        .machine_identifier
        .filter(|identifier| !identifier.is_empty())
        .ok_or_else(|| BackendError::ParseError {
            message: "Plex identity has no machine identifier".into(),
            source: None,
        })
}

/// List the servers and devices the account token can reach. The token goes
/// only to plex.tv.
async fn account_resources(
    account_token: &str,
    resources_url: &str,
) -> BackendResult<Vec<PlexResource>> {
    let url = Url::parse(resources_url).map_err(|e| BackendError::ConnectionFailed {
        message: format!("Invalid plex.tv URL: {e}"),
        source: Some(Box::new(e)),
    })?;
    let resp = unauthenticated_client(&url, None)?
        .get(url.as_str())
        .header("X-Plex-Token", plex_auth_header(account_token)?)
        .timeout(AUTH_RESPONSE_DEADLINE)
        .send()
        .await
        .map_err(|e| {
            let e = strip_request_url(e);
            BackendError::ConnectionFailed {
                message: format!("plex.tv resources request failed: {e}"),
                source: Some(Box::new(e)),
            }
        })?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(BackendError::AuthenticationFailed {
            message: "plex.tv rejected the account token".into(),
        });
    }
    if !status.is_success() {
        return Err(BackendError::ConnectionFailed {
            message: format!("plex.tv resources HTTP {status}"),
            source: None,
        });
    }
    let body = read_limited(resp, MAX_AUTH_BODY_BYTES, AUTH_RESPONSE_DEADLINE)
        .await
        .map_err(|error| response_body_error("Failed to parse plex.tv resources", error))?;
    parse_remote_json("Failed to parse plex.tv resources", &body)
}

/// A plex.tv-published way to reach one of the account's servers.
#[derive(Debug)]
struct LinkedServer {
    url: Url,
    access_token: String,
    route: Option<AdvertisedHttpRoute>,
}

/// Select the account's server with `machine_identifier` and its preferred
/// HTTPS connection (local before remote, never relayed).
///
/// The connection's published address is pinned as the route, so reaching a
/// `plex.direct` name does not depend on public DNS (which routers with
/// DNS-rebinding protection refuse); TLS still verifies the server's
/// `plex.direct` certificate.
fn linked_server(
    resources: Vec<PlexResource>,
    machine_identifier: &str,
) -> BackendResult<LinkedServer> {
    let Some(resource) = resources.into_iter().find(|resource| {
        resource.client_identifier == machine_identifier
            && resource
                .provides
                .as_deref()
                .is_some_and(|provides| provides.split(',').any(|role| role.trim() == "server"))
    }) else {
        warn!("Discovered Plex server is not one of the signed-in account's servers");
        return Err(BackendError::AuthenticationFailed {
            message: "This Plex server is not available to the signed-in account".into(),
        });
    };
    let Some(access_token) = resource.access_token.filter(|token| !token.is_empty()) else {
        return Err(BackendError::AuthenticationFailed {
            message: "plex.tv listed no access token for this Plex server".into(),
        });
    };
    let url = resource
        .connections
        .unwrap_or_default()
        .into_iter()
        .filter(|connection| {
            !connection.relay
                && connection
                    .protocol
                    .as_deref()
                    .is_some_and(|protocol| protocol.eq_ignore_ascii_case("https"))
        })
        .filter_map(|connection| {
            let url = Url::parse(connection.uri.as_deref()?).ok()?;
            (url.scheme() == "https" && validate_base_url(&url).is_ok()).then_some((
                connection.local,
                connection.address,
                url,
            ))
        })
        .min_by_key(|(local, _, _)| !*local);
    let Some((_, address, url)) = url else {
        warn!("Discovered Plex server publishes no HTTPS connection; refusing to send a token");
        return Err(BackendError::ConnectionFailed {
            message: "This Plex server publishes no secure connection".into(),
            source: None,
        });
    };
    let route = address
        .and_then(|address| address.parse::<IpAddr>().ok())
        .zip(url.port_or_known_default())
        .and_then(|(ip, port)| AdvertisedHttpRoute::new(&url, [SocketAddr::new(ip, port)]));
    Ok(LinkedServer {
        url,
        access_token,
        route,
    })
}

/// Build a `reqwest::Client` with Plex auth and identification headers.
fn build_http_client(
    auth_token: &str,
    base_url: &Url,
    advertised_route: Option<&AdvertisedHttpRoute>,
) -> BackendResult<Client> {
    let mut default_headers = identification_headers();
    default_headers.insert("X-Plex-Token", plex_auth_header(auth_token)?);

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
    use std::io::ErrorKind;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    use axum::http::{Method, StatusCode};

    use crate::architecture::remote_json::rendered_error_chain;
    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};
    use crate::plex::api::{PlexResource, PlexSectionsResponse};

    use super::*;

    fn advertised_route(origin: &str) -> AdvertisedHttpRoute {
        let origin = Url::parse(origin).expect("route origin");
        AdvertisedHttpRoute::new(&origin, [SocketAddr::from((Ipv4Addr::LOCALHOST, 45_323))])
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
                .resolved_stream_request(
                    "/library/parts/1/file.flac",
                    MediaRepresentation::buffered_unknown(),
                )
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

    #[test]
    fn api_and_media_paths_preserve_reverse_proxy_prefixes_exactly() {
        for (base, prefix) in [
            ("https://plex.example.test", ""),
            ("https://plex.example.test//", ""),
            ("https://plex.example.test/share", "/share"),
            ("https://plex.example.test/share/", "/share"),
            ("https://plex.example.test/share//", "/share/"),
            (
                "https://plex.example.test/tenant%2Fmusic/",
                "/tenant%2Fmusic",
            ),
        ] {
            let client = PlexClient::new(base, "token").expect("client");
            assert_eq!(
                client.api_url("library/sections").as_str(),
                format!("https://plex.example.test{prefix}/library/sections"),
                "base URL: {base}"
            );
            assert_eq!(
                client
                    .resolved_stream_request(
                        "/library/parts/file%2Fname.flac",
                        MediaRepresentation::buffered_unknown()
                    )
                    .expect("stream request")
                    .endpoint()
                    .as_str(),
                format!("https://plex.example.test{prefix}/library/parts/file%2Fname.flac"),
                "base URL: {base}"
            );
            assert_eq!(
                client
                    .resolved_artwork_request("/library/metadata/1/thumb/2")
                    .expect("artwork request")
                    .endpoint()
                    .as_str(),
                format!("https://plex.example.test{prefix}/library/metadata/1/thumb/2"),
                "base URL: {base}"
            );
        }
    }

    #[test]
    fn media_paths_cannot_escape_reverse_proxy_prefix() {
        let client = PlexClient::new("https://plex.example.test/share/", "token").expect("client");
        for path in [
            "../outside-prefix",
            "%2e%2e/outside-prefix",
            ".%2e/outside-prefix",
        ] {
            let error = client
                .resolved_stream_request(path, MediaRepresentation::buffered_unknown())
                .err()
                .expect("plain or encoded dot segments must not escape the configured prefix");
            assert!(matches!(error, BackendError::ConnectionFailed { .. }));
            assert!(!error.to_string().contains("outside-prefix"));
        }
    }

    #[tokio::test]
    async fn rejected_auth_returns_typed_redacted_error() {
        let service = MockHttpService::start(vec![MockRoute::new(
            Method::POST,
            "/account/users/sign_in.json",
        )
        .reply(MockResponse::status(StatusCode::UNAUTHORIZED))])
        .await;
        let username = uuid::Uuid::new_v4().to_string();
        let password = uuid::Uuid::new_v4().to_string();
        let sign_in_url = format!("{}/account/users/sign_in.json", service.base_url());
        let result = PlexClient::authenticate_with_route_at(
            "https://plex.example.test/gateway/",
            &username,
            &password,
            None,
            &sign_in_url,
        )
        .await;
        let error = result.err().expect("fixture authentication must fail");

        assert!(matches!(error, BackendError::AuthenticationFailed { .. }));
        let rendered = error.to_string();
        assert!(!rendered.contains(&username));
        assert!(!rendered.contains(&password));
        let requests = service.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers.get("x-plex-client-identifier"),
            Some(&HeaderValue::from_static(install_id()))
        );
        service.finish().await;
    }

    #[test]
    fn advertised_route_reaches_stream_and_artwork_requests() {
        let origin = "https://plex.example.test";
        let route = advertised_route(origin);
        let client = PlexClient::new_with_route(origin, "route-token", Some(route.clone()))
            .expect("routed client");

        for request in [
            client
                .resolved_stream_request(
                    "/library/parts/1/file.flac",
                    MediaRepresentation::buffered_unknown(),
                )
                .unwrap(),
            client
                .resolved_artwork_request("/library/metadata/1/thumb/2")
                .unwrap(),
        ] {
            assert_eq!(request.advertised_route(), Some(&route));
            assert_eq!(request.endpoint().host_str(), Some("plex.example.test"));
        }

        let ordinary = PlexClient::new(origin, "token").expect("ordinary client");
        assert!(ordinary
            .resolved_stream_request(
                "/library/parts/1/file.flac",
                MediaRepresentation::buffered_unknown()
            )
            .unwrap()
            .advertised_route()
            .is_none());
    }

    #[test]
    fn mismatched_advertised_route_fails_without_exposing_credentials() {
        let auth_token = uuid::Uuid::new_v4().to_string();
        let Err(error) = PlexClient::new_with_route(
            "https://plex.example.test",
            &auth_token,
            Some(advertised_route("https://other.example.test")),
        ) else {
            panic!("mismatched route must fail");
        };

        assert!(!error.to_string().contains(&auth_token));
    }

    #[tokio::test]
    async fn mismatched_route_is_rejected_before_plex_tv_receives_credentials() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind sign-in sentinel");
        listener
            .set_nonblocking(true)
            .expect("make sign-in sentinel nonblocking");
        let sign_in_url = format!(
            "http://{}/users/sign_in.json",
            listener.local_addr().expect("sign-in sentinel address")
        );
        let username = uuid::Uuid::new_v4().to_string();
        let password = uuid::Uuid::new_v4().to_string();

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            PlexClient::authenticate_with_route_at(
                "https://plex.example.test",
                &username,
                &password,
                Some(advertised_route("https://other.example.test")),
                &sign_in_url,
            ),
        )
        .await
        .expect("route mismatch must fail before sign-in I/O");
        let Err(error) = outcome else {
            panic!("mismatched route must fail");
        };

        let rendered = error.to_string();
        assert!(rendered.contains("advertised HTTP route does not match"));
        assert!(!rendered.contains(&username));
        assert!(!rendered.contains(&password));
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == ErrorKind::WouldBlock
        ));
    }

    fn plex_resources(connections: serde_json::Value) -> serde_json::Value {
        serde_json::json!([
            {
                "clientIdentifier": "player-device",
                "provides": "player",
                "accessToken": null,
                "connections": null
            },
            {
                "clientIdentifier": "home-server",
                "provides": "server",
                "accessToken": "home-server-token",
                "connections": connections
            }
        ])
    }

    fn https_connection(address: &str, local: bool, relay: bool) -> serde_json::Value {
        let host = address.replace('.', "-");
        serde_json::json!({
            "protocol": "https",
            "address": address,
            "port": 32400,
            "uri": format!("https://{host}.0123abcd.plex.direct:32400"),
            "local": local,
            "relay": relay
        })
    }

    /// Run the discovered-server flow against a discovered host that reports
    /// `machine_identifier` and a plex.tv fixture listing `resources`, and
    /// assert that no token ever reached the discovered host.
    async fn authenticate_discovered_fixture(
        machine_identifier: &str,
        resources: serde_json::Value,
    ) -> BackendError {
        let account_token = uuid::Uuid::new_v4().to_string();
        let discovered = MockHttpService::start(vec![MockRoute::get("/identity").reply(
            MockResponse::json(serde_json::json!({
                "MediaContainer": {"machineIdentifier": machine_identifier}
            })),
        )])
        .await;
        let plex_tv = MockHttpService::start(vec![
            MockRoute::new(Method::POST, "/users/sign_in.json").reply(MockResponse::json(
                serde_json::json!({"user": {"authToken": account_token}}),
            )),
            MockRoute::get("/api/v2/resources").reply(MockResponse::json(resources)),
        ])
        .await;

        let error = PlexClient::authenticate_discovered_at(
            &discovered.base_url(),
            "user",
            "password",
            None,
            &format!("{}/users/sign_in.json", plex_tv.base_url()),
            &format!("{}/api/v2/resources?includeHttps=1", plex_tv.base_url()),
        )
        .await
        .err()
        .expect("the discovered host must not be accepted");

        let discovered_requests = discovered.requests();
        assert_eq!(discovered_requests.len(), 1);
        let identity = &discovered_requests[0];
        assert_eq!(identity.uri.path(), "/identity");
        assert!(identity.uri.query().is_none());
        assert!(identity.headers.get("x-plex-token").is_none());
        for value in identity.headers.values() {
            let value = value.to_str().unwrap_or_default();
            assert!(!value.contains(&account_token) && !value.contains("home-server-token"));
        }
        let resources_request = &plex_tv.requests()[1];
        assert_eq!(
            resources_request
                .headers
                .get("x-plex-token")
                .and_then(|value| value.to_str().ok()),
            Some(account_token.as_str())
        );
        discovered.finish().await;
        plex_tv.finish().await;
        error
    }

    #[tokio::test]
    async fn discovered_host_with_an_unlisted_identity_never_receives_a_token() {
        let error = authenticate_discovered_fixture(
            "impostor",
            plex_resources(serde_json::json!([https_connection(
                "192.0.2.10",
                true,
                false
            )])),
        )
        .await;
        assert!(matches!(error, BackendError::AuthenticationFailed { .. }));
    }

    #[tokio::test]
    async fn discovered_server_without_an_https_connection_is_refused() {
        let error = authenticate_discovered_fixture(
            "home-server",
            plex_resources(serde_json::json!([{
                "protocol": "http",
                "address": "192.0.2.10",
                "port": 32400,
                "uri": "http://192.0.2.10:32400",
                "local": true,
                "relay": false
            }])),
        )
        .await;
        assert!(matches!(error, BackendError::ConnectionFailed { .. }));
    }

    #[test]
    fn linked_server_prefers_local_https_with_its_own_token_and_address() {
        let resources: Vec<PlexResource> =
            serde_json::from_value(plex_resources(serde_json::json!([
                https_connection("203.0.113.7", false, false),
                https_connection("198.51.100.1", true, true),
                https_connection("192.0.2.10", true, false)
            ])))
            .expect("resources fixture");

        let server = linked_server(resources, "home-server").expect("listed server");

        assert_eq!(
            server.url.as_str(),
            "https://192-0-2-10.0123abcd.plex.direct:32400/"
        );
        assert_eq!(server.access_token, "home-server-token");
        let route = server.route.expect("published address is pinned");
        assert!(route.matches_origin(&server.url));
        assert_eq!(
            route.addresses(),
            [SocketAddr::from(([192, 0, 2, 10], 32400))]
        );

        let players_only: Vec<PlexResource> =
            serde_json::from_value(plex_resources(serde_json::json!([])))
                .expect("resources fixture");
        assert!(matches!(
            linked_server(players_only, "player-device"),
            Err(BackendError::AuthenticationFailed { .. })
        ));
    }

    /// A Plex sign-in body whose `user` is a string instead of the expected
    /// object makes `serde_json` quote the wrong-type value. The production
    /// authentication path must drop it rather than log or surface it.
    #[tokio::test]
    async fn auth_parse_failures_omit_response_content_from_diagnostics() {
        let sentinel = "PLEX-AUTH-PARSE-SENTINEL-5d13";
        let service =
            MockHttpService::start(vec![MockRoute::new(Method::POST, "/users/sign_in.json")
                .reply(MockResponse::json(serde_json::json!({ "user": sentinel })))])
            .await;
        let username = uuid::Uuid::new_v4().to_string();
        let password = uuid::Uuid::new_v4().to_string();
        let sign_in_url = format!("{}/users/sign_in.json", service.base_url());
        let error = PlexClient::authenticate_with_route_at(
            "https://plex.example.test",
            &username,
            &password,
            None,
            &sign_in_url,
        )
        .await
        .err()
        .expect("wrong-type sign-in body must fail");
        service.finish().await;

        assert_parse_error_omits(&error, sentinel, &[&username, &password]);
    }

    /// A Plex catalogue body whose `MediaContainer.size` is a string instead
    /// of the expected integer exercises the generic catalogue parser with a
    /// short and a large sentinel value.
    #[tokio::test]
    async fn catalogue_parse_failures_omit_response_content_from_diagnostics() {
        let cases = [
            "PLEX-CATALOGUE-SENTINEL-91ac".to_string(),
            format!("{}{}", "z".repeat(48 * 1024), "PLEX-LARGE-SENTINEL-4b60"),
        ];

        for payload in cases {
            let service = MockHttpService::start(vec![MockRoute::get("/library/sections").reply(
                MockResponse::json(serde_json::json!({
                    "MediaContainer": { "size": payload }
                })),
            )])
            .await;
            let client = PlexClient::new(&service.base_url(), "token").expect("client");
            let error = client
                .get::<PlexSectionsResponse>("library/sections")
                .await
                .expect_err("wrong-type catalogue body must fail");
            service.finish().await;

            assert_parse_error_omits(&error, &payload, &["token"]);
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
