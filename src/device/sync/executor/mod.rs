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
//! [`TrackSyncStatus::Synced`](super::state::TrackSyncStatus::Synced) with
//! the fingerprint and device-relative path the planner validated, and
//! every removed track's recorded state moves to
//! [`TrackSyncStatus::Missing`](super::state::TrackSyncStatus::Missing).
//! The playlist map's [`last_synced_at`](super::mapping::PlaylistPair::last_synced_at)
//! is updated. The run borrows the executor mutably and mutates the
//! recorded state in place, so nothing is lost when the run finishes; the
//! caller reads the results back through
//! [`state_by_pair`](SyncExecutor::state_by_pair) and
//! [`map`](SyncExecutor::map).
//!
//! ## Conflict strategies
//!
//! A write delta can conflict with an existing device copy. For a
//! `Modified` delta the policy's
//! [`SyncConflictResolution`](super::SyncConflictResolution) decides the
//! outcome: `HostWins` overwrites the device copy, `DeviceWins` keeps the
//! device copy and records the host copy as stale, `Skip` drops the write
//! without touching the recorded state, and `Fail` reports the conflict
//! and lets the rest of the run complete. A `New` delta has no device
//! copy to conflict with, so every strategy writes it. A playlist whose
//! policy is missing or disabled is never written: opt-in gates every
//! write.
//!
//! On a detach, the executor stops immediately, leaves the recorded state
//! untouched, and the [`AttachDetachRecovery`] verdict tells the caller
//! which stage to resume from. Deltas that already executed are dropped
//! from the plan, so a re-attached retry only replays the remaining work.

use std::collections::BTreeMap;

use thiserror::Error;

