//! Read-only offline catalogue resolution.
//!
//! The catalogue answers one question for one `(SourceId, TrackId)` pair: is
//! there a committed snapshot (`Cached`), a committed but licence-revoked
//! row (`Revoked`), or only the live endpoint (`LiveOnly`)? It never mints
//! identifiers and never mutates state; the engine is the sole writer.

use std::collections::HashMap;

use crate::architecture::identity::{MediaKey, SourceId};
use crate::architecture::offline::{CommittedSnapshot, OfflineCatalogueEntry};

/// The committed-snapshot index.
///
/// Keyed by `MediaKey`; one committed snapshot per key at a time. A refresh
/// that commits a newer snapshot replaces the mapping after the new bytes are
/// verified and published — the predecessor is returned to the engine for
/// unlink, so refresh siblings are bounded, never accumulated.
#[derive(Default)]
pub struct OfflineCatalog {
    committed: HashMap<MediaKey, CommittedSnapshot>,
    revoked: HashMap<MediaKey, CommittedSnapshot>,
}

impl OfflineCatalog {
    /// Resolve the offline read for one media key.
    pub fn resolve(&self, key: &MediaKey) -> OfflineCatalogueEntry {
        if let Some(snapshot) = self.revoked.get(key) {
            return OfflineCatalogueEntry::Revoked(snapshot.clone());
        }
        match self.committed.get(key) {
            Some(snapshot) => OfflineCatalogueEntry::Cached(snapshot.clone()),
            None => OfflineCatalogueEntry::LiveOnly,
        }
    }

    /// Record a freshly committed snapshot as playable. Any predecessor
    /// for the same key — a committed snapshot or a licence-revoked row —
    /// is returned so the engine can settle its bytes and quota charge;
    /// the committed mapping itself is replaced atomically.
    pub fn publish(&mut self, snapshot: CommittedSnapshot) -> Option<CommittedSnapshot> {
        let predecessor = self
            .revoked
            .remove(&snapshot.media_key)
            .or_else(|| self.committed.remove(&snapshot.media_key));
        self.committed.insert(snapshot.media_key.clone(), snapshot);
        predecessor
    }

    /// Retire a row whose licence was revoked. The file is preserved; the
    /// row simply stops being playable offline.
    ///
    /// Returns `false` when no committed row exists for the key.
    pub fn retire(&mut self, key: &MediaKey) -> bool {
        if let Some(snapshot) = self.committed.remove(key) {
            self.revoked.insert(key.clone(), snapshot);
            true
        } else {
            false
        }
    }

    /// Remove a row entirely (user-driven cache deletion or eviction).
    /// Returns the removed snapshot so the engine can unlink its file and
    /// release its quota charge.
    pub fn remove(&mut self, key: &MediaKey) -> Option<CommittedSnapshot> {
        self.revoked
            .remove(key)
            .or_else(|| self.committed.remove(key))
    }

    /// All committed snapshots for one source, newest-first.
    #[must_use]
    pub fn snapshots_for_source(&self, source: SourceId) -> Vec<CommittedSnapshot> {
        let mut rows: Vec<CommittedSnapshot> = self
            .committed
            .iter()
            .filter(|(key, _)| key.source_id == source)
            .map(|(_, snapshot)| snapshot.clone())
            .collect();
        rows.sort_by(|left, right| {
            right
                .committed_at_epoch_secs
                .cmp(&left.committed_at_epoch_secs)
        });
        rows
    }

