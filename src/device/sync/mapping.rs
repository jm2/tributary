//! Host-to-device playlist mapping.
//!
//! The pair is the anchor that lets incremental sync recognise what changed
//! on either side without re-pairing on every run. Each pair records:
//!
//! * the host playlist id (opaque to the sync module),
//! * the device-side identifier the host playlist was last written under,
//! * the device root relative path the playlist body lives under,
//! * the last wall-clock instant the pair was successfully synced, if any.
//!
//! A `PlaylistMap` is a collection of pairs that can be persisted to disk
//! and reloaded across sessions. The map's invariants are:
//!
//! 1. No two pairs share a host playlist id.
//! 2. No two pairs share a device-side identifier.
//! 3. Every pair's destination root is a valid relative path inside the
//!    device root.
//!
//! Violations are surfaced as [`PlaylistPairingError`]; the executor refuses
//! to plan against a map that fails validation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::policy::validate_relative_path;
use super::{HostPlaylistId, PolicyError};

/// One pairing of a host playlist with its counterpart on the device.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlaylistPair {
    host: HostPlaylistId,
    /// Opaque identifier the device side uses to refer to this playlist.
    /// For mounted filesystems this is the filename of the playlist body;
    /// for MTP devices it is the object handle the planner last wrote to.
    /// The string must not contain `/` or `\\` so it cannot be confused
    /// with a path.
    device_side_id: String,
    /// Path relative to the device root where the playlist body lives.
    destination_root: PathBuf,
    /// Last successful sync instant. `None` until the first run completes.
    last_synced_at: Option<u64>,
}

impl PlaylistPair {
    /// Construct a new pair. The device-side id must be a single component;
    /// the destination root must be a relative path under the device root.
    pub fn new(
        host: HostPlaylistId,
        device_side_id: impl Into<String>,
        destination_root: PathBuf,
    ) -> Result<Self, PlaylistPairingError> {
        let device_side_id = device_side_id.into();
        if device_side_id.is_empty() {
            return Err(PlaylistPairingError::EmptyDeviceSideId);
        }
        if device_side_id.contains('/') || device_side_id.contains('\\') {
            return Err(PlaylistPairingError::DeviceSideIdLooksLikePath {
                value: device_side_id,
            });
        }
        validate_relative_path(&destination_root, "destination")
            .map_err(|error| PlaylistPairingError::Policy { source: error })?;
        Ok(Self {
            host,
            device_side_id,
            destination_root,
            last_synced_at: None,
        })
    }

    /// The host playlist id.
    pub fn host(&self) -> &HostPlaylistId {
        &self.host
    }

    /// The device-side identifier for this pair.
    pub fn device_side_id(&self) -> &str {
        &self.device_side_id
    }

    /// Path relative to the device root where the playlist body lives.
    pub fn destination_root(&self) -> &Path {
        &self.destination_root
    }

    /// Last successful sync instant (seconds since UNIX epoch), if any.
    pub fn last_synced_at(&self) -> Option<u64> {
        self.last_synced_at
    }

    /// Record a successful sync instant. Only the executor calls this.
    pub fn record_synced(&mut self, instant_seconds: u64) {
        self.last_synced_at = Some(instant_seconds);
    }
}

/// Why a `PlaylistPair` or `PlaylistMap` was rejected.
#[derive(Debug, thiserror::Error)]
pub enum PlaylistPairingError {
    /// The device-side identifier was empty.
    #[error("device-side playlist id is empty")]
    EmptyDeviceSideId,
    /// The device-side identifier contained a path separator and could
    /// be mistaken for a filesystem path.
    #[error("device-side playlist id {value:?} looks like a path")]
    DeviceSideIdLooksLikePath { value: String },
    /// The destination root failed policy validation.
    #[error("policy validation rejected the destination: {source}")]
    Policy {
        #[source]
        source: PolicyError,
    },
    /// The map already has a pair for this host playlist id.
    #[error("host playlist {host} is already paired with {existing}")]
    HostAlreadyPaired {
        host: HostPlaylistId,
        existing: String,
    },
    /// The map already has a pair using this device-side identifier.
    #[error("device-side id {device_side_id} is already used by {host}")]
    DeviceSideIdAlreadyUsed {
        device_side_id: String,
        host: HostPlaylistId,
    },
    /// The map's invariants were violated on load.
    #[error("playlist map invariants violated: {reason}")]
    InvariantViolated { reason: String },
}

/// A collection of [`PlaylistPair`]s. The map is indexed both ways so the
/// planner can look up by host id or by device-side id without scanning.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlaylistMap {
    by_host: BTreeMap<HostPlaylistId, PlaylistPair>,
    by_device: BTreeMap<String, HostPlaylistId>,
}

impl PlaylistMap {
    /// Construct an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a new pair. The pair's host id and device-side id must not
    /// already be present in the map.
    pub fn insert(&mut self, pair: PlaylistPair) -> Result<(), PlaylistPairingError> {
        if let Some(existing) = self.by_host.get(pair.host()) {
            return Err(PlaylistPairingError::HostAlreadyPaired {
                host: pair.host().clone(),
                existing: existing.device_side_id().to_string(),
            });
        }
        if let Some(existing_host) = self.by_device.get(pair.device_side_id()) {
            return Err(PlaylistPairingError::DeviceSideIdAlreadyUsed {
                device_side_id: pair.device_side_id().to_string(),
                host: existing_host.clone(),
            });
        }
        let device_side_id = pair.device_side_id().to_string();
        let host = pair.host().clone();
        self.by_device.insert(device_side_id, host.clone());
        self.by_host.insert(host, pair);
        Ok(())
    }

