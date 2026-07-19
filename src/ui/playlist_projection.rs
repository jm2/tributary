//! Pure regular-playlist projection from durable occurrences and live source
//! authority into UI-ready row specifications.
//!
//! Storage order is authoritative. Every durable occurrence produces exactly
//! one row specification or the entire projection fails closed. Remote
//! results are consumed positionally only for non-local entries and must name
//! the exact same `(SourceId, TrackId)`. Unavailable rows carry no fingerprint
//! or stale display metadata.

use thiserror::Error;

use crate::architecture::SourceId;
use crate::db::entities::track;
use crate::local::playlist_manager::LoadedPlaylistEntry;
use crate::source_registry::{
    RegularPlaylistTrack, RegularPlaylistTrackResolution, RegularPlaylistUnavailableReason,
};

use super::objects::{PlaylistOccurrenceBinding, PlaylistRowUnavailableReason};

/// Display content for one durable regular-playlist occurrence.
///
/// An unavailable row is deliberately a unit variant: normalized persistence
/// fingerprints and stale catalogue metadata cannot cross this boundary.
#[derive(Clone)]
pub enum PlaylistRowContent {
    AvailableLocal(track::Model),
    AvailableRemote(RegularPlaylistTrack),
    Unavailable,
}

/// One ordered UI projection row with its exact durable occurrence binding.
#[derive(Clone)]
pub struct PlaylistRowSpec {
    binding: PlaylistOccurrenceBinding,
    content: PlaylistRowContent,
}

impl PlaylistRowSpec {
    fn new(binding: PlaylistOccurrenceBinding, content: PlaylistRowContent) -> Self {
        Self { binding, content }
    }

    #[cfg(test)]
    pub fn binding(&self) -> &PlaylistOccurrenceBinding {
        &self.binding
    }

    #[cfg(test)]
    pub fn content(&self) -> &PlaylistRowContent {
        &self.content
    }

    pub fn into_parts(self) -> (PlaylistOccurrenceBinding, PlaylistRowContent) {
        (self.binding, self.content)
    }
}

/// Closed structural failure. It contains no persisted identity, fingerprint,
/// locator, registry error, or adapter detail.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PlaylistProjectionError {
    #[error("playlist remote-resolution count did not match durable occurrences")]
    RemoteResolutionCountMismatch,
    #[error("playlist remote resolution did not match its durable media identity")]
    RemoteIdentityMismatch,
    #[error("playlist local row did not match its durable media identity")]
    LocalIdentityMismatch,
    #[error("playlist occurrence could not form a valid row binding")]
    InvalidOccurrenceBinding,
}

/// Project ordered durable entries using ordered live resolutions for only
/// their non-local occurrences.
///
/// The result is all-or-none. Duplicates remain separate because the durable
/// `entry_id` is copied into every binding rather than deduplicating by media
/// identity.
pub fn project_playlist_rows(
    entries: Vec<LoadedPlaylistEntry>,
    remote_resolutions: Vec<RegularPlaylistTrackResolution>,
) -> Result<Vec<PlaylistRowSpec>, PlaylistProjectionError> {
    let expected_remote = entries
        .iter()
        .filter(|entry| entry.stored.source_id != SourceId::local())
        .count();
    if expected_remote != remote_resolutions.len() {
        return Err(PlaylistProjectionError::RemoteResolutionCountMismatch);
    }

    let mut remote_resolutions = remote_resolutions.into_iter();
    let mut rows = Vec::with_capacity(entries.len());
    for loaded in entries {
        let row = if loaded.stored.source_id == SourceId::local() {
            project_local(loaded)?
        } else {
            let resolution = remote_resolutions
                .next()
                .ok_or(PlaylistProjectionError::RemoteResolutionCountMismatch)?;
            project_remote(loaded, resolution)?
        };
        rows.push(row);
    }
    debug_assert_eq!(remote_resolutions.len(), 0);
    Ok(rows)
}

