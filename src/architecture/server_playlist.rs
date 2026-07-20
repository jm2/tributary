//! Bounded, locator-free snapshots of playlists owned by a remote server.
//!
//! A native playlist ID and its ordered native track IDs are authoritative
//! only within the owning source session. Names, owners, and advertised
//! counts are presentation hints: they never authorize membership, matching,
//! persistence, playback, or mutation.

use std::fmt;

use super::identity::{NativePlaylistId, TrackId, MAX_REMOTE_TRACK_ID_BYTES};

/// Maximum number of summaries accepted from one server listing.
pub const MAX_SERVER_PLAYLISTS_PER_LIST: usize = 10_000;

/// Maximum number of ordered track occurrences accepted in one snapshot.
pub const MAX_SERVER_PLAYLIST_ENTRIES: usize = 100_000;

/// Maximum UTF-8 byte length of an optional server presentation hint.
pub const MAX_SERVER_PLAYLIST_HINT_BYTES: usize = 16 * 1024;

/// Bounded presentation metadata for one server-owned playlist.
#[derive(Clone, Eq, PartialEq)]
pub struct ServerPlaylistSummary {
    native_id: NativePlaylistId,
    name: Option<String>,
    owner: Option<String>,
    advertised_track_count: Option<u64>,
}

impl ServerPlaylistSummary {
    pub fn new(
        native_id: NativePlaylistId,
        name: Option<String>,
        owner: Option<String>,
        advertised_track_count: Option<u64>,
    ) -> Result<Self, ServerPlaylistDataError> {
        validate_hint(&name, ServerPlaylistDataError::NameHintTooLong)?;
        validate_hint(&owner, ServerPlaylistDataError::OwnerHintTooLong)?;
        Ok(Self {
            native_id,
            name,
            owner,
            advertised_track_count,
        })
    }

    pub fn native_id(&self) -> &NativePlaylistId {
        &self.native_id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    pub const fn advertised_track_count(&self) -> Option<u64> {
        self.advertised_track_count
    }
}

impl fmt::Debug for ServerPlaylistSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistSummary")
            .field("native_id", &self.native_id)
            .field("name_byte_len", &self.name.as_ref().map(String::len))
            .field("owner_byte_len", &self.owner.as_ref().map(String::len))
            .field("advertised_track_count", &self.advertised_track_count)
            .finish()
    }
}

/// One complete, ordered server-owned playlist snapshot.
///
/// Duplicate track IDs are distinct occurrences and remain in exact server
/// order. The advertised count is deliberately not compared with
/// `track_ids.len()`: it is untrusted display metadata, while the bounded
/// vector is the authoritative membership returned by the detail endpoint.
#[derive(Clone, Eq, PartialEq)]
pub struct ServerPlaylistSnapshot {
    native_id: NativePlaylistId,
    name: Option<String>,
    owner: Option<String>,
    advertised_track_count: Option<u64>,
    track_ids: Vec<TrackId>,
}

impl ServerPlaylistSnapshot {
    pub fn new(
        native_id: NativePlaylistId,
        name: Option<String>,
        owner: Option<String>,
        advertised_track_count: Option<u64>,
        track_ids: Vec<TrackId>,
    ) -> Result<Self, ServerPlaylistDataError> {
        validate_hint(&name, ServerPlaylistDataError::NameHintTooLong)?;
        validate_hint(&owner, ServerPlaylistDataError::OwnerHintTooLong)?;
        if track_ids.len() > MAX_SERVER_PLAYLIST_ENTRIES {
            return Err(ServerPlaylistDataError::TooManyEntries);
        }
        if track_ids
            .iter()
            .any(|track_id| track_id.as_str().len() > MAX_REMOTE_TRACK_ID_BYTES)
        {
            return Err(ServerPlaylistDataError::InvalidTrackIdentity);
        }
        Ok(Self {
            native_id,
            name,
            owner,
            advertised_track_count,
            track_ids,
        })
    }

    pub fn native_id(&self) -> &NativePlaylistId {
        &self.native_id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    pub const fn advertised_track_count(&self) -> Option<u64> {
        self.advertised_track_count
    }

    pub fn track_ids(&self) -> &[TrackId] {
        &self.track_ids
    }

    pub fn into_track_ids(self) -> Vec<TrackId> {
        self.track_ids
    }
}

impl fmt::Debug for ServerPlaylistSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistSnapshot")
            .field("native_id", &self.native_id)
            .field("name_byte_len", &self.name.as_ref().map(String::len))
            .field("owner_byte_len", &self.owner.as_ref().map(String::len))
            .field("advertised_track_count", &self.advertised_track_count)
            .field("track_count", &self.track_ids.len())
            .finish()
    }
}

