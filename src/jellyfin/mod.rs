//! Jellyfin media server backend.
//!
//! Connects to a Jellyfin instance via its REST API, auto-discovers
//! music libraries, and (in later phases) syncs track metadata into
//! the unified Tributary data model.

pub mod api;
pub mod backend;
pub mod client;

// Re-export will be used once the UI wires up Jellyfin connections.
#[allow(unused_imports)]
pub use backend::JellyfinBackend;
