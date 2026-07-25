//! Sync planner: compute deltas and build transfer plans.
//!
//! The planner is the read-only half of sync. It walks every host playlist
//! the caller asks it to sync, compares the host track set against the
//! recorded [`IncrementalSyncState`](super::state::IncrementalSyncState),
//! and emits a [`SyncPlan`] describing the writes (and optional deletes)
//! the executor must run.
//!
//! The planner never touches the host filesystem or the device filesystem.
//! It accepts the host track list and the host track fingerprints as
//! inputs. This keeps the planner independent of the host library's
//! concrete [`MediaBackend`](crate::local::backend::MediaBackend) and
//! ensures the test surface is plain data.
//!
//! ## Conflict semantics
//!
//! The planner does not decide a conflict outcome; the executor does. The
//! planner records *what changed on each side*, and the executor uses the
//! policy's [`ConflictResolution`](super::SyncConflictResolution) to pick
//! the outcome. This separation keeps the planner testable without an
//! authority and the executor focused on the side-effect ordering.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::mapping::PlaylistPair;
use super::policy::validate_relative_path;
use super::state::{IncrementalSyncState, TrackSyncStatus};
use super::{HostPlaylistId, PolicyError};

/// A description of one host track the planner needs the fingerprint for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostTrackEntry {
    /// Stable host-side track id.
    pub track_id: String,
    /// Current fingerprint. The planner compares this against the recorded
    /// fingerprint to decide whether the track is unchanged, modified, or
    /// brand-new.
    pub fingerprint: String,
    /// Where the track should land on the device, relative to the
    /// playlist pair's destination root.
    pub device_relative_path: PathBuf,
}

/// What the planner learned about one track for one playlist pair.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SyncDeltaKind {
    /// The host track has never been written to the device.
    New,
    /// The host track has been edited since the last sync.
    Modified,
    /// The host track was previously synced and is unchanged.
    Unchanged,
    /// The track was on the device but the host playlist no longer has it.
    Removed {
        /// Where the executor should remove the file from if the policy
        /// allows it. Relative to the device root.
        device_relative_path: PathBuf,
        /// Last fingerprint the executor wrote.
        last_known_fingerprint: String,
    },
}

/// One per-track outcome from the planner.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SyncDelta {
    pub host: HostPlaylistId,
    pub track_id: String,
    pub kind: SyncDeltaKind,
}

/// What the planner was told up front.
#[derive(Clone, Debug)]
pub struct SyncRequest {
    /// Pairs the caller wants to sync.
    pub pairs: Vec<PlaylistPair>,
    /// The host track set, keyed by host playlist id.
    pub tracks_by_pair: Vec<HostTrackSet>,
    /// The recorded incremental state, keyed by host playlist id. The
    /// planner consults this to decide what changed.
    pub state_by_pair: Vec<IncrementalSyncState>,
}

/// One playlist's worth of host tracks plus the recorded state for that
/// pair.
#[derive(Clone, Debug)]
pub struct HostTrackSet {
    pub host: HostPlaylistId,
    pub tracks: Vec<HostTrackEntry>,
    pub state: IncrementalSyncState,
}

/// The planner's output: every per-track delta grouped by playlist pair.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncPlan {
    pub deltas: Vec<SyncDelta>,
    /// Total bytes the executor will write. Used for capacity budgeting.
    pub expected_write_bytes: u64,
    /// Number of files the executor will write.
    pub write_count: u32,
    /// Number of files the executor will remove (subject to policy).
    pub remove_count: u32,
}

/// The sync planner. Stateless and `Clone` so the same planner can serve
/// multiple requests in sequence.
#[derive(Clone, Debug, Default)]
pub struct SyncPlanner;

impl SyncPlanner {
    /// Construct a new planner.
    pub fn new() -> Self {
        Self
    }