fn validate_hint(
    value: &Option<String>,
    error: ServerPlaylistDataError,
) -> Result<(), ServerPlaylistDataError> {
    if value
        .as_ref()
        .is_some_and(|value| value.len() > MAX_SERVER_PLAYLIST_HINT_BYTES)
    {
        return Err(error);
    }
    Ok(())
}

/// Closed, content-redacted reasons that a server playlist DTO was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ServerPlaylistDataError {
    #[error("server playlist name hint exceeds the byte limit")]
    NameHintTooLong,
    #[error("server playlist owner hint exceeds the byte limit")]
    OwnerHintTooLong,
    #[error("server playlist contains too many track occurrences")]
    TooManyEntries,
    #[error("server playlist contains an invalid remote track identity")]
    InvalidTrackIdentity,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist_id(value: &str) -> NativePlaylistId {
        NativePlaylistId::new(value).expect("native playlist ID")
    }

    fn remote_track(value: &str) -> TrackId {
        TrackId::remote(value).expect("remote track ID")
    }

    #[test]
    fn summary_hints_are_exact_bounded_and_non_authoritative() {
        let summary = ServerPlaylistSummary::new(
            playlist_id("playlist-id"),
            Some(" Case-sensitive name ☃".to_string()),
            Some(String::new()),
            Some(u64::MAX),
        )
        .expect("bounded summary");
        assert_eq!(summary.native_id().as_str(), "playlist-id");
        assert_eq!(summary.name(), Some(" Case-sensitive name ☃"));
        assert_eq!(summary.owner(), Some(""));
        assert_eq!(summary.advertised_track_count(), Some(u64::MAX));

        assert!(ServerPlaylistSummary::new(
            playlist_id("playlist-id"),
            Some("x".repeat(MAX_SERVER_PLAYLIST_HINT_BYTES)),
            None,
            None,
        )
        .is_ok());
        assert_eq!(
            ServerPlaylistSummary::new(
                playlist_id("playlist-id"),
                Some("x".repeat(MAX_SERVER_PLAYLIST_HINT_BYTES + 1)),
                None,
                None,
            ),
            Err(ServerPlaylistDataError::NameHintTooLong)
        );
        assert_eq!(
            ServerPlaylistSummary::new(
                playlist_id("playlist-id"),
                None,
                Some("x".repeat(MAX_SERVER_PLAYLIST_HINT_BYTES + 1)),
                None,
            ),
            Err(ServerPlaylistDataError::OwnerHintTooLong)
        );
    }

    #[test]
    fn snapshot_preserves_order_duplicates_and_ignores_count_mismatch() {
        let snapshot = ServerPlaylistSnapshot::new(
            playlist_id("playlist-id"),
            Some("name".to_string()),
            Some("owner".to_string()),
            Some(999),
            vec![
                remote_track("second"),
                remote_track("first"),
                remote_track("second"),
            ],
        )
        .expect("bounded snapshot");
        assert_eq!(
            snapshot
                .track_ids()
                .iter()
                .map(TrackId::as_str)
                .collect::<Vec<_>>(),
            ["second", "first", "second"]
        );
        assert_eq!(snapshot.advertised_track_count(), Some(999));
    }

    #[test]
    fn snapshot_rejects_entry_and_remote_identity_overflow_all_or_none() {
        let too_many = vec![remote_track("same"); MAX_SERVER_PLAYLIST_ENTRIES + 1];
        assert_eq!(
            ServerPlaylistSnapshot::new(playlist_id("playlist-id"), None, None, None, too_many,),
            Err(ServerPlaylistDataError::TooManyEntries)
        );

        let oversized = TrackId::new("secret".repeat(MAX_REMOTE_TRACK_ID_BYTES))
            .expect("application-bounded track ID");
        assert_eq!(
            ServerPlaylistSnapshot::new(
                playlist_id("playlist-id"),
                None,
                None,
                None,
                vec![remote_track("valid"), oversized],
            ),
            Err(ServerPlaylistDataError::InvalidTrackIdentity)
        );
    }

    #[test]
    fn diagnostics_redact_every_server_controlled_string() {
        let summary = ServerPlaylistSummary::new(
            playlist_id("secret-native-playlist-id"),
            Some("secret playlist name".to_string()),
            Some("secret owner".to_string()),
            Some(1),
        )
        .expect("summary");
        let snapshot = ServerPlaylistSnapshot::new(
            playlist_id("secret-native-playlist-id"),
            Some("secret playlist name".to_string()),
            Some("secret owner".to_string()),
            Some(1),
            vec![remote_track("secret-track-id")],
        )
        .expect("snapshot");
        for diagnostic in [format!("{summary:?}"), format!("{snapshot:?}")] {
            assert!(!diagnostic.contains("secret"));
        }
        let error = ServerPlaylistDataError::InvalidTrackIdentity;
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
}