fn project_local(loaded: LoadedPlaylistEntry) -> Result<PlaylistRowSpec, PlaylistProjectionError> {
    let stored = loaded.stored;
    match loaded.local_track {
        Some(track) => {
            let Some(track_id) = stored.track_id else {
                return Err(PlaylistProjectionError::LocalIdentityMismatch);
            };
            if stored.local_track_id.as_ref() != Some(&track_id) || track.id != track_id.as_str() {
                return Err(PlaylistProjectionError::LocalIdentityMismatch);
            }
            let binding = PlaylistOccurrenceBinding::available_local(stored.id, track_id)
                .ok_or(PlaylistProjectionError::InvalidOccurrenceBinding)?;
            Ok(PlaylistRowSpec::new(
                binding,
                PlaylistRowContent::AvailableLocal(track),
            ))
        }
        None => {
            let (track_id, reason) = match stored.track_id {
                Some(track_id) => (
                    Some(track_id),
                    PlaylistRowUnavailableReason::LocalTrackMissing,
                ),
                None => (None, PlaylistRowUnavailableReason::LocalTrackUnmatched),
            };
            let binding = PlaylistOccurrenceBinding::unavailable(
                stored.id,
                SourceId::local(),
                track_id,
                reason,
            )
            .ok_or(PlaylistProjectionError::InvalidOccurrenceBinding)?;
            Ok(PlaylistRowSpec::new(
                binding,
                PlaylistRowContent::Unavailable,
            ))
        }
    }
}

fn project_remote(
    loaded: LoadedPlaylistEntry,
    resolution: RegularPlaylistTrackResolution,
) -> Result<PlaylistRowSpec, PlaylistProjectionError> {
    if loaded.local_track.is_some() {
        return Err(PlaylistProjectionError::LocalIdentityMismatch);
    }
    let stored = loaded.stored;
    let media_key = stored
        .media_key()
        .ok_or(PlaylistProjectionError::RemoteIdentityMismatch)?;
    if resolution.media_key() != &media_key {
        return Err(PlaylistProjectionError::RemoteIdentityMismatch);
    }

    match resolution {
        RegularPlaylistTrackResolution::Available(track) => {
            let track = *track;
            let binding = PlaylistOccurrenceBinding::available_remote(
                stored.id,
                media_key.source_id,
                media_key.track_id,
                track.guard(),
            )
            .ok_or(PlaylistProjectionError::InvalidOccurrenceBinding)?;
            Ok(PlaylistRowSpec::new(
                binding,
                PlaylistRowContent::AvailableRemote(track),
            ))
        }
        RegularPlaylistTrackResolution::Unavailable(unavailable) => {
            let binding = PlaylistOccurrenceBinding::unavailable(
                stored.id,
                media_key.source_id,
                Some(media_key.track_id),
                map_registry_reason(unavailable.reason()),
            )
            .ok_or(PlaylistProjectionError::InvalidOccurrenceBinding)?;
            Ok(PlaylistRowSpec::new(
                binding,
                PlaylistRowContent::Unavailable,
            ))
        }
    }
}

