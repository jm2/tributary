//! The offline quota and eviction policy.
//!
//! This module is the only place that decides eviction
//! (`docs/offline-media.md`, "Cancellation, quota, and eviction"). It is
//! pure policy over caller-supplied accounting — no filesystem, no job
//! state:
//!
//! - **Quota is global.** The application has one offline quota expressed
//!   in bytes; per-source caps are advisory only at admission time.
//! - **Eviction is newest-first within source, oldest-first across
//!   sources.** Victims walk sources in oldest-cache-first order and,
//!   within a source, newest snapshot first.
//! - **Eviction is content-aware.** The engine executes each planned
//!   victim as one unlink-plus-row-retirement step; a failed unlink aborts
//!   the walk and leaves the row intact, so no half-evicted state exists.

use std::collections::BTreeMap;

use super::{OfflineError, OfflineSnapshot, OperationalLicence};
use crate::architecture::SourceId;

/// One committed row offered to the eviction planner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvictionCandidate {
    pub source_id: SourceId,
    pub byte_size: u64,
    /// Committed-at epoch in seconds; the eviction clock.
    pub committed_at_epoch_secs: u64,
}

/// The admission verdict for one proposed download, decided before any
/// network work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionVerdict {
    /// The source's global-capability and licence gates passed; proceed to
    /// duplicate-job and quota checks.
    Admitted,
    /// The capability refused (`Ok(None)` undeclared or an explicit
    /// refusal) — no offline layer is created.
    UnsupportedSource,
    /// The licence is `Denied` or `Revoked` at admission.
    LicenceDenied,
    /// The per-source advisory cap is already exhausted.
    SourceCapExhausted,
    /// The global quota has no headroom and eviction cannot restore any.
    QuotaExceeded,
}

/// Decide the local, network-free admission gates for one proposed job.
///
/// `capability` is the backend's `offline_snapshot()` verdict verbatim:
/// `Ok(None)` (not declared) and `Err(UnsupportedSource)` (explicit
/// refusal) are distinct upstream but both refuse admission — only an
/// opted-in source with a declared [`OfflineSnapshot`] byte cap may
/// proceed. The licence must be [`OperationalLicence::SourceDeclared`].
pub fn admission_verdict(
    capability: &Result<Option<OfflineSnapshot>, OfflineError>,
    licence: OperationalLicence,
    source_committed_bytes: u64,
    global_committed_bytes: u64,
    global_quota_bytes: u64,
) -> AdmissionVerdict {
    let snapshot = match capability {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return AdmissionVerdict::UnsupportedSource,
        Err(OfflineError::UnsupportedSource) => return AdmissionVerdict::UnsupportedSource,
        Err(_) => return AdmissionVerdict::UnsupportedSource,
    };
    if licence != OperationalLicence::SourceDeclared {
        return AdmissionVerdict::LicenceDenied;
    }
    if source_committed_bytes >= snapshot.source_byte_cap {
        return AdmissionVerdict::SourceCapExhausted;
    }
    if global_committed_bytes >= global_quota_bytes {
        return AdmissionVerdict::QuotaExceeded;
    }
    AdmissionVerdict::Admitted
}

/// Whether receiving `incoming_bytes` more would push the global total
/// past the quota. In-flight bytes count toward the total; the commit-time
/// charge equals the bytes actually received.
pub fn receive_overruns_quota(
    global_used_bytes: u64,
    incoming_bytes: u64,
    global_quota_bytes: u64,
) -> bool {
    global_used_bytes.saturating_add(incoming_bytes) > global_quota_bytes
}

