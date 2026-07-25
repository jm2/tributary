//! Per-playlist sync policy.
//!
//! Auto-sync is **opt-in**: every policy starts disabled. The user (or the
//! sync UX) must explicitly call [`SyncPolicy::enable`] with a destination
//! inside the device root. The destination is the relative directory the
//! playlist files will be staged under; the executor pins it to the device
//! authority so a policy that points outside the root is rejected at the
//! boundary rather than producing an out-of-bounds write at runtime.
//!
//! Conflict resolution covers the three cases the existing transfer planner
//! does not own:
//! * Host edited since last sync, device unchanged -> host-wins is the safe
//!   default; the device copy is overwritten.
//! * Device edited since last sync, host unchanged -> device-wins preserves
//!   the user-managed device copy and the host playlist is marked stale.
//! * Both edited -> manual-or-skip forces the user to resolve, never
//!   silently clobbering work on either side.
//!
//! The policy is plain data; the planner reads it. The executor never
//! inspects a policy directly.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::PolicyError;

/// How a sync run resolves a destination that already exists with a
/// different content hash than the host copy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum ConflictResolution {
    /// Host copy wins. The device copy is overwritten.
    #[default]
    HostWins,
    /// Device copy wins. The host copy is preserved but the device's
    /// newer content is left in place and the host state is recorded as
    /// stale.
    DeviceWins,
    /// Skip the file. No write, no read, no error — surfaced as a
    /// skipped entry in the run summary.
    Skip,
    /// Refuse to sync the file. The run is allowed to complete the rest
    /// of the playlist but this entry is reported as a conflict.
    Fail,
}

/// A user's chosen sync behaviour for one host playlist.
///
/// Policies are plain data: the planner reads them, the executor never
/// does. New fields can be added without touching the executor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncPolicy {
    enabled: bool,
    /// Destination relative to the device root. Empty when `enabled` is
    /// `false`. Validated as a relative path on construction.
    destination: PathBuf,
    conflict_strategy: ConflictResolution,
    /// Whether the sync run should delete tracks on the device that are
    /// no longer present on the host. Off by default: a missing host
    /// track never silently removes a device track.
    delete_missing: bool,
}

impl Default for SyncPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            destination: PathBuf::new(),
            conflict_strategy: ConflictResolution::default(),
            delete_missing: false,
        }
    }
}

impl SyncPolicy {
    /// Construct an enabled policy with the given destination and conflict
    /// strategy. The destination is validated as a relative path inside the
    /// device root.
    pub fn enable(
        destination: PathBuf,
        conflict_strategy: ConflictResolution,
    ) -> Result<Self, PolicyError> {
        validate_relative_path(&destination, "destination")?;
        Ok(Self {
            enabled: true,
            destination,
            conflict_strategy,
            delete_missing: false,
        })
    }

    /// Construct a disabled policy — the safe default.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// True when the policy allows auto-sync.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Destination the policy targets, relative to the device root.
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Conflict resolution the policy picked.
    pub fn conflict_strategy(&self) -> ConflictResolution {
        self.conflict_strategy
    }

    /// True when the executor should remove device tracks whose host
    /// counterpart is gone.
    pub fn deletes_missing(&self) -> bool {
        self.delete_missing
    }

    /// Allow the executor to remove device tracks whose host counterpart
    /// is gone. Off by default; callers must opt in explicitly.
    pub fn set_delete_missing(&mut self, value: bool) {
        self.delete_missing = value;
    }
}

pub fn validate_relative_path(path: &Path, field: &'static str) -> Result<(), PolicyError> {
    if path.as_os_str().is_empty() {
        return Err(PolicyError::InvalidPath {
            path: path.to_path_buf(),
            reason: match field {
                "destination" => "destination is empty",
                _ => "path is empty",
            },
        });
    }
    if path.is_absolute() {
        return Err(PolicyError::InvalidPath {
            path: path.to_path_buf(),
            reason: match field {
                "destination" => "destination is absolute",
                _ => "path is absolute",
            },
        });
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            let reason = match field {
                "destination" => "destination contains a non-normal component",
                _ => "path contains a non-normal component",
            };
            return Err(PolicyError::InvalidPath {
                path: path.to_path_buf(),
                reason,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_disabled_and_skips_delete() {
        let policy = SyncPolicy::default();
        assert!(!policy.is_enabled());
        assert!(!policy.deletes_missing());
        assert_eq!(policy.conflict_strategy(), ConflictResolution::HostWins);
    }

    #[test]
    fn enable_rejects_empty_destination() {
        let error = SyncPolicy::enable(PathBuf::new(), ConflictResolution::HostWins)
            .expect_err("empty destination");
        assert!(matches!(error, PolicyError::InvalidPath { .. }));
    }

    #[test]
    fn enable_rejects_absolute_destination() {
        let error = SyncPolicy::enable(PathBuf::from("/etc/music"), ConflictResolution::HostWins)
            .expect_err("absolute destination");
        assert!(matches!(error, PolicyError::InvalidPath { .. }));
    }

    #[test]
    fn enable_rejects_dotdot_components() {
        let error = SyncPolicy::enable(
            PathBuf::from("Music/../escape"),
            ConflictResolution::HostWins,
        )
        .expect_err("dotdot");
        assert!(matches!(error, PolicyError::InvalidPath { .. }));
    }

    #[test]
    fn enable_accepts_relative_destination() {
        let policy = SyncPolicy::enable(
            PathBuf::from("Music/Playlists"),
            ConflictResolution::DeviceWins,
        )
        .expect("policy");
        assert!(policy.is_enabled());
        assert_eq!(policy.destination(), Path::new("Music/Playlists"));
        assert_eq!(policy.conflict_strategy(), ConflictResolution::DeviceWins);
    }

    #[test]
    fn set_delete_missing_updates_flag() {
        let mut policy = SyncPolicy::default();
        assert!(!policy.deletes_missing());
        policy.set_delete_missing(true);
        assert!(policy.deletes_missing());
    }

    #[test]
    fn conflict_resolution_default_is_host_wins() {
        assert_eq!(ConflictResolution::default(), ConflictResolution::HostWins);
    }
}