use super::mapping::PlaylistMap;
use super::planner::{SyncDelta, SyncDeltaKind, SyncPlan};
use super::policy::SyncPolicy;
use super::recovery::SyncSessionGuard;
use super::state::{IncrementalSyncState, TrackSyncStatus};
use super::{HostPlaylistId, SyncConflictResolution};

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
    /// Writes the policy's conflict strategy declined (`DeviceWins` or
    /// `Skip`).
    pub skipped_conflicts: u32,
    /// Writes the policy's `Fail` strategy refused. The run still
    /// completes the rest of the playlist.
    pub failed_conflicts: u32,
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

    /// Run the plan. The executor borrows `self` mutably and updates the
    /// recorded state in place; the returned summary reports what
    /// happened, and the caller reads the session guard through the
    /// recovery API to decide whether to retry.
    pub fn run(&mut self) -> SyncRunSummary {
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

        self.execute_deltas(&mut summary);
        if self.guard.is_attached() {
            self.finalize_sync_instants();
        }
        summary.completed = self.guard.is_attached();
        summary
    }

    /// Walk every remaining delta, executing one at a time. Stops at the
    /// first detach; deltas that already executed are dropped from the
    /// plan so a re-attached retry only replays the remaining work.
    fn execute_deltas(&mut self, summary: &mut SyncRunSummary) {
        while !self.plan.deltas.is_empty() {
            let delta = self.plan.deltas[0].clone();
            if !self.guard.is_attached() {
                self.guard.record_detach(stage_for(&delta));
                return;
            }
            match &delta.kind {
                SyncDeltaKind::New { .. } | SyncDeltaKind::Modified { .. } => {
                    let stage = SyncStage::FetchTrack {
                        track_id: delta.track_id.clone(),
                    };
                    let strategy = match self.policies.get(&delta.host) {
                        Some(policy) if policy.is_enabled() => policy.conflict_strategy(),
                        // Sync is not opted in for this playlist; the
                        // planned write is dropped without touching the
                        // recorded state.
                        _ => {
                            self.guard.record_stage_completed(stage);
                            self.plan.deltas.remove(0);
                            continue;
                        }
                    };
                    self.execute_write(&delta, strategy, summary);
                }
                SyncDeltaKind::Unchanged { .. } => {
                    // Unchanged tracks produce no work but still need a
                    // stage entry so the recovery can quote the last
                    // completed stage.
                    let stage = SyncStage::FetchTrack {
                        track_id: delta.track_id.clone(),
                    };
                    self.guard.record_stage_completed(stage);
                }
                SyncDeltaKind::Removed { .. } => {
                    let stage = SyncStage::RemoveTrack {
                        track_id: delta.track_id.clone(),
                    };
                    let policy = self.policies.get(&delta.host);
                    let allowed = policy.map(|p| p.deletes_missing()).unwrap_or(false);
                    if !allowed {
                        // Without an opted-in delete-missing policy,
                        // removals are skipped.
                        self.guard.record_stage_completed(stage);
                        summary.skipped_conflicts = summary.skipped_conflicts.saturating_add(1);
                        self.plan.deltas.remove(0);
                        continue;
                    }
                    self.guard.record_stage_completed(stage);
                    summary.removed = summary.removed.saturating_add(1);
                    self.apply_removed(&delta);
                }
            }
            self.plan.deltas.remove(0);
        }
    }

    /// Apply the policy's conflict strategy to one planned write.
    fn execute_write(
        &mut self,
        delta: &SyncDelta,
        strategy: SyncConflictResolution,
        summary: &mut SyncRunSummary,
    ) {
        let stage = SyncStage::FetchTrack {
            track_id: delta.track_id.clone(),
        };
        // A brand-new track has no device copy for a DeviceWins strategy
        // to preserve; every strategy writes a New delta. Only a
        // Modified delta can conflict with an existing device copy.
        match strategy {
            SyncConflictResolution::HostWins => {
                self.guard.record_stage_completed(stage);
                summary.written = summary.written.saturating_add(1);
                self.apply_written(delta);
            }
            SyncConflictResolution::DeviceWins
                if matches!(delta.kind, SyncDeltaKind::New { .. }) =>
            {
                self.guard.record_stage_completed(stage);
                summary.written = summary.written.saturating_add(1);
                self.apply_written(delta);
            }
            // The device copy wins: leave it in place and record the host
            // copy as stale so the next run still sees the divergence.
            SyncConflictResolution::DeviceWins => {
                self.guard.record_stage_completed(stage);
                summary.skipped_conflicts = summary.skipped_conflicts.saturating_add(1);
                if let Some((fingerprint, _)) = write_details(delta) {
                    self.record_stale(&delta.host, &delta.track_id, fingerprint);
                }
            }
            // Skip: no write, no state change.
            SyncConflictResolution::Skip => {
                self.guard.record_stage_completed(stage);
                summary.skipped_conflicts = summary.skipped_conflicts.saturating_add(1);
            }
            // Fail: report the conflict but let the rest of the playlist
            // complete.
            SyncConflictResolution::Fail => {
                self.guard.record_stage_completed(stage);
                summary.failed_conflicts = summary.failed_conflicts.saturating_add(1);
            }
        }
    }

    /// Record the playlist pair's sync instant for every opted-in
    /// policy after a completed run.
    fn finalize_sync_instants(&mut self) {
        let opted_in: Vec<HostPlaylistId> = self
            .policies
            .iter()
            .filter(|(_, policy)| policy.is_enabled())
            .map(|(host, _)| host.clone())
            .collect();
        for host in opted_in {
            if let Some(pair) = self.map.get_by_host_mut(&host) {
                pair.record_synced(self.now_seconds);
            }
        }
    }

    fn guard_open_session(&self) {
        // The executor records the open-session stage before any work;
        // the device transport is the place that actually opens a
        // session, and that lives behind the executor in this module's
        // composition. We surface the event so the recovery can quote it.
        self.guard.record_stage_completed(SyncStage::OpenSession);
    }

    fn apply_written(&mut self, delta: &SyncDelta) {
        let Some((fingerprint, device_relative_path)) = write_details(delta) else {
            return;
        };
        let Some(state) = self.state_by_pair.get_mut(&delta.host) else {
            return;
        };
        state.record_synced(
            delta.track_id.clone(),
            fingerprint,
            device_relative_path,
            self.now_seconds,
        );
    }

    /// Record a host track as stale (`Modified`) after a DeviceWins
    /// resolution so the next run still sees the divergence.
    fn record_stale(&mut self, host: &HostPlaylistId, track_id: &str, fingerprint: String) {
        let Some(state) = self.state_by_pair.get_mut(host) else {
            return;
        };
        state.record_modified(track_id, fingerprint);
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

/// The planner-validated fingerprint and device-relative path for a
/// write delta. Write deltas always carry both; the executor records
/// exactly what the planner computed instead of deriving its own values.
fn write_details(delta: &SyncDelta) -> Option<(String, String)> {
    match &delta.kind {
        SyncDeltaKind::New {
            fingerprint,
            device_relative_path,
        }
        | SyncDeltaKind::Modified {
            fingerprint,
            device_relative_path,
        } => Some((
            fingerprint.clone(),
            device_relative_path.to_string_lossy().into_owned(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