    /// Build a plan from a request.
    ///
    /// The planner iterates the request's pairs, looks up the matching host
    /// track set and recorded state, and produces one [`SyncDelta`] per
    /// track. Pairs without a matching host track set or recorded state
    /// are reported as a removed delta for every previously-synced track.
    #[allow(clippy::unused_self)]
    pub fn plan(&self, request: &SyncRequest) -> Result<SyncPlan, PlannerError> {
        if request.pairs.len() != request.tracks_by_pair.len()
            || request.pairs.len() != request.state_by_pair.len()
        {
            return Err(PlannerError::RequestShapeMismatch {
                pairs: request.pairs.len(),
                tracks: request.tracks_by_pair.len(),
                states: request.state_by_pair.len(),
            });
        }

        let mut deltas: Vec<SyncDelta> = Vec::new();
        let mut write_count: u32 = 0;
        let mut remove_count: u32 = 0;

        for (index, pair) in request.pairs.iter().enumerate() {
            let tracks = &request.tracks_by_pair[index];
            if &tracks.host != pair.host() {
                return Err(PlannerError::HostMismatch {
                    pair: pair.host().clone(),
                    tracks: tracks.host.clone(),
                });
            }
            if tracks.state != request.state_by_pair[index] {
                return Err(PlannerError::StateMismatch {
                    host: tracks.host.clone(),
                });
            }
            validate_relative_path(pair.destination_root(), "destination")
                .map_err(|source| PlannerError::Policy { source })?;

            // Build a set of host track ids the planner sees on the host.
            let host_track_ids: BTreeSet<&str> = tracks
                .tracks
                .iter()
                .map(|entry| entry.track_id.as_str())
                .collect();

            // Walk the host tracks.
            for entry in &tracks.tracks {
                if entry.fingerprint.is_empty() {
                    return Err(PlannerError::EmptyFingerprint {
                        host: tracks.host.clone(),
                        track_id: entry.track_id.clone(),
                    });
                }
                let status = tracks.state.status(&entry.track_id);
                let destination_relative =
                    pair.destination_root().join(&entry.device_relative_path);
                validate_relative_path(&destination_relative, "destination")
                    .map_err(|source| PlannerError::Policy { source })?;

                match status {
                    None => {
                        // The state has no entry for this track. It is brand
                        // new from the planner's perspective.
                        deltas.push(SyncDelta {
                            host: tracks.host.clone(),
                            track_id: entry.track_id.clone(),
                            kind: SyncDeltaKind::New,
                        });
                        write_count = write_count.saturating_add(1);
                    }
                    Some(TrackSyncStatus::Pending) => {
                        deltas.push(SyncDelta {
                            host: tracks.host.clone(),
                            track_id: entry.track_id.clone(),
                            kind: SyncDeltaKind::New,
                        });
                        write_count = write_count.saturating_add(1);
                    }
                    Some(TrackSyncStatus::Synced { fingerprint, .. }) => {
                        if fingerprint == &entry.fingerprint {
                            deltas.push(SyncDelta {
                                host: tracks.host.clone(),
                                track_id: entry.track_id.clone(),
                                kind: SyncDeltaKind::Unchanged,
                            });
                        } else {
                            deltas.push(SyncDelta {
                                host: tracks.host.clone(),
                                track_id: entry.track_id.clone(),
                                kind: SyncDeltaKind::Modified,
                            });
                            write_count = write_count.saturating_add(1);
                        }
                    }
                    Some(TrackSyncStatus::Modified { fingerprint: _ }) => {
                        // The recorded modification matches (or is older
                        // than) what the host is currently serving; treat
                        // as modified so the executor still tries to push.
                        deltas.push(SyncDelta {
                            host: tracks.host.clone(),
                            track_id: entry.track_id.clone(),
                            kind: SyncDeltaKind::Modified,
                        });
                        write_count = write_count.saturating_add(1);
                    }
                    Some(TrackSyncStatus::Missing { .. }) => {
                        // The track was previously recorded as missing. If
                        // the host has re-added it, treat as new.
                        deltas.push(SyncDelta {
                            host: tracks.host.clone(),
                            track_id: entry.track_id.clone(),
                            kind: SyncDeltaKind::New,
                        });
                        write_count = write_count.saturating_add(1);
                    }
                }
            }

            // Walk tracks that were previously synced but no longer appear
            // on the host.
            for (track_id, status) in tracks.state.iter() {
                if host_track_ids.contains(track_id) {
                    continue;
                }
                match status {
                    TrackSyncStatus::Synced {
                        device_relative_path,
                        last_synced_at,
                        ..
                    } => {
                        deltas.push(SyncDelta {
                            host: tracks.host.clone(),
                            track_id: track_id.to_string(),
                            kind: SyncDeltaKind::Removed {
                                device_relative_path: PathBuf::from(device_relative_path),
                                last_known_fingerprint: status
                                    .fingerprint()
                                    .unwrap_or_default()
                                    .to_string(),
                            },
                        });
                        let _ = last_synced_at;
                        remove_count = remove_count.saturating_add(1);
                    }
                    TrackSyncStatus::Missing { .. } => {
                        // Already missing; nothing to do.
                    }
                    TrackSyncStatus::Modified { .. } | TrackSyncStatus::Pending => {
                        // Never written; nothing on the device to remove.
                    }
                }
            }
        }

        Ok(SyncPlan {
            deltas,
            expected_write_bytes: 0,
            write_count,
            remove_count,
        })
    }
}

