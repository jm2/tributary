//! Headless offline download/cache engine.
//!
//! This module implements the bounded download/cache engine that the
//! source-scoped offline media contract (`docs/offline-media.md`, P3.1)
//! requires. It is deliberately headless: the engine is a single-owner
//! supervisor that never runs on a GTK thread and never exposes a URL,
//! credential, token, or on-disk path to any caller. The GTK storage panel
//! consumes only the redacted [`engine::OfflineBoard`] projection.
//!
//! Sub-module boundaries mirror the contract:
//!
//! - [`storage`] is the only place that touches the filesystem. It enforces
//!   verify-before-publish with a same-directory atomic rename.
//! - [`quota`] is the only place that decides byte accounting and eviction
//!   order (oldest-first across sources, newest-first within a source).
//! - [`catalog`] is the read path that resolves a [`MediaKey`] to either the
//!   live endpoint or a committed, playable snapshot.
//! - [`engine`] orchestrates download jobs through the typed state machine
//!   from [`crate::architecture::offline`], behind a pluggable
//!   [`engine::TransferBackend`] seam. The real HTTP adapter (exact-origin
//!   proxy lane) and the durable SQLite job table land as separate slices;
//!   this module keeps job state in memory and fsyncs every committed file
//!   segment so a future durable journal can adopt the same offsets.

pub mod catalog;
pub mod engine;
pub mod quota;
pub mod storage;

// The re-exports below are intentional public surface for the offline
// engine's consumers (the GTK storage panel and future source-lifecycle
// wiring). They are not yet referenced outside this module, so silence the
// unused-import lint at the bin root while keeping the module surface
// unchanged — the same pattern `architecture/mod.rs` uses for the contracts.
#[allow(unused_imports)]
pub use catalog::OfflineCatalog;
#[allow(unused_imports)]
pub use engine::{
    OfflineBoard, OfflineEngine, OfflineRowLabels, OfflineRowSnapshot, TransferBackend,
    TransferOpen,
};
#[allow(unused_imports)]
pub use quota::{EvictionVictim, QuotaLedger};
#[allow(unused_imports)]
pub use storage::CacheStore;
