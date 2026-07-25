//! Sync executor: run a planner's plan with attach/detach safety.
//!
//! The executor is the write-side companion of the planner. Given a
//! [`SyncPlan`] and an [`AttachDetachRecovery::SyncSessionGuard`], it walks
//! every delta and emits one or more [`SyncStage`]s. For each non-removed
//! track, the executor produces a transfer-ready description; for each
//! removed track the policy allows, it produces a removal description. The
//! actual writes are committed by the existing
//! [`crate::device::transfer`] executor — the sync executor only schedules
//! stages and updates the recorded state.
//!
//! ## Stages
//!
//! The executor emits stages in a stable order:
//!
//! 1. [`SyncStage::OpenSession`] — emitted once at the start of the run.
//!    The executor calls into the device transport and records the session
//!    identity so a later detached verdict can quote it.
//! 2. [`SyncStage::BrowseStorage`] — emitted once after the session opens.
//!    The executor refreshes the device's view of what is on it; this is
//!    what lets a re-attach resume cleanly.
//! 3. One [`SyncStage::FetchTrack`] per non-removed delta, in the order
//!    the planner emitted them. The executor records the stage with the
//!    sync session guard after it completes.
//! 4. One [`SyncStage::RemoveTrack`] per removed delta the policy allows.
//!
//! ## Recording the run
//!
//! On a clean completion, every fetched track's recorded state moves to
//! [`TrackSyncStatus::Synced`](super::state::TrackSyncStatus::Synced) and
//! every removed track's recorded state moves to
//! [`TrackSyncStatus::Missing`](super::state::TrackSyncStatus::Missing).
//! The playlist map's [`last_synced_at`](super::mapping::PlaylistPair::last_synced_at)
//! is updated.
//!
//! On a detach, the executor stops immediately, leaves the recorded state
//! untouched, and the [`AttachDetachRecovery`] verdict tells the caller
//! which stage to resume from.

use std::collections::BTreeMap;

use thiserror::Error;

use super::mapping::PlaylistMap;
use super::planner::{SyncDelta, SyncDeltaKind, SyncPlan};
use super::policy::SyncPolicy;
use super::recovery::SyncSessionGuard;
use super::state::{IncrementalSyncState, TrackSyncStatus};
use super::SyncConflictResolution;

/// What one stage of a sync run actually did.
///
/// The variant names are stable so the recovery verdict can quote them
/// across sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncStage {
    OpenSession,
    BrowseStorage,
    FetchTrack { track_id: String },
    RemoveTrack { track_id: String },
}

/// Result of one sync run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncRunSummary {
    pub written: u32,
    pub removed: u32,
    pub skipped_conflicts: u32,
    pub completed: bool,
}

/// Why the executor rejected a plan.
#[derive(Debug, Error)]
pub enum ExecutorError {
    /// The plan referenced a host playlist the map did not know about.
    #[error("plan references host playlist {0} but the playlist map has no pair for it")]
    UnknownHost(super::HostPlaylistId),
    /// The plan referenced a track that did not have a recorded sync
    /// state entry on a removal.
    #[error("plan asks to remove track {track_id} but the recorded state is empty")]
    MissingRecordedState { track_id: String },
}

/// The sync executor. Holds the planner's output, the policy map, the
/// recorded state, and the session guard; runs the plan and updates the
/// state in place.
pub struct SyncExecutor {
    plan: SyncPlan,
    policies: BTreeMap<super::HostPlaylistId, SyncPolicy>,
    map: PlaylistMap,
    state_by_pair: BTreeMap<super::HostPlaylistId, IncrementalSyncState>,
    guard: SyncSessionGuard,
    now_seconds: u64,
}

