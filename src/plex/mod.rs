//! Plex media server backend.
//!
//! Connects to a Plex instance via its REST API, auto-discovers
//! music libraries, and publishes their track metadata through the
//! unified Tributary data model. Stream and artwork requests are resolved
//! from backend-native rating keys only when the live source is used.

pub mod api;
pub mod backend;
pub mod client;

// Public backend type used by the source lifecycle and connection flows.
#[allow(unused_imports)]
pub use backend::PlexBackend;
