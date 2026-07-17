//! Low-level DAAP HTTP client — session handshake, request building,
//! and DMAP binary deserialization.
//!
//! DAAP is a stateful HTTP protocol. A session is established via a
//! strict 4-step handshake before the library can be read:
//!
//! 1. `GET /server-info`
//! 2. `GET /login` → session-id
//! 3. `GET /update` → revision-number
//! 4. `GET /databases` → database-id

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, USER_AGENT};
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

use super::dmap::{self, DmapNode, DmapValue};

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

/// Connection-establishment timeout for DAAP requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle read timeout.  Guards against a malicious or hung DAAP server
/// that accepts the connection but then stalls (or only trickles) the
/// response body — without capping the total time for a large-but-healthy
/// library transfer (the timeout resets after each successful read).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum response bodies for handshake/control, database-list, and item-list
/// requests, respectively.
const MAX_CONTROL_BODY_BYTES: u64 = 1024 * 1024;
const MAX_DATABASES_BODY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ITEMS_BODY_BYTES: u64 = 256 * 1024 * 1024;

/// End-to-end and body-phase deadlines for finite DAAP requests.
const CONTROL_RESPONSE_DEADLINE: Duration = Duration::from_secs(30);
const ITEMS_RESPONSE_DEADLINE: Duration = Duration::from_mins(2);
const PROBE_RESPONSE_DEADLINE: Duration = Duration::from_secs(5);

/// Client version advertised to the DAAP server.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// DAAP protocol version we advertise.
const DAAP_VERSION: &str = "3.12";

/// DMAP content type expected in responses.
const DMAP_CONTENT_TYPE: &str = "application/x-dmap-tagged";

/// The `meta` query parameter requesting the fields Tributary needs.
const TRACK_META: &str = "dmap.itemid,dmap.itemname,daap.songartist,daap.songalbum,\
daap.songtime,daap.songtracknumber,daap.songdiscnumber,daap.songgenre,\
daap.songyear,daap.songformat,daap.songbitrate,daap.songsamplerate,\
daap.songdatemodified";

/// Holds DAAP session state and a reusable `reqwest::Client`.
#[derive(Clone)]
pub struct DaapClient {
    base_url: Url,
    session_id: u32,
    revision: u32,
    database_id: u32,
    http: Client,
    advertised_route: Option<AdvertisedHttpRoute>,
}

impl DaapClient {
    /// Execute the full DAAP handshake and return a connected client.
    ///
    /// # Arguments
    /// * `server_url` — Base URL (e.g. `http://192.168.1.50:3689`)
    /// * `password` — Optional share password (DAAP uses password-only auth)
    pub async fn connect(server_url: &str, password: Option<&str>) -> BackendResult<Self> {
        Self::connect_with_route(server_url, password, None).await
    }

    /// Connect while preserving an mDNS-advertised direct route for this
    /// exact DAAP origin. The URL hostname remains authoritative for HTTP and
    /// TLS; only direct socket resolution uses the retained addresses.
    pub(crate) async fn connect_with_route(
        server_url: &str,
        password: Option<&str>,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> BackendResult<Self> {
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid DAAP server URL: {e}"),
            source: Some(Box::new(e)),
        })?;
        validate_base_url(&base_url).map_err(|message| BackendError::ConnectionFailed {
            message: message.replace("server URL", "DAAP server URL"),
            source: None,
        })?;

        let http = build_http_client(&base_url, advertised_route.as_ref())?;

        // ── Step A: Server Info ─────────────────────────────────────
        let server_info_url = format!("{}/server-info", base_url.as_str().trim_end_matches('/'));
        debug!(url = %redact_url_secrets(&server_info_url), "DAAP: requesting server-info");

