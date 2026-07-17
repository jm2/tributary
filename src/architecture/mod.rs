//! Core architecture module for Tributary.
//!
//! This module defines the unified data model and backend traits that allow
//! the UI to work transparently with local libraries (SQLite), Subsonic,
//! DAAP, Jellyfin, and any future media source.

pub mod backend;
pub mod error;
pub mod identity;
pub mod media;
pub mod models;

pub use backend::MediaBackend;
pub use identity::SourceId;
pub use media::{AdvertisedHttpRoute, RemoteMediaResolver, ResolvedHttpRequest};