/// Why the planner rejected a request.
#[derive(Debug, thiserror::Error)]
pub enum PlannerError {
    /// The request had a different number of pairs, host track sets, and
    /// recorded states.
    #[error("sync request is malformed: {pairs} pairs, {tracks} track sets, {states} states")]
    RequestShapeMismatch {
        pairs: usize,
        tracks: usize,
        states: usize,
    },
    /// The host id of a track set did not match its pair.
    #[error(
        "host playlist {tracks} is paired with {pair} but the host track set was for {tracks}"
    )]
    HostMismatch {
        pair: HostPlaylistId,
        tracks: HostPlaylistId,
    },
    /// The provided host track set state did not match the recorded state.
    #[error("recorded state does not match the supplied state for host {host}")]
    StateMismatch { host: HostPlaylistId },
    /// A track had an empty fingerprint.
    #[error("host {host} track {track_id} has an empty fingerprint")]
    EmptyFingerprint {
        host: HostPlaylistId,
        track_id: String,
    },
    /// The destination root failed policy validation.
    #[error("planner destination is invalid: {source}")]
    Policy {
        #[source]
        source: PolicyError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::sync::mapping::PlaylistPair;

    fn host(value: &str) -> HostPlaylistId {
        HostPlaylistId::new(value).expect("host")
    }

    fn pair(host_id: &str, dest: &str) -> PlaylistPair {
        PlaylistPair::new(
            host(host_id),
            format!("device-{host_id}"),
            PathBuf::from(dest),
        )
        .expect("pair")
    }

    #[test]
    fn plan_marks_new_track_as_new() {
        let mut state = IncrementalSyncState::new();
        let plan = SyncPlanner::new()
            .plan(&SyncRequest {
                pairs: vec![pair("a", "Music")],
                tracks_by_pair: vec![HostTrackSet {
                    host: host("a"),
                    tracks: vec![HostTrackEntry {
                        track_id: "track-1".into(),
                        fingerprint: "fp1".into(),
                        device_relative_path: PathBuf::from("a.flac"),
                    }],
                    state: IncrementalSyncState::new(),
                }],
                state_by_pair: vec![std::mem::take(&mut state)],
            })
            .expect("plan");
        assert_eq!(plan.write_count, 1);
        assert_eq!(plan.remove_count, 0);
        assert_eq!(plan.deltas.len(), 1);
        assert!(matches!(plan.deltas[0].kind, SyncDeltaKind::New));
    }

    #[test]
    fn plan_marks_unchanged_track_as_unchanged() {
        let mut state = IncrementalSyncState::new();
        state.record_synced("track-1", "fp1", "Music/a.flac", 100);
        let stored = state.clone();
        let plan = SyncPlanner::new()
            .plan(&SyncRequest {
                pairs: vec![pair("a", "Music")],
                tracks_by_pair: vec![HostTrackSet {
                    host: host("a"),
                    tracks: vec![HostTrackEntry {
                        track_id: "track-1".into(),
                        fingerprint: "fp1".into(),
                        device_relative_path: PathBuf::from("a.flac"),
                    }],
                    state: stored.clone(),
                }],
                state_by_pair: vec![stored],
            })
            .expect("plan");
        assert_eq!(plan.write_count, 0);
        assert_eq!(plan.remove_count, 0);
        assert!(matches!(plan.deltas[0].kind, SyncDeltaKind::Unchanged));
    }

    #[test]
    fn plan_marks_modified_track_as_modified() {
        let mut state = IncrementalSyncState::new();
        state.record_synced("track-1", "fp1", "Music/a.flac", 100);
        let stored = state.clone();
        let plan = SyncPlanner::new()
            .plan(&SyncRequest {
                pairs: vec![pair("a", "Music")],
                tracks_by_pair: vec![HostTrackSet {
                    host: host("a"),
                    tracks: vec![HostTrackEntry {
                        track_id: "track-1".into(),
                        fingerprint: "fp2".into(),
                        device_relative_path: PathBuf::from("a.flac"),
                    }],
                    state: stored.clone(),
                }],
                state_by_pair: vec![stored],
            })
            .expect("plan");
        assert_eq!(plan.write_count, 1);
        assert!(matches!(plan.deltas[0].kind, SyncDeltaKind::Modified));
    }

    #[test]
    fn plan_marks_dropped_track_as_removed() {
        let mut state = IncrementalSyncState::new();
        state.record_synced("track-1", "fp1", "Music/a.flac", 100);
        let stored = state.clone();
        let plan = SyncPlanner::new()
            .plan(&SyncRequest {
                pairs: vec![pair("a", "Music")],
                tracks_by_pair: vec![HostTrackSet {
                    host: host("a"),
                    tracks: vec![],
                    state: stored.clone(),
                }],
                state_by_pair: vec![stored],
            })
            .expect("plan");
        assert_eq!(plan.write_count, 0);
        assert_eq!(plan.remove_count, 1);
        assert!(matches!(plan.deltas[0].kind, SyncDeltaKind::Removed { .. }));
    }

    #[test]
    fn plan_rejects_shape_mismatch() {
        let error = SyncPlanner
            .plan(&SyncRequest {
                pairs: vec![pair("a", "Music")],
                tracks_by_pair: vec![],
                state_by_pair: vec![IncrementalSyncState::new()],
            })
            .expect_err("shape");
        assert!(matches!(error, PlannerError::RequestShapeMismatch { .. }));
    }

    #[test]
    fn plan_rejects_empty_fingerprint() {
        let error = SyncPlanner
            .plan(&SyncRequest {
                pairs: vec![pair("a", "Music")],
                tracks_by_pair: vec![HostTrackSet {
                    host: host("a"),
                    tracks: vec![HostTrackEntry {
                        track_id: "track-1".into(),
                        fingerprint: String::new(),
                        device_relative_path: PathBuf::from("a.flac"),
                    }],
                    state: IncrementalSyncState::new(),
                }],
                state_by_pair: vec![IncrementalSyncState::new()],
            })
            .expect_err("fingerprint");
        assert!(matches!(error, PlannerError::EmptyFingerprint { .. }));
    }

    #[test]
    fn plan_rejects_host_mismatch() {
        let error = SyncPlanner
            .plan(&SyncRequest {
                pairs: vec![pair("a", "Music")],
                tracks_by_pair: vec![HostTrackSet {
                    host: host("b"),
                    tracks: vec![],
                    state: IncrementalSyncState::new(),
                }],
                state_by_pair: vec![IncrementalSyncState::new()],
            })
            .expect_err("host mismatch");
        assert!(matches!(error, PlannerError::HostMismatch { .. }));
    }
}