        let resp = http
            .get(&server_info_url)
            .timeout(CONTROL_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|error| daap_request_error("DAAP server-info request failed", error))?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP server-info HTTP {}", resp.status()),
                source: None,
            });
        }

        let bytes = read_limited(resp, MAX_CONTROL_BODY_BYTES, CONTROL_RESPONSE_DEADLINE)
            .await
            .map_err(|error| daap_body_error("Failed to read server-info body", error))?;

        let nodes = dmap::parse_dmap(&bytes)?;
        // The top-level node is typically `msrv` (server-info container).
        let server_info_children = unwrap_container(&nodes, b"msrv")?;

        let status = dmap::find_u32(server_info_children, b"mstt").unwrap_or(0);
        if status != 200 {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP server-info returned status {status}"),
                source: None,
            });
        }

        let server_name = dmap::find_string(server_info_children, b"minm")
            .unwrap_or_else(|| "Unknown DAAP Server".to_string());
        info!(name = %server_name, "DAAP server-info OK");

        // ── Step B: Login ───────────────────────────────────────────
        let login_url = format!("{}/login", base_url.as_str().trim_end_matches('/'));
        debug!(url = %redact_url_secrets(&login_url), "DAAP: requesting login");

        let mut login_req = http.get(&login_url);
        if let Some(pw) = password {
            if !pw.is_empty() {
                login_req = login_req.basic_auth("", Some(pw));
            }
        }

        let resp = login_req
            .timeout(CONTROL_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|error| daap_request_error("DAAP login request failed", error))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(BackendError::AuthenticationFailed {
                message: "DAAP login failed — check password".to_string(),
            });
        }

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP login HTTP {}", resp.status()),
                source: None,
            });
        }

        let bytes = read_limited(resp, MAX_CONTROL_BODY_BYTES, CONTROL_RESPONSE_DEADLINE)
            .await
            .map_err(|error| daap_body_error("Failed to read login body", error))?;

        let nodes = dmap::parse_dmap(&bytes)?;
        let login_children = unwrap_container(&nodes, b"mlog")?;

        let session_id = dmap::find_u32(login_children, b"mlid").ok_or_else(|| {
            BackendError::AuthenticationFailed {
                message: "DAAP login response missing session-id (mlid)".to_string(),
            }
        })?;

        info!("DAAP login OK");

        // ── Step C: Update ──────────────────────────────────────────
        let update_url = format!(
            "{}/update?session-id={}&revision-number=1",
            base_url.as_str().trim_end_matches('/'),
            session_id
        );
        debug!(url = %redact_url_secrets(&update_url), "DAAP: requesting update");

        let resp = // lgtm[rs/cleartext-transmission] DAAP is a LAN-only protocol; plaintext HTTP is by design.
            http.get(&update_url)
                .timeout(CONTROL_RESPONSE_DEADLINE)
                .send()
                .await
                .map_err(|error| daap_request_error("DAAP update request failed", error))?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP update HTTP {}", resp.status()),
                source: None,
            });
        }

        let bytes = read_limited(resp, MAX_CONTROL_BODY_BYTES, CONTROL_RESPONSE_DEADLINE)
            .await
            .map_err(|error| daap_body_error("Failed to read update body", error))?;

        let nodes = dmap::parse_dmap(&bytes)?;
        let update_children = unwrap_container(&nodes, b"mupd")?;

        let revision = dmap::find_u32(update_children, b"musr").unwrap_or(1);
        info!(revision, "DAAP update OK");

        // ── Step D: Databases ───────────────────────────────────────
        let databases_url = format!(
            "{}/databases?session-id={}&revision-number={}",
            base_url.as_str().trim_end_matches('/'),
            session_id,
            revision
        );
        debug!(url = %redact_url_secrets(&databases_url), "DAAP: requesting databases");

        let resp = // lgtm[rs/cleartext-transmission] DAAP is a LAN-only protocol; plaintext HTTP is by design.
            http.get(&databases_url)
                .timeout(CONTROL_RESPONSE_DEADLINE)
                .send()
                .await
                .map_err(|error| daap_request_error("DAAP databases request failed", error))?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP databases HTTP {}", resp.status()),
                source: None,
            });
        }

        let bytes = read_limited(resp, MAX_DATABASES_BODY_BYTES, CONTROL_RESPONSE_DEADLINE)
            .await
            .map_err(|error| daap_body_error("Failed to read databases body", error))?;

        let nodes = dmap::parse_dmap(&bytes)?;
        let avdb_children = unwrap_container(&nodes, b"avdb")?;
        let mlcl_children = unwrap_nested_container(avdb_children, b"mlcl")?;
        let mlit_items = dmap::find_containers(mlcl_children, b"mlit");

        let first_db = mlit_items
            .first()
            .ok_or_else(|| BackendError::ConnectionFailed {
                message: "No DAAP databases found".to_string(),
                source: None,
            })?;

        let database_id =
            dmap::find_u32(first_db, b"miid").ok_or_else(|| BackendError::ConnectionFailed {
                message: "DAAP database entry missing item id (miid)".to_string(),
                source: None,
            })?;

        let db_name = dmap::find_string(first_db, b"minm")
            .unwrap_or_else(|| format!("Database {database_id}"));
        info!(database_id, name = %db_name, "DAAP database discovered");

        // ── Step E: Done ────────────────────────────────────────────
        info!(database_id, revision, "DAAP session established");

        Ok(Self {
            base_url,
            session_id,
            revision,
            database_id,
            http,
            advertised_route,
        })
    }

    /// Fetch all tracks from the DAAP library.
    ///
    /// Returns a list of `mlit` node sets, each representing one track.
    pub async fn fetch_tracks(&self) -> BackendResult<Vec<Vec<DmapNode>>> {
        let url = format!(
            "{}/databases/{}/items?session-id={}&revision-number={}&meta={}",
            self.base_url.as_str().trim_end_matches('/'),
            self.database_id,
            self.session_id,
            self.revision,
            TRACK_META,
        );
        debug!(url = %redact_url_secrets(&url), "DAAP: fetching tracks");

        let resp = // lgtm[rs/cleartext-transmission] DAAP is a LAN-only protocol; plaintext HTTP is by design.
            self.http
                .get(&url)
                .timeout(ITEMS_RESPONSE_DEADLINE)
                .send()
                .await
                .map_err(|error| daap_request_error("DAAP items request failed", error))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(BackendError::AuthenticationFailed {
                message: "DAAP session expired or unauthorized".to_string(),
            });
        }

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP items HTTP {}", resp.status()),
                source: None,
            });
        }

        let bytes = read_limited(resp, MAX_ITEMS_BODY_BYTES, ITEMS_RESPONSE_DEADLINE)
            .await
            .map_err(|error| daap_body_error("Failed to read items body", error))?;

        let nodes = dmap::parse_dmap(&bytes)?;

        // Top-level is `adbs` (database songs response).
        let adbs_children = unwrap_container(&nodes, b"adbs")?;
        let mlcl_children = unwrap_nested_container(adbs_children, b"mlcl")?;

        let mlit_items = dmap::find_containers(mlcl_children, b"mlit");

        info!(count = mlit_items.len(), "DAAP: tracks received");

        // Convert from borrowed slices to owned Vecs.
        Ok(mlit_items.into_iter().map(|s| s.to_vec()).collect())
    }

    /// Issue a bounded server-info request to verify the active server is
    /// still responsive.
    pub async fn ping(&self) -> BackendResult<()> {
        let url = format!(
            "{}/server-info",
            self.base_url.as_str().trim_end_matches('/')
        );
        let resp = self
            .http
            .get(&url)
            .timeout(CONTROL_RESPONSE_DEADLINE)
            .send()
            .await
            .map_err(|error| daap_request_error("DAAP ping failed", error))?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP ping HTTP {}", resp.status()),
                source: None,
            });
        }

        read_limited(resp, MAX_CONTROL_BODY_BYTES, CONTROL_RESPONSE_DEADLINE)
            .await
            .map_err(|error| daap_body_error("Failed to read DAAP ping body", error))?;
        Ok(())
    }

    /// Construct a credential-isolated cover-art request for a track.
    ///
    /// DAAP serves artwork at `/databases/{db}/items/{id}/extra_data/artwork`.
    /// `mw` and `mh` remain public request-shaping fields. The bearer
    /// `session-id` stays isolated until Tributary performs the request.
    pub(crate) fn cover_art_request(&self, song_id: u32) -> BackendResult<ResolvedHttpRequest> {
        let mut endpoint = self.base_url.clone();
        let database_id = self.database_id.to_string();
        let song_id = song_id.to_string();
        append_base_path_segments(
            &mut endpoint,
            [
                "databases",
                database_id.as_str(),
                "items",
                song_id.as_str(),
                "extra_data",
                "artwork",
            ],
        );
        endpoint
            .query_pairs_mut()
            .append_pair("mw", "300")
            .append_pair("mh", "300");
        self.resolved_media_request(endpoint)
    }

    /// Construct a credential-isolated streaming request for a track.
    ///
    /// The untrusted format is encoded as part of one path segment, and the
    /// bearer `session-id` stays isolated until the app-owned fetch boundary.
    pub(crate) fn stream_request(
        &self,
        song_id: u32,
        format: &str,
    ) -> BackendResult<ResolvedHttpRequest> {
        let mut endpoint = self.base_url.clone();
        let item = format!("{song_id}.{format}");
        let database_id = self.database_id.to_string();
        append_base_path_segments(
            &mut endpoint,
            ["databases", database_id.as_str(), "items", item.as_str()],
        );
        self.resolved_media_request(endpoint)
    }

    fn resolved_media_request(&self, endpoint: Url) -> BackendResult<ResolvedHttpRequest> {
        let mut request = ResolvedHttpRequest::new(endpoint)?
            .with_private_query_pair("session-id", self.session_id.to_string())?;
        for (name, value) in daap_required_headers() {
            request = request.with_required_header(name.clone(), value.clone())?;
        }
        if let Some(route) = &self.advertised_route {
            request = request.with_advertised_route(route.clone())?;
        }
        Ok(request)
    }

    /// Send a best-effort logout request to end the DAAP session.
    pub async fn logout(&self) {
        let url = format!(
            "{}/logout?session-id={}",
            self.base_url.as_str().trim_end_matches('/'),
            self.session_id
        );
        match self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            // lgtm[rs/cleartext-transmission] DAAP uses plaintext HTTP by design.
            Ok(_) => info!("DAAP logout OK"),
            Err(error) => {
                let error = strip_request_url(error);
                warn!(%error, "DAAP logout failed (best-effort)");
            }
        }
    }

    /// Probe a DAAP server's `/server-info` to check whether it requires
    /// a password.
    ///
    /// Returns `Some(false)` for open shares (msau == 0 or absent),
    /// `Some(true)` for password-protected shares, or `None` on error.
    pub async fn probe_requires_password(server_url: &str) -> Option<bool> {
        Self::probe_requires_password_with_route(server_url, None).await
    }

    /// Probe through the exact mDNS-advertised route, when one is available.
    pub(crate) async fn probe_requires_password_with_route(
        server_url: &str,
        advertised_route: Option<AdvertisedHttpRoute>,
    ) -> Option<bool> {
        let base_url = Url::parse(server_url).ok()?;
        validate_base_url(&base_url).ok()?;
        let http = build_http_client(&base_url, advertised_route.as_ref()).ok()?;
        let url = format!("{}/server-info", base_url.as_str().trim_end_matches('/'));
        // Cap the probe at 5s — a malicious or hung DAAP server should
        // not be able to stall the discovery flow forever. The shared
        // client already sets connect/read timeouts; this tighter total
        // per-request timeout keeps the discovery probe snappy.
        let resp = http
            .get(&url)
            .timeout(PROBE_RESPONSE_DEADLINE)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = read_limited(resp, MAX_CONTROL_BODY_BYTES, PROBE_RESPONSE_DEADLINE)
            .await
            .ok()?;
        let nodes = dmap::parse_dmap(&bytes).ok()?;
        let children = match dmap::find_node(&nodes, b"msrv") {
            Some(node) => match &node.data {
                dmap::DmapValue::Container(c) => c.as_slice(),
                _ => return None,
            },
            None => return None,
        };
        // msau: 0 = no auth, 1 = basic, 2 = digest
        let auth_method = dmap::find_u8(children, b"msau").unwrap_or(0);
        Some(auth_method != 0)
    }

    // ── Accessors ───────────────────────────────────────────────────

    /// The base URL of the DAAP server.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// The active session ID.
    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// The database ID.
    #[allow(dead_code)]
    pub fn database_id(&self) -> u32 {
        self.database_id
    }
}