    /// Look up a pair by host id.
    pub fn get_by_host(&self, host: &HostPlaylistId) -> Option<&PlaylistPair> {
        self.by_host.get(host)
    }

    /// Mutable lookup by host id. Used by the executor to record sync
    /// instants.
    pub fn get_by_host_mut(&mut self, host: &HostPlaylistId) -> Option<&mut PlaylistPair> {
        self.by_host.get_mut(host)
    }

    /// Look up a pair by device-side id.
    pub fn get_by_device(&self, device_side_id: &str) -> Option<&PlaylistPair> {
        let host = self.by_device.get(device_side_id)?;
        self.by_host.get(host)
    }

    /// Number of pairs in the map.
    pub fn len(&self) -> usize {
        self.by_host.len()
    }

    /// True when the map has no pairs.
    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }

    /// Iterate every pair in stable host-id order.
    pub fn iter(&self) -> impl Iterator<Item = (&HostPlaylistId, &PlaylistPair)> {
        self.by_host.iter()
    }

    /// Validate that every pair still satisfies the construction invariants.
    /// The map holds the invariants by construction in normal use; this
    /// method exists for reload paths where a persisted map might have been
    /// hand-edited.
    pub fn validate(&self) -> Result<(), PlaylistPairingError> {
        if self.by_host.len() != self.by_device.len() {
            return Err(PlaylistPairingError::InvariantViolated {
                reason: format!(
                    "host index has {} entries but device index has {}",
                    self.by_host.len(),
                    self.by_device.len()
                ),
            });
        }
        for (host, pair) in &self.by_host {
            let back_ref = self.by_device.get(pair.device_side_id()).ok_or_else(|| {
                PlaylistPairingError::InvariantViolated {
                    reason: format!(
                        "host {host} points at device id {:?} but device index has no entry",
                        pair.device_side_id()
                    ),
                }
            })?;
            if back_ref != host {
                return Err(PlaylistPairingError::InvariantViolated {
                    reason: format!(
                        "host {host} points at device id {:?} but device index points at {back_ref}",
                        pair.device_side_id()
                    ),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(value: &str) -> HostPlaylistId {
        HostPlaylistId::new(value).expect("host")
    }

    #[test]
    fn pair_rejects_empty_device_side_id() {
        let error = PlaylistPair::new(host("a"), "", PathBuf::from("Music")).expect_err("empty id");
        assert!(matches!(error, PlaylistPairingError::EmptyDeviceSideId));
    }

    #[test]
    fn pair_rejects_device_side_id_with_separator() {
        let error = PlaylistPair::new(host("a"), "Music/Playlists/abc", PathBuf::from("Music"))
            .expect_err("separator");
        assert!(matches!(
            error,
            PlaylistPairingError::DeviceSideIdLooksLikePath { .. }
        ));
    }

    #[test]
    fn pair_rejects_absolute_destination() {
        let error =
            PlaylistPair::new(host("a"), "abc", PathBuf::from("/etc")).expect_err("absolute");
        assert!(matches!(error, PlaylistPairingError::Policy { .. }));
    }

    #[test]
    fn map_rejects_duplicate_host() {
        let mut map = PlaylistMap::new();
        map.insert(PlaylistPair::new(host("a"), "x", PathBuf::from("Music")).expect("pair"))
            .expect("insert");
        let error = map
            .insert(PlaylistPair::new(host("a"), "y", PathBuf::from("Music")).expect("pair"))
            .expect_err("dup");
        assert!(matches!(
            error,
            PlaylistPairingError::HostAlreadyPaired { .. }
        ));
    }

    #[test]
    fn map_rejects_duplicate_device_id() {
        let mut map = PlaylistMap::new();
        map.insert(PlaylistPair::new(host("a"), "x", PathBuf::from("Music")).expect("pair"))
            .expect("insert");
        let error = map
            .insert(PlaylistPair::new(host("b"), "x", PathBuf::from("Music")).expect("pair"))
            .expect_err("dup");
        assert!(matches!(
            error,
            PlaylistPairingError::DeviceSideIdAlreadyUsed { .. }
        ));
    }

    #[test]
    fn map_lookup_by_device_returns_pair() {
        let mut map = PlaylistMap::new();
        let pair = PlaylistPair::new(host("a"), "x", PathBuf::from("Music")).expect("pair");
        map.insert(pair).expect("insert");
        let looked_up = map.get_by_device("x").expect("found");
        assert_eq!(looked_up.host().as_str(), "a");
    }

    #[test]
    fn validate_passes_for_consistent_map() {
        let mut map = PlaylistMap::new();
        map.insert(PlaylistPair::new(host("a"), "x", PathBuf::from("Music")).expect("pair"))
            .expect("insert");
        map.insert(PlaylistPair::new(host("b"), "y", PathBuf::from("Music")).expect("pair"))
            .expect("insert");
        assert!(map.validate().is_ok());
    }
}
