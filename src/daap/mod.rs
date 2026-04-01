//! DAAP (Digital Audio Access Protocol / iTunes Sharing) backend.
//!
//! Discovers DAAP servers via mDNS (`_daap._tcp.local.`), connects
//! using the proprietary DMAP binary protocol, and syncs track
//! metadata into the unified Tributary data model.
//!
//! # Protocol Overview
//!
//! DAAP is a stateful HTTP-based protocol that uses a proprietary binary
//! encoding called DMAP (tag-length-value). A session is established via
//! a strict handshake sequence:
//!
//! 1. `GET /server-info` — discover server capabilities
//! 2. `GET /login` — acquire a session-id
//! 3. `GET /update` — acquire a revision-number
//! 4. `GET /databases` — find the main library database ID
//! 5. `GET /databases/{id}/items` — fetch the track listing
//!
//! Audio streaming is via plain HTTP GET to:
//! `/databases/{db_id}/items/{song_id}.{ext}?session-id={session_id}`

pub mod backend;
pub mod client;
pub mod dmap;

pub use backend::DaapBackend;