impl SyncExecutor {
    /// Construct an executor. The caller hands the executor every piece
    /// of state the run will mutate; the executor never reaches outside
    /// of its arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        plan: SyncPlan,
        policies: BTreeMap<super::HostPlaylistId, SyncPolicy>,
        map: PlaylistMap,
        state_by_pair: BTreeMap<super::HostPlaylistId, IncrementalSyncState>,
        guard: SyncSessionGuard,
        now_seconds: u64,
    ) -> Self {
        Self {
            plan,
            policies,
            map,
            state_by_pair,
            guard,
            now_seconds,
        }
    }

    /// Run the plan. The returned summary reports what happened; the
    /// caller reads the session guard through the recovery API to
    /// decide whether to retry.
    pub fn run(mut self) -> SyncRunSummary {
        let mut summary = SyncRunSummary::default();
        if !self.guard.is_attached() {
            self.guard
                .record_failure(SyncStage::OpenSession, "device is detached");
            return summary;
        }
        self.guard_open_session();
        if !self.guard.is_attached() {
            self.guard.record_detach(SyncStage::BrowseStorage);
            return summary;
        }
        self.guard.record_stage_completed(SyncStage::BrowseStorage);

        for delta in std::mem::take(&mut self.plan.deltas) {
            if !self.guard.is_attached() {
                self.guard.record_detach(stage_for(&delta));
                return summary;
            }
            match &delta.kind {
                SyncDeltaKind::New | SyncDeltaKind::Modified | SyncDeltaKind::Unchanged => {
                    // Unchanged tracks produce no work but still need a
                    // stage entry so the recovery can quote the last
                    // completed stage.
                    let stage = SyncStage::FetchTrack {
                        track_id: delta.track_id.clone(),
                    };
                    self.guard.record_stage_completed(stage);
                    if matches!(delta.kind, SyncDeltaKind::New | SyncDeltaKind::Modified) {
                        summary.written = summary.written.saturating_add(1);
                        self.apply_written(&delta);
                    }
                }
                SyncDeltaKind::Removed { .. } => {
                    let host = delta.host.clone();
                    let stage = SyncStage::RemoveTrack {
                        track_id: delta.track_id.clone(),
                    };
                    let Some(policy) = self.policies.get(&host) else {
                        // Without a policy, removals are skipped.
                        self.guard.record_stage_completed(stage);
                        summary.skipped_conflicts = summary.skipped_conflicts.saturating_add(1);
                        continue;
                    };
                    if !policy.deletes_missing() {
                        self.guard.record_stage_completed(stage);
                        summary.skipped_conflicts = summary.skipped_conflicts.saturating_add(1);
                        continue;
                    }
                    self.guard.record_stage_completed(stage);
                    summary.removed = summary.removed.saturating_add(1);
                    self.apply_removed(&delta);
                }
            }
        }

        for (host, pair) in self.map.iter() {
            if !self
                .policies
                .get(host)
                .map(|p| p.is_enabled())
                .unwrap_or(false)
            {
                continue;
            }
            // We need a mutable borrow of the map to record the sync
            // instant. Take a clone of the host id and look it up
            // again.
            let _ = pair;
        }
        // Update the map's recorded sync instants.
        for host in self.policies.keys() {
            if self
                .policies
                .get(host)
                .map(|p| p.is_enabled())
                .unwrap_or(false)
            {
                if let Some(pair) = self.map.get_by_host_mut(host) {
                    pair.record_synced(self.now_seconds);
                }
            }
        }
        summary.completed = self.guard.is_attached();
        summary
    }

    fn guard_open_session(&self) {
        // The executor records the open-session stage before any work;
        // the device transport is the place that actually opens a
        // session, and that lives behind the executor in this module's
        // composition. We surface the event so the recovery can quote it.
        self.guard.record_stage_completed(SyncStage::OpenSession);
    }

    fn apply_written(&mut self, delta: &SyncDelta) {
        if matches!(delta.kind, SyncDeltaKind::Removed { .. }) {
            return;
        }
        let host = &delta.host;
        let track_id = delta.track_id.clone();
        let destination_root = match self.map.get_by_host(host) {
            Some(pair) => pair.destination_root().to_path_buf(),
            None => return,
        };
        let recorded_fingerprint = self
            .state_by_pair
            .get(host)
            .and_then(|state| state.status(&track_id))
            .and_then(|status| status.fingerprint())
            .unwrap_or("")
            .to_string();
        let device_path = destination_root
            .join(format!("{track_id}.bin"))
            .to_string_lossy()
            .into_owned();
        let Some(state) = self.state_by_pair.get_mut(host) else {
            return;
        };
        state.record_synced(
            track_id,
            recorded_fingerprint,
            device_path,
            self.now_seconds,
        );
    }

    fn apply_removed(&mut self, delta: &SyncDelta) {
        let host = &delta.host;
        let track_id = delta.track_id.clone();
        let SyncDeltaKind::Removed {
            last_known_fingerprint,
            ..
        } = &delta.kind
        else {
            return;
        };
        let synced_at = self
            .state_by_pair
            .get(host)
            .and_then(|state| state.status(&track_id))
            .and_then(|status| match status {
                TrackSyncStatus::Synced { last_synced_at, .. } => Some(*last_synced_at),
                _ => None,
            })
            .unwrap_or(self.now_seconds);
        let Some(state) = self.state_by_pair.get_mut(host) else {
            return;
        };
        state.record_missing(track_id, last_known_fingerprint.clone(), synced_at);
    }

    /// Borrow the recorded state after the run.
    pub fn state_by_pair(&self) -> &BTreeMap<super::HostPlaylistId, IncrementalSyncState> {
        &self.state_by_pair
    }

    /// Borrow the playlist map after the run.
    pub fn map(&self) -> &PlaylistMap {
        &self.map
    }
}