    /// Every committed and revoked snapshot currently mapped.
    #[must_use]
    pub fn all_snapshots(&self) -> Vec<CommittedSnapshot> {
        let mut rows: Vec<CommittedSnapshot> = self
            .committed
            .values()
            .chain(self.revoked.values())
            .cloned()
            .collect();
        rows.sort_by(|left, right| {
            left.media_key
                .track_id
                .as_str()
                .cmp(right.media_key.track_id.as_str())
        });
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::offline::{DigestProvenance, OperationalLicence};
    use crate::architecture::{SourceId, TrackId};

    fn snapshot(track: &str, at: u64) -> CommittedSnapshot {
        CommittedSnapshot {
            media_key: MediaKey::new(SourceId::local(), TrackId::new(track).unwrap()),
            capability_epoch: 1,
            byte_size: 10,
            sha256_hex: "ab".repeat(32),
            digest_provenance: DigestProvenance::Advertised,
            cache_path: format!("/cache/{track}/media"),
            licence_label: OperationalLicence::SourceDeclared,
            committed_at_epoch_secs: at,
        }
    }

    #[test]
    fn unresolved_keys_are_live_only() {
        let catalog = OfflineCatalog::default();
        let key = MediaKey::new(SourceId::local(), TrackId::new("missing").unwrap());
        assert_eq!(catalog.resolve(&key), OfflineCatalogueEntry::LiveOnly);
    }

    #[test]
    fn publish_then_resolve_is_cached_and_refresh_siblings_are_bounded() {
        let mut catalog = OfflineCatalog::default();
        let first = snapshot("track-1", 100);
        let key = first.media_key.clone();
        assert!(catalog.publish(first).is_none());
        assert!(matches!(
            catalog.resolve(&key),
            OfflineCatalogueEntry::Cached(_)
        ));
        // A refresh commits a sibling; the predecessor comes back for unlink.
        let predecessor = catalog.publish(snapshot("track-1", 200)).unwrap();
        assert_eq!(predecessor.committed_at_epoch_secs, 100);
        let resolved = catalog.resolve(&key);
        match resolved {
            OfflineCatalogueEntry::Cached(current) => {
                assert_eq!(current.committed_at_epoch_secs, 200);
            }
            other => panic!("expected cached, got {other:?}"),
        }
    }

    #[test]
    fn revocation_retires_playability_but_keeps_the_row() {
        let mut catalog = OfflineCatalog::default();
        let row = snapshot("track-2", 100);
        let key = row.media_key.clone();
        catalog.publish(row);
        assert!(catalog.retire(&key));
        assert!(matches!(
            catalog.resolve(&key),
            OfflineCatalogueEntry::Revoked(_)
        ));
        assert!(!catalog.retire(&key), "no committed row left to retire");
    }

    #[test]
    fn publish_returns_a_revoked_predecessor_for_settlement() {
        let mut catalog = OfflineCatalog::default();
        let row = snapshot("track-3", 100);
        let key = row.media_key.clone();
        catalog.publish(row);
        catalog.retire(&key);
        // A refresh over a revoked row must return it: the engine owes
        // its quota charge a release, and dropping it silently would
        // orphan the charge with no row left to evict.
        let fresh = snapshot("track-3", 200);
        let predecessor = catalog.publish(fresh).unwrap();
        assert_eq!(predecessor.committed_at_epoch_secs, 100);
        assert!(matches!(
            catalog.resolve(&key),
            OfflineCatalogueEntry::Cached(_)
        ));
        // Exactly one row remains for the key, and republishing without a
        // predecessor in either map returns none.
        assert!(catalog.publish(snapshot("track-3", 300)).is_some());
        let rows = catalog.all_snapshots();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].committed_at_epoch_secs, 300);
    }

    #[test]
    fn removal_drops_both_kinds_and_reports_what_was_removed() {
        let mut catalog = OfflineCatalog::default();
        let committed = snapshot("track-3", 100);
        let revoked = snapshot("track-4", 100);
        let committed_key = committed.media_key.clone();
        let revoked_key = revoked.media_key.clone();
        catalog.publish(committed);
        catalog.publish(revoked);
        catalog.retire(&revoked_key);
        assert!(catalog.remove(&committed_key).is_some());
        assert!(catalog.remove(&revoked_key).is_some());
        assert!(catalog.remove(&committed_key).is_none());
        assert_eq!(
            catalog.resolve(&committed_key),
            OfflineCatalogueEntry::LiveOnly
        );
    }

    #[test]
    fn source_snapshot_queries_are_newest_first() {
        let mut catalog = OfflineCatalog::default();
        catalog.publish(snapshot("track-5", 100));
        catalog.publish(snapshot("track-6", 300));
        catalog.publish(snapshot("track-7", 200));
        let rows = catalog.snapshots_for_source(SourceId::local());
        let stamps: Vec<u64> = rows.iter().map(|row| row.committed_at_epoch_secs).collect();
        assert_eq!(stamps, vec![300, 200, 100]);
    }
}
