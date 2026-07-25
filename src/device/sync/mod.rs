//! Device playlist synchronization.
//!
//! GitHub issue #8 / P3.2 require playlist mapping, incremental state, conflict
//! resolution, and an explicitly opted-in auto-sync with safe attach/detach
//! recovery. This module owns that work end-to-end:
//!
//! * [`SyncPolicy`] describes, per host playlist, whether the playlist is
//!   opted into device sync, where on the device it should land, and how a
//!   conflict between host and device copies is resolved. The default policy
//!   is "no sync" — every playlist starts unlinked until the user explicitly
//!   opts in. Opt-in is the only path to write a playlist to a device.
//! * [`PlaylistMap`] pairs host playlist ids with the device-side identifier
//!   they map to. The pairing is what makes incremental sync possible:
//!   without an established pair the planner has no anchor to detect a
//!   rename or a delete.
//! * [`SyncState`] records per-track incremental state — last sync time,
//!   whether the host copy was edited since the last sync, and whether the
//!   device copy has been verified. A re-attached device resumes from the
//!   stored state rather than retransferring everything.
//! * [`SyncPlanner`] computes the next transfer plan: for every opted-in
//!   host playlist, what tracks are new on the host, what tracks were
//!   removed, and what tracks the device currently holds that the host no
//!   longer has. The plan is described in terms of [`crate::device::transfer`]
//!   stages so the existing executor can run it unchanged.
//! * [`SyncExecutor`] runs the plan with attach/detach safety: if the device
//!   disappears mid-run, the executor stops cleanly, rolls back any partial
//!   work, and reports which tracks still need to be transferred so a later
//!   re-attach can resume from the right place.
//!
//! The module never speaks to the host filesystem on its own. The caller
//! hands it a [`crate::local::root_authority::MountedRootAuthority`] for the
//! device root; every read goes through that authority and every write
//! through [`crate::local::write_authority::MountedWriteAuthority`]. A
//! pre-attached plan that outlives the device is unusable — the executor
//! detects that and refuses to run it.

mod executor;
mod mapping;
mod planner;
mod policy;
mod recovery;
mod state;

#[allow(unused_imports)]
pub use executor::{SyncExecutor, SyncRunSummary, SyncStage};
#[allow(unused_imports)]
pub use mapping::{PlaylistMap, PlaylistPair, PlaylistPairingError};
#[allow(unused_imports)]
pub use planner::{SyncDelta, SyncDeltaKind, SyncPlan, SyncPlanner, SyncRequest};
#[allow(unused_imports)]
pub use policy::{ConflictResolution as SyncConflictResolution, SyncPolicy};
#[allow(unused_imports)]
pub use recovery::{AttachDetachEvent, AttachDetachRecovery, RecoveryError, SyncSessionGuard};
#[allow(unused_imports)]
pub use state::{IncrementalSyncState, TrackSyncStatus};

/// Stable identifier for one host-side playlist involved in sync.
///
/// The id is opaque to the sync module; the host layer translates it to
/// whatever persistence layer it owns. The id is wrapped so callers cannot
/// accidentally use it as a device identifier.
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct HostPlaylistId(pub String);

impl std::fmt::Display for HostPlaylistId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("host:playlist:")?;
        formatter.write_str(&self.0)
    }
}

impl HostPlaylistId {
    /// Construct from a caller-supplied string, rejecting empty input.
    pub fn new(value: impl Into<String>) -> Result<Self, PolicyError> {
        let inner = value.into();
        if inner.trim().is_empty() {
            return Err(PolicyError::InvalidPlaylistId {
                value: inner,
                reason: "playlist id is empty",
            });
        }
        Ok(Self(inner))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why the sync module rejected an otherwise well-formed call.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// A playlist id was empty or contained a forbidden character.
    #[error("playlist id {value:?} is invalid: {reason}")]
    InvalidPlaylistId { value: String, reason: &'static str },
    /// A relative path was absolute, empty, or contained a non-normal
    /// component.
    #[error("sync path {path:?} is invalid: {reason}")]
    InvalidPath {
        path: std::path::PathBuf,
        reason: &'static str,
    },
    /// A sync policy referenced a destination outside the device root.
    #[error("policy destination {path:?} escapes the device root")]
    DestinationEscapesRoot { path: std::path::PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_playlist_id_rejects_empty_string() {
        let error = HostPlaylistId::new("   ").expect_err("empty id");
        assert!(matches!(error, PolicyError::InvalidPlaylistId { .. }));
    }

    #[test]
    fn host_playlist_id_round_trips_display() {
        let id = HostPlaylistId::new("playlist-42").expect("id");
        assert_eq!(id.to_string(), "host:playlist:playlist-42");
    }

    #[test]
    fn sync_policy_default_is_disabled() {
        // Default policy must NOT auto-sync; opt-in is the only safe
        // shape.
        let policy = SyncPolicy::default();
        assert!(!policy.is_enabled());
        assert_eq!(policy.conflict_strategy(), SyncConflictResolution::HostWins);
    }
}
