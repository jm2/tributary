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

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
use reqwest::Client;
use tracing::{debug, info, warn};
use url::Url;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;

use super::dmap::{self, DmapNode, DmapValue};

/// Client identifier sent with every request.
const CLIENT_NAME: &str = "Tributary";

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
pub struct DaapClient {
    base_url: Url,
    session_id: u32,
    revision: u32,
    database_id: u32,
    http: Client,
}

impl DaapClient {
    /// Execute the full DAAP handshake and return a connected client.
    ///
    /// # Arguments
    /// * `server_url` — Base URL (e.g. `http://192.168.1.50:3689`)
    /// * `password` — Optional share password (DAAP uses password-only auth)
    pub async fn connect(server_url: &str, password: Option<&str>) -> BackendResult<Self> {
        let base_url = Url::parse(server_url).map_err(|e| BackendError::ConnectionFailed {
            message: format!("Invalid DAAP server URL: {e}"),
            source: Some(Box::new(e)),
        })?;

        let http = build_http_client()?;

        // ── Step A: Server Info ─────────────────────────────────────
        let server_info_url = format!("{}/server-info", base_url.as_str().trim_end_matches('/'));
        debug!(url = %server_info_url, "DAAP: requesting server-info");

        let resp = http.get(&server_info_url).send().await.map_err(|e| {
            BackendError::ConnectionFailed {
                message: format!("DAAP server-info request failed: {e}"),
                source: Some(Box::new(e)),
            }
        })?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP server-info HTTP {}", resp.status()),
                source: None,
            });
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to read server-info body: {e}"),
                source: Some(Box::new(e)),
            })?;

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
        debug!(url = %login_url, "DAAP: requesting login");

        let mut login_req = http.get(&login_url);
        if let Some(pw) = password {
            if !pw.is_empty() {
                login_req = login_req.basic_auth("", Some(pw));
            }
        }

        let resp = login_req
            .send()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("DAAP login request failed: {e}"),
                source: Some(Box::new(e)),
            })?;

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

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to read login body: {e}"),
                source: Some(Box::new(e)),
            })?;

        let nodes = dmap::parse_dmap(&bytes)?;
        let login_children = unwrap_container(&nodes, b"mlog")?;

        let session_id = dmap::find_u32(login_children, b"mlid").ok_or_else(|| {
            BackendError::AuthenticationFailed {
                message: "DAAP login response missing session-id (mlid)".to_string(),
            }
        })?;

        info!(session_id, "DAAP login OK");

        // ── Step C: Update ──────────────────────────────────────────
        let update_url = format!(
            "{}/update?session-id={}&revision-number=1",
            base_url.as_str().trim_end_matches('/'),
            session_id
        );
        debug!(url = %update_url, "DAAP: requesting update");

        let resp = // lgtm[rs/cleartext-transmission] DAAP is a LAN-only protocol; plaintext HTTP is by design.
            http.get(&update_url)
                .send()
                .await
                .map_err(|e| BackendError::ConnectionFailed {
                    message: format!("DAAP update request failed: {e}"),
                    source: Some(Box::new(e)),
                })?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP update HTTP {}", resp.status()),
                source: None,
            });
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to read update body: {e}"),
                source: Some(Box::new(e)),
            })?;

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
        debug!(url = %databases_url, "DAAP: requesting databases");

        let resp = // lgtm[rs/cleartext-transmission] DAAP is a LAN-only protocol; plaintext HTTP is by design.
            http.get(&databases_url)
                .send()
                .await
                .map_err(|e| BackendError::ConnectionFailed {
                    message: format!("DAAP databases request failed: {e}"),
                    source: Some(Box::new(e)),
                })?;

        if !resp.status().is_success() {
            return Err(BackendError::ConnectionFailed {
                message: format!("DAAP databases HTTP {}", resp.status()),
                source: None,
            });
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to read databases body: {e}"),
                source: Some(Box::new(e)),
            })?;

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
        info!(
            session_id,
            database_id, revision, "DAAP session established"
        );

        Ok(Self {
            base_url,
            session_id,
            revision,
            database_id,
            http,
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
        debug!(url = %url, "DAAP: fetching tracks");

        let resp = // lgtm[rs/cleartext-transmission] DAAP is a LAN-only protocol; plaintext HTTP is by design.
            self.http
                .get(&url)
                .send()
                .await
                .map_err(|e| BackendError::ConnectionFailed {
                    message: format!("DAAP items request failed: {e}"),
                    source: Some(Box::new(e)),
                })?;

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

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BackendError::ConnectionFailed {
                message: format!("Failed to read items body: {e}"),
                source: Some(Box::new(e)),
            })?;

        let nodes = dmap::parse_dmap(&bytes)?;

        // Top-level is `adbs` (database songs response).
        let adbs_children = unwrap_container(&nodes, b"adbs")?;
        let mlcl_children = unwrap_nested_container(adbs_children, b"mlcl")?;

        let mlit_items = dmap::find_containers(mlcl_children, b"mlit");

        info!(count = mlit_items.len(), "DAAP: tracks received");

        // Convert from borrowed slices to owned Vecs.
        Ok(mlit_items.into_iter().map(|s| s.to_vec()).collect())
    }

    /// Construct a cover art URL for a track.
    ///
    /// DAAP serves artwork at `/databases/{db}/items/{id}/extra_data/artwork`.
    /// The session-id is included as a query parameter.  `mw` and `mh`
    /// request a maximum width/height so the server can down-scale.
    pub fn cover_art_url(&self, song_id: u32) -> Url {
        let path = format!(
            "/databases/{}/items/{}/extra_data/artwork",
            self.database_id, song_id
        );
        let mut url = self.base_url.clone();
        url.set_path(&path);
        url.query_pairs_mut()
            .append_pair("session-id", &self.session_id.to_string())
            .append_pair("mw", "300")
            .append_pair("mh", "300");
        url
    }

    /// Construct a streaming URL for a track.
    ///
    /// The URL includes the session-id as a query parameter so
    /// GStreamer's `playbin3` can fetch audio without custom headers.
    pub fn stream_url(&self, song_id: u32, format: &str) -> Url {
        let path = format!(
            "/databases/{}/items/{}.{}",
            self.database_id, song_id, format
        );
        let mut url = self.base_url.clone();
        url.set_path(&path);
        url.query_pairs_mut()
            .append_pair("session-id", &self.session_id.to_string());
        url
    }

    /// Send a best-effort logout request to end the DAAP session.
    pub async fn logout(&self) {
        let url = format!(
            "{}/logout?session-id={}",
            self.base_url.as_str().trim_end_matches('/'),
            self.session_id
        );
        match self.http.get(&url).send().await { // lgtm[rs/cleartext-transmission] DAAP uses plaintext HTTP by design.
            Ok(_) => info!("DAAP logout OK"),
            Err(e) => warn!(error = %e, "DAAP logout failed (best-effort)"),
        }
    }

    /// Probe a DAAP server's `/server-info` to check whether it requires
    /// a password.
    ///
    /// Returns `Some(false)` for open shares (msau == 0 or absent),
    /// `Some(true)` for password-protected shares, or `None` on error.
    pub async fn probe_requires_password(server_url: &str) -> Option<bool> {
        let http = build_http_client().ok()?;
        let url = format!("{}/server-info", server_url.trim_end_matches('/'));
        let resp = http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
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

    /// Clone the inner HTTP client (cheap — `reqwest::Client` is `Arc`-based).
    pub fn http_clone(&self) -> Client {
        self.http.clone()
    }
}

// ── Internal helpers ────────────────────────────────────────────────────

/// Build a `reqwest::Client` with DAAP-required default headers.
fn build_http_client() -> BackendResult<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static(DMAP_CONTENT_TYPE));
    headers.insert(
        "Client-DAAP-Version",
        HeaderValue::from_static(DAAP_VERSION),
    );
    headers.insert("Client-DAAP-Access-Index", HeaderValue::from_static("2"));

    Client::builder()
        .user_agent(format!("{CLIENT_NAME}/{CLIENT_VERSION}"))
        .default_headers(headers)
        .build()
        .map_err(|e| BackendError::ConnectionFailed {
            message: format!("Failed to build DAAP HTTP client: {e}"),
            source: Some(Box::new(e)),
        })
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