// ── Internal helpers ────────────────────────────────────────────────────

/// Build a `reqwest::Client` with DAAP-required default headers.
fn build_http_client(
    origin: &Url,
    advertised_route: Option<&AdvertisedHttpRoute>,
) -> BackendResult<Client> {
    let headers = daap_required_headers().clone();

    let builder = authenticated_client_builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT);
    apply_advertised_http_route(builder, origin, advertised_route)
        .map_err(|message| BackendError::ConnectionFailed {
            message: message.to_string(),
            source: None,
        })?
        .build()
        .map_err(|error| daap_request_error("Failed to build DAAP HTTP client", error))
}

/// Headers required by DAAP control and protected media requests alike.
///
/// Keeping one map for both paths prevents proxied playback from drifting
/// away from the protocol identity used during the session handshake.
fn daap_required_headers() -> &'static HeaderMap {
    static HEADERS: OnceLock<HeaderMap> = OnceLock::new();
    HEADERS.get_or_init(|| {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(DMAP_CONTENT_TYPE));
        headers.insert(
            HeaderName::from_static("client-daap-version"),
            HeaderValue::from_static(DAAP_VERSION),
        );
        headers.insert(
            HeaderName::from_static("client-daap-access-index"),
            HeaderValue::from_static("2"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("{CLIENT_NAME}/{CLIENT_VERSION}"))
                .expect("package version forms a valid DAAP user agent"),
        );
        headers
    })
}

