//! The offline catalogue: the read path from `MediaKey` to committed
//! snapshots.
//!
//! The catalogue is read-only from every caller's perspective: it never
//! mints `SourceId` or `TrackId` values, never mutates a committed row,
//! and only answers whether a committed snapshot is available, returns its
//! immutable record, or returns "live endpoint is the source of truth"
//! (`docs/offline-media.md`, "Reconciliation"). The engine owns mutation
//! through `pub(crate)` methods; the durable state behind the index lives
//! in the per-job journals under [`super::storage`].

use std::collections::HashMap;

use super::quota::EvictionCandidate;
use super::{CommittedSnapshot, OfflineCatalogueEntry, OperationalLicence};
use crate::architecture::{MediaKey, SourceId};

#[derive(Clone, Debug)]
struct CatalogRow {
    snapshot: CommittedSnapshot,
    revoked: bool,
}

/// The committed-row index: the durable cache table of this slice, rebuilt
/// from the journals on restart. A SQLite-backed table replaces it when
/// that slice lands; the read path below is its contract.
#[derive(Clone, Debug, Default)]
pub struct CatalogueIndex {
    rows: HashMap<MediaKey, CatalogRow>,
}

impl CatalogueIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve the offline catalogue read for one `(SourceId, TrackId)`
    /// pair. `LiveOnly` means no cached snapshot exists and the live
    /// endpoint is the source of truth.
    pub fn resolve(&self, media_key: &MediaKey) -> OfflineCatalogueEntry {
        match self.rows.get(media_key) {
            None => OfflineCatalogueEntry::LiveOnly,
            Some(row) if row.revoked => OfflineCatalogueEntry::Revoked(row.snapshot.clone()),
            Some(row) => OfflineCatalogueEntry::Cached(row.snapshot.clone()),
        }
    }

    /// Every committed snapshot recorded for one source (revoked rows
    /// included — retirement preserves the row, it only bars playback).
    pub fn source_snapshots(&self, source_id: SourceId) -> Vec<CommittedSnapshot> {
        let mut snapshots: Vec<CommittedSnapshot> = self
            .rows
            .iter()
            .filter(|(key, _)| key.source_id == source_id)
            .map(|(_, row)| row.snapshot.clone())
            .collect();
        snapshots.sort_by(|a, b| {
            a.committed_at_epoch_secs
                .cmp(&b.committed_at_epoch_secs)
                .then_with(|| a.media_key.track_id.cmp(&b.media_key.track_id))
        });
        snapshots
    }

    /// Total committed bytes across every row, revoked rows included:
    /// retired rows preserve their files, so they still occupy the quota.
    pub fn total_committed_bytes(&self) -> u64 {
        self.rows.values().map(|row| row.snapshot.byte_size).sum()
    }

    /// Total committed bytes for one source.
    pub fn source_committed_bytes(&self, source_id: SourceId) -> u64 {
        self.rows
            .iter()
            .filter(|(key, _)| key.source_id == source_id)
            .map(|(_, row)| row.snapshot.byte_size)
            .sum()
    }

    /// Number of committed rows (revoked included).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the index records no committed rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Whether one media key's row is licence-revoked.
    pub(crate) fn is_revoked(&self, media_key: &MediaKey) -> bool {
        self.rows.get(media_key).is_some_and(|row| row.revoked)
    }

    /// Every playable (non-revoked) row offered to the eviction planner.
    pub(crate) fn eviction_candidates(&self) -> Vec<(MediaKey, EvictionCandidate)> {
        self.rows
            .iter()
            .filter(|(_, row)| !row.revoked)
            .map(|(key, row)| {
                (
                    key.clone(),
                    EvictionCandidate {
                        source_id: key.source_id,
                        byte_size: row.snapshot.byte_size,
                        committed_at_epoch_secs: row.snapshot.committed_at_epoch_secs,
                    },
                )
            })
            .collect()
    }

    /// Insert (or replace) the committed row for one media key. A fresh
    /// commit clears revocation: the new snapshot committed under a
    /// SourceDeclared licence.
    pub(crate) fn insert(&mut self, snapshot: CommittedSnapshot) {
        self.rows.insert(
            snapshot.media_key.clone(),
            CatalogRow {
                snapshot,
                revoked: false,
            },
        );
    }

    /// Remove one row entirely (eviction, supersession).
    pub(crate) fn remove(&mut self, media_key: &MediaKey) {
        self.rows.remove(media_key);
    }

    /// Retire one row without removing it: the file is preserved, the row
    /// is no longer a playable offline row.
    pub(crate) fn revoke(&mut self, media_key: &MediaKey) {
        if let Some(row) = self.rows.get_mut(media_key) {
            row.revoked = true;
        }
    }

    /// The licence label recorded at commit, if a row exists.
    pub(crate) fn licence_of(&self, media_key: &MediaKey) -> Option<OperationalLicence> {
        self.rows
            .get(media_key)
            .map(|row| row.snapshot.licence_label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::{DigestProvenance, TrackId};

    fn media_key(track: &str) -> MediaKey {
        MediaKey::new(SourceId::local(), TrackId::remote(track).expect("track id"))
    }

    fn snapshot(track: &str, bytes: u64, committed_at: u64) -> CommittedSnapshot {
        CommittedSnapshot {
            media_key: media_key(track),
            capability_epoch: 1,
            byte_size: bytes,
            sha256_hex: "ab".repeat(32),
            digest_provenance: DigestProvenance::Advertised,
            cache_path: "a/b/media.bin".into(),
            licence_label: OperationalLicence::SourceDeclared,
            committed_at_epoch_secs: committed_at,
        }
    }

    #[test]
    fn resolution_is_live_only_without_a_row() {
        let index = CatalogueIndex::new();
        assert_eq!(
            index.resolve(&media_key("missing")),
            OfflineCatalogueEntry::LiveOnly
        );
    }

    #[test]
    fn resolution_reports_cached_rows_then_revoked_rows() {
        let mut index = CatalogueIndex::new();
        let key = media_key("track-1");
        index.insert(snapshot("track-1", 100, 5));

        let OfflineCatalogueEntry::Cached(snapshot) = index.resolve(&key) else {
            panic!("committed row must resolve as Cached");
        };
        assert_eq!(snapshot.byte_size, 100);

        index.revoke(&key);
        let OfflineCatalogueEntry::Revoked(revoked) = index.resolve(&key) else {
            panic!("retired row must resolve as Revoked");
        };
        assert_eq!(revoked.byte_size, 100, "revocation preserves the row");
    }

    #[test]
    fn totals_count_revoked_rows_because_their_files_are_preserved() {
        let mut index = CatalogueIndex::new();
        index.insert(snapshot("a", 100, 1));
        index.insert(snapshot("b", 50, 2));
        assert_eq!(index.total_committed_bytes(), 150);
        index.revoke(&media_key("a"));
        assert_eq!(index.total_committed_bytes(), 150);
        assert_eq!(index.source_committed_bytes(SourceId::local()), 150);
    }

    #[test]
    fn eviction_candidates_exclude_revoked_rows() {
        let mut index = CatalogueIndex::new();
        index.insert(snapshot("playable", 100, 1));
        index.insert(snapshot("retired", 200, 2));
        index.revoke(&media_key("retired"));

        let candidates = index.eviction_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].1.byte_size, 100);
    }

    #[test]
    fn a_fresh_commit_clears_revocation() {
        let mut index = CatalogueIndex::new();
        let key = media_key("track-1");
        index.insert(snapshot("track-1", 100, 1));
        index.revoke(&key);
        assert!(index.is_revoked(&key));

        index.insert(snapshot("track-1", 120, 9));
        assert!(!index.is_revoked(&key));
        let OfflineCatalogueEntry::Cached(snapshot) = index.resolve(&key) else {
            panic!("fresh commit must be playable");
        };
        assert_eq!(snapshot.byte_size, 120);
    }
}