const fn map_registry_reason(
    reason: RegularPlaylistUnavailableReason,
) -> PlaylistRowUnavailableReason {
    match reason {
        RegularPlaylistUnavailableReason::SourceUnavailable => {
            PlaylistRowUnavailableReason::SourceUnavailable
        }
        RegularPlaylistUnavailableReason::UnsupportedSource => {
            PlaylistRowUnavailableReason::UnsupportedSource
        }
        RegularPlaylistUnavailableReason::InvalidCatalogue => {
            PlaylistRowUnavailableReason::InvalidCatalogue
        }
        RegularPlaylistUnavailableReason::TrackMissing => {
            PlaylistRowUnavailableReason::TrackMissing
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::architecture::{MediaKey, TrackId};
    use crate::local::playlist_manager::StoredPlaylistEntry;
    use crate::source_registry::SourceRegistry;

    use super::*;

    fn local_track(id: &str, title: &str) -> track::Model {
        track::Model {
            id: id.to_string(),
            file_path: format!("/library/{id}.flac"),
            title: title.to_string(),
            artist_name: "Artist".to_string(),
            album_artist_name: None,
            album_title: "Album".to_string(),
            genre: None,
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            last_played_at_ms: None,
            rating: None,
            date_added: String::new(),
            date_modified: String::new(),
            file_size_bytes: None,
        }
    }

    fn loaded_local(
        entry_id: &str,
        position: i32,
        track_id: Option<&str>,
        local_track: Option<track::Model>,
    ) -> LoadedPlaylistEntry {
        let track_id = track_id.map(|id| TrackId::new(id).expect("local track ID"));
        LoadedPlaylistEntry {
            stored: StoredPlaylistEntry {
                id: entry_id.to_string(),
                playlist_id: "playlist".to_string(),
                position,
                source_id: SourceId::local(),
                track_id: track_id.clone(),
                local_track_id: local_track.as_ref().and_then(|_| track_id.clone()),
                match_title: "private normalized title sentinel".to_string(),
                match_artist: "private normalized artist sentinel".to_string(),
                match_album: "private normalized album sentinel".to_string(),
                match_duration_secs: Some(123),
                match_file_path: Some("/private/reconciliation/sentinel.flac".to_string()),
            },
            local_track,
        }
    }

    fn loaded_remote(
        entry_id: &str,
        position: i32,
        source_id: SourceId,
        track_id: &str,
    ) -> LoadedPlaylistEntry {
        LoadedPlaylistEntry {
            stored: StoredPlaylistEntry {
                id: entry_id.to_string(),
                playlist_id: "playlist".to_string(),
                position,
                source_id,
                track_id: Some(TrackId::remote(track_id).expect("remote track ID")),
                local_track_id: None,
                match_title: "private remote title sentinel".to_string(),
                match_artist: "private remote artist sentinel".to_string(),
                match_album: "private remote album sentinel".to_string(),
                match_duration_secs: Some(321),
                match_file_path: None,
            },
            local_track: None,
        }
    }

    #[test]
    fn local_projection_preserves_order_duplicates_and_missing_states() {
        let first = local_track("same-track", "First current title");
        let rows = project_playlist_rows(
            vec![
                loaded_local("entry-one", 0, Some("same-track"), Some(first.clone())),
                loaded_local("entry-missing", 1, Some("missing-track"), None),
                loaded_local("entry-two", 2, Some("same-track"), Some(first)),
                loaded_local("entry-unmatched", 3, None, None),
            ],
            Vec::new(),
        )
        .expect("ordered local projection");

        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].binding().entry_id(), "entry-one");
        assert_eq!(rows[1].binding().entry_id(), "entry-missing");
        assert_eq!(rows[2].binding().entry_id(), "entry-two");
        assert_eq!(rows[3].binding().entry_id(), "entry-unmatched");
        assert_eq!(
            rows[0].binding().track_id(),
            rows[2].binding().track_id(),
            "duplicate media identity must retain separate durable rows"
        );
        assert_eq!(
            rows[1].binding().state(),
            crate::ui::objects::PlaylistOccurrenceState::Unavailable(
                PlaylistRowUnavailableReason::LocalTrackMissing
            )
        );
        assert_eq!(
            rows[3].binding().state(),
            crate::ui::objects::PlaylistOccurrenceState::Unavailable(
                PlaylistRowUnavailableReason::LocalTrackUnmatched
            )
        );
        assert!(matches!(
            rows[0].content(),
            PlaylistRowContent::AvailableLocal(track) if track.title == "First current title"
        ));
        assert!(matches!(rows[1].content(), PlaylistRowContent::Unavailable));
    }

    #[tokio::test]
    async fn mixed_rows_keep_order_source_scope_and_no_fingerprints() {
        let registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let first_source = SourceId::random();
        let second_source = SourceId::random();
        let local = local_track("local-track", "Current local title");
        let shared_id = TrackId::remote("shared-native-id").expect("remote track ID");
        let keys = vec![
            MediaKey::new(first_source, shared_id.clone()),
            MediaKey::new(second_source, shared_id),
        ];
        let resolutions = registry.resolve_regular_playlist_tracks(&keys);
        let rows = project_playlist_rows(
            vec![
                loaded_local("local-entry", 0, Some("local-track"), Some(local)),
                loaded_remote("first-entry", 1, first_source, "shared-native-id"),
                loaded_local("missing-entry", 2, Some("missing-local"), None),
                loaded_remote("second-entry", 3, second_source, "shared-native-id"),
            ],
            resolutions,
        )
        .expect("mixed projection");

        assert_eq!(rows[0].binding().entry_id(), "local-entry");
        assert_eq!(rows[0].binding().source_id(), SourceId::local());
        assert_eq!(rows[1].binding().entry_id(), "first-entry");
        assert_eq!(rows[1].binding().source_id(), first_source);
        assert_eq!(rows[2].binding().entry_id(), "missing-entry");
        assert_eq!(rows[3].binding().entry_id(), "second-entry");
        assert_eq!(rows[3].binding().source_id(), second_source);
        for row in [&rows[1], &rows[3]] {
            assert_eq!(
                row.binding().state(),
                crate::ui::objects::PlaylistOccurrenceState::Unavailable(
                    PlaylistRowUnavailableReason::SourceUnavailable
                )
            );
            assert!(matches!(row.content(), PlaylistRowContent::Unavailable));
            let debug = format!("{:?}", row.binding());
            assert!(!debug.contains("private remote"));
            assert!(!debug.contains("reconciliation"));
        }
        assert!(matches!(
            rows[0].content(),
            PlaylistRowContent::AvailableLocal(track) if track.title == "Current local title"
        ));
        assert!(matches!(rows[2].content(), PlaylistRowContent::Unavailable));

        registry.shutdown().wait().await;
    }

    #[tokio::test]
    async fn remote_resolution_count_and_full_identity_mismatches_fail_closed() {
        let registry = SourceRegistry::new(tokio::runtime::Handle::current());
        let stored_source = SourceId::random();
        let another_source = SourceId::random();
        let entry = loaded_remote("entry", 0, stored_source, "same-id");

        assert!(matches!(
            project_playlist_rows(vec![entry.clone()], Vec::new()),
            Err(PlaylistProjectionError::RemoteResolutionCountMismatch)
        ));

        let wrong = registry.resolve_regular_playlist_tracks(&[MediaKey::new(
            another_source,
            TrackId::remote("same-id").expect("remote track ID"),
        )]);
        assert!(matches!(
            project_playlist_rows(vec![entry], wrong),
            Err(PlaylistProjectionError::RemoteIdentityMismatch)
        ));

        registry.shutdown().wait().await;
    }

    #[test]
    fn malformed_local_resolution_fails_the_whole_projection_without_fallback() {
        let mismatched = local_track("different-track", "Wrong current row");
        assert!(matches!(
            project_playlist_rows(
                vec![loaded_local(
                    "entry",
                    0,
                    Some("expected-track"),
                    Some(mismatched),
                )],
                Vec::new(),
            ),
            Err(PlaylistProjectionError::LocalIdentityMismatch)
        ));
    }

    #[test]
    fn every_registry_unavailability_reason_has_one_closed_ui_state() {
        assert_eq!(
            map_registry_reason(RegularPlaylistUnavailableReason::SourceUnavailable),
            PlaylistRowUnavailableReason::SourceUnavailable
        );
        assert_eq!(
            map_registry_reason(RegularPlaylistUnavailableReason::UnsupportedSource),
            PlaylistRowUnavailableReason::UnsupportedSource
        );
        assert_eq!(
            map_registry_reason(RegularPlaylistUnavailableReason::InvalidCatalogue),
            PlaylistRowUnavailableReason::InvalidCatalogue
        );
        assert_eq!(
            map_registry_reason(RegularPlaylistUnavailableReason::TrackMissing),
            PlaylistRowUnavailableReason::TrackMissing
        );
    }
}