fn daap_request_error(context: &str, error: reqwest::Error) -> BackendError {
    let error = strip_request_url(error);
    BackendError::ConnectionFailed {
        message: format!("{context}: {error}"),
        source: Some(Box::new(error)),
    }
}

fn daap_body_error(context: &str, error: ResponseBodyError) -> BackendError {
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
        error => BackendError::ConnectionFailed {
            message: format!("{context}: {error}"),
            source: Some(Box::new(error)),
        },
    }
}

/// Unwrap the first top-level container node with the given tag,
/// returning a reference to its children.
fn unwrap_container<'a>(nodes: &'a [DmapNode], tag: &[u8; 4]) -> BackendResult<&'a [DmapNode]> {
    let node = dmap::find_node(nodes, tag).ok_or_else(|| BackendError::ParseError {
        message: format!(
            "Expected DMAP container '{}' not found",
            String::from_utf8_lossy(tag)
        ),
        source: None,
    })?;

    match &node.data {
        DmapValue::Container(children) => Ok(children.as_slice()),
        _ => Err(BackendError::ParseError {
            message: format!(
                "DMAP node '{}' is not a container",
                String::from_utf8_lossy(tag)
            ),
            source: None,
        }),
    }
}

/// Find a nested container within an already-unwrapped parent.
fn unwrap_nested_container<'a>(
    parent_children: &'a [DmapNode],
    tag: &[u8; 4],
) -> BackendResult<&'a [DmapNode]> {
    unwrap_container(parent_children, tag)
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use super::*;

    fn client(base_url: &str) -> DaapClient {
        let base_url = Url::parse(base_url).expect("DAAP base URL");
        DaapClient {
            base_url: base_url.clone(),
            session_id: 42,
            revision: 2,
            database_id: 1,
            http: build_http_client(&base_url, None).expect("DAAP client"),
            advertised_route: None,
        }
    }

    #[test]
    fn maps_response_body_deadline_to_timeout() {
        let error = daap_body_error(
            "body",
            ResponseBodyError::DeadlineExceeded {
                deadline: Duration::from_secs(7),
            },
        );

        assert!(matches!(error, BackendError::Timeout { duration_secs: 7 }));
    }

    #[test]
    fn daap_base_url_rejects_embedded_credentials_and_non_http_schemes() {
        let safe = Url::parse("http://music.test:3689/share").expect("safe URL");
        assert!(validate_base_url(&safe).is_ok());

        for unsafe_url in [
            "http://user:secret@music.test:3689",
            "ftp://music.test/share",
            "music.test:3689",
        ] {
            let url = Url::parse(unsafe_url).expect("syntactically valid unsafe URL");
            let error = validate_base_url(&url).expect_err("unsafe URL must be rejected");
            let rendered = error.to_string();
            assert!(!rendered.contains("secret"));
            assert!(!rendered.contains(unsafe_url));
        }
    }

    #[test]
    fn media_requests_preserve_root_and_reverse_proxy_base_paths_exactly() {
        for (base, prefix) in [
            ("http://music.test:3689", ""),
            ("http://music.test:3689/share", "/share"),
            ("http://music.test:3689/share/", "/share"),
            ("http://music.test:3689/tenant%2Fmusic/", "/tenant%2Fmusic"),
        ] {
            let client = client(base);
            let stream = client.stream_request(7, "flac").expect("stream request");
            assert_eq!(
                stream.endpoint().as_str(),
                format!("http://music.test:3689{prefix}/databases/1/items/7.flac"),
                "base URL: {base}"
            );
            assert_eq!(stream.required_headers(), daap_required_headers());

            let artwork = client.cover_art_request(7).expect("artwork request");
            assert_eq!(
                artwork.endpoint().as_str(),
                format!(
                    "http://music.test:3689{prefix}/databases/1/items/7/extra_data/artwork?mw=300&mh=300"
                ),
                "base URL: {base}"
            );
            assert_eq!(artwork.required_headers(), daap_required_headers());

            let malicious = client
                .stream_request(7, "flac/../../logout")
                .expect("untrusted format is one segment");
            assert_eq!(
                malicious.endpoint().as_str(),
                format!(
                    "http://music.test:3689{prefix}/databases/1/items/7.flac%2F..%2F..%2Flogout"
                ),
                "base URL: {base}"
            );
            assert!(!malicious.endpoint().as_str().contains("%252F"));
        }
    }

    #[test]
    fn required_header_map_has_the_exact_daap_protocol_identity() {
        let headers = daap_required_headers();
        assert_eq!(headers.len(), 4);
        assert_eq!(
            headers.get(ACCEPT),
            Some(&HeaderValue::from_static("application/x-dmap-tagged"))
        );
        assert_eq!(
            headers.get("client-daap-version"),
            Some(&HeaderValue::from_static("3.12"))
        );
        assert_eq!(
            headers.get("client-daap-access-index"),
            Some(&HeaderValue::from_static("2"))
        );
        assert_eq!(
            headers.get(USER_AGENT),
            Some(
                &HeaderValue::from_str(&format!("Tributary/{CLIENT_VERSION}"))
                    .expect("valid user agent")
            )
        );
    }

    #[test]
    fn media_requests_keep_session_id_private_and_preserve_advertised_hostname() {
        let base_url = Url::parse("http://mini.local:3689").expect("DAAP origin");
        let route = AdvertisedHttpRoute::new(
            &base_url,
            [SocketAddr::from((Ipv4Addr::new(192, 0, 2, 40), 9999))],
        )
        .expect("advertised route");
        let client = DaapClient {
            base_url: base_url.clone(),
            session_id: 42,
            revision: 2,
            database_id: 1,
            http: build_http_client(&base_url, Some(&route)).expect("routed client"),
            advertised_route: Some(route.clone()),
        };

        let stream = client
            .stream_request(7, "flac/../../logout")
            .expect("stream request");
        assert_eq!(stream.endpoint().host_str(), Some("mini.local"));
        assert_eq!(stream.endpoint().port(), Some(3689));
        assert!(stream.endpoint().query().is_none());
        assert!(!stream.endpoint().path().contains("/logout"));
        assert_eq!(stream.advertised_route(), Some(&route));
        assert_eq!(
            stream.private_query_pairs(),
            &[("session-id".to_string(), "42".to_string())]
        );

        let artwork = client.cover_art_request(7).expect("artwork request");
        assert_eq!(artwork.endpoint().host_str(), Some("mini.local"));
        assert_eq!(
            artwork.endpoint().query_pairs().collect::<Vec<_>>(),
            [("mw".into(), "300".into()), ("mh".into(), "300".into())]
        );
        assert_eq!(artwork.advertised_route(), Some(&route));
        assert_eq!(
            artwork.private_query_pairs(),
            &[("session-id".to_string(), "42".to_string())]
        );
    }
}
