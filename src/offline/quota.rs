//! Global offline quota accounting and content-aware eviction order.
//!
//! The contract fixes one policy: the application has a single byte quota;
//! eviction walks sources in oldest-cache-first order and, within a source,
//! newest-first; evicted rows are `Deleted` rows in the same step — never a
//! half-evicted state. This module owns that decision so no other layer can
//! improvise an eviction order.

use std::collections::HashMap;

use crate::architecture::identity::{MediaKey, SourceId};

/// Byte accounting for the global offline quota.
///
/// The ledger tracks committed bytes only; an in-flight job's reservation is
/// charged at admission against the projected total so concurrent admissions
/// cannot collectively overshoot the quota.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaLedger {
    quota_bytes: u64,
    committed_bytes: u64,
}

impl QuotaLedger {
    /// A ledger with the supplied global quota.
    #[must_use]
    pub const fn new(quota_bytes: u64) -> Self {
        Self {
            quota_bytes,
            committed_bytes: 0,
        }
    }

    /// Total bytes currently charged to the quota.
    #[must_use]
    pub const fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    /// The configured global quota.
    #[must_use]
    pub const fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }

    /// Whether `projected_total` additional bytes fit under the quota.
    #[must_use]
    pub const fn admits(&self, projected_total: u64) -> bool {
        self.committed_bytes.saturating_add(projected_total) <= self.quota_bytes
    }

    /// Charge committed bytes. The engine charges exactly the committed size
    /// after publish; quota is charged once per committed snapshot.
    pub fn commit(&mut self, byte_size: u64) {
        self.committed_bytes = self.committed_bytes.saturating_add(byte_size);
    }

    /// Release bytes after an eviction or deletion.
    pub fn release(&mut self, byte_size: u64) {
        self.committed_bytes = self.committed_bytes.saturating_sub(byte_size);
    }
}

/// One candidate row the eviction walk may remove.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvictionCandidate {
    pub key: MediaKey,
    pub byte_size: u64,
    pub committed_at_epoch_secs: u64,
}

/// The row an eviction walk selected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvictionVictim {
    pub key: MediaKey,
    pub byte_size: u64,
}

/// Select the next eviction victim: oldest source first, newest row first
/// within that source.
///
/// Source age is the oldest committed row the source currently holds; within
/// one source the newest committed row is surrendered first so per-source
/// browsing keeps its deepest history intact. Ties break on the media key's
/// track identity so the walk is total and deterministic.
#[must_use]
pub fn next_eviction_victim(candidates: &[EvictionCandidate]) -> Option<EvictionVictim> {
    if candidates.is_empty() {
        return None;
    }
    let mut source_oldest: HashMap<SourceId, u64> = HashMap::new();
    for candidate in candidates {
        let oldest = source_oldest
            .entry(candidate.key.source_id)
            .or_insert(candidate.committed_at_epoch_secs);
        *oldest = (*oldest).min(candidate.committed_at_epoch_secs);
    }
    // Oldest source first; break ties by source id so the walk is total.
    let mut source_order: Vec<(SourceId, u64)> = source_oldest.into_iter().collect();
    source_order.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
    });
    let chosen_source = source_order[0].0;
    let mut within: Vec<&EvictionCandidate> = candidates
        .iter()
        .filter(|candidate| candidate.key.source_id == chosen_source)
        .collect();
    // Newest-first within the source; ties break by track id for a total
    // order.
    within.sort_by(|left, right| {
        right
            .committed_at_epoch_secs
            .cmp(&left.committed_at_epoch_secs)
            .then_with(|| right.key.track_id.as_str().cmp(left.key.track_id.as_str()))
    });
    within.first().map(|victim| EvictionVictim {
        key: victim.key.clone(),
        byte_size: victim.byte_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(seed: u64) -> SourceId {
        let uuid = uuid::Uuid::from_u128(u128::from(seed));
        SourceId::from_uuid(uuid)
    }

    fn candidate(id: u64, src: SourceId, bytes: u64, at: u64) -> EvictionCandidate {
        EvictionCandidate {
            key: crate::architecture::identity::MediaKey::new(
                src,
                crate::architecture::identity::TrackId::new(format!("track-{id}")).unwrap(),
            ),
            byte_size: bytes,
            committed_at_epoch_secs: at,
        }
    }

    #[test]
    fn ledger_charges_and_releases_bytes() {
        let mut ledger = QuotaLedger::new(100);
        assert!(ledger.admits(100));
        assert!(!ledger.admits(101));
        ledger.commit(40);
        assert_eq!(ledger.committed_bytes(), 40);
        assert!(ledger.admits(60));
        assert!(!ledger.admits(61));
        ledger.release(40);
        assert_eq!(ledger.committed_bytes(), 0);
    }

    #[test]
    fn release_never_underflows() {
        let mut ledger = QuotaLedger::new(10);
        ledger.commit(10);
        ledger.release(99);
        assert_eq!(ledger.committed_bytes(), 0);
    }

    #[test]
    fn eviction_walks_oldest_source_first() {
        let a = source(1);
        let b = source(2);
        let candidates = vec![
            candidate(1, b, 10, 200),
            candidate(2, a, 10, 500),
            candidate(3, b, 10, 100),
        ];
        // Source b holds the oldest row (100) so it is walked first; within
        // b the newest row (200) is surrendered first.
        let victim = next_eviction_victim(&candidates).unwrap();
        assert_eq!(victim.key.track_id.as_str(), "track-1");
    }

    #[test]
    fn within_one_source_the_newest_row_is_evicted_first() {
        let a = source(3);
        let candidates = vec![
            candidate(1, a, 10, 100),
            candidate(2, a, 10, 900),
            candidate(3, a, 10, 500),
        ];
        let victim = next_eviction_victim(&candidates).unwrap();
        assert_eq!(victim.key.track_id.as_str(), "track-2");
    }

    #[test]
    fn empty_candidates_evict_nothing() {
        assert!(next_eviction_victim(&[]).is_none());
    }

    #[test]
    fn tie_breaks_are_total_and_deterministic() {
        let a = source(4);
        let candidates = vec![candidate(7, a, 10, 100), candidate(2, a, 10, 100)];
        let victim = next_eviction_victim(&candidates).unwrap();
        assert_eq!(victim.key.track_id.as_str(), "track-7");
        let shuffled: Vec<_> = candidates.iter().rev().cloned().collect();
        assert_eq!(next_eviction_victim(&shuffled).unwrap(), victim);
    }
}
