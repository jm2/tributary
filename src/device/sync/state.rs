//! Per-track incremental sync state.
//!
//! A re-attached device must not retransmit files it already has. The state
//! records, for every track that has ever been written, the fingerprint the
//! last successful run sent, the device-side path it was sent under, and
//! the wall-clock instant the write completed.
//!
//! The state lives in `IncrementalSyncState`, an append-only collection of
//! [`TrackSyncStatus`] entries indexed by host track id. The planner reads
//! the state to decide whether a track on the host is new, modified, or
//! unchanged since the last sync. The executor updates the state as it
//! commits each file.
//!
//! Invariants:
//! * A track that is recorded as `Synced` always has a non-empty
//!   fingerprint and a recorded sync instant.
//! * A track that is recorded as `Modified` has a fingerprint but no
//!   sync instant — the planner has decided the device copy is stale.
//! * A track that is recorded as `Pending` has no fingerprint; the
//!   executor has not yet committed anything for it.
//! * A track that is recorded as `Missing` was on the device but the host
//!   no longer has it. The executor uses this to honour the policy's
//!   delete-missing flag.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What we know about one host track relative to its last sync run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TrackSyncStatus {
    /// The host track has been written to the device at least once.
    /// The fingerprint and path describe the last successful write.
    Synced {
        fingerprint: String,
        device_relative_path: String,
        last_synced_at: u64,
    },
    /// The host track has been edited since the last sync. The fingerprint
    /// is the host's current value; the device copy is stale.
    Modified { fingerprint: String },
    /// The host track has not yet been written to the device.
    Pending,
    /// The host track was once synced but is no longer present in the
    /// host playlist. The device copy is a candidate for deletion if the
    /// policy allows it.
    Missing {
        last_known_fingerprint: String,
        last_synced_at: u64,
    },
}

impl TrackSyncStatus {
    /// True when the status is `Pending` — the executor should plan a
    /// write.
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// True when the status is `Missing`.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }

    /// Last known fingerprint, when one is recorded.
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Synced { fingerprint, .. } => Some(fingerprint),
            Self::Modified { fingerprint } => Some(fingerprint),
            Self::Missing {
                last_known_fingerprint,
                ..
            } => Some(last_known_fingerprint),
            Self::Pending => None,
        }
    }
}

/// A collection of [`TrackSyncStatus`] entries indexed by host track id.
///
/// The host track id is opaque to the sync module — the caller decides how
/// to construct it. A common choice is a stable per-track fingerprint the
/// library already computes; whatever it is, two calls to
/// [`IncrementalSyncState::record`] with the same id must refer to the
/// same track.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct IncrementalSyncState {
    entries: BTreeMap<String, TrackSyncStatus>,
}

impl IncrementalSyncState {
    /// Construct an empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tracked tracks.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no tracks are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up the status of one host track.
    pub fn status(&self, track_id: &str) -> Option<&TrackSyncStatus> {
        self.entries.get(track_id)
    }

    /// Record a successful sync. The status becomes
    /// [`TrackSyncStatus::Synced`].
    pub fn record_synced(
        &mut self,
        track_id: impl Into<String>,
        fingerprint: impl Into<String>,
        device_relative_path: impl Into<String>,
        instant_seconds: u64,
    ) {
        let fingerprint = fingerprint.into();
        let device_relative_path = device_relative_path.into();
        debug_assert!(!fingerprint.is_empty(), "fingerprint must be non-empty");
        debug_assert!(
            !device_relative_path.is_empty(),
            "device path must be non-empty"
        );
        self.entries.insert(
            track_id.into(),
            TrackSyncStatus::Synced {
                fingerprint,
                device_relative_path,
                last_synced_at: instant_seconds,
            },
        );
    }

    /// Record that the host track has been edited since the last sync.
    pub fn record_modified(&mut self, track_id: impl Into<String>, fingerprint: impl Into<String>) {
        self.entries.insert(
            track_id.into(),
            TrackSyncStatus::Modified {
                fingerprint: fingerprint.into(),
            },
        );
    }

    /// Record that the host track is no longer present. The executor uses
    /// this to honour the policy's delete-missing flag.
    pub fn record_missing(
        &mut self,
        track_id: impl Into<String>,
        last_known_fingerprint: impl Into<String>,
        last_synced_at: u64,
    ) {
        self.entries.insert(
            track_id.into(),
            TrackSyncStatus::Missing {
                last_known_fingerprint: last_known_fingerprint.into(),
                last_synced_at,
            },
        );
    }

    /// Drop the entry for a track. Called when the host playlist no longer
    /// references the track and the executor has decided not to keep its
    /// history.
    pub fn forget(&mut self, track_id: &str) {
        self.entries.remove(track_id);
    }

    /// Iterate every recorded status.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &TrackSyncStatus)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_synced_replaces_prior_status() {
        let mut state = IncrementalSyncState::new();
        state.record_synced("a", "fp1", "Music/a.flac", 100);
        state.record_synced("a", "fp2", "Music/a.flac", 200);
        let status = state.status("a").expect("status");
        assert!(matches!(status, TrackSyncStatus::Synced { .. }));
        assert_eq!(status.fingerprint(), Some("fp2"));
    }

    #[test]
    fn record_modified_marks_track_stale() {
        let mut state = IncrementalSyncState::new();
        state.record_synced("a", "fp1", "Music/a.flac", 100);
        state.record_modified("a", "fp2");
        let status = state.status("a").expect("status");
        assert!(matches!(status, TrackSyncStatus::Modified { .. }));
    }

    #[test]
    fn record_missing_keeps_last_known_fingerprint() {
        let mut state = IncrementalSyncState::new();
        state.record_synced("a", "fp1", "Music/a.flac", 100);
        state.record_missing("a", "fp1", 100);
        let status = state.status("a").expect("status");
        assert!(status.is_missing());
        assert_eq!(status.fingerprint(), Some("fp1"));
    }

    #[test]
    fn forget_removes_entry() {
        let mut state = IncrementalSyncState::new();
        state.record_synced("a", "fp1", "Music/a.flac", 100);
        state.forget("a");
        assert!(state.status("a").is_none());
    }

    #[test]
    fn pending_status_has_no_fingerprint() {
        let status = TrackSyncStatus::Pending;
        assert!(status.fingerprint().is_none());
        assert!(status.is_pending());
    }
}
