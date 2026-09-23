//! Library surface for the `tributary` crate.
//!
//! Tributary is primarily a binary (`src/main.rs`). This library exposes
//! the small subset of modules that benefit from being exercised in
//! isolation by integration tests and the `tributary-fuzz` harness: the
//! parsers that read untrusted bytes (DMAP, XSPF, Rhythmbox XML, Last.fm
//! auth responses, and the cast relay's URL and `Range` handling).
//!
//! It is intentionally minimal: rather than re-declaring the entire
//! application module tree, it declares only those parser modules and the
//! leaf modules they import. The networking backends and the GTK UI
//! continue to live solely in the binary.
//!
//! The clippy `warn`/`allow` block below mirrors `src/main.rs` so the
//! modules shared between the binary and this library lint identically
//! under `cargo clippy --all-targets`. `dead_code` is allowed because
//! this library deliberately exposes only a slice of each module, so
//! items used elsewhere in the binary appear unused here.
#![warn(clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::doc_markdown,            // Too many false positives on technical terms (GLib, SQLite, etc.)
    clippy::similar_names,           // Intentional: artist_resp/artists_resp, value/value2 are clear
    clippy::too_many_lines,          // GTK UI builders are inherently long
    clippy::redundant_clone,         // GTK GObject clones are required for move closures
    clippy::wildcard_imports,        // Standard pattern for gtk::prelude::*
    clippy::cast_possible_truncation,// Deliberate u64↔i64↔u32 conversions for DB/UI interop
    clippy::cast_sign_loss,          // Deliberate i32→u32 for DB model conversions
    clippy::cast_possible_wrap,      // Deliberate u32→i32 for SeaORM compatibility
    clippy::cast_precision_loss,     // u64→f64 for progress/duration display
    clippy::cast_lossless,           // Allow explicit `as` casts for clarity
    clippy::struct_field_names,      // track_number on Track is intentional
    clippy::module_name_repetitions, // Acceptable for backend::BackendError etc.
    clippy::items_after_statements,  // Common pattern in GTK signal handler setup
    clippy::significant_drop_tightening, // False positives with GTK widget builders
    clippy::redundant_closure_for_method_calls, // Often clearer with explicit closures
    clippy::option_if_let_else,      // if-let is often clearer than map_or
    clippy::match_same_arms,         // Intentional for exhaustive match documentation
    clippy::trivially_copy_pass_by_ref, // &bool/&u32 in trait impls
    clippy::needless_pass_by_value,  // GTK signal handlers require owned values
    clippy::unreadable_literal,      // Constants like 86400, 604800 are well-known
    clippy::map_unwrap_or,           // .map().unwrap_or() is often clearer than .map_or()
    clippy::uninlined_format_args,   // format!("{}", x) vs format!("{x}") — both fine
    clippy::unnecessary_literal_bound, // &str return types in trait impls
    clippy::missing_const_for_fn,    // Many fns could be const but aren't worth marking
    clippy::assigning_clones,        // clone_from() not always clearer
    clippy::if_not_else,             // !x.is_empty() is often the natural condition
    clippy::iter_over_hash_type,     // HashSet iteration order is fine for our use cases
    clippy::ref_option,              // Option<&T> vs &Option<T> — existing API signatures
    clippy::single_match_else,       // match with _ => {} is fine for clarity
    clippy::derive_partial_eq_without_eq, // Not all PartialEq types need Eq
)]
// Public-API lints that only fire because these modules are now *exported*
// through a library (a binary crate has no public API). They enforce
// published-API documentation/ergonomics, which is irrelevant for this
// internal fuzz/test surface — the same code lints clean inside the binary.
#![allow(
    clippy::missing_errors_doc,      // parse_dmap returns Result; callers handle errors directly
    clippy::missing_panics_doc,      // Internal surface; not a documented public API
    clippy::must_use_candidate,      // Accessors are used immediately, not at risk of being ignored
)]
#![allow(dead_code)] // This library exposes only a slice of each module.

/// Core architecture types: the error type plus the media-route leaves
/// that `http_security` imports.
pub mod architecture {
    pub mod backend;
    pub mod error;
    pub mod identity;
    pub mod media;
    pub mod models;

    pub use media::AdvertisedHttpRoute;
}

/// Cast relay request parsing (ticket extensions and `Range` headers).
pub mod audio {
    pub mod cast_http_parse;
}

/// DAAP (iTunes Sharing) protocol support.
///
/// Only the binary DMAP parser is exposed for fuzzing/testing; the
/// networking `backend` and `client` submodules live in the binary.
pub mod daap {
    pub mod dmap;
}

/// Database entity leaves imported by the playlist and Rhythmbox parsers.
pub mod db {
    pub mod entities {
        pub mod rhythmbox_import_receipt;
        pub mod track;
    }
}

pub mod http_body;
pub mod http_security;
// The Last.fm client's unit tests import this stub HTTP service.
#[cfg(test)]
mod http_test_service;

/// Last.fm client (strict auth-response validation) and its credential types.
pub mod lastfm {
    pub mod client;
    pub mod credentials;
}

/// Playlist and Rhythmbox import parsers.
pub mod local {
    pub mod playlist_io;
    pub mod rhythmbox_import;
}
