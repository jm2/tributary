//! Core architecture module for Tributary.
//!
//! This module defines the unified data model and backend traits that allow
//! the UI to work transparently with local libraries (SQLite), Subsonic,
//! DAAP, Jellyfin, and any future media source.
#![allow(unused_imports)]

pub mod backend;
pub mod error;
pub mod models;

pub use backend::MediaBackend;
pub use error::BackendError;
pub use models::*;