/// Order eviction victims: sources oldest-cache-first (their oldest
/// snapshot's commit time), within a source newest snapshot first. The
/// planner returns the keys of the minimal victim prefix whose bytes
/// restore the global total to at or under the quota.
pub fn plan_eviction<K: Clone>(
    candidates: impl IntoIterator<Item = (K, EvictionCandidate)>,
    global_used_bytes: u64,
    global_quota_bytes: u64,
) -> Vec<K> {
    let mut over = global_used_bytes.saturating_sub(global_quota_bytes);
    let collected: Vec<(K, EvictionCandidate)> = candidates.into_iter().collect();
    if over == 0 {
        return Vec::new();
    }

    // Oldest snapshot per source defines the source's walk order.
    let mut source_oldest: BTreeMap<SourceId, u64> = BTreeMap::new();
    for (_, candidate) in &collected {
        let oldest = source_oldest
            .entry(candidate.source_id)
            .or_insert(candidate.committed_at_epoch_secs);
        *oldest = (*oldest).min(candidate.committed_at_epoch_secs);
    }

    let mut ordered: Vec<&(K, EvictionCandidate)> = collected.iter().collect();
    ordered.sort_by(|(_, a), (_, b)| {
        let a_source = source_oldest.get(&a.source_id).copied().unwrap_or(u64::MAX);
        let b_source = source_oldest.get(&b.source_id).copied().unwrap_or(u64::MAX);
        a_source
            .cmp(&b_source)
            // Newest-first within a source.
            .then_with(|| b.committed_at_epoch_secs.cmp(&a.committed_at_epoch_secs))
    });

    let mut plan = Vec::new();
    for (key, candidate) in ordered {
        if over == 0 {
            break;
        }
        over = over.saturating_sub(candidate.byte_size);
        plan.push(key.clone());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::identity::MediaKey;
    use crate::architecture::TrackId;

    fn source(seed: u64) -> SourceId {
        SourceId::from_uuid(uuid::Uuid::from_u64_pair(0x0123_4567_89ab_cdef, seed))
    }

    fn candidate(source: SourceId, size: u64, committed_at: u64) -> EvictionCandidate {
        EvictionCandidate {
            source_id: source,
            byte_size: size,
            committed_at_epoch_secs: committed_at,
        }
    }

    #[test]
    fn undeclared_and_refused_capabilities_both_refuse_admission() {
        let undeclared: Result<Option<OfflineSnapshot>, OfflineError> = Ok(None);
        let refused: Result<Option<OfflineSnapshot>, OfflineError> =
            Err(OfflineError::UnsupportedSource);
        let declared: Result<Option<OfflineSnapshot>, OfflineError> =
            Ok(Some(OfflineSnapshot::new(1024)));
        let licence = OperationalLicence::SourceDeclared;

        assert_eq!(
            admission_verdict(&undeclared, licence, 0, 0, 4096),
            AdmissionVerdict::UnsupportedSource
        );
        assert_eq!(
            admission_verdict(&refused, licence, 0, 0, 4096),
            AdmissionVerdict::UnsupportedSource
        );
        assert_eq!(
            admission_verdict(&declared, licence, 0, 0, 4096),
            AdmissionVerdict::Admitted
        );
    }

    #[test]
    fn denied_or_revoked_licence_refuses_admission_before_network() {
        let declared: Result<Option<OfflineSnapshot>, OfflineError> =
            Ok(Some(OfflineSnapshot::new(1024)));
        for licence in [
            OperationalLicence::Denied,
            OperationalLicence::Revoked,
            OperationalLicence::default(),
        ] {
            assert_eq!(
                admission_verdict(&declared, licence, 0, 0, 4096),
                AdmissionVerdict::LicenceDenied
            );
        }
    }

    #[test]
    fn source_cap_is_authoritative_within_its_source_only() {
        let declared: Result<Option<OfflineSnapshot>, OfflineError> =
            Ok(Some(OfflineSnapshot::new(100)));
        // Source at its cap: refused...
        assert_eq!(
            admission_verdict(
                &declared,
                OperationalLicence::SourceDeclared,
                100,
                100,
                4096
            ),
            AdmissionVerdict::SourceCapExhausted
        );
        // ...but the same global usage under a fresh cap admits.
        assert_eq!(
            admission_verdict(&declared, OperationalLicence::SourceDeclared, 0, 100, 4096),
            AdmissionVerdict::Admitted
        );
    }

    #[test]
    fn a_full_global_quota_refuses_admission() {
        let declared: Result<Option<OfflineSnapshot>, OfflineError> =
            Ok(Some(OfflineSnapshot::new(1024)));
        assert_eq!(
            admission_verdict(&declared, OperationalLicence::SourceDeclared, 0, 4096, 4096),
            AdmissionVerdict::QuotaExceeded
        );
        assert_eq!(
            admission_verdict(&declared, OperationalLicence::SourceDeclared, 0, 4095, 4096),
            AdmissionVerdict::Admitted
        );
    }

    #[test]
    fn receive_overrun_counts_inflight_bytes_against_the_global_quota() {
        assert!(receive_overruns_quota(900, 101, 1000));
        assert!(!receive_overruns_quota(900, 100, 1000));
        assert!(!receive_overruns_quota(1000, 0, 1000));
    }

    #[test]
    fn eviction_walks_oldest_source_first_and_newest_snapshot_first_within_it() {
        let a = source(1);
        let b = source(2);
        let candidates = [
            ("a-old", candidate(a, 100, 10)),  // A oldest snapshot
            ("a-new", candidate(a, 200, 20)),  // A newest snapshot
            ("b-only", candidate(b, 400, 30)), // B only snapshot
        ];
        // 700 used against a 450 quota: 250 bytes must go. Oldest source is
        // A; within A, newest first — so A's newest (200) then A's oldest
        // (100) — which alone restores 300 >= 250, leaving B untouched.
        let plan = plan_eviction(candidates, 700, 450);
        assert_eq!(plan, vec!["a-new", "a-old"]);
    }

    #[test]
    fn eviction_moves_to_the_next_source_only_after_older_sources_are_spent() {
        let a = source(1);
        let b = source(2);
        let c = source(3);
        let candidates = [
            ("a", candidate(a, 100, 10)),
            ("b", candidate(b, 200, 20)),
            ("c", candidate(c, 300, 5)), // C's oldest snapshot makes C the oldest source
        ];
        // 600 used, quota 350: 250 must go. C is oldest (5 < 10 < 20), so
        // C's newest first (only row, 300 bytes) — done in one victim.
        let plan = plan_eviction(candidates, 600, 350);
        assert_eq!(plan, vec!["c"]);
    }

    #[test]
    fn no_eviction_is_planned_while_within_quota() {
        let a = source(1);
        assert!(plan_eviction([("a", candidate(a, 100, 1))], 100, 4096).is_empty());
        let empty: [(&str, EvictionCandidate); 0] = [];
        assert!(plan_eviction(empty, 4096, 4096).is_empty());
    }

    #[test]
    fn eviction_planning_keys_on_source_identity_not_track_values() {
        let a = source(1);
        let media_key = MediaKey::new(a, TrackId::remote("track").expect("track id"));
        let _ = media_key; // identity-shape guard: candidates carry SourceId only
        let candidates = [("t2", candidate(a, 10, 1)), ("t1", candidate(a, 20, 2))];
        let plan = plan_eviction(candidates, 30, 5);
        assert_eq!(plan, vec!["t1", "t2"]);
    }
}