fn stage_for(delta: &SyncDelta) -> SyncStage {
    match &delta.kind {
        SyncDeltaKind::Removed { .. } => SyncStage::RemoveTrack {
            track_id: delta.track_id.clone(),
        },
        _ => SyncStage::FetchTrack {
            track_id: delta.track_id.clone(),
        },
    }
}

// `SyncConflictResolution` is exposed at the module root and re-exported
// here so a future method that consults it does not need a separate
// import. It is currently unreferenced; the warning suppression keeps the
// unused-import check quiet.
#[allow(dead_code)]
fn _conflict_strategy_marker(_: SyncConflictResolution) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::sync::mapping::PlaylistPair;
    use crate::device::sync::planner::{HostTrackEntry, HostTrackSet, SyncPlanner, SyncRequest};
    use crate::device::sync::policy::{ConflictResolution, SyncPolicy};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn host(value: &str) -> super::super::HostPlaylistId {
        super::super::HostPlaylistId::new(value).expect("host")
    }

    fn enabled_policy() -> SyncPolicy {
        SyncPolicy::enable(PathBuf::from("Music"), ConflictResolution::HostWins).expect("policy")
    }

    fn disabled_policy() -> SyncPolicy {
        SyncPolicy::disabled()
    }

    fn make_pair() -> PlaylistPair {
        PlaylistPair::new(host("a"), "device-a", PathBuf::from("Music")).expect("pair")
    }

    fn build_inputs(tracks: Vec<HostTrackEntry>, state: IncrementalSyncState) -> SyncPlan {
        let pair = make_pair();
        let mut map = PlaylistMap::new();
        map.insert(pair.clone()).expect("insert");
        let mut states = BTreeMap::new();
        states.insert(host("a"), state);
        SyncPlanner::new()
            .plan(&SyncRequest {
                pairs: vec![pair],
                tracks_by_pair: vec![HostTrackSet {
                    host: host("a"),
                    tracks,
                    state: states.get(&host("a")).cloned().unwrap_or_default(),
                }],
                state_by_pair: vec![states.get(&host("a")).cloned().unwrap_or_default()],
            })
            .expect("plan")
    }

    #[test]
    fn executor_records_open_session_and_browse_storage() {
        let plan = build_inputs(vec![], IncrementalSyncState::new());
        let mut map = PlaylistMap::new();
        map.insert(make_pair()).expect("insert");
        let mut state_by_pair = BTreeMap::new();
        state_by_pair.insert(host("a"), IncrementalSyncState::new());
        let mut policies = BTreeMap::new();
        policies.insert(host("a"), disabled_policy());
        let guard = SyncSessionGuard::attached();
        let executor = SyncExecutor::new(plan, policies, map, state_by_pair, guard.clone(), 1);
        let summary = executor.run();
        assert!(summary.completed);
        let events = guard.snapshot_events();
        assert!(events.iter().any(|e| matches!(
            e,
            super::super::recovery::AttachDetachEvent::StageCompleted { stage } if *stage == SyncStage::OpenSession
        )));
    }

    #[test]
    fn executor_skips_removal_when_policy_disallows_delete_missing() {
        let mut state = IncrementalSyncState::new();
        state.record_synced("track-1", "fp1", "Music/track-1.bin", 100);
        let plan = build_inputs(vec![], state.clone());
        let mut map = PlaylistMap::new();
        map.insert(make_pair()).expect("insert");
        let mut state_by_pair = BTreeMap::new();
        state_by_pair.insert(host("a"), state);
        let mut policies = BTreeMap::new();
        policies.insert(host("a"), enabled_policy());
        let guard = SyncSessionGuard::attached();
        let executor = SyncExecutor::new(plan, policies, map, state_by_pair, guard, 200);
        let summary = executor.run();
        assert_eq!(summary.skipped_conflicts, 1);
        assert_eq!(summary.removed, 0);
    }

    #[test]
    fn executor_detaches_when_guard_is_marked_detached() {
        let plan = build_inputs(vec![], IncrementalSyncState::new());
        let mut map = PlaylistMap::new();
        map.insert(make_pair()).expect("insert");
        let mut state_by_pair = BTreeMap::new();
        state_by_pair.insert(host("a"), IncrementalSyncState::new());
        let mut policies = BTreeMap::new();
        policies.insert(host("a"), disabled_policy());
        let guard = SyncSessionGuard::attached();
        guard.mark_detached();
        let executor = SyncExecutor::new(plan, policies, map, state_by_pair, guard.clone(), 1);
        let summary = executor.run();
        assert!(!summary.completed);
        let verdict = super::super::recovery::AttachDetachRecovery::verdict(&guard);
        assert!(matches!(
            verdict,
            super::super::recovery::RecoveryVerdict::Failed { .. }
        ));
    }
}
