//! Playback context and track navigation helpers.
//!
//! This module provides:
//! - [`PlaybackContext`] — shared state passed to playback functions
//! - [`play_track_at`] — load and play a specific track by position
//! - [`advance_track`] — move to the next track (shuffle/repeat aware)
//! - [`format_ms`] — format milliseconds as `m:ss` or `h:mm:ss`

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tracing::warn;

use crate::architecture::{MediaKey, SourceId, TrackId, ViewOrigin};
use crate::audio::output::AudioOutput;
use crate::audio::{PlayerEvent, PlayerEventGeneration, PlayerState};
use crate::lastfm::playback::LastFmPlaybackEvidenceError;
use crate::lastfm::playback_coordinator::{
    LastFmPlaybackCoordinatorBinding, LastFmPlaybackRetirement,
};
use crate::lastfm::playback_owner::{
    LastFmAcceptedOutputFreshness, LastFmAcceptedOutputLoad, LastFmAcceptedPlayback,
    LastFmOutputIntent, LastFmPlaybackOccurrenceIdentity, LastFmPlaybackSource,
};
use crate::local::playback_history::PlaybackHistoryProgress;
use crate::source_registry::{
    PlaybackAttributionProfile, PlaybackSourceReference, RegularPlaylistCatalogueGuard,
    SourceRegistry,
};
use crate::ui::header_bar::RepeatMode;
use crate::ui::objects::{PlaylistOccurrenceState, TrackObject};

use super::album_art;

/// The source key of the local library.
pub const LOCAL_SOURCE_KEY: &str = "local";

/// The source-key prefix of every playlist view. A playlist remains only the
/// view origin; each queue item carries its actual media owner's source and
/// native track identity.
pub const PLAYLIST_SOURCE_PREFIX: &str = "playlist:";

/// Previous restarts the current item only after this position. At exactly
/// three seconds it still walks the queue or retained shuffle history.
const PREVIOUS_RESTART_THRESHOLD_MS: u64 = 3_000;

/// Whether a queue source is backed by the local library database, and its
/// track IDs are therefore library track IDs.
///
/// Remote backends key tracks by their own native IDs, removable tracks use a
/// lossless mount-relative ID, and external sessions mint ephemeral IDs. A
/// library update must never reinterpret one of those as one of its own.
fn is_library_source(source_id: SourceId) -> bool {
    source_id == SourceId::local()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QueueView {
    source_id: SourceId,
    origin: Option<ViewOrigin>,
}

fn queue_view(source_key: &str) -> Option<QueueView> {
    if source_key == LOCAL_SOURCE_KEY {
        return Some(QueueView {
            source_id: SourceId::local(),
            origin: None,
        });
    }
    if let Some(playlist_id) = source_key.strip_prefix(PLAYLIST_SOURCE_PREFIX) {
        return Some(QueueView {
            source_id: SourceId::local(),
            origin: Some(ViewOrigin::playlist(playlist_id).ok()?),
        });
    }
    if source_key.starts_with("radio-") {
        return Some(QueueView {
            source_id: SourceId::radio_browser(),
            origin: Some(super::radio::radio_view_origin(source_key)?),
        });
    }
    if let Ok(source_id) = source_key.parse::<SourceId>() {
        return Some(QueueView {
            source_id,
            origin: None,
        });
    }
    Some(QueueView {
        source_id: SourceId::removable(source_key).ok()?,
        origin: None,
    })
}

fn identity_belongs_to_source(identity: &PlaybackIdentity, source_key: &str) -> bool {
    if source_key == LOCAL_SOURCE_KEY {
        return identity.media_key.source_id == SourceId::local();
    }
    if let Some(playlist_id) = source_key.strip_prefix(PLAYLIST_SOURCE_PREFIX) {
        return identity.view_origin == Some(ViewOrigin::Playlist(playlist_id.to_string()));
    }
    if super::radio::is_radio_backend(source_key) {
        return super::radio::radio_view_origin(source_key).as_ref()
            == identity.view_origin.as_ref();
    }
    identity.view_origin.is_none()
        && queue_view(source_key).is_some_and(|view| view.source_id == identity.media_key.source_id)
}

/// Whether `source_key` names the media-source session that owns this item.
///
/// Playlist and radio-feed keys are view origins, not source owners. They are
/// intentionally accepted by `identity_belongs_to_source` for GTK visibility,
/// but retiring one view must not revoke media owned by the local library or
/// shared Radio-Browser source.
fn identity_is_owned_by_source(identity: &PlaybackIdentity, source_key: &str) -> bool {
    let source_id = if source_key == LOCAL_SOURCE_KEY {
        Some(SourceId::local())
    } else if source_key.starts_with(PLAYLIST_SOURCE_PREFIX)
        || super::radio::is_radio_backend(source_key)
    {
        None
    } else if let Ok(source_id) = source_key.parse::<SourceId>() {
        Some(source_id)
    } else {
        SourceId::removable(source_key).ok()
    };
    source_id == Some(identity.media_key.source_id)
}

/// Overlay committed local-library URIs onto an existing playlist projection.
///
/// Playlist rows and the local library share stable track IDs, but each
/// playlist occurrence owns a distinct row identity. Mutating only the URI in
/// place preserves duplicate ordering and selection while ensuring a later
/// click builds its queue from the committed path. Empty replacement URIs are
/// ignored so a transiently unplayable update cannot strand a valid row.
pub(super) fn refresh_projected_library_uris(
    projected_rows: &[TrackObject],
    committed_local_rows: &[TrackObject],
) -> usize {
    if projected_rows.is_empty() || committed_local_rows.is_empty() {
        return 0;
    }

    let accepts_local_refresh = |row: &TrackObject| {
        row.source_id() == Some(SourceId::local())
            && row
                .playlist_occurrence_binding()
                .is_none_or(|binding| binding.state() == PlaylistOccurrenceState::AvailableLocal)
    };
    let projected_ids: HashSet<String> = projected_rows
        .iter()
        .filter(|row| accepts_local_refresh(row))
        .map(TrackObject::track_id)
        .collect();
    let mut committed_uris = HashMap::with_capacity(projected_ids.len());
    for track in committed_local_rows {
        let track_id = track.track_id();
        if !projected_ids.contains(&track_id) {
            continue;
        }
        let uri = track.uri();
        if !uri.is_empty() {
            committed_uris.insert(track_id, uri);
        }
    }

    let mut refreshed = 0;
    for row in projected_rows {
        if !accepts_local_refresh(row) {
            continue;
        }
        let Some(uri) = committed_uris.get(&row.track_id()) else {
            continue;
        };
        if row.uri() != *uri {
            row.set_uri(uri);
            refreshed += 1;
        }
    }
    refreshed
}

/// Stable media identity plus the view that supplied this queue occurrence.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackIdentity {
    pub media_key: MediaKey,
    pub view_origin: Option<ViewOrigin>,
}

impl PlaybackIdentity {
    fn new(view: &QueueView, source_id: SourceId, track_id: TrackId) -> Self {
        Self {
            media_key: MediaKey::new(source_id, track_id),
            view_origin: view.origin.clone(),
        }
    }
}

/// A committed library change, addressed to the queue by stable track ID.
///
/// A rename moves a track's file without changing what it is, so the queue must
/// re-resolve where to play it from while keeping the identity, position, and
/// history it captured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueTrackRefresh {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub duration_secs: Option<u64>,
    pub cover_art_url: String,
}

impl QueueTrackRefresh {
    pub fn from_track(track: &TrackObject) -> Self {
        let album_artist = track.album_artist();
        let track_number = track.track_number();
        let duration_secs = track.duration_secs();
        Self {
            title: track.title(),
            artist: track.artist(),
            album: track.album(),
            album_artist: (!album_artist.is_empty()).then_some(album_artist),
            track_number: (track_number != 0).then_some(track_number),
            duration_secs: (duration_secs != 0).then_some(duration_secs),
            cover_art_url: track.cover_art_url(),
        }
    }
}

/// Immutable metadata captured when a playback queue is created.
///
/// Keeping this snapshot outside GTK's mutable sort/filter models means view
/// changes cannot silently retarget Next, Previous, repeat, or EOS handling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueItem {
    pub identity: PlaybackIdentity,
    /// Zero-based occurrence of this stable track identity in the captured
    /// source queue. Playlist entries may intentionally reference one track
    /// more than once; this disambiguates row selection without changing the
    /// stable source/track identity used for playback ownership.
    occurrence: usize,
    /// Concrete GTK row captured with the queue. This distinguishes duplicate
    /// playlist entries while that source model is alive; `occurrence` remains
    /// a fallback if the view is rebuilt with fresh row objects.
    row_instance_id: Option<u64>,
    /// Exact source session epoch that published this row.
    /// Stable media identity remains `(SourceId, TrackId)`; the epoch prevents
    /// a captured queue from being retargeted to a replacement login.
    source_session_epoch: Option<u64>,
    /// Exact accepted catalogue authority carried by a source-scoped regular
    /// playlist occurrence. This is transient queue state and is revalidated
    /// separately for stream and artwork at the point of use.
    regular_playlist_guard: Option<RegularPlaylistCatalogueGuard>,
    /// This exact queue item owns a hidden ephemeral external-file source.
    /// Random SourceIds are also valid persisted remote identities, so source
    /// shape alone cannot safely recover this lifecycle distinction later.
    external_session: bool,
    /// Exact attribution authority captured before this queue occurrence was
    /// created. Eligible managed rows receive it only through a registry-minted
    /// session/catalogue reference; external files carry their session proof.
    lastfm_source: Option<LastFmPlaybackSource>,
    /// Exact structured payload bound into `lastfm_source`. Candidate fields
    /// are derived from this profile, never from mutable/display metadata.
    lastfm_profile: Option<PlaybackAttributionProfile>,
    /// Positive metadata duration captured with this queue occurrence.
    /// Zero remains unknown so an output may supply its first real duration.
    duration_ms: Option<u64>,
    /// Exact positive structured duration retained for Last.fm metadata.
    /// Unlike output duration events, this is frozen from source metadata.
    duration_secs: Option<u64>,
    uri: String,
    title: String,
    artist: String,
    album: String,
    album_artist: Option<String>,
    track_number: Option<u32>,
    cover_art_url: String,
}

impl QueueItem {
    fn from_track(
        identity: PlaybackIdentity,
        track: &TrackObject,
        occurrence: usize,
        regular_playlist_guard: Option<RegularPlaylistCatalogueGuard>,
        playback_source: Option<PlaybackSourceReference>,
    ) -> Self {
        let is_library = is_library_source(identity.media_key.source_id);
        let raw_duration_secs = track.duration_secs();
        let duration_secs = (raw_duration_secs != 0).then_some(raw_duration_secs);
        let duration_ms = duration_secs.map(|duration_secs| duration_secs.saturating_mul(1_000));
        let album_artist = track.album_artist();
        let track_number = track.track_number();
        let lastfm_profile = playback_source
            .as_ref()
            .map(|reference| reference.profile().clone());
        let lastfm_source = playback_source.map(LastFmPlaybackSource::managed);
        Self {
            identity,
            occurrence,
            row_instance_id: Some(track.row_instance_id()),
            source_session_epoch: regular_playlist_guard
                .map(RegularPlaylistCatalogueGuard::session_epoch)
                .or_else(|| track.source_session_epoch()),
            regular_playlist_guard,
            external_session: false,
            // Only an opaque reference minted from the exact live registry
            // session/catalogue can authorize attribution. Its immutable
            // profile, rather than mutable GTK display fields, supplies the
            // Last.fm candidate below.
            lastfm_source,
            lastfm_profile,
            duration_ms,
            duration_secs,
            // Local and lifecycle-owned rows (including either kind of
            // regular-playlist occurrence) retain identity, ordering, and
            // metadata but never a locator. Every output load resolves the
            // exact source/track/authority at the point of use.
            uri: if is_library || track.source_session_epoch().is_some() {
                String::new()
            } else {
                track.uri()
            },
            title: track.title(),
            artist: track.artist(),
            album: track.album(),
            album_artist: (!album_artist.is_empty()).then_some(album_artist),
            track_number: (track_number != 0).then_some(track_number),
            cover_art_url: track.cover_art_url(),
        }
    }

    pub(crate) fn external(session: &crate::source_registry::ExternalFileSession) -> Self {
        let track = session.track();
        let attribution = session.playback_source().map(|reference| {
            (
                LastFmPlaybackSource::managed(reference.clone()),
                reference.profile().clone(),
            )
        });
        let (lastfm_source, lastfm_profile) = attribution
            .map(|(source, profile)| (Some(source), Some(profile)))
            .unwrap_or((None, None));
        Self {
            identity: PlaybackIdentity {
                media_key: MediaKey::new(session.source_id(), session.track_id().clone()),
                view_origin: None,
            },
            occurrence: 0,
            row_instance_id: None,
            source_session_epoch: Some(session.session_epoch()),
            regular_playlist_guard: None,
            external_session: true,
            lastfm_source,
            lastfm_profile,
            duration_ms: track
                .duration_secs
                .filter(|duration_secs| *duration_secs > 0)
                .map(|duration_secs| duration_secs.saturating_mul(1_000)),
            duration_secs: track
                .duration_secs
                .filter(|duration_secs| *duration_secs > 0),
            uri: String::new(),
            title: track.title.clone(),
            artist: track.artist_name.clone(),
            album: track.album_title.clone(),
            album_artist: track
                .album_artist_name
                .clone()
                .filter(|album_artist| !album_artist.is_empty()),
            track_number: track.track_number.filter(|track_number| *track_number != 0),
            cover_art_url: String::new(),
        }
    }

    fn lastfm_occurrence_candidate(&self) -> Option<LastFmOccurrenceCandidate> {
        let source = self.lastfm_source.clone()?;
        let profile = self.lastfm_profile.as_ref()?;
        Some(LastFmOccurrenceCandidate {
            identity: LastFmPlaybackOccurrenceIdentity::fresh(),
            source,
            artist: profile.artist().to_string(),
            title: profile.title().to_string(),
            album: profile.album().map(str::to_string),
            album_artist: profile.album_artist().map(str::to_string),
            track_number: profile.track_number(),
            duration_secs: profile.duration_secs(),
        })
    }

    #[cfg(test)]
    pub(crate) fn direct_for_test(
        uri: String,
        title: String,
        artist: String,
        album: String,
    ) -> Self {
        Self {
            identity: PlaybackIdentity {
                media_key: MediaKey::new(SourceId::random(), TrackId::external()),
                view_origin: None,
            },
            occurrence: 0,
            row_instance_id: None,
            source_session_epoch: None,
            regular_playlist_guard: None,
            external_session: false,
            lastfm_source: None,
            lastfm_profile: None,
            duration_ms: None,
            duration_secs: None,
            uri,
            title,
            artist,
            album,
            album_artist: None,
            track_number: None,
            cover_art_url: String::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn external_for_test(source_id: SourceId, session_epoch: u64) -> Self {
        Self {
            identity: PlaybackIdentity {
                media_key: MediaKey::new(source_id, TrackId::external()),
                view_origin: None,
            },
            occurrence: 0,
            row_instance_id: None,
            source_session_epoch: Some(session_epoch),
            regular_playlist_guard: None,
            external_session: true,
            lastfm_source: None,
            lastfm_profile: None,
            duration_ms: None,
            duration_secs: None,
            uri: String::new(),
            title: "External".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            album_artist: None,
            track_number: None,
            cover_art_url: String::new(),
        }
    }

    #[cfg(test)]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    #[cfg(test)]
    pub(crate) const fn source_session_epoch(&self) -> Option<u64> {
        self.source_session_epoch
    }
}

/// Immutable Last.fm input owned by one genuine queue occurrence.
///
/// This private candidate is created with the occurrence, before any output
/// retry. It intentionally keeps the original structured values even if a
/// later library refresh rewrites the queue item's display metadata. Each
/// accepted retry consumes a newly constructed move-only accepted value while
/// retaining this candidate's exact occurrence identity and frozen payload.
#[derive(Clone)]
struct LastFmOccurrenceCandidate {
    identity: LastFmPlaybackOccurrenceIdentity,
    source: LastFmPlaybackSource,
    artist: String,
    title: String,
    album: Option<String>,
    album_artist: Option<String>,
    track_number: Option<u32>,
    duration_secs: Option<u64>,
}

impl LastFmOccurrenceCandidate {
    fn accepted(&self) -> Result<LastFmAcceptedPlayback, LastFmPlaybackEvidenceError> {
        LastFmAcceptedPlayback::try_new(
            self.identity.clone(),
            self.source.clone(),
            self.artist.clone(),
            self.title.clone(),
            self.album.clone(),
            self.album_artist.clone(),
            self.track_number,
            self.duration_secs,
        )
    }
}

impl std::fmt::Debug for LastFmOccurrenceCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LastFmOccurrenceCandidate(<redacted>)")
    }
}

/// Non-cloneable witness that only `PlaybackSession` can issue after its
/// exact current generation crosses synchronous output acceptance.
///
/// The type is crate-visible solely so the GTK-free owner can require it;
/// the production constructor remains private to this module. Test-only
/// owner fixtures may mint an isolated witness without creating a GTK session.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct LastFmAcceptedOutputMint(());

impl LastFmAcceptedOutputMint {
    const fn issue() -> Self {
        Self(())
    }

    #[cfg(test)]
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) const fn for_test() -> Self {
        Self(())
    }
}

/// Non-cloneable witness that only `PlaybackSession` can issue after it has
/// committed the complete predecessor-to-successor output transition.
///
/// The GTK-free owner requires this witness to construct an output intent, so
/// no production caller can suspend or retire occurrence evidence from raw
/// generation values.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct LastFmOutputIntentMint(());

impl LastFmOutputIntentMint {
    const fn issue() -> Self {
        Self(())
    }
}

impl std::fmt::Debug for LastFmOutputIntentMint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LastFmOutputIntentMint(<opaque>)")
    }
}

/// Rollback-safe clone of only the predecessor's accepted-load freshness.
///
/// Tentative navigation stores this clone in the successor session. Dropping
/// it during rollback changes nothing; committing an output attempt revokes it
/// before either the Last.fm coordinator or the output can observe the
/// transition. It contains no metadata, identity, or source authority.
struct AcceptedLastFmLoadSuspension(LastFmAcceptedOutputFreshness);

impl AcceptedLastFmLoadSuspension {
    fn commit(self) {
        self.0.revoke();
    }
}

impl std::fmt::Debug for AcceptedLastFmLoadSuspension {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AcceptedLastFmLoadSuspension(<redacted>)")
    }
}

impl std::fmt::Debug for LastFmAcceptedOutputMint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LastFmAcceptedOutputMint(<opaque>)")
    }
}

/// Move-only, one-shot authority for handing one accepted output load to the
/// Last.fm playback owner.
///
/// `PlaybackSession` is intentionally not cloneable. Transactional UI paths
/// move this state into their rollback snapshots, so rollback cannot duplicate
/// an accepted load. A retry becomes available only after a genuinely new
/// generation crosses output acceptance. Its shared freshness gate is revoked
/// by every successor or terminal path, making delayed extracted loads inert.
#[derive(Default)]
enum AcceptedLastFmLoad {
    #[default]
    Unavailable,
    Available {
        generation: PlayerEventGeneration,
        freshness: LastFmAcceptedOutputFreshness,
    },
    Consumed {
        generation: PlayerEventGeneration,
        freshness: LastFmAcceptedOutputFreshness,
    },
}

impl AcceptedLastFmLoad {
    fn install(&mut self, generation: PlayerEventGeneration) {
        if self.generation() == Some(generation) {
            return;
        }
        self.revoke();
        *self = Self::Available {
            generation,
            freshness: LastFmAcceptedOutputFreshness::fresh(),
        };
    }

    fn take(&mut self, generation: PlayerEventGeneration) -> Option<LastFmAcceptedOutputFreshness> {
        let Self::Available {
            generation: current,
            freshness,
        } = self
        else {
            return None;
        };
        if *current != generation {
            return None;
        }
        let retained = freshness.clone();
        let output = freshness.clone();
        *self = Self::Consumed {
            generation,
            freshness: retained,
        };
        Some(output)
    }

    fn revoke(&mut self) {
        *self = match std::mem::take(self) {
            Self::Available {
                generation,
                freshness,
            }
            | Self::Consumed {
                generation,
                freshness,
            } => {
                freshness.revoke();
                Self::Consumed {
                    generation,
                    freshness,
                }
            }
            Self::Unavailable => Self::Unavailable,
        };
    }

    fn suspension(&self) -> Option<AcceptedLastFmLoadSuspension> {
        match self {
            Self::Available { freshness, .. } | Self::Consumed { freshness, .. } => {
                Some(AcceptedLastFmLoadSuspension(freshness.clone()))
            }
            Self::Unavailable => None,
        }
    }

    const fn generation(&self) -> Option<PlayerEventGeneration> {
        match self {
            Self::Unavailable => None,
            Self::Available { generation, .. } | Self::Consumed { generation, .. } => {
                Some(*generation)
            }
        }
    }

    #[cfg(test)]
    const fn is_consumed(&self) -> bool {
        matches!(self, Self::Consumed { .. })
    }
}

impl std::fmt::Debug for AcceptedLastFmLoad {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "AcceptedLastFmLoad::Unavailable",
            Self::Available { .. } => "AcceptedLastFmLoad::Available(<redacted>)",
            Self::Consumed { .. } => "AcceptedLastFmLoad::Consumed(<redacted>)",
        })
    }
}

/// History state owned by one genuine playback occurrence.
///
/// Output generations are deliberately only delivery proofs. A retry can
/// replace `accepted_generation` while retaining `progress`; queue navigation
/// and repeat-one instead install a brand-new value.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PlaybackHistoryOccurrence {
    track_id: TrackId,
    progress: PlaybackHistoryProgress,
    accepted_generation: Option<PlayerEventGeneration>,
    playing: bool,
    needs_reanchor: bool,
    /// Initial accepted loads, retries, and Buffering may lack a clean
    /// Playing transition, so their first position is authoritative playback
    /// proof. Explicit Paused/Stopped states revoke that permission: remote
    /// status polls may keep advancing even though no audio is playing.
    position_may_prove_playing: bool,
}

impl PlaybackHistoryOccurrence {
    fn from_item(item: &QueueItem) -> Option<Self> {
        is_library_source(item.identity.media_key.source_id).then(|| Self {
            track_id: item.identity.media_key.track_id.clone(),
            progress: PlaybackHistoryProgress::new(item.duration_ms),
            accepted_generation: None,
            playing: false,
            needs_reanchor: true,
            position_may_prove_playing: false,
        })
    }

    fn retire_delivery(&mut self) {
        self.accepted_generation = None;
        self.playing = false;
        self.needs_reanchor = true;
        self.position_may_prove_playing = false;
    }
}

/// Maximum number of real queue occurrences retained before the current one.
const SHUFFLE_PRIOR_LIMIT: usize = 10;
const SHUFFLE_TIMELINE_CAPACITY: usize = SHUFFLE_PRIOR_LIMIT + 1;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShuffleState {
    /// Bounded chronological playback timeline. `cursor` identifies the
    /// current occurrence; entries after it are fixed forward history created
    /// by Previous and must be replayed before another random draw.
    history: VecDeque<usize>,
    cursor: usize,
    /// Queue occurrences not yet drawn from the active shuffle cycle.
    remaining: Vec<usize>,
}

impl ShuffleState {
    fn current(&self) -> Option<usize> {
        self.history.get(self.cursor).copied()
    }

    fn step_forward(&mut self) -> Option<usize> {
        let next = self.cursor.checked_add(1)?;
        let selected = self.history.get(next).copied()?;
        self.cursor = next;
        Some(selected)
    }

    fn step_back(&mut self) -> Option<usize> {
        self.cursor = self.cursor.checked_sub(1)?;
        self.current()
    }

    fn record_selection(&mut self, selected: usize) {
        debug_assert_eq!(self.cursor.checked_add(1), Some(self.history.len()));
        self.history.push_back(selected);
        if self.history.len() > SHUFFLE_TIMELINE_CAPACITY {
            let removed = self.history.pop_front();
            debug_assert!(removed.is_some());
        }
        // A recorded selection is always the timeline frontier. Deriving the
        // cursor from the retained deque avoids arithmetic underflow while
        // restoring that invariant even if a caller arrived with stale state.
        self.cursor = self.history.len() - 1;
    }
}

/// Playback-owned queue and cursor.
///
/// A session is replaced only when the user explicitly starts a track (or an
/// external file), reaches the unrepeated end, stops playback, or changes the
/// output target. Sorting, filtering, and sidebar navigation never mutate it.
/// Sequential Next/Previous follow snapshot order; repeat-all wraps that
/// snapshot. Shuffle visits every snapshot occurrence once per cycle, retains
/// the current occurrence plus ten real predecessors, and replays fixed
/// forward history after Previous before drawing again. Reaching the retained
/// boundary never invents a predecessor. Repeat-one is an EOS policy
/// implemented by [`replay_current`], so manual Next still moves.
#[derive(Debug, Default)]
pub struct PlaybackSession {
    queue: Vec<QueueItem>,
    current_index: Option<usize>,
    shuffle: Option<ShuffleState>,
    event_generation: PlayerEventGeneration,
    /// A protected source reference is being resolved for this generation.
    /// Play/toggle requests are accepted as no-ops until the exact request is
    /// handed to the output, so they cannot revive a superseded track.
    pending_resolution: Option<PlayerEventGeneration>,
    /// The current item failed before reaching an output or was synchronously
    /// rejected by it. A later Play/Toggle retries the load (and protected
    /// resolution when applicable) instead of issuing `play()` to an output
    /// that has no loaded media.
    resolution_failed: bool,
    /// Move-only one-shot proof for the exact current generation whose media
    /// load the output accepted. Resolution completion alone never sets this
    /// proof.
    accepted_lastfm_load: AcceptedLastFmLoad,
    /// Freshness-only predecessor suspension installed by tentative queue
    /// navigation or Repeat One. Rollback drops it inert; the first committed
    /// output attempt consumes it before advancing generation ownership.
    pending_lastfm_output_suspension: Option<AcceptedLastFmLoadSuspension>,
    /// Durable-history accounting for the current local queue occurrence.
    /// Remote, removable, radio, and external items deliberately leave this
    /// empty even when their backend-native track ID resembles a local ID.
    history_occurrence: Option<PlaybackHistoryOccurrence>,
    /// Frozen candidate shared by accepted retries of the current genuine
    /// queue occurrence. Queue navigation and Repeat One replace it even when
    /// the stable media key and metadata are unchanged.
    lastfm_occurrence_candidate: Option<LastFmOccurrenceCandidate>,
}

impl Drop for PlaybackSession {
    fn drop(&mut self) {
        // An accepted load may already have been extracted into a delayed
        // coordinator task. Closing the owning playback session must revoke
        // that shared freshness gate even when teardown bypasses `clear`.
        self.accepted_lastfm_load.revoke();
    }
}

impl PlaybackSession {
    pub(crate) fn replace_queue(&mut self, queue: Vec<QueueItem>, start_index: usize) -> bool {
        if queue.get(start_index).is_none() {
            return false;
        }
        self.queue = queue;
        self.current_index = Some(start_index);
        self.shuffle = None;
        self.pending_resolution = None;
        self.resolution_failed = false;
        self.begin_history_occurrence_for_current();
        true
    }

    pub fn clear(&mut self) {
        // Close every extracted accepted-load clone before mutating terminal
        // session ownership. A concurrent coordinator claim can therefore
        // never attach the predecessor after Stop/source/output retirement.
        self.revoke_accepted_lastfm_load();
        self.pending_lastfm_output_suspension = None;
        self.queue.clear();
        self.current_index = None;
        self.shuffle = None;
        self.event_generation = self.event_generation.next();
        self.pending_resolution = None;
        self.resolution_failed = false;
        self.history_occurrence = None;
        self.lastfm_occurrence_candidate = None;
    }

    /// Start a fresh shuffle traversal without changing the queue or current
    /// item. The UI calls this for either direction of a shuffle toggle, so an
    /// old randomized path cannot unexpectedly reappear after a mode change.
    pub(crate) fn reset_shuffle_navigation(&mut self) {
        self.shuffle = None;
    }

    /// Clear playback only when the current queue belongs to `source_id`.
    ///
    /// Remote source replacement and removal retire every opaque reference
    /// captured by that queue. Keeping it would strand Next/Previous on a
    /// revoked lease, while retargeting it could cross into a different login
    /// or library that reused the same server-native track identifiers.
    pub(crate) fn clear_if_source(&mut self, source_id: &str) -> bool {
        if self
            .queue
            .iter()
            .all(|item| !identity_is_owned_by_source(&item.identity, source_id))
        {
            return false;
        }
        self.clear();
        true
    }

    /// Clear a mixed regular-playlist queue only when it retained a guard
    /// minted by the replaced catalogue. Ordinary source queues intentionally
    /// keep their epoch-based behavior across same-session refreshes.
    pub(crate) fn clear_if_playlist_authority(&mut self, source_id: SourceId) -> bool {
        if self.queue.iter().all(|item| {
            item.regular_playlist_guard
                .is_none_or(|guard| guard.source_id() != source_id)
        }) {
            return false;
        }
        self.clear();
        true
    }

    /// Stable local-library IDs currently retained by the queue.
    ///
    /// A full library snapshot can be very large. Publishing this small set
    /// lets the GTK receiver avoid cloning refresh metadata for tracks the
    /// queue does not own, while preserving source namespacing.
    pub(crate) fn library_track_ids(&self) -> HashSet<&TrackId> {
        self.queue
            .iter()
            .filter(|item| is_library_source(item.identity.media_key.source_id))
            .map(|item| &item.identity.media_key.track_id)
            .collect()
    }

    /// Refresh display metadata for queued library identities whose current
    /// database row changed.
    ///
    /// File locations are deliberately absent from local/playlist queue items;
    /// playback resolves the current row by ID at every load. This update can
    /// therefore change only the metadata snapshot, never install a locator.
    ///
    /// Items are rewritten in place. Queue length, order, and the cursor are the
    /// coordinate system that `current_index` and the shuffle history index
    /// into, so identity — not position — is what an update may address.
    pub(crate) fn refresh_library_tracks(
        &mut self,
        updates: &HashMap<TrackId, QueueTrackRefresh>,
    ) -> usize {
        if updates.is_empty() {
            return 0;
        }

        let mut refreshed = 0;
        for item in &mut self.queue {
            if !is_library_source(item.identity.media_key.source_id) {
                continue;
            }
            let Some(update) = updates.get(&item.identity.media_key.track_id) else {
                continue;
            };
            if item.title == update.title
                && item.artist == update.artist
                && item.album == update.album
                && item.album_artist == update.album_artist
                && item.track_number == update.track_number
                && item.duration_secs == update.duration_secs
                && item.cover_art_url == update.cover_art_url
            {
                continue;
            }

            item.title = update.title.clone();
            item.artist = update.artist.clone();
            item.album = update.album.clone();
            item.album_artist = update.album_artist.clone();
            item.track_number = update.track_number;
            item.duration_secs = update.duration_secs;
            item.cover_art_url = update.cover_art_url.clone();
            refreshed += 1;
        }

        refreshed
    }

    pub fn has_current(&self) -> bool {
        self.current().is_some()
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.current_index.and_then(|index| self.queue.get(index))
    }

    pub fn current_identity(&self) -> Option<&PlaybackIdentity> {
        self.current().map(|item| &item.identity)
    }

    /// The generation currently authorized to publish player events.
    ///
    /// Seek callers capture this immediately before issuing the output seek,
    /// then pass it back to [`Self::observe_history_seek`]. A simultaneous
    /// queue transition therefore turns the observation into a no-op instead
    /// of retargeting it to the replacement occurrence.
    pub(crate) const fn current_event_generation(&self) -> PlayerEventGeneration {
        self.event_generation
    }

    /// Observe one generation-owned player event for durable local history.
    ///
    /// Only a successfully accepted output load may earn credit. Playback
    /// state and delivery generation are independent: pause, buffering,
    /// retry, and resume re-anchor the next position without replacing the
    /// occurrence, while stale and rejected deliveries cannot contribute.
    /// The exact stable local [`TrackId`] is returned once, at the point the
    /// occurrence first qualifies for persistence.
    pub(crate) fn observe_history_event(&mut self, event: &PlayerEvent) -> Option<TrackId> {
        let generation = event.generation();
        if !self.accepts_event_generation(generation) {
            return None;
        }
        if matches!(
            event,
            PlayerEvent::Error { .. } | PlayerEvent::TrackEnded { .. }
        ) {
            self.revoke_accepted_lastfm_load();
        }

        let current_track_id = self.current().and_then(|item| {
            is_library_source(item.identity.media_key.source_id)
                .then(|| item.identity.media_key.track_id.clone())
        })?;
        let occurrence = self.history_occurrence.as_mut()?;
        if occurrence.track_id != current_track_id
            || occurrence.accepted_generation != Some(generation)
        {
            return None;
        }

        let counted = match event {
            PlayerEvent::StateChanged { state, .. } => {
                match state {
                    PlayerState::Playing => {
                        if !occurrence.playing {
                            occurrence.needs_reanchor = true;
                        }
                        occurrence.playing = true;
                        occurrence.position_may_prove_playing = false;
                    }
                    PlayerState::Buffering => {
                        occurrence.playing = false;
                        occurrence.needs_reanchor = true;
                        occurrence.position_may_prove_playing = true;
                    }
                    PlayerState::Stopped | PlayerState::Paused => {
                        occurrence.playing = false;
                        occurrence.needs_reanchor = true;
                        occurrence.position_may_prove_playing = false;
                    }
                }
                false
            }
            PlayerEvent::PositionChanged {
                position_ms,
                duration_ms,
                ..
            } => {
                let duration_counted = occurrence.progress.observe_duration(*duration_ms);
                let position_counted = if !occurrence.playing || occurrence.needs_reanchor {
                    occurrence.progress.observe_reanchor(*position_ms);
                    if occurrence.playing {
                        // Explicit Playing after a pause/resume re-anchors its
                        // first sample before later positions earn credit.
                        occurrence.needs_reanchor = false;
                    } else if occurrence.position_may_prove_playing {
                        // Some accepted remote loads and Buffering recoveries
                        // never publish a clean Playing transition. Their
                        // first position proves playback but earns no credit.
                        occurrence.playing = true;
                        occurrence.needs_reanchor = false;
                        occurrence.position_may_prove_playing = false;
                    }
                    false
                } else {
                    occurrence.progress.observe_position(*position_ms)
                };
                duration_counted || position_counted
            }
            PlayerEvent::TrackEnded { .. } => {
                occurrence.playing = false;
                occurrence.needs_reanchor = true;
                occurrence.position_may_prove_playing = false;
                occurrence.progress.observe_natural_end()
            }
            PlayerEvent::Error { .. } => {
                occurrence.retire_delivery();
                false
            }
        };

        counted.then_some(current_track_id)
    }

    /// Re-anchor one accepted local occurrence around an explicit user seek.
    ///
    /// `actual_position_ms` is sampled before the command and intentionally
    /// earns no unsampled credit. Comparing `target_ms` with that real anchor
    /// records only genuine forward-skip evidence. The next output position is
    /// re-anchored again because a backend may land on a nearby keyframe.
    pub(crate) fn observe_history_seek(
        &mut self,
        generation: PlayerEventGeneration,
        actual_position_ms: u64,
        target_ms: u64,
    ) -> bool {
        if !self.accepts_event_generation(generation) {
            return false;
        }
        let Some(current_track_id) = self.current().and_then(|item| {
            is_library_source(item.identity.media_key.source_id)
                .then(|| item.identity.media_key.track_id.clone())
        }) else {
            return false;
        };
        let Some(occurrence) = self.history_occurrence.as_mut() else {
            return false;
        };
        if occurrence.track_id != current_track_id
            || occurrence.accepted_generation != Some(generation)
        {
            return false;
        }

        occurrence.progress.observe_reanchor(actual_position_ms);
        occurrence.progress.observe_seek(target_ms);
        occurrence.needs_reanchor = true;
        true
    }

    fn begin_history_occurrence_for_current(&mut self) {
        self.install_history_occurrence_for_current();
    }

    fn install_history_occurrence_for_current(&mut self) {
        self.lastfm_occurrence_candidate = self
            .current()
            .and_then(QueueItem::lastfm_occurrence_candidate);
        self.history_occurrence = self
            .current()
            .and_then(PlaybackHistoryOccurrence::from_item);
    }

    fn begin_repeat_one_occurrence(&mut self) {
        // Repeat One is installed tentatively: a failure before handoff must
        // restore the predecessor's move-only accepted-load proof. The caller
        // owns that proof until the new occurrence either commits or rolls
        // back.
        debug_assert!(matches!(
            &self.accepted_lastfm_load,
            AcceptedLastFmLoad::Unavailable
        ));
        self.install_history_occurrence_for_current();
    }

    fn revoke_accepted_lastfm_load(&mut self) {
        self.accepted_lastfm_load.revoke();
    }

    /// Consume the one-shot Last.fm handoff proof only after its exact current
    /// output generation crossed [`Self::mark_load_accepted`]. Resolution
    /// completion alone is insufficient. The proof is consumed before source
    /// eligibility or metadata validation, so an ineligible or invalid load
    /// cannot be retrieved again. This method never consults mutable GTK rows,
    /// output duration, a filename, URI, or backend lookup.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn take_accepted_lastfm_output_load(
        &mut self,
        generation: PlayerEventGeneration,
    ) -> Option<LastFmAcceptedOutputLoad> {
        if self.pending_resolution.is_some()
            || self.resolution_failed
            || !self.accepts_event_generation(generation)
        {
            return None;
        }
        let freshness = self.accepted_lastfm_load.take(generation)?;
        Some(
            match self
                .lastfm_occurrence_candidate
                .as_ref()
                .and_then(|candidate| candidate.accepted().ok())
            {
                Some(accepted) => LastFmAcceptedOutputLoad::eligible(
                    LastFmAcceptedOutputMint::issue(),
                    generation,
                    freshness,
                    accepted,
                ),
                None => LastFmAcceptedOutputLoad::ineligible(
                    LastFmAcceptedOutputMint::issue(),
                    generation,
                    freshness,
                ),
            },
        )
    }

    fn mark_history_load_accepted(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution.is_some()
            || self.resolution_failed
            || !self.accepts_event_generation(generation)
        {
            return false;
        }
        let Some(current_track_id) = self.current().and_then(|item| {
            is_library_source(item.identity.media_key.source_id)
                .then(|| item.identity.media_key.track_id.clone())
        }) else {
            return false;
        };
        let Some(occurrence) = self.history_occurrence.as_mut() else {
            return false;
        };
        if occurrence.track_id != current_track_id {
            return false;
        }

        occurrence.accepted_generation = Some(generation);
        occurrence.playing = false;
        occurrence.needs_reanchor = true;
        occurrence.position_may_prove_playing = true;
        true
    }

    /// Record the synchronous output-acceptance boundary for every source.
    /// Local history attaches to the same generation when applicable.
    fn mark_load_accepted(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution.is_some()
            || self.resolution_failed
            || !self.accepts_event_generation(generation)
        {
            return false;
        }
        // Re-observing acceptance for one generation must not reopen a proof
        // that was already consumed.
        self.accepted_lastfm_load.install(generation);
        let _ = self.mark_history_load_accepted(generation);
        true
    }

    fn finish_output_load(&mut self, generation: PlayerEventGeneration, accepted: bool) -> bool {
        if accepted {
            self.mark_load_accepted(generation)
        } else {
            self.mark_load_rejected(generation)
        }
    }

    /// Consume and revoke one exact accepted proof without constructing or
    /// validating occurrence metadata. Dormant and closed coordinators use
    /// this path so disabled Last.fm support does no attribution work.
    fn discard_accepted_lastfm_output_load(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution.is_some()
            || self.resolution_failed
            || !self.accepts_event_generation(generation)
        {
            return false;
        }
        let Some(freshness) = self.accepted_lastfm_load.take(generation) else {
            return false;
        };
        freshness.revoke();
        true
    }

    /// Exact hidden external-file source currently owned by playback.
    ///
    /// This must be read before `clear`: baseline visibility is not terminal
    /// ownership, and a random source identity is not by itself proof that a
    /// queue item came from the OS-open adapter.
    pub(crate) fn current_external_source_id(&self) -> Option<SourceId> {
        self.current()
            .filter(|item| item.external_session)
            .map(|item| item.identity.media_key.source_id)
    }

    /// Decide terminal retirement from exact playback ownership.
    ///
    /// Repeat-one and repeat-all completion are not terminal. Delayed events
    /// from an older output generation can never retire the current source.
    pub(crate) fn external_source_for_terminal(
        &self,
        generation: PlayerEventGeneration,
        repeated: bool,
    ) -> Option<SourceId> {
        (!repeated && self.accepts_event_generation(generation))
            .then(|| self.current_external_source_id())
            .flatten()
    }

    fn begin_output_attempt(&mut self) -> OutputAttempt {
        let previous_generation = self.event_generation;

        // A tentative predecessor snapshot is revoked only at this commit
        // point. This must happen before the current proof, coordinator, or
        // output can observe the successor attempt.
        if let Some(suspension) = self.pending_lastfm_output_suspension.take() {
            suspension.commit();
        }
        self.revoke_accepted_lastfm_load();
        self.event_generation = previous_generation.next();
        self.pending_resolution = None;
        self.resolution_failed = false;
        if let Some(occurrence) = self.history_occurrence.as_mut() {
            occurrence.retire_delivery();
        }
        let intent = LastFmOutputIntent::new(
            LastFmOutputIntentMint::issue(),
            previous_generation,
            self.event_generation,
            self.lastfm_occurrence_candidate
                .as_ref()
                .map(|candidate| candidate.identity.clone()),
        );
        OutputAttempt {
            generation: self.event_generation,
            intent,
        }
    }

    #[cfg(test)]
    fn begin_event_generation(&mut self) -> PlayerEventGeneration {
        self.begin_output_attempt().generation
    }

    /// Hand the current direct item to an output under a fresh event owner.
    ///
    /// This is the production boundary shared by initial playback, queue
    /// navigation, EOS replay, and a retry after synchronous rejection. The
    /// queue cursor supplies the URI; callers cannot accidentally load a row
    /// from the mutable GTK projection instead. A rejected load retains that
    /// exact item and generation as retryable, while a later attempt advances
    /// ownership before it calls the output again.
    fn prepare_current_direct_load(&mut self) -> Option<PreparedDirectLoad> {
        let uri = self.current()?.uri.clone();
        if uri.is_empty() || self.current()?.source_session_epoch.is_some() {
            return None;
        }
        let attempt = self.begin_output_attempt();
        Some(PreparedDirectLoad {
            uri,
            generation: attempt.generation,
            intent: attempt.intent,
        })
    }

    #[cfg(test)]
    pub(super) fn load_current_direct(
        &mut self,
        output: &dyn AudioOutput,
    ) -> Option<DirectLoadAttempt> {
        let PreparedDirectLoad {
            uri,
            generation,
            intent: _,
        } = self.prepare_current_direct_load()?;
        output.set_event_generation(generation);
        let accepted = output.load_uri(&uri);
        let marked = self.finish_output_load(generation, accepted);
        debug_assert!(marked, "current direct load owns playback delivery state");
        Some(DirectLoadAttempt {
            generation,
            accepted,
        })
    }

    fn begin_pending_resolution_attempt(&mut self) -> OutputAttempt {
        let attempt = self.begin_output_attempt();
        self.pending_resolution = Some(attempt.generation);
        self.resolution_failed = false;
        attempt
    }

    #[cfg(test)]
    fn begin_pending_resolution(&mut self) -> PlayerEventGeneration {
        self.begin_pending_resolution_attempt().generation
    }

    /// Claim a completed resolution only while its queue item/generation still
    /// owns playback. Stop, Next, output replacement, and a newer replay all
    /// invalidate this proof before they can reach the output boundary.
    fn finish_pending_resolution(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution != Some(generation) || !self.accepts_event_generation(generation)
        {
            return false;
        }
        self.pending_resolution = None;
        self.resolution_failed = false;
        true
    }

    fn fail_pending_resolution(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution != Some(generation) || !self.accepts_event_generation(generation)
        {
            return false;
        }
        self.pending_resolution = None;
        self.resolution_failed = true;
        self.revoke_accepted_lastfm_load();
        if let Some(occurrence) = self.history_occurrence.as_mut() {
            occurrence.retire_delivery();
        }
        true
    }

    /// Cancel the current protected resolution while keeping its queue item
    /// available for a later Play retry.
    ///
    /// No media has reached the output yet, so pausing the output cannot
    /// preserve the user's intent. Clearing the exact pending claim makes its
    /// async completion a no-op; `resolution_failed` routes the next Play
    /// through a fresh playback-time resolution instead of calling `play()` on
    /// an empty output.
    pub(crate) fn cancel_pending_resolution_for_retry(&mut self) -> bool {
        if !self.is_resolution_pending() {
            return false;
        }
        self.pending_resolution = None;
        self.resolution_failed = true;
        self.revoke_accepted_lastfm_load();
        if let Some(occurrence) = self.history_occurrence.as_mut() {
            occurrence.retire_delivery();
        }
        true
    }

    /// Retain a synchronously rejected item as an explicit retryable load.
    ///
    /// The generation remains current so the output's required actionable
    /// error is still displayed. A later Play begins a fresh generation and
    /// attempts the load again instead of toggling an empty output session.
    fn mark_load_rejected(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution.is_some() || !self.accepts_event_generation(generation) {
            return false;
        }
        self.resolution_failed = true;
        self.revoke_accepted_lastfm_load();
        if let Some(occurrence) = self.history_occurrence.as_mut() {
            occurrence.retire_delivery();
        }
        true
    }

    /// Turn an ID/session-resolved output failure into a fresh-resolution retry
    /// state.
    ///
    /// Resolution has already finished by the time a protected request or
    /// exact local file URI reaches an output, so proxy startup, daemon,
    /// receiver, decoder, or filesystem failures arrive through
    /// `PlayerEvent::Error`. Without this transition, a later Play would call
    /// `play()` on an output that may never have accepted media instead of
    /// resolving the identity again. Advancing the event generation also
    /// rejects delayed state emitted by the failed load.
    pub(crate) fn mark_resolved_load_failed(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution.is_some()
            // A synchronously rejected load is already retryable and never
            // created output state to stop. Keep its generation current so
            // the queued guidance remains visible without issuing cleanup.
            || self.resolution_failed
            || !self.accepts_event_generation(generation)
            || !self.current().is_some_and(|item| {
                is_library_source(item.identity.media_key.source_id)
                    || item.source_session_epoch.is_some()
            })
        {
            return false;
        }

        self.event_generation = self.event_generation.next();
        self.resolution_failed = true;
        self.revoke_accepted_lastfm_load();
        if let Some(occurrence) = self.history_occurrence.as_mut() {
            occurrence.retire_delivery();
        }
        true
    }

    fn is_resolution_pending(&self) -> bool {
        self.pending_resolution == Some(self.event_generation) && self.has_current()
    }

    pub fn accepts_event_generation(&self, generation: PlayerEventGeneration) -> bool {
        self.has_current() && self.event_generation == generation
    }

    fn initialize_shuffle(&mut self) {
        let Some(current) = self.current_index else {
            return;
        };
        let mut remaining: Vec<usize> = (0..self.queue.len())
            .filter(|&index| index != current)
            .collect();
        fastrand::shuffle(&mut remaining);
        self.shuffle = Some(ShuffleState {
            history: VecDeque::from([current]),
            cursor: 0,
            remaining,
        });
    }

    /// Start a complete Repeat All cycle while avoiding an immediate repeat at
    /// the rollover boundary. Unlike the initial cycle (whose manually chosen
    /// current item is already counted), every later cycle contains every
    /// queue occurrence exactly once.
    fn refill_shuffle_cycle(state: &mut ShuffleState, queue_len: usize, current: usize) {
        state.remaining = (0..queue_len).collect();
        fastrand::shuffle(&mut state.remaining);
        if queue_len > 1 && state.remaining.last() == Some(&current) {
            // Condition the uniform permutation on a different first draw by
            // swapping the boundary item with a uniformly selected earlier
            // slot. Choosing a fixed slot would bias otherwise valid cycles.
            let replacement = fastrand::usize(0..(queue_len - 1));
            state.remaining.swap(replacement, queue_len - 1);
        }
    }

    fn advance(&mut self, repeat_mode: RepeatMode, shuffle: bool) -> Option<usize> {
        let current = self.current_index?;

        if !shuffle {
            self.shuffle = None;
            let next = current + 1;
            let selected = if next < self.queue.len() {
                next
            } else if repeat_mode == RepeatMode::All {
                0
            } else {
                return None;
            };
            self.current_index = Some(selected);
            self.pending_resolution = None;
            self.resolution_failed = false;
            self.begin_history_occurrence_for_current();
            return Some(selected);
        }

        if self.shuffle.is_none() {
            self.initialize_shuffle();
        }

        let state = self.shuffle.as_mut()?;
        if let Some(selected) = state.step_forward() {
            self.current_index = Some(selected);
            self.pending_resolution = None;
            self.resolution_failed = false;
            self.begin_history_occurrence_for_current();
            return Some(selected);
        }

        if state.remaining.is_empty() {
            if repeat_mode != RepeatMode::All {
                return None;
            }

            // A one-item queue repeats itself under repeat-all.
            if self.queue.len() == 1 {
                state.record_selection(current);
                self.pending_resolution = None;
                self.resolution_failed = false;
                self.begin_history_occurrence_for_current();
                return Some(current);
            }

            Self::refill_shuffle_cycle(state, self.queue.len(), current);
        }

        let selected = state.remaining.pop()?;
        state.record_selection(selected);
        self.current_index = Some(selected);
        self.pending_resolution = None;
        self.resolution_failed = false;
        self.begin_history_occurrence_for_current();
        Some(selected)
    }

    fn previous(&mut self, repeat_mode: RepeatMode, shuffle: bool) -> Option<usize> {
        let current = self.current_index?;

        if !shuffle {
            self.shuffle = None;
            let selected = if current > 0 {
                current - 1
            } else if repeat_mode == RepeatMode::All {
                self.queue.len().checked_sub(1)?
            } else {
                return None;
            };
            self.current_index = Some(selected);
            self.pending_resolution = None;
            self.resolution_failed = false;
            self.begin_history_occurrence_for_current();
            return Some(selected);
        }

        if self.shuffle.is_none() {
            self.initialize_shuffle();
        }
        let state = self.shuffle.as_mut()?;
        let selected = state.step_back()?;
        self.current_index = Some(selected);
        self.pending_resolution = None;
        self.resolution_failed = false;
        self.begin_history_occurrence_for_current();
        Some(selected)
    }

    #[cfg(test)]
    pub(super) fn advance_for_test(
        &mut self,
        repeat_mode: RepeatMode,
        shuffle: bool,
    ) -> Option<usize> {
        self.advance(repeat_mode, shuffle)
    }

    #[cfg(test)]
    pub(super) fn previous_for_test(
        &mut self,
        repeat_mode: RepeatMode,
        shuffle: bool,
    ) -> Option<usize> {
        self.previous(repeat_mode, shuffle)
    }
}

struct OutputAttempt {
    generation: PlayerEventGeneration,
    intent: LastFmOutputIntent,
}

struct PreparedDirectLoad {
    uri: String,
    generation: PlayerEventGeneration,
    intent: LastFmOutputIntent,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectLoadAttempt {
    pub(super) generation: PlayerEventGeneration,
    pub(super) accepted: bool,
}

#[derive(Debug)]
struct CapturedQueue {
    items: Vec<QueueItem>,
    selected_index: usize,
}

fn row_playback_identity(
    view: &QueueView,
    track: &TrackObject,
) -> Option<(PlaybackIdentity, Option<RegularPlaylistCatalogueGuard>)> {
    if let Some(binding) = track.playlist_occurrence_binding() {
        let track_id = binding.track_id()?.clone();
        let guard = match binding.state() {
            PlaylistOccurrenceState::AvailableLocal => None,
            PlaylistOccurrenceState::AvailableRemote(guard) => Some(guard),
            PlaylistOccurrenceState::Unavailable(_) => return None,
        };
        return Some((
            PlaybackIdentity::new(view, binding.source_id(), track_id),
            guard,
        ));
    }

    let source_id = track.source_id().unwrap_or(view.source_id);
    let track_id = TrackId::new(track.track_id()).ok()?;
    Some((PlaybackIdentity::new(view, source_id, track_id), None))
}

fn row_position_identity(view: &QueueView, track: &TrackObject) -> Option<(u64, MediaKey)> {
    let (identity, _) = row_playback_identity(view, track)?;
    Some((track.row_instance_id(), identity.media_key))
}

fn playable_model_positions(model: &impl IsA<gtk::gio::ListModel>, source_key: &str) -> Vec<u32> {
    let Some(view) = queue_view(source_key) else {
        return Vec::new();
    };
    (0..model.n_items())
        .filter(|position| {
            model
                .item(*position)
                .and_downcast::<TrackObject>()
                .is_some_and(|track| row_playback_identity(&view, &track).is_some())
        })
        .collect()
}

/// Capture the current sorted/filtered projection as a playback-owned queue.
///
/// All entry points that start playback from the track list go through this
/// function. Once returned, the queue has no dependency on the mutable GTK
/// model: sorting, filtering, rebuilding, or navigating to another source can
/// no longer change the current identity or the meaning of Next/Previous.
fn capture_visible_queue(
    model: &impl IsA<gtk::gio::ListModel>,
    source_key: &str,
    selected_position: u32,
    source_registry: &SourceRegistry,
) -> Option<CapturedQueue> {
    let view = queue_view(source_key)?;
    let mut selected_index = None;
    let mut items = Vec::with_capacity(model.n_items() as usize);
    let mut occurrences: HashMap<MediaKey, usize> = HashMap::new();
    // Persisted per-remote consent is intentionally not part of this slice.
    // The registry's closed policy therefore admits only intrinsically
    // eligible managed sources (currently removable media for visible rows).
    let enabled_remote_sources = HashSet::new();

    for model_index in 0..model.n_items() {
        let Some(track) = model.item(model_index).and_downcast::<TrackObject>() else {
            continue;
        };
        let Some((identity, regular_playlist_guard)) = row_playback_identity(&view, &track) else {
            continue;
        };
        if model_index == selected_position {
            selected_index = Some(items.len());
        }
        let playback_source = match regular_playlist_guard {
            Some(guard) => source_registry.mint_regular_playlist_playback_source(
                identity.media_key.clone(),
                guard,
                &enabled_remote_sources,
            ),
            None => track.source_session_epoch().and_then(|session_epoch| {
                source_registry.mint_session_playback_source(
                    identity.media_key.clone(),
                    session_epoch,
                    &enabled_remote_sources,
                )
            }),
        };
        let occurrence = occurrences.entry(identity.media_key.clone()).or_default();
        items.push(QueueItem::from_track(
            identity,
            &track,
            *occurrence,
            regular_playlist_guard,
            playback_source,
        ));
        *occurrence += 1;
    }

    Some(CapturedQueue {
        items,
        selected_index: selected_index?,
    })
}

/// Retire queue/event ownership before asking the active output to stop.
///
/// Both the explicit Stop control and output replacement use this ordering, so
/// a backend that publishes `Stopped` synchronously cannot mutate the cleared
/// or replacement session.
#[cfg(test)]
pub(super) fn stop_owned_playback(session: &mut PlaybackSession, output: &dyn AudioOutput) {
    session.clear();
    output.stop();
}

/// Debounce state for the play-button buffering spinner.
///
/// A reset or newer player state increments the token. Timeout callbacks keep
/// their original token and therefore become harmless no-ops when playback is
/// stopped, reaches EOS, changes output, or advances to another track.
#[derive(Debug, Default)]
pub struct BufferingTracker {
    generation: Cell<u64>,
    buffering: Cell<bool>,
}

impl BufferingTracker {
    pub fn begin(&self) -> u64 {
        let generation = self.invalidate();
        self.buffering.set(true);
        generation
    }

    pub fn invalidate(&self) -> u64 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.buffering.set(false);
        generation
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.buffering.get() && self.generation.get() == generation
    }

    pub fn is_buffering(&self) -> bool {
        self.buffering.get()
    }
}

/// Shared state for playback operations.
///
/// Passed to [`play_track_at`] and [`advance_track`] so they can load
/// tracks, update the now-playing UI, and track the current position.
pub struct PlaybackContext {
    pub model: gtk::SortListModel,
    pub active_source_key: Rc<RefCell<String>>,
    pub active_output: Rc<RefCell<Box<dyn AudioOutput>>>,
    pub album_art: gtk::Image,
    pub title_label: gtk::Label,
    pub artist_label: gtk::Label,
    pub media_ctrl: Rc<RefCell<Option<crate::desktop_integration::MediaController>>>,
    pub session: Rc<RefCell<PlaybackSession>>,
    /// Current in-process configuration. Local resolution snapshots its roots
    /// before background work and rechecks them immediately before output.
    pub app_config: Rc<RefCell<super::preferences::AppConfig>>,
    /// Tokio runtime used for exact local-database resolution without
    /// blocking GTK's main thread.
    pub rt_handle: tokio::runtime::Handle,
    /// Sole at-use resolver and lifecycle authority for source-owned media.
    pub source_registry: crate::source_registry::SourceRegistry,
    /// Cloneable window binding to the single process-lifetime Last.fm
    /// playback coordinator. It remains inert while the feature is dormant.
    pub lastfm_playback: LastFmPlaybackCoordinatorBinding,
    /// The tracklist `ColumnView` — used to scroll the currently
    /// playing row into view on track change so the user doesn't lose
    /// their place when sequential / shuffled playback advances.
    pub column_view: gtk::ColumnView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayRequest {
    Resume,
    StartAt(u32),
    Unavailable,
}

fn resolve_play_request(has_current: bool, item_count: u32, shuffle: bool) -> PlayRequest {
    if has_current {
        return PlayRequest::Resume;
    }
    if item_count == 0 {
        return PlayRequest::Unavailable;
    }
    PlayRequest::StartAt(if shuffle {
        fastrand::u32(..item_count)
    } else {
        0
    })
}

/// Read the session behind a function boundary so its `Ref` is released before
/// the caller handles a request that may replace the queue.
fn resolve_session_play_request(
    session: &RefCell<PlaybackSession>,
    item_count: u32,
    shuffle: bool,
) -> PlayRequest {
    resolve_play_request(session.borrow().has_current(), item_count, shuffle)
}

/// Apply one Play/Toggle command only while its captured generation still
/// owns playback, re-anchoring Last.fm evidence before touching the output.
///
/// The callbacks are deliberately sequential: neither the session gate nor
/// coordinator ingress can retain a `RefCell` borrow across an output method,
/// including an output that publishes a synchronous callback.
fn apply_current_output_control(
    generation: PlayerEventGeneration,
    accepts: impl FnOnce(PlayerEventGeneration) -> bool,
    observe_discontinuity: impl FnOnce(PlayerEventGeneration),
    control_output: impl FnOnce(),
) -> bool {
    if !accepts(generation) {
        return false;
    }
    observe_discontinuity(generation);
    control_output();
    true
}

fn control_current_output(ctx: &PlaybackContext, control: impl FnOnce(&dyn AudioOutput)) -> bool {
    let generation = ctx.session.borrow().current_event_generation();
    apply_current_output_control(
        generation,
        |generation| ctx.session.borrow().accepts_event_generation(generation),
        |generation| {
            let _ = ctx.lastfm_playback.observe_discontinuity(generation);
        },
        || {
            let output = ctx.active_output.borrow();
            control(output.as_ref());
        },
    )
}

/// Try to play the track at `position` in the given model.
///
/// Captures the visible sorted model as an immutable playback queue, then
/// starts the selected item. Later view mutations do not alter that queue.
pub fn play_track_at(position: u32, ctx: &PlaybackContext) -> bool {
    let source_key = ctx.active_source_key.borrow().clone();
    let Some(captured) =
        capture_visible_queue(&ctx.model, &source_key, position, &ctx.source_registry)
    else {
        return false;
    };
    let selected = &captured.items[captured.selected_index];
    if selected.uri.is_empty()
        && !is_library_source(selected.identity.media_key.source_id)
        && selected.source_session_epoch.is_none()
    {
        warn!("Track has no playable URI");
        return false;
    }

    // A visible-track selection is a newer playback intent than any
    // OS-open admission still parsing in the background.
    super::open_files::invalidate_admission();
    let previous = std::mem::take(&mut *ctx.session.borrow_mut());
    let previous_external = previous.current_external_source_id();
    let predecessor_suspension = previous.accepted_lastfm_load.suspension();
    // The replacement is fresh except for the monotonic event generation.
    // Reusing `Default`'s zero here could let a delayed event from an old
    // output collide after enough whole-session replacements.
    {
        let mut replacement = ctx.session.borrow_mut();
        replacement.event_generation = previous.event_generation;
        replacement.pending_lastfm_output_suspension = predecessor_suspension;
    }
    if !ctx
        .session
        .borrow_mut()
        .replace_queue(captured.items, captured.selected_index)
    {
        *ctx.session.borrow_mut() = previous;
        return false;
    }

    if play_current(ctx) {
        // `play_current` has already committed and published the replacement
        // output intent, so registry retirement cannot race ahead of Last.fm
        // predecessor retirement.
        if let Some(source_id) = previous_external {
            let _ = ctx.source_registry.retire_external(source_id);
        }
        true
    } else if let Some(source_id) = previous_external {
        // A previous external capability is never restored as a retry target.
        // No replacement intent was emitted, so abandon playback explicitly
        // before revoking its registry authority.
        abandon_external_playback(
            &ctx.session,
            &ctx.lastfm_playback,
            &ctx.active_output,
            &ctx.source_registry,
            source_id,
        );
        false
    } else {
        *ctx.session.borrow_mut() = previous;
        false
    }
}

/// Resume the session's current item, or create a new queue from the visible
/// model when playback is idle (including after an OS Stop action).
pub fn play_or_start(ctx: &PlaybackContext, shuffle: bool) -> bool {
    // Play is a newer explicit playback intent than any OS-open delivery
    // still parsing off the GTK thread.
    super::open_files::invalidate_admission();
    if ctx.session.borrow().is_resolution_pending() {
        return true;
    }
    if ctx.session.borrow().resolution_failed {
        return play_current(ctx);
    }
    // Do not borrow the RefCell directly in the match scrutinee: scrutinee
    // temporaries live through the selected arm, and StartAt mutably borrows
    // the same session while installing the newly captured queue.
    match resolve_session_play_request(&ctx.session, ctx.model.n_items(), shuffle) {
        PlayRequest::Resume => control_current_output(ctx, |output| output.play()),
        PlayRequest::StartAt(_) => {
            let source_key = ctx.active_source_key.borrow().clone();
            let playable = playable_model_positions(&ctx.model, &source_key);
            let position = if shuffle {
                playable
                    .get(fastrand::usize(..playable.len().max(1)))
                    .copied()
            } else {
                playable.first().copied()
            };
            position.is_some_and(|position| play_track_at(position, ctx))
        }
        PlayRequest::Unavailable => false,
    }
}

/// Header/OS-toggle behavior: toggle a loaded item, otherwise start a queue.
pub fn toggle_or_start(ctx: &PlaybackContext, shuffle: bool) -> bool {
    // Toggle covers both explicit Pause and Play requests.
    super::open_files::invalidate_admission();
    if ctx.session.borrow().is_resolution_pending() {
        true
    } else if ctx.session.borrow().resolution_failed {
        play_current(ctx)
    } else if ctx.session.borrow().has_current() {
        // The output abstraction does not expose a race-free prediction of
        // which direction Toggle will take. Conservatively re-anchor both
        // Pause and Resume before issuing the exact current-generation
        // command.
        control_current_output(ctx, |output| output.toggle_play_pause())
    } else {
        play_or_start(ctx, shuffle)
    }
}

/// Invalidate the session before stopping the output so synchronously emitted
/// Stopped events are already stale. The caller owns the widget reset.
pub fn stop_playback(ctx: &PlaybackContext) {
    super::open_files::invalidate_admission();
    let external_source = ctx.session.borrow().current_external_source_id();
    ctx.session.borrow_mut().clear();
    let _ = ctx.lastfm_playback.retire(LastFmPlaybackRetirement::Stop);
    ctx.active_output.borrow().stop();
    if let Some(source_id) = external_source {
        let _ = ctx.source_registry.retire_external(source_id);
    }
}

/// Terminally abandon an external occurrence in one fixed authority order.
/// Each callback finishes before the next begins, keeping session and output
/// borrows outside coordinator and registry ingress.
fn apply_external_abandonment(
    clear_session: impl FnOnce(),
    retire_lastfm: impl FnOnce(),
    stop_output: impl FnOnce(),
    retire_source: impl FnOnce(),
) {
    clear_session();
    retire_lastfm();
    stop_output();
    retire_source();
}

fn abandon_external_playback(
    session: &Rc<RefCell<PlaybackSession>>,
    lastfm_playback: &LastFmPlaybackCoordinatorBinding,
    active_output: &Rc<RefCell<Box<dyn AudioOutput>>>,
    source_registry: &SourceRegistry,
    source_id: SourceId,
) {
    apply_external_abandonment(
        || session.borrow_mut().clear(),
        || {
            let _ = lastfm_playback.retire(LastFmPlaybackRetirement::QueueAbandoned);
        },
        || active_output.borrow().stop(),
        || {
            let _ = source_registry.retire_external(source_id);
        },
    );
}

/// Commit one output attempt, release playback-session ownership, and only
/// then notify the GTK-free coordinator. The caller may invoke an output after
/// this returns without carrying a `RefCell` borrow into coordinator ingress.
fn begin_pending_output(ctx: &PlaybackContext) -> PlayerEventGeneration {
    let attempt = ctx.session.borrow_mut().begin_pending_resolution_attempt();
    let generation = attempt.generation;
    let _ = ctx.lastfm_playback.observe_output_intent(attempt.intent);
    generation
}

/// Record one synchronous output result before asking the coordinator to
/// consume it. Active coordination lazily freezes metadata; dormant or closed
/// coordination consumes and revokes the exact proof without constructing it.
fn finish_coordinated_output_load(
    session: &Rc<RefCell<PlaybackSession>>,
    lastfm_playback: &LastFmPlaybackCoordinatorBinding,
    generation: PlayerEventGeneration,
    accepted: bool,
) -> bool {
    if !session
        .borrow_mut()
        .finish_output_load(generation, accepted)
    {
        return false;
    }
    if !accepted {
        return true;
    }

    let build_session = Rc::clone(session);
    let discard_session = Rc::clone(session);
    let _ = lastfm_playback.accept_output_load_lazy(
        generation,
        move || {
            build_session
                .borrow_mut()
                .take_accepted_lastfm_output_load(generation)
        },
        move || {
            let _ = discard_session
                .borrow_mut()
                .discard_accepted_lastfm_output_load(generation);
        },
    );
    true
}

/// Load the current immutable queue item and refresh now-playing UI.
fn play_current(ctx: &PlaybackContext) -> bool {
    let session = ctx.session.borrow();
    let Some(item) = session.current().cloned() else {
        return false;
    };
    let identity = session.current_identity().cloned();
    drop(session);
    if identity
        .as_ref()
        .is_some_and(|identity| is_library_source(identity.media_key.source_id))
    {
        // Stop and supersede the prior output before beginning the async DB
        // lookup. Stop, Next, Previous, and replay all advance the generation,
        // so a late result can never reach a receiver after ownership moves.
        let generation = begin_pending_output(ctx);
        ctx.active_output.borrow().stop();
        ctx.active_output.borrow().set_event_generation(generation);
        update_now_playing_ui(ctx, &item, identity.as_ref(), None);

        let track_id = identity
            .as_ref()
            .map(|identity| identity.media_key.track_id.clone())
            .expect("local queue item has an identity");
        let configured_roots = ctx.app_config.borrow().library_paths.clone();
        let (resolved_tx, resolved_rx) = async_channel::bounded(1);
        ctx.rt_handle.spawn(async move {
            let resolved = match crate::db::connection::init_db().await {
                Ok(db) => {
                    crate::local::resolver::resolve_track(&db, track_id.as_str(), &configured_roots)
                        .await
                }
                Err(source) => {
                    Err(crate::local::resolver::LocalMediaResolutionError::Database { source })
                }
            };
            let _ = resolved_tx.send(resolved).await;
        });

        let session = Rc::clone(&ctx.session);
        let active_output = Rc::clone(&ctx.active_output);
        let media_ctrl = Rc::clone(&ctx.media_ctrl);
        let app_config = Rc::clone(&ctx.app_config);
        let lastfm_playback = ctx.lastfm_playback.clone();
        let album_art = ctx.album_art.clone();
        glib::MainContext::default().spawn_local(async move {
            match resolved_rx.recv().await {
                Ok(Ok(media))
                    if media.matches_current_configuration(&app_config.borrow().library_paths) =>
                {
                    if session.borrow_mut().finish_pending_resolution(generation) {
                        let artwork_media = media.clone();
                        let accepted = active_output.borrow().load_local(media);
                        if !accepted {
                            let marked = finish_coordinated_output_load(
                                &session,
                                &lastfm_playback,
                                generation,
                                false,
                            );
                            debug_assert!(marked, "current local load remains retryable");
                            album_art::invalidate();
                        } else {
                            let marked = finish_coordinated_output_load(
                                &session,
                                &lastfm_playback,
                                generation,
                                true,
                            );
                            debug_assert!(marked, "accepted local load owns playback delivery");
                            album_art::update_resolved_file_album_art(
                                &album_art,
                                artwork_media,
                            );
                        }
                    }
                }
                Ok(Ok(_)) => {
                    if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!("Local media root changed before output handoff");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
                Ok(Err(error)) => {
                    if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!(error = %error, "Could not resolve local track by its library identity");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
                Err(_) => {
                    if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!("Local media resolver stopped before returning a result");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
            }
        });
        return true;
    }

    if let Some(expected_session_epoch) = item.source_session_epoch {
        // Retire the prior output/ticket before awaiting the source resolver.
        // The Stop captures the old event generation; the new generation below
        // therefore rejects any delayed terminal event it produces.
        let generation = begin_pending_output(ctx);
        ctx.active_output.borrow().stop();
        ctx.active_output.borrow().set_event_generation(generation);
        update_now_playing_ui(ctx, &item, identity.as_ref(), None);

        let identity = identity
            .as_ref()
            .expect("managed-source queue item has stable media identity");
        let source_id = identity.media_key.source_id;
        let track_id = identity.media_key.track_id.clone();
        let source_registry = ctx.source_registry.clone();
        let session = Rc::clone(&ctx.session);
        let active_output = Rc::clone(&ctx.active_output);
        let media_ctrl = Rc::clone(&ctx.media_ctrl);
        let lastfm_playback = ctx.lastfm_playback.clone();
        let album_art = ctx.album_art.clone();
        let external_session = item.external_session;
        let regular_playlist_guard = item.regular_playlist_guard;
        glib::MainContext::default().spawn_local(async move {
            let resolved = if let Some(guard) = regular_playlist_guard {
                source_registry
                    .resolve_regular_playlist_stream(guard, track_id)
                    .await
                    .map_err(|error| error.to_string())
            } else {
                source_registry
                    .resolve_stream(source_id, expected_session_epoch, track_id)
                    .await
                    .map_err(|error| error.to_string())
            };
            match resolved {
                Ok(request) => {
                    if !session.borrow_mut().finish_pending_resolution(generation) {
                        return;
                    }
                    let accepted = match request {
                        crate::source_registry::ResolvedSourceStream::Http(request) => {
                            match request {
                                crate::architecture::media::MediaRequest::ProtectedHttp(request) => {
                                    active_output.borrow().load_resolved(*request)
                                }
                                crate::architecture::media::MediaRequest::PublicHttp(request) => {
                                    match request.consume() {
                                        Ok(url) => active_output.borrow().load_uri(url.as_str()),
                                        Err(error) => {
                                            warn!(error = %error, "Public stream authority expired before output handoff");
                                            let marked = finish_coordinated_output_load(
                                                &session,
                                                &lastfm_playback,
                                                generation,
                                                false,
                                            );
                                            debug_assert!(
                                                marked,
                                                "expired public load remains retryable"
                                            );
                                            if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                                                ctrl.update_playback(false);
                                            }
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        crate::source_registry::ResolvedSourceStream::File(media) => {
                            // AudioOutput consumes its retained capability. A
                            // second clone is reserved for embedded art and is
                            // used only after the output accepts the exact
                            // object.
                            let artwork_media = media.clone();
                            let accepted = active_output.borrow().load_local(media);
                            if accepted {
                                album_art::update_resolved_file_album_art(
                                    &album_art,
                                    artwork_media,
                                );
                            }
                            accepted
                        }
                    };
                    if accepted {
                        let marked = finish_coordinated_output_load(
                            &session,
                            &lastfm_playback,
                            generation,
                            true,
                        );
                        debug_assert!(marked, "accepted resolved load owns playback delivery");
                    } else {
                        if external_session
                            && session.borrow().accepts_event_generation(generation)
                            && session.borrow().current_external_source_id() == Some(source_id)
                        {
                            super::open_files::invalidate_admission();
                            abandon_external_playback(
                                &session,
                                &lastfm_playback,
                                &active_output,
                                &source_registry,
                                source_id,
                            );
                            album_art::invalidate();
                        } else {
                            let marked = finish_coordinated_output_load(
                                &session,
                                &lastfm_playback,
                                generation,
                                false,
                            );
                            debug_assert!(marked, "current resolved load remains retryable");
                        }
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
                Err(error) => {
                    let owns_external = external_session
                        && session.borrow().accepts_event_generation(generation)
                        && session.borrow().current_external_source_id() == Some(source_id);
                    if owns_external {
                        super::open_files::invalidate_admission();
                        warn!(error = %error, "Could not resolve external media through its live source session");
                        abandon_external_playback(
                            &session,
                            &lastfm_playback,
                            &active_output,
                            &source_registry,
                            source_id,
                        );
                        album_art::invalidate();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    } else if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!(error = %error, "Could not resolve track through its live source session");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
            }
        });
        return true;
    }

    if item.uri.is_empty() {
        warn!("Track has no playable URI");
        return false;
    }

    let playback_uri = item.uri.clone();

    // Each output resolves the source at its own trust boundary. Chromecast
    // serves local files over its embedded LAN server; authenticated remote
    // media is exchanged for an app-owned proxy ticket before Chromecast,
    // MPD, local playbin, or AirPlay uridecodebin can consume it. No output
    // swap is needed here.

    tracing::debug!("Playing track");

    let prepared = { ctx.session.borrow_mut().prepare_current_direct_load() };
    let Some(prepared) = prepared else {
        debug_assert!(false, "current direct queue item is loadable");
        return false;
    };
    let PreparedDirectLoad {
        uri,
        generation,
        intent,
    } = prepared;
    let _ = ctx.lastfm_playback.observe_output_intent(intent);
    let accepted = {
        let output = ctx.active_output.borrow();
        output.set_event_generation(generation);
        output.load_uri(&uri)
    };
    let marked =
        finish_coordinated_output_load(&ctx.session, &ctx.lastfm_playback, generation, accepted);
    debug_assert!(marked, "current direct load owns playback delivery state");
    tracing::debug!(
        generation = ?generation,
        accepted,
        "Direct queue item handed to output"
    );
    update_now_playing_ui(ctx, &item, identity.as_ref(), Some(&playback_uri));

    true
}

fn update_now_playing_ui(
    ctx: &PlaybackContext,
    item: &QueueItem,
    identity: Option<&PlaybackIdentity>,
    direct_playback_uri: Option<&str>,
) {
    ctx.title_label.set_label(&item.title);
    ctx.title_label.set_tooltip_text(Some(&item.title));
    let artist_album = format!("{} \u{2014} {}", item.artist, item.album);
    ctx.artist_label.set_label(&artist_album);
    ctx.artist_label.set_tooltip_text(Some(&artist_album));

    // Scroll only when the queue's source and item are present in the current
    // view. Navigation still works when the user is viewing another source or
    // has filtered the playing item out.
    let active_source_key = ctx.active_source_key.borrow().clone();
    if let Some((identity, view)) = identity
        .filter(|identity| identity_belongs_to_source(identity, &active_source_key))
        .zip(queue_view(&active_source_key))
    {
        if let Some(position) = find_queue_item_position(
            ctx.model.n_items(),
            &identity.media_key,
            item.occurrence,
            item.row_instance_id,
            |index| {
                ctx.model
                    .item(index)
                    .and_downcast::<TrackObject>()
                    .and_then(|track| row_position_identity(&view, &track))
            },
        ) {
            ctx.column_view.scroll_to(
                position,
                None,
                gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                None,
            );
        }
    }

    // ── Update album art ─────────────────────────────────────────
    if let (Some(identity), Some(expected_session_epoch)) = (identity, item.source_session_epoch) {
        let generation = album_art::begin_remote_album_art(&ctx.album_art);
        let source_registry = ctx.source_registry.clone();
        let source_id = identity.media_key.source_id;
        let track_id = identity.media_key.track_id.clone();
        let regular_playlist_guard = item.regular_playlist_guard;
        let album_art = ctx.album_art.clone();
        glib::MainContext::default().spawn_local(async move {
            let resolved = if let Some(guard) = regular_playlist_guard {
                source_registry
                    .resolve_regular_playlist_artwork(guard, track_id)
                    .await
                    .map_err(|error| error.to_string())
            } else {
                source_registry
                    .resolve_artwork(source_id, expected_session_epoch, track_id)
                    .await
                    .map_err(|error| error.to_string())
            };
            match resolved {
                Ok(Some(request)) => {
                    album_art::fetch_resolved_album_art(&album_art, request, generation);
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(error = %error, "Could not resolve artwork through its live source session");
                }
            }
        });
    } else if !item.cover_art_url.is_empty() {
        album_art::fetch_remote_album_art(&ctx.album_art, &item.cover_art_url);
    } else if let Some(playback_uri) = direct_playback_uri {
        // Transitional direct file sources extract from their locator until
        // their source adapters provide retained file authority.
        album_art::update_direct_file_album_art(&ctx.album_art, playback_uri);
    } else {
        album_art::invalidate();
        ctx.album_art
            .set_icon_name(Some("audio-x-generic-symbolic"));
    }

    if let Some(ref mut ctrl) = *ctx.media_ctrl.borrow_mut() {
        ctrl.update_metadata(&item.title, &item.artist, &item.album);
        // The OS transports have no buffering state. Publish Playing when a
        // load is accepted and let a later Paused/Stopped event correct it.
        ctrl.update_playback(true);
    }
}

fn find_queue_item_position(
    item_count: u32,
    media_key: &MediaKey,
    target_occurrence: usize,
    row_instance_id: Option<u64>,
    mut item_at: impl FnMut(u32) -> Option<(u64, MediaKey)>,
) -> Option<u32> {
    let items: Vec<Option<(u64, MediaKey)>> = (0..item_count).map(&mut item_at).collect();
    if let Some(row_instance_id) = row_instance_id {
        if let Some(position) = items.iter().position(|item| {
            item.as_ref()
                .is_some_and(|(candidate, _)| *candidate == row_instance_id)
        }) {
            return u32::try_from(position).ok();
        }
    }

    let mut occurrence = 0usize;
    for (index, item) in items.into_iter().enumerate() {
        if item.as_ref().map(|(_, key)| key) != Some(media_key) {
            continue;
        }
        if occurrence == target_occurrence {
            return u32::try_from(index).ok();
        }
        occurrence += 1;
    }
    None
}

/// Advance to the next track, respecting shuffle and repeat-all.
///
/// Returns `true` if a new track was loaded, `false` if we've reached
/// the end (caller should reset to idle).
pub fn advance_track(ctx: &PlaybackContext, repeat_mode: RepeatMode, shuffle: bool) -> bool {
    navigate_and_play(
        ctx.session.as_ref(),
        |session| session.advance(repeat_mode, shuffle),
        || play_current(ctx),
    )
}

/// Explicit Next behavior. Natural EOS uses [`advance_track`] directly so an
/// automatic transition cannot supersede a newer OS-open delivery.
pub fn advance_track_from_user(
    ctx: &PlaybackContext,
    repeat_mode: RepeatMode,
    shuffle: bool,
) -> bool {
    super::open_files::invalidate_admission();
    advance_track(ctx, repeat_mode, shuffle)
}

/// Step back to the previous track, respecting repeat-all wrap-around.
///
/// This is the positional inverse of [`advance_track`] and intentionally
/// has no "restart current track if past N seconds" behaviour — that
/// heuristic belongs to the UI/key callers, which know what threshold
/// they want to use. Returns `true` if a new track was loaded.
pub fn previous_track(ctx: &PlaybackContext, repeat_mode: RepeatMode, shuffle: bool) -> bool {
    navigate_and_play(
        ctx.session.as_ref(),
        |session| session.previous(repeat_mode, shuffle),
        || play_current(ctx),
    )
}

fn navigate_and_play(
    session: &RefCell<PlaybackSession>,
    navigate: impl FnOnce(&mut PlaybackSession) -> Option<usize>,
    play: impl FnOnce() -> bool,
) -> bool {
    // Queue and shuffle data remain available to navigation. Only the
    // authority-bearing occurrence state is moved out, while inexpensive
    // positional state is copied/cloned for a possible pre-handoff rollback.
    let (
        previous_index,
        previous_shuffle,
        previous_pending_resolution,
        previous_resolution_failed,
        previous_history_occurrence,
        previous_lastfm_candidate,
        mut previous_accepted_lastfm_load,
        previous_generation,
        selected,
    ) = {
        let mut session = session.borrow_mut();
        let previous_index = session.current_index;
        let previous_shuffle = session.shuffle.clone();
        let previous_pending_resolution = session.pending_resolution;
        let previous_resolution_failed = session.resolution_failed;
        let previous_history_occurrence = session.history_occurrence.take();
        let previous_lastfm_candidate = session.lastfm_occurrence_candidate.take();
        let predecessor_suspension = session.accepted_lastfm_load.suspension();
        let previous_accepted_lastfm_load = std::mem::take(&mut session.accepted_lastfm_load);
        session.pending_lastfm_output_suspension = predecessor_suspension;
        let previous_generation = session.event_generation;
        let selected = navigate(&mut session);
        (
            previous_index,
            previous_shuffle,
            previous_pending_resolution,
            previous_resolution_failed,
            previous_history_occurrence,
            previous_lastfm_candidate,
            previous_accepted_lastfm_load,
            previous_generation,
            selected,
        )
    };
    if selected.is_none() {
        let mut session = session.borrow_mut();
        session.pending_lastfm_output_suspension = None;
        session.history_occurrence = previous_history_occurrence;
        session.lastfm_occurrence_candidate = previous_lastfm_candidate;
        session.accepted_lastfm_load = previous_accepted_lastfm_load;
        return false;
    }
    if play() {
        // The new output handoff committed, so an already-extracted proof for
        // the predecessor must become inert before this retained clone is
        // dropped. Pre-handoff rollback deliberately restores it unchanged.
        session.borrow_mut().pending_lastfm_output_suspension = None;
        previous_accepted_lastfm_load.revoke();
        true
    } else {
        let mut session = session.borrow_mut();
        if session.event_generation != previous_generation {
            // A callback that advanced output ownership cannot roll the old
            // occurrence back, even if it incorrectly reports failure. Revoke
            // its delayed proof before the debug assertion can unwind.
            previous_accepted_lastfm_load.revoke();
            debug_assert!(
                false,
                "a failed navigation handoff must not advance output ownership"
            );
            return false;
        }
        session.current_index = previous_index;
        session.shuffle = previous_shuffle;
        session.pending_resolution = previous_pending_resolution;
        session.resolution_failed = previous_resolution_failed;
        session.history_occurrence = previous_history_occurrence;
        session.lastfm_occurrence_candidate = previous_lastfm_candidate;
        session.pending_lastfm_output_suspension = None;
        session.accepted_lastfm_load = previous_accepted_lastfm_load;
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviousDispatch {
    Stepped,
    Restarted,
}

fn dispatch_previous(
    position_ms: u64,
    step: impl FnOnce() -> bool,
    restart: impl FnOnce(),
) -> PreviousDispatch {
    if position_ms > PREVIOUS_RESTART_THRESHOLD_MS {
        restart();
        return PreviousDispatch::Restarted;
    }

    if step() {
        PreviousDispatch::Stepped
    } else {
        restart();
        PreviousDispatch::Restarted
    }
}

/// Shared header-button and OS-media-control Previous behavior.
///
/// The output position borrow is deliberately released before navigation can
/// load another item. This keeps both entry points on one exact threshold and
/// avoids carrying a `RefCell` borrow into output callbacks.
pub fn previous_or_restart_from_user(
    ctx: &PlaybackContext,
    repeat_mode: RepeatMode,
    shuffle: bool,
) {
    super::open_files::invalidate_admission();
    let position_ms = {
        let output = ctx.active_output.borrow();
        output.position_ms().unwrap_or(0)
    };
    let event_generation = ctx.session.borrow().current_event_generation();
    let _ = dispatch_previous(
        position_ms,
        || previous_track(ctx, repeat_mode, shuffle),
        || {
            let _ = ctx
                .session
                .borrow_mut()
                .observe_history_seek(event_generation, position_ms, 0);
            let _ = ctx.lastfm_playback.observe_discontinuity(event_generation);
            ctx.active_output.borrow().seek_to(0);
        },
    );
}

/// Replay the current queue item without consulting the mutable view.
pub fn replay_current(ctx: &PlaybackContext) -> bool {
    replay_current_occurrence(ctx.session.as_ref(), || play_current(ctx))
}

fn replay_current_occurrence(
    session: &RefCell<PlaybackSession>,
    play: impl FnOnce() -> bool,
) -> bool {
    // `play_current` returns false only before it begins a new output/event
    // generation. Snapshot just the tentative occurrence so a pre-handoff
    // failure can restore its predecessor without cloning or rolling back the
    // queue, shuffle traversal, resolution state, or event ownership.
    let (
        previous_history_occurrence,
        previous_lastfm_candidate,
        mut previous_accepted_lastfm_load,
        previous_generation,
    ) = {
        let mut session = session.borrow_mut();
        let history_occurrence = session.history_occurrence.take();
        let lastfm_candidate = session.lastfm_occurrence_candidate.take();
        let predecessor_suspension = session.accepted_lastfm_load.suspension();
        let accepted_lastfm_load = std::mem::take(&mut session.accepted_lastfm_load);
        session.pending_lastfm_output_suspension = predecessor_suspension;
        let generation = session.event_generation;
        session.begin_repeat_one_occurrence();
        (
            history_occurrence,
            lastfm_candidate,
            accepted_lastfm_load,
            generation,
        )
    };
    if play() {
        // Repeat One is a new genuine occurrence even when the media identity
        // is unchanged. Revoke any delayed accepted-load proof belonging to
        // the predecessor once the replay handoff commits.
        session.borrow_mut().pending_lastfm_output_suspension = None;
        previous_accepted_lastfm_load.revoke();
        true
    } else {
        let mut session = session.borrow_mut();
        if session.event_generation == previous_generation {
            session.pending_lastfm_output_suspension = None;
            session.history_occurrence = previous_history_occurrence;
            session.lastfm_occurrence_candidate = previous_lastfm_candidate;
            session.accepted_lastfm_load = previous_accepted_lastfm_load;
        } else {
            // Generation advancement committed output ownership even though
            // the callback reported failure. The predecessor cannot safely be
            // restored, so make every extracted clone inert before asserting.
            previous_accepted_lastfm_load.revoke();
            debug_assert!(
                false,
                "a failed repeat-one handoff must not advance output ownership"
            );
        }
        false
    }
}

/// Install one already-admitted, pathless external-file session as the exact
/// one-item playback queue.
///
/// Opening, validation, and tag parsing happen before this GTK boundary. The
/// queue retains only source/track identity and the publishing epoch; every
/// output resolves the retained file capability at use time.
pub fn play_external_session(
    external: &crate::source_registry::ExternalFileSession,
    ctx: &PlaybackContext,
) -> bool {
    // Consume the delivery generation before replacing terminal ownership.
    // Any duplicate/late completion from the same delivery is now stale.
    super::open_files::invalidate_admission();
    let item = QueueItem::external(external);
    let previous_external = ctx.session.borrow().current_external_source_id();
    if !ctx.session.borrow_mut().replace_queue(vec![item], 0) {
        return false;
    }
    if !play_current(ctx) {
        abandon_external_playback(
            &ctx.session,
            &ctx.lastfm_playback,
            &ctx.active_output,
            &ctx.source_registry,
            external.source_id(),
        );
        if previous_external != Some(external.source_id()) {
            if let Some(source_id) = previous_external {
                let _ = ctx.source_registry.retire_external(source_id);
            }
        }
        return false;
    }
    if let Some(source_id) = previous_external {
        let _ = ctx.source_registry.retire_external(source_id);
    }
    tracing::info!("OS-opened external playback started");
    true
}

/// Format milliseconds as `m:ss` (or `h:mm:ss` for ≥ 1 hour).
pub fn format_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingOutputState {
        generations: Vec<PlayerEventGeneration>,
        loads: Vec<String>,
        stops: usize,
    }

    struct RecordingOutput {
        state: Rc<RefCell<RecordingOutputState>>,
        reject_loads: Cell<usize>,
        session_borrow_probe: Option<std::rc::Weak<RefCell<PlaybackSession>>>,
        volume: f64,
    }

    struct PlaybackRegistryFixture {
        runtime: tokio::runtime::Runtime,
        registry: SourceRegistry,
    }

    impl PlaybackRegistryFixture {
        fn new() -> Self {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            let registry = SourceRegistry::new(runtime.handle().clone());
            Self { runtime, registry }
        }

        fn capture(
            &self,
            model: &impl IsA<gtk::gio::ListModel>,
            source_key: &str,
            selected_position: u32,
        ) -> Option<CapturedQueue> {
            capture_visible_queue(model, source_key, selected_position, &self.registry)
        }

        fn wait_for_catalogue(
            &self,
            source_id: SourceId,
        ) -> (u64, Vec<crate::architecture::models::Track>) {
            self.runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        if let Some(snapshot) = self.registry.snapshot(source_id) {
                            if snapshot.state == crate::source_lifecycle::SourceState::Ready {
                                let session_epoch = snapshot
                                    .session_epoch
                                    .expect("ready source owns a session epoch");
                                let tracks = snapshot
                                    .catalogue
                                    .expect("ready source publishes a catalogue")
                                    .value
                                    .tracks()
                                    .to_vec();
                                return (session_epoch, tracks);
                            }
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("source catalogue becomes ready")
            })
        }

        fn disconnect(&self, source_id: SourceId) {
            let waiter = self
                .registry
                .disconnect(source_id)
                .expect("live source begins retirement");
            self.runtime.block_on(waiter.wait());
        }
    }

    impl Drop for PlaybackRegistryFixture {
        fn drop(&mut self) {
            let barrier = self.registry.shutdown();
            self.runtime.block_on(barrier.wait());
        }
    }

    impl RecordingOutput {
        fn new(reject_loads: usize) -> (Self, Rc<RefCell<RecordingOutputState>>) {
            let state = Rc::new(RefCell::new(RecordingOutputState::default()));
            (
                Self {
                    state: Rc::clone(&state),
                    reject_loads: Cell::new(reject_loads),
                    session_borrow_probe: None,
                    volume: 0.5,
                },
                state,
            )
        }
    }

    impl AudioOutput for RecordingOutput {
        fn name(&self) -> &str {
            "recording"
        }

        fn output_type(&self) -> crate::audio::output::OutputType {
            crate::audio::output::OutputType::Local
        }

        fn supports_volume(&self) -> bool {
            true
        }

        fn load_uri(&self, uri: &str) -> bool {
            if let Some(session) = self
                .session_borrow_probe
                .as_ref()
                .and_then(std::rc::Weak::upgrade)
            {
                assert!(
                    session.try_borrow_mut().is_ok(),
                    "output callbacks must run after the playback-session borrow is released"
                );
            }
            self.state.borrow_mut().loads.push(uri.to_string());
            let remaining = self.reject_loads.get();
            if remaining == 0 {
                true
            } else {
                self.reject_loads.set(remaining - 1);
                false
            }
        }

        fn load_resolved(&self, _request: crate::architecture::media::ResolvedHttpRequest) -> bool {
            false
        }

        fn load_local(&self, _media: crate::local::resolver::ResolvedLocalMedia) -> bool {
            false
        }

        fn set_event_generation(&self, generation: PlayerEventGeneration) {
            self.state.borrow_mut().generations.push(generation);
        }

        fn play(&self) {}

        fn pause(&self) {}

        fn stop(&self) {
            self.state.borrow_mut().stops += 1;
        }

        fn toggle_play_pause(&self) {}

        fn seek_to(&self, _position_ms: u64) {}

        fn set_volume(&mut self, level: f64) {
            self.volume = level;
        }

        fn volume(&self) -> f64 {
            self.volume
        }

        fn state(&self) -> crate::audio::PlayerState {
            crate::audio::PlayerState::Stopped
        }

        fn position_ms(&self) -> Option<u64> {
            None
        }
    }

    fn item(source: &str, id: &str) -> QueueItem {
        let view = queue_view(source).expect("test source identity");
        let identity = PlaybackIdentity::new(
            &view,
            view.source_id,
            TrackId::new(id.to_string()).expect("test track identity"),
        );
        QueueItem {
            identity,
            occurrence: 0,
            row_instance_id: None,
            source_session_epoch: None,
            regular_playlist_guard: None,
            external_session: false,
            lastfm_source: None,
            lastfm_profile: None,
            duration_ms: None,
            duration_secs: None,
            uri: if is_library_source(view.source_id) {
                String::new()
            } else {
                format!("https://media.invalid/{id}")
            },
            title: id.to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            album_artist: None,
            track_number: None,
            cover_art_url: String::new(),
        }
    }

    fn history_item(source: &str, id: &str, duration_ms: Option<u64>) -> QueueItem {
        let mut item = item(source, id);
        item.duration_ms = duration_ms.filter(|duration_ms| *duration_ms > 0);
        item
    }

    fn authoritative_lastfm_item(source: &str, id: &str) -> QueueItem {
        let mut item = item(source, id);
        item.lastfm_source = LastFmPlaybackSource::local(item.identity.media_key.clone());
        sync_test_lastfm_profile(&mut item);
        item
    }

    fn sync_test_lastfm_profile(item: &mut QueueItem) {
        item.lastfm_profile = Some(PlaybackAttributionProfile::for_test(
            item.title.clone(),
            item.artist.clone(),
            (!item.album.is_empty()).then_some(item.album.as_str()),
            item.album_artist.as_deref(),
            item.track_number,
            item.duration_secs,
        ));
    }

    fn assert_newer_lastfm_load_survives_stale_predecessor(
        newer: LastFmAcceptedOutputLoad,
        newer_generation: PlayerEventGeneration,
        delayed_predecessor: LastFmAcceptedOutputLoad,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = crate::source_registry::SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();

        assert!(owner
            .accept_output_load(newer, &registry, &enabled_remote_sources)
            .admitted());
        let stale =
            owner.accept_output_load(delayed_predecessor, &registry, &enabled_remote_sources);
        assert!(stale.stale());
        let (handoff, error) = stale.into_update().into_parts();
        assert!(handoff.is_none());
        assert!(error.is_none());

        let (handoff, error) = owner
            .observe_event(&PlayerEvent::state(newer_generation, PlayerState::Playing))
            .into_parts();
        assert!(error.is_none());
        assert_eq!(
            handoff.as_ref().map(|handoff| handoff.kind()),
            Some(crate::lastfm::playback_owner::LastFmPlaybackHandoffKind::NowPlaying),
            "the delayed predecessor cannot retire the committed successor"
        );

        drop(owner);
        runtime.block_on(registry.shutdown().wait());
    }

    fn accept_history_load(session: &mut PlaybackSession) -> PlayerEventGeneration {
        let generation = session.begin_event_generation();
        assert!(session.mark_history_load_accepted(generation));
        generation
    }

    fn observe_playing(session: &mut PlaybackSession, generation: PlayerEventGeneration) {
        assert_eq!(
            session.observe_history_event(&PlayerEvent::state(generation, PlayerState::Playing,)),
            None
        );
    }

    fn observe_position(
        session: &mut PlaybackSession,
        generation: PlayerEventGeneration,
        position_ms: u64,
        duration_ms: u64,
    ) -> Option<TrackId> {
        session.observe_history_event(&PlayerEvent::position(generation, position_ms, duration_ms))
    }

    fn protected_item(source: &str, id: &str) -> QueueItem {
        let mut item = item(source, id);
        item.uri.clear();
        item.source_session_epoch = Some(7);
        item
    }

    fn projected_row(id: &str, uri: &str) -> TrackObject {
        let row = TrackObject::new(
            1, "Title", 60, "Artist", "Album", "", "", 0, "", 0, 0, 0, "", uri,
        );
        row.set_track_id(id);
        row
    }

    fn playback_row(id: &str) -> TrackObject {
        projected_row(id, &format!("file:///music/{id}.flac"))
    }

    fn write_lastfm_flac(path: &std::path::Path, title: Option<&str>, artist: Option<&str>) {
        let mut fixture = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/audio/silence.flac"
        ))
        .to_vec();
        assert_eq!(&fixture[..4], b"fLaC");
        // Raise the shared 100 ms fixture's declared duration above Last.fm's
        // strict 30-second boundary without adding another binary fixture.
        let packed = u64::from_be_bytes(
            fixture[18..26]
                .try_into()
                .expect("FLAC STREAMINFO sample word"),
        );
        let sample_rate = (packed >> 44) & 0x0f_ffff;
        assert!(sample_rate > 0);
        let total_samples = sample_rate * 31;
        assert!(total_samples < (1_u64 << 36));
        let declared = (packed & !((1_u64 << 36) - 1)) | total_samples;
        fixture[18..26].copy_from_slice(&declared.to_be_bytes());
        std::fs::write(path, fixture).expect("copy FLAC fixture");
        crate::local::tag_writer::write_tags(
            path,
            &crate::local::tag_writer::TagEdits {
                // Explicit empty edits prove tag absence rather than relying
                // on the shared fixture's current metadata.
                title: Some(title.unwrap_or_default().to_string()),
                artist: Some(artist.unwrap_or_default().to_string()),
                album: Some(String::new()),
                ..Default::default()
            },
        )
        .expect("write exact removable fixture tags");
    }

    fn managed_row(
        track: &crate::architecture::models::Track,
        source_id: SourceId,
        session_epoch: Option<u64>,
        display_title: &str,
        display_artist: &str,
    ) -> TrackObject {
        let row = TrackObject::new(
            track.track_number.unwrap_or(0),
            display_title,
            track.duration_secs.unwrap_or(0),
            display_artist,
            &track.album_title,
            track.genre.as_deref().unwrap_or("Unknown"),
            track.composer.as_deref().unwrap_or(""),
            track.year.unwrap_or(0),
            "",
            track.bitrate_kbps.unwrap_or(0),
            track.sample_rate_hz.unwrap_or(0),
            track.play_count.unwrap_or(0),
            track.format.as_deref().unwrap_or(""),
            "",
        );
        row.set_track_id(
            track
                .native_track_id
                .as_ref()
                .expect("managed track has an exact native identity")
                .as_str(),
        );
        assert!(row.set_source_id(source_id));
        if let Some(session_epoch) = session_epoch {
            row.set_source_session_epoch(session_epoch);
        }
        row
    }

    fn ids(session: &PlaybackSession) -> Vec<String> {
        session
            .queue
            .iter()
            .map(|entry| entry.identity.media_key.track_id.as_str().to_string())
            .collect()
    }

    fn library_ids(session: &PlaybackSession) -> HashSet<&str> {
        session
            .library_track_ids()
            .into_iter()
            .map(TrackId::as_str)
            .collect()
    }

    fn current_id(session: &PlaybackSession) -> &str {
        session
            .current_identity()
            .expect("current identity")
            .media_key
            .track_id
            .as_str()
    }

    fn current_source(session: &PlaybackSession) -> SourceId {
        session
            .current_identity()
            .expect("current identity")
            .media_key
            .source_id
    }

    fn item_from_row(source: &str, row: &TrackObject, occurrence: usize) -> QueueItem {
        let view = queue_view(source).expect("test source identity");
        let identity = PlaybackIdentity::new(
            &view,
            row.source_id().unwrap_or(view.source_id),
            TrackId::new(row.track_id()).expect("test track identity"),
        );
        QueueItem::from_track(identity, row, occurrence, None, None)
    }

    fn refreshed_metadata() -> QueueTrackRefresh {
        QueueTrackRefresh {
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            album_artist: Some("Album Artist".to_string()),
            track_number: Some(7),
            duration_secs: Some(181),
            cover_art_url: String::new(),
        }
    }

    fn refresh(session: &mut PlaybackSession, track_id: &str, update: QueueTrackRefresh) -> usize {
        session.refresh_library_tracks(&HashMap::from([(
            TrackId::new(track_id.to_string()).expect("test track identity"),
            update,
        )]))
    }

    #[test]
    fn a_library_metadata_refresh_preserves_pathless_queue_identity_and_shuffle() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("local", "a"), item("local", "b"), item("local", "c")],
            0,
        ));
        // Enter shuffle so the cycle's index bookkeeping is live.
        assert!(session.advance(RepeatMode::Off, true).is_some());
        let cursor = session.current_index;
        let shuffle = session.shuffle.clone().expect("shuffle state exists");

        assert_eq!(refresh(&mut session, "b", refreshed_metadata()), 1);

        assert!(session.queue.iter().all(|item| item.uri.is_empty()));
        assert_eq!(session.queue[1].title, "Title");
        assert_eq!(
            session.queue[1].album_artist.as_deref(),
            Some("Album Artist")
        );
        assert_eq!(session.queue[1].track_number, Some(7));
        assert_eq!(session.queue[1].duration_secs, Some(181));
        assert_eq!(
            ids(&session),
            ["a", "b", "c"],
            "identity, order, and length are the coordinates the cursor indexes into"
        );
        assert_eq!(session.current_index, cursor);
        assert_eq!(
            session.shuffle.as_ref().map(|state| &state.remaining),
            Some(&shuffle.remaining)
        );
    }

    #[test]
    fn a_metadata_refresh_reaches_the_item_playing_right_now_without_a_locator() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a"), item("local", "b")], 1));

        assert_eq!(refresh(&mut session, "b", refreshed_metadata()), 1);

        let current = session.current().expect("current item");
        assert_eq!(current.title, "Title");
        assert!(current.uri().is_empty());
    }

    #[test]
    fn a_playlist_queue_follows_the_same_library_track() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("playlist:favourites", "a"), item("local", "a")],
            0,
        ));
        assert_eq!(
            library_ids(&session),
            HashSet::from(["a"]),
            "duplicate playlist/local occurrences need one snapshot lookup"
        );

        assert_eq!(
            refresh(&mut session, "a", refreshed_metadata()),
            2,
            "a playlist is a projection of the library, so it holds library track IDs"
        );
    }

    #[test]
    fn a_library_refresh_never_reinterprets_another_source_s_track_id() {
        let mut session = PlaybackSession::default();
        let external = QueueItem::direct_for_test(
            "file:///downloads/a.flac".to_string(),
            "External".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        assert!(session.replace_queue(
            vec![
                external,
                item("https://subsonic.invalid", "a"),
                item("local", "a")
            ],
            0,
        ));
        assert_eq!(
            library_ids(&session),
            HashSet::from(["a"]),
            "only local-library sources participate in snapshot filtering"
        );

        // "a" is a library UUID here, but a remote backend's native ID — and an
        // external file's URI — are namespaced by their own source.
        assert_eq!(refresh(&mut session, "a", refreshed_metadata()), 1);
        assert_eq!(session.queue[0].uri, "file:///downloads/a.flac");
        assert_eq!(session.queue[1].uri, "https://media.invalid/a");
        assert!(session.queue[2].uri.is_empty());
    }

    #[test]
    fn local_and_playlist_queue_capture_discards_the_row_locator() {
        let local = projected_row("legacy:local-id", "file:///music/captured.flac");
        local.set_album_artist("Exact Album Artist");
        let playlist = projected_row("legacy:local-id", "file:///music/captured.flac");
        let remote = projected_row("remote-id", "https://media.invalid/stream");

        let local_item = item_from_row("local", &local, 0);
        let playlist_item = item_from_row("playlist:favourites", &playlist, 0);
        let remote_item = item_from_row("https://server.invalid", &remote, 0);

        assert_eq!(
            local_item.identity.media_key.track_id.as_str(),
            "legacy:local-id"
        );
        assert_eq!(
            playlist_item.identity.media_key.track_id.as_str(),
            "legacy:local-id"
        );
        assert_eq!(
            local_item.identity.media_key,
            playlist_item.identity.media_key
        );
        assert_eq!(
            playlist_item.identity.view_origin,
            Some(ViewOrigin::Playlist("favourites".to_string()))
        );
        assert!(local_item.uri.is_empty());
        assert_eq!(
            local_item.album_artist.as_deref(),
            Some("Exact Album Artist")
        );
        assert_eq!(local_item.track_number, Some(1));
        assert_eq!(local_item.duration_secs, Some(60));
        assert!(playlist_item.uri.is_empty());
        assert_eq!(remote_item.uri, "https://media.invalid/stream");
    }

    #[test]
    fn production_track_object_keeps_display_metadata_but_has_no_lastfm_candidate_without_provenance(
    ) {
        let row = projected_row(
            "structured-looking",
            "file:///music/structured-looking.flac",
        );
        row.set_album_artist("Displayed Album Artist");
        let captured = item_from_row("local", &row, 0);
        assert_eq!(captured.title, "Title");
        assert_eq!(captured.artist, "Artist");
        assert_eq!(captured.album, "Album");
        assert_eq!(
            captured.album_artist.as_deref(),
            Some("Displayed Album Artist")
        );
        assert_eq!(captured.duration_secs, Some(60));
        assert!(captured.lastfm_source.is_none());
        assert!(captured.lastfm_profile.is_none());

        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![captured], 0));
        assert!(session.lastfm_occurrence_candidate.is_none());
        let generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(generation));
        let ineligible = session
            .take_accepted_lastfm_output_load(generation)
            .expect("every exact accepted output load reaches the owner boundary");
        assert_eq!(
            format!("{ineligible:?}"),
            "LastFmAcceptedOutputLoad::Ineligible"
        );
        assert!(session.accepted_lastfm_load.is_consumed());
        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_none());
    }

    #[test]
    fn managed_source_queue_capture_preserves_epoch_and_stays_pathless() {
        let remote = projected_row("native/remote.id", "");
        remote.set_source_session_epoch(77);

        let captured = item_from_row("https://server.invalid", &remote, 0);

        assert_eq!(
            captured.identity.media_key.track_id.as_str(),
            "native/remote.id"
        );
        assert_eq!(captured.source_session_epoch(), Some(77));
        assert!(captured.uri().is_empty());
        assert!(captured.cover_art_url.is_empty());

        let direct = item_from_row(
            "https://another.invalid",
            &projected_row("direct", "https://media.invalid/direct"),
            0,
        );
        assert_eq!(direct.source_session_epoch(), None);
        assert_eq!(direct.uri(), "https://media.invalid/direct");
    }

    #[test]
    fn radio_queue_capture_discards_a_projected_locator_and_retains_view_identity() {
        let radio = projected_row("Case/Sensitive Station ID", "https://radio.invalid/live");
        radio.set_source_session_epoch(88);

        let captured = item_from_row(super::super::radio::TOP_VOTE_SOURCE_KEY, &radio, 0);

        assert_eq!(
            captured.identity.media_key.source_id,
            SourceId::radio_browser()
        );
        assert_eq!(
            captured.identity.view_origin,
            Some(ViewOrigin::Radio("top-voted".to_string()))
        );
        assert_eq!(captured.source_session_epoch(), Some(88));
        assert!(captured.uri().is_empty());
    }

    #[test]
    fn managed_source_item_cannot_bypass_resolution_with_a_cached_uri() {
        let mut session = PlaybackSession::default();
        let mut remote = protected_item("https://server.invalid", "native/remote.id");
        remote.uri = "https://media.invalid/should-not-load".to_string();
        assert!(session.replace_queue(vec![remote], 0));

        let (output, output_state) = RecordingOutput::new(0);
        assert!(session.load_current_direct(&output).is_none());
        assert!(output_state.borrow().loads.is_empty());
        assert!(output_state.borrow().generations.is_empty());
    }

    #[test]
    fn projected_playlist_rows_follow_committed_uris_without_losing_occurrences() {
        let first = projected_row("a", "file:///music/old-a.flac");
        let unrelated = projected_row("b", "file:///music/b.flac");
        let duplicate = projected_row("a", "file:///music/old-a.flac");
        for row in [&first, &unrelated, &duplicate] {
            assert!(row.set_source_id(SourceId::local()));
        }
        let rows = vec![first, unrelated, duplicate];
        let identities: Vec<u64> = rows.iter().map(TrackObject::row_instance_id).collect();

        let renamed = projected_row("a", "file:///music/renamed-a.flac");
        let empty = projected_row("b", "");
        assert!(renamed.set_source_id(SourceId::local()));
        assert!(empty.set_source_id(SourceId::local()));
        assert_eq!(refresh_projected_library_uris(&rows, &[renamed, empty]), 2);

        assert_eq!(rows[0].uri(), "file:///music/renamed-a.flac");
        assert_eq!(rows[1].uri(), "file:///music/b.flac");
        assert_eq!(rows[2].uri(), "file:///music/renamed-a.flac");
        assert_eq!(
            rows.iter()
                .map(TrackObject::row_instance_id)
                .collect::<Vec<_>>(),
            identities,
            "URI refresh must preserve duplicate occurrence identity and order"
        );
    }

    #[test]
    fn local_uri_refresh_never_retargets_a_remote_playlist_identity_collision() {
        let local = projected_row("shared-id", "file:///music/old.flac");
        assert!(local.set_source_id(SourceId::local()));
        let remote = projected_row("shared-id", "");
        let remote_source = SourceId::random();
        assert!(remote.set_source_id(remote_source));
        let replacement = projected_row("shared-id", "file:///music/new.flac");

        assert_eq!(
            refresh_projected_library_uris(&[local.clone(), remote.clone()], &[replacement]),
            1
        );
        assert_eq!(local.uri(), "file:///music/new.flac");
        assert_eq!(remote.uri(), "");
        assert_eq!(remote.source_id(), Some(remote_source));
    }

    #[test]
    fn local_uri_refresh_does_not_revive_an_unavailable_playlist_occurrence() {
        use crate::ui::objects::{PlaylistOccurrenceBinding, PlaylistRowUnavailableReason};

        let missing_id = TrackId::new("missing-local").expect("local track ID");
        let unavailable = projected_row(missing_id.as_str(), "");
        unavailable.set_playlist_occurrence_binding(
            PlaylistOccurrenceBinding::unavailable(
                "entry-missing",
                SourceId::local(),
                Some(missing_id),
                PlaylistRowUnavailableReason::LocalTrackMissing,
            )
            .expect("missing local occurrence"),
        );
        let replacement = projected_row("missing-local", "file:///music/reappeared.flac");

        assert_eq!(
            refresh_projected_library_uris(std::slice::from_ref(&unavailable), &[replacement],),
            0,
            "only a new authoritative playlist projection may make the row available",
        );
        assert!(unavailable.uri().is_empty());
    }

    #[test]
    fn sorting_the_view_does_not_reorder_the_playback_snapshot() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("local", "a"), item("local", "b"), item("local", "c")],
            1,
        ));

        // This represents the independent GTK view after a descending sort.
        let sorted_view = ["c", "b", "a"];
        assert_eq!(sorted_view[0], "c");
        assert_eq!(ids(&session), ["a", "b", "c"]);

        assert_eq!(session.advance(RepeatMode::Off, false), Some(2));
        assert_eq!(current_id(&session), "c");
    }

    #[test]
    fn production_snapshot_survives_sort_filter_navigation_and_owns_output_events() {
        let registry = PlaybackRegistryFixture::new();
        let source_key = "fixture-device-a";
        let store = gtk::gio::ListStore::new::<TrackObject>();
        for id in ["a", "b", "c"] {
            store.append(&playback_row(id));
        }
        let captured = registry
            .capture(&store, source_key, 1)
            .expect("visible B is captured");
        assert_eq!(
            captured
                .items
                .iter()
                .map(|entry| entry.identity.media_key.track_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );

        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(captured.items, captured.selected_index));
        let (output, output_state) = RecordingOutput::new(0);
        let first = session
            .load_current_direct(&output)
            .expect("captured B reaches the output");
        assert!(first.accepted);
        let stale_b_event = crate::audio::PlayerEvent::ended(first.generation);
        assert_eq!(
            current_source(&session),
            SourceId::removable(source_key).expect("fixture source ID")
        );
        assert_eq!(current_id(&session), "b");

        // Exercise the same ListModel projection boundary consumed from
        // production's SortListModel, without constructing a display-bound
        // GTK widget/model in headless CI.
        store.remove_all();
        for id in ["c", "b", "a"] {
            store.append(&playback_row(id));
        }
        assert_eq!(
            (0..store.n_items())
                .map(|index| {
                    store
                        .item(index)
                        .and_downcast::<TrackObject>()
                        .expect("sorted TrackObject")
                        .track_id()
                })
                .collect::<Vec<_>>(),
            ["c", "b", "a"]
        );

        // Browser filtering rebuilds the underlying projection. Remove B and
        // prove Next still follows the playback-owned A/B/C snapshot.
        store.remove(1);
        assert_eq!(store.n_items(), 2);
        assert_eq!(session.advance(RepeatMode::Off, false), Some(2));
        let c_load = session
            .load_current_direct(&output)
            .expect("filtered-out queue neighbor does not retarget Next");
        assert!(c_load.accepted);
        assert_eq!(current_id(&session), "c");
        assert!(!session.accepts_event_generation(stale_b_event.generation()));
        assert!(session.accepts_event_generation(c_load.generation));

        // Sidebar navigation replaces the projection and source key, but does
        // not install a queue until the user explicitly starts one there.
        store.remove_all();
        store.append(&playback_row("remote-x"));
        let remote_projection = registry
            .capture(&store, "remote-server", 0)
            .expect("remote view captures");
        assert_eq!(
            remote_projection.items[0].identity.media_key.source_id,
            SourceId::removable("remote-server").expect("replacement source ID")
        );
        assert_eq!(session.previous(RepeatMode::Off, false), Some(1));
        let b_again = session
            .load_current_direct(&output)
            .expect("Previous loads B from the local snapshot, not remote view");
        assert_eq!(
            current_source(&session),
            SourceId::removable(source_key).expect("fixture source ID")
        );
        assert_eq!(current_id(&session), "b");
        assert!(session.accepts_event_generation(b_again.generation));
        assert_eq!(
            output_state.borrow().loads,
            [
                "file:///music/b.flac",
                "file:///music/c.flac",
                "file:///music/b.flac"
            ]
        );

        stop_owned_playback(&mut session, &output);
        assert!(!session.has_current());
        assert!(!session.accepts_event_generation(b_again.generation));
        assert_eq!(output_state.borrow().stops, 1);
    }

    #[test]
    fn visible_queue_capture_freezes_only_registry_owned_removable_attribution() {
        let registry = PlaybackRegistryFixture::new();
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let tagged_path = mount.path().join("eligible.flac");
        let untagged_path = mount.path().join("untagged.flac");
        write_lastfm_flac(
            &tagged_path,
            Some("Exact Tagged Title"),
            Some("Exact Tagged Artist"),
        );
        write_lastfm_flac(&untagged_path, None, None);

        let source_id =
            SourceId::removable("playback:capture-attribution").expect("removable source identity");
        let provenance = registry
            .registry
            .claim_provenance(
                source_id,
                crate::source_lifecycle::SourceProvenance::Removable,
            )
            .expect("claim removable provenance");
        registry
            .registry
            .connect_removable(source_id, mount.path().to_path_buf(), |_| {})
            .expect("removable connection admitted");
        let (session_epoch, tracks) = registry.wait_for_catalogue(source_id);
        assert_eq!(tracks.len(), 2);

        let tagged_id =
            TrackId::removable_relative(mount.path(), &tagged_path).expect("tagged track identity");
        let untagged_id = TrackId::removable_relative(mount.path(), &untagged_path)
            .expect("untagged track identity");
        let tagged_track = tracks
            .iter()
            .find(|track| track.native_track_id.as_ref() == Some(&tagged_id))
            .expect("tagged track published");
        let untagged_track = tracks
            .iter()
            .find(|track| track.native_track_id.as_ref() == Some(&untagged_id))
            .expect("untagged track published");

        // The GTK projection is deliberately inconsistent with the parser's
        // exact tags. Registry minting, not mutable display metadata, must
        // decide the frozen Last.fm payload.
        let tagged_row = managed_row(
            tagged_track,
            source_id,
            Some(session_epoch),
            "Changed Display Title",
            "Changed Display Artist",
        );
        let untagged_row = managed_row(
            untagged_track,
            source_id,
            Some(session_epoch),
            "Convincing Display Title",
            "Convincing Display Artist",
        );
        let store = gtk::gio::ListStore::new::<TrackObject>();
        store.append(&tagged_row);
        store.append(&untagged_row);

        let captured = registry
            .capture(&store, &source_id.to_string(), 0)
            .expect("live removable projection captures");
        let tagged_item = captured
            .items
            .iter()
            .find(|item| item.identity.media_key.track_id == tagged_id)
            .expect("tagged queue item");
        assert_eq!(tagged_item.title, "Changed Display Title");
        assert_eq!(tagged_item.artist, "Changed Display Artist");
        let tagged_profile = tagged_item
            .lastfm_profile
            .as_ref()
            .expect("registry-minted removable profile");
        assert_eq!(tagged_profile.title(), "Exact Tagged Title");
        assert_eq!(tagged_profile.artist(), "Exact Tagged Artist");
        assert_eq!(tagged_profile.album(), None);
        assert_eq!(tagged_profile.duration_secs(), Some(31));
        let frozen_candidate = tagged_item
            .lastfm_occurrence_candidate()
            .expect("exact tagged profile creates one occurrence candidate");
        assert_eq!(frozen_candidate.title, "Exact Tagged Title");
        assert_eq!(frozen_candidate.artist, "Exact Tagged Artist");
        assert_eq!(frozen_candidate.album, None);

        let untagged_item = captured
            .items
            .iter()
            .find(|item| item.identity.media_key.track_id == untagged_id)
            .expect("untagged queue item");
        assert!(untagged_item.lastfm_source.is_none());
        assert!(untagged_item.lastfm_profile.is_none());
        assert!(untagged_item.lastfm_occurrence_candidate().is_none());

        // A second registry, a stale epoch, a missing epoch, and a different
        // source identity cannot turn the same convincing display row into
        // attribution authority.
        let foreign_registry = PlaybackRegistryFixture::new();
        let foreign = foreign_registry
            .capture(&store, &source_id.to_string(), 0)
            .expect("foreign registry still captures a playable queue");
        assert!(foreign
            .items
            .iter()
            .all(|item| item.lastfm_source.is_none() && item.lastfm_profile.is_none()));

        let stale_store = gtk::gio::ListStore::new::<TrackObject>();
        stale_store.append(&managed_row(
            tagged_track,
            source_id,
            session_epoch.checked_add(1),
            "Exact Tagged Title",
            "Exact Tagged Artist",
        ));
        let stale = registry
            .capture(&stale_store, &source_id.to_string(), 0)
            .expect("stale row remains a captured playback item");
        assert!(stale.items[0].lastfm_source.is_none());

        let missing_store = gtk::gio::ListStore::new::<TrackObject>();
        missing_store.append(&managed_row(
            tagged_track,
            source_id,
            None,
            "Exact Tagged Title",
            "Exact Tagged Artist",
        ));
        let missing = registry
            .capture(&missing_store, &source_id.to_string(), 0)
            .expect("epoch-less row remains a captured playback item");
        assert!(missing.items[0].lastfm_source.is_none());

        let remote_source = SourceId::random();
        let remote_store = gtk::gio::ListStore::new::<TrackObject>();
        remote_store.append(&managed_row(
            tagged_track,
            remote_source,
            Some(session_epoch),
            "Exact Tagged Title",
            "Exact Tagged Artist",
        ));
        let remote = registry
            .capture(&remote_store, &remote_source.to_string(), 0)
            .expect("remote-shaped row remains playable");
        assert!(remote.items[0].lastfm_source.is_none());

        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![tagged_item.clone()], 0));
        let generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(generation));
        let accepted = session
            .take_accepted_lastfm_output_load(generation)
            .expect("live exact profile creates an accepted output proof");
        assert_eq!(
            format!("{accepted:?}"),
            "LastFmAcceptedOutputLoad::Eligible(<redacted>)"
        );

        registry.disconnect(source_id);
        let retired = registry
            .capture(&store, &source_id.to_string(), 0)
            .expect("retired rows can still be copied as inert queue data");
        assert!(retired
            .items
            .iter()
            .all(|item| item.lastfm_source.is_none() && item.lastfm_profile.is_none()));
        assert_eq!(
            frozen_candidate.title, "Exact Tagged Title",
            "the already-captured metadata snapshot remains immutable"
        );
        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        let rejected = owner.accept_output_load(accepted, &registry.registry, &HashSet::new());
        assert!(!rejected.admitted());
        assert!(!rejected.stale());
        assert!(registry.registry.release_provenance(source_id, provenance));
    }

    #[test]
    fn regular_playlist_capture_skips_unavailable_rows_without_losing_duplicate_occurrences() {
        use crate::ui::objects::{PlaylistOccurrenceBinding, PlaylistRowUnavailableReason};

        let registry = PlaybackRegistryFixture::new();
        let playlist_key = "playlist:mixed";
        let stable_id = TrackId::new("same-local-track").expect("local track ID");
        let first = projected_row(stable_id.as_str(), "file:///music/same.flac");
        first.set_playlist_occurrence_binding(
            PlaylistOccurrenceBinding::available_local("entry-first", stable_id.clone())
                .expect("first occurrence"),
        );
        let missing = projected_row("missing-local-track", "");
        missing.set_playlist_occurrence_binding(
            PlaylistOccurrenceBinding::unavailable(
                "entry-missing",
                SourceId::local(),
                Some(TrackId::new("missing-local-track").expect("missing local ID")),
                PlaylistRowUnavailableReason::LocalTrackMissing,
            )
            .expect("missing occurrence"),
        );
        let duplicate = projected_row(stable_id.as_str(), "file:///music/same.flac");
        duplicate.set_playlist_occurrence_binding(
            PlaylistOccurrenceBinding::available_local("entry-duplicate", stable_id)
                .expect("duplicate occurrence"),
        );

        let store = gtk::gio::ListStore::new::<TrackObject>();
        store.append(&first);
        store.append(&missing);
        store.append(&duplicate);

        assert_eq!(playable_model_positions(&store, playlist_key), [0, 2]);
        assert!(
            registry.capture(&store, playlist_key, 1).is_none(),
            "activating an unavailable row must not install a replacement queue"
        );
        let captured = registry
            .capture(&store, playlist_key, 2)
            .expect("duplicate is playable");
        assert_eq!(captured.selected_index, 1);
        assert_eq!(captured.items.len(), 2);
        assert_eq!(
            captured
                .items
                .iter()
                .map(|item| item.identity.media_key.track_id.as_str())
                .collect::<Vec<_>>(),
            ["same-local-track", "same-local-track"]
        );
        assert_eq!(captured.items[0].occurrence, 0);
        assert_eq!(captured.items[1].occurrence, 1);
        assert!(captured.items.iter().all(|item| {
            item.identity.view_origin == Some(ViewOrigin::Playlist("mixed".to_string()))
        }));
    }

    #[test]
    fn available_remote_projection_carries_exact_guard_into_duplicate_queue_occurrences() {
        use crate::local::playlist_manager::{LoadedPlaylistEntry, StoredPlaylistEntry};
        use crate::source_registry::{RegularPlaylistTrack, RegularPlaylistTrackResolution};
        use crate::ui::playlist_projection::project_playlist_rows;

        let registry = PlaybackRegistryFixture::new();
        let source_id = SourceId::random();
        let track_id = TrackId::remote("remote/native-id").expect("remote track ID");
        let media_key = MediaKey::new(source_id, track_id.clone());
        let catalogue_track = crate::architecture::models::Track {
            id: uuid::Uuid::new_v4(),
            native_track_id: Some(track_id.clone()),
            title: "Current remote title".to_string(),
            artist_name: "Current remote artist".to_string(),
            album_artist_name: None,
            artist_id: None,
            album_title: "Current remote album".to_string(),
            album_id: None,
            track_number: Some(3),
            disc_number: Some(1),
            duration_secs: Some(211),
            composer: None,
            genre: Some("Remote genre".to_string()),
            year: Some(2026),
            file_path: Some("/private/must-not-cross.flac".to_string()),
            stream_url: Some(
                url::Url::parse("https://secret.invalid/audio?token=private")
                    .expect("fixture stream URL"),
            ),
            cover_art_url: Some(
                url::Url::parse("https://secret.invalid/art?token=private")
                    .expect("fixture artwork URL"),
            ),
            date_added: None,
            date_modified: None,
            bitrate_kbps: Some(1_024),
            sample_rate_hz: Some(96_000),
            format: Some("flac".to_string()),
            play_count: Some(9),
            rating: crate::architecture::models::TrackRating::read_only(None),
            last_played: None,
        };
        let available =
            RegularPlaylistTrack::for_ui_test(media_key.clone(), 17, 29, &catalogue_track);
        let loaded = |entry_id: &str, position: i32| LoadedPlaylistEntry {
            stored: StoredPlaylistEntry {
                id: entry_id.to_string(),
                playlist_id: "mixed".to_string(),
                position,
                source_id,
                track_id: Some(track_id.clone()),
                local_track_id: None,
                match_title: "private persisted fingerprint".to_string(),
                match_artist: "private persisted artist".to_string(),
                match_album: "private persisted album".to_string(),
                match_duration_secs: Some(999),
                match_file_path: None,
            },
            local_track: None,
        };
        let projected = project_playlist_rows(
            vec![loaded("entry-first", 0), loaded("entry-duplicate", 1)],
            vec![
                RegularPlaylistTrackResolution::Available(Box::new(available.clone())),
                RegularPlaylistTrackResolution::Available(Box::new(available)),
            ],
        )
        .expect("exact remote projection");
        let rows = projected
            .into_iter()
            .map(crate::ui::source_connect::playlist_row_to_object)
            .collect::<Vec<_>>();

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row.source_id() == Some(source_id)
                && row.source_session_epoch() == Some(17)
                && row.source_catalogue_generation() == Some(29)
                && row.title() == "Current remote title"
                && row.uri().is_empty()
                && row.cover_art_url().is_empty()
        }));
        assert_ne!(rows[0].row_instance_id(), rows[1].row_instance_id());

        let store = gtk::gio::ListStore::new::<TrackObject>();
        for row in &rows {
            store.append(row);
        }
        let captured = registry
            .capture(&store, "playlist:mixed", 1)
            .expect("duplicate remote occurrence is playable");
        assert_eq!(captured.selected_index, 1);
        assert_eq!(captured.items.len(), 2);
        for (occurrence, item) in captured.items.iter().enumerate() {
            assert_eq!(item.identity.media_key, media_key);
            assert_eq!(
                item.identity.view_origin,
                Some(ViewOrigin::playlist("mixed").expect("playlist origin"))
            );
            assert_eq!(item.occurrence, occurrence);
            assert_eq!(item.source_session_epoch, Some(17));
            let guard = item
                .regular_playlist_guard
                .expect("available remote occurrence keeps its closed guard");
            assert_eq!(guard.source_id(), source_id);
            assert_eq!(guard.session_epoch(), 17);
            assert_eq!(guard.catalogue_generation(), 29);
            assert!(item.uri().is_empty());
            assert!(item.cover_art_url.is_empty());
            assert!(item.lastfm_source.is_none());
            assert!(item.lastfm_profile.is_none());
        }
    }

    #[test]
    fn filtering_the_view_does_not_remove_items_from_the_playback_snapshot() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("local", "a"), item("local", "b"), item("local", "c")],
            0,
        ));

        // The playing queue still contains B even if the current view does not.
        let filtered_view = ["a", "c"];
        assert!(!filtered_view.contains(&"b"));
        assert_eq!(session.advance(RepeatMode::Off, false), Some(1));
        assert_eq!(current_id(&session), "b");
    }

    #[test]
    fn source_navigation_does_not_change_playing_source_or_track() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "track-a")], 0));

        let active_view_source = "remote-server";
        assert_ne!(
            current_source(&session),
            SourceId::removable(active_view_source).expect("source ID")
        );
        assert_eq!(current_source(&session), SourceId::local());
        assert_eq!(current_id(&session), "track-a");
    }

    #[test]
    fn sequential_previous_and_repeat_all_use_snapshot_boundaries() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c")
            ],
            2,
        ));

        assert_eq!(session.advance(RepeatMode::Off, false), None);
        assert_eq!(current_id(&session), "c");
        assert_eq!(session.advance(RepeatMode::All, false), Some(0));
        assert_eq!(current_id(&session), "a");
        assert_eq!(session.previous(RepeatMode::All, false), Some(2));
        assert_eq!(current_id(&session), "c");
    }

    #[test]
    fn eos_repeat_policies_are_bound_to_the_snapshot_cursor() {
        let queue = vec![item("source", "a"), item("source", "b")];

        let mut repeat_one = PlaybackSession::default();
        assert!(repeat_one.replace_queue(queue.clone(), 1));
        // EOS repeat-one calls replay_current and does not move the cursor.
        assert_eq!(current_id(&repeat_one), "b");

        let mut repeat_off = PlaybackSession::default();
        assert!(repeat_off.replace_queue(queue.clone(), 1));
        assert_eq!(repeat_off.advance(RepeatMode::Off, false), None);

        let mut repeat_all = PlaybackSession::default();
        assert!(repeat_all.replace_queue(queue, 1));
        assert_eq!(repeat_all.advance(RepeatMode::All, false), Some(0));
        assert_eq!(current_id(&repeat_all), "a");
    }

    #[test]
    fn shuffle_visits_each_snapshot_item_once_before_repeat() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c")
            ],
            0,
        ));

        let mut visited = HashSet::from(["a".to_string()]);
        for _ in 0..2 {
            assert!(session.advance(RepeatMode::Off, true).is_some());
            visited.insert(current_id(&session).to_string());
        }
        assert_eq!(visited, HashSet::from(["a".into(), "b".into(), "c".into()]));
        assert_eq!(session.advance(RepeatMode::Off, true), None);

        // Repeat-all starts a complete new cycle and does not immediately
        // repeat the item at the rollover boundary.
        let rollover_from = session.current_index.expect("current index");
        let first = session
            .advance(RepeatMode::All, true)
            .expect("repeat-all begins another cycle");
        assert_ne!(first, rollover_from);
        let state = session.shuffle.as_ref().expect("shuffle state");
        assert_eq!(state.remaining.len(), 2);
        assert!(state.remaining.contains(&rollover_from));

        let mut second_cycle = HashSet::from([first]);
        for _ in 0..2 {
            second_cycle.insert(
                session
                    .advance(RepeatMode::All, true)
                    .expect("the complete repeat cycle remains available"),
            );
        }
        assert_eq!(second_cycle, HashSet::from([0, 1, 2]));
    }

    #[test]
    fn shuffled_previous_and_next_walk_exact_occurrence_history() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c"),
                item("source", "d"),
                item("source", "e"),
                item("source", "f"),
                item("source", "g"),
                item("source", "h"),
            ],
            0,
        ));

        let mut actual = vec![0];
        for _ in 0..4 {
            actual.push(
                session
                    .advance(RepeatMode::Off, true)
                    .expect("unvisited shuffle occurrence"),
            );
        }

        for expected in actual[1..4].iter().rev() {
            assert_eq!(session.previous(RepeatMode::All, true), Some(*expected));
        }
        for expected in &actual[2..=4] {
            assert_eq!(
                session.advance(RepeatMode::Off, true),
                Some(*expected),
                "fixed forward history precedes another random draw"
            );
        }

        let drawn = session
            .advance(RepeatMode::Off, true)
            .expect("an unvisited random occurrence follows forward history");
        assert!(!actual.contains(&drawn));
    }

    #[test]
    fn shuffle_history_is_bounded_and_never_fabricates_a_predecessor() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c"),
                item("source", "d"),
            ],
            0,
        ));

        let mut actual = vec![0];
        for _ in 0..40 {
            actual.push(
                session
                    .advance(RepeatMode::All, true)
                    .expect("repeat-all keeps selecting occurrences"),
            );
            let state = session.shuffle.as_ref().expect("shuffle state");
            assert!(state.history.len() <= SHUFFLE_TIMELINE_CAPACITY);
            assert!(state.cursor < state.history.len());
        }

        let retained = actual[actual.len() - SHUFFLE_TIMELINE_CAPACITY..].to_vec();
        assert_eq!(
            session
                .shuffle
                .as_ref()
                .expect("shuffle state")
                .history
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            retained
        );

        for expected in retained[..SHUFFLE_PRIOR_LIMIT].iter().rev() {
            assert_eq!(session.previous(RepeatMode::All, true), Some(*expected));
        }
        let boundary = session.shuffle.clone();
        assert_eq!(session.previous(RepeatMode::All, true), None);
        assert_eq!(
            session.shuffle, boundary,
            "the retained boundary is a no-op"
        );

        for expected in &retained[1..] {
            assert_eq!(session.advance(RepeatMode::Off, true), Some(*expected));
        }
    }

    #[test]
    fn duplicate_tracks_remain_distinct_shuffle_occurrences() {
        let mut first = item("source", "duplicate");
        first.occurrence = 0;
        let other = item("source", "other");
        let mut duplicate = item("source", "duplicate");
        duplicate.occurrence = 1;

        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![first, other, duplicate], 0));
        let mut visited = vec![(0, 0)];
        for _ in 0..2 {
            let index = session
                .advance(RepeatMode::Off, true)
                .expect("distinct queue occurrence");
            visited.push((index, session.current().expect("current").occurrence));
        }
        assert_eq!(
            visited
                .iter()
                .map(|(index, _)| *index)
                .collect::<HashSet<_>>(),
            HashSet::from([0, 1, 2])
        );
        assert!(visited.contains(&(0, 0)));
        assert!(visited.contains(&(2, 1)));

        let previous = visited[1];
        assert_eq!(session.previous(RepeatMode::Off, true), Some(previous.0));
        assert_eq!(session.current().expect("current").occurrence, previous.1);
        assert_eq!(session.advance(RepeatMode::Off, true), Some(visited[2].0));
        assert_eq!(session.current().expect("current").occurrence, visited[2].1);
    }

    #[test]
    fn one_and_two_item_shuffle_repeat_semantics_are_explicit() {
        let mut one = PlaybackSession::default();
        assert!(one.replace_queue(vec![item("source", "only")], 0));
        assert_eq!(one.advance(RepeatMode::Off, true), None);
        assert_eq!(one.advance(RepeatMode::One, true), None);
        for _ in 0..20 {
            assert_eq!(one.advance(RepeatMode::All, true), Some(0));
        }
        assert_eq!(
            one.shuffle.as_ref().expect("shuffle state").history.len(),
            SHUFFLE_TIMELINE_CAPACITY
        );
        for _ in 0..SHUFFLE_PRIOR_LIMIT {
            assert_eq!(one.previous(RepeatMode::All, true), Some(0));
        }
        assert_eq!(one.previous(RepeatMode::All, true), None);

        let mut two = PlaybackSession::default();
        assert!(two.replace_queue(vec![item("source", "left"), item("source", "right")], 0,));
        assert_eq!(two.advance(RepeatMode::Off, true), Some(1));
        assert_eq!(two.advance(RepeatMode::One, true), None);
        assert_eq!(two.previous(RepeatMode::All, true), Some(0));
        assert_eq!(two.advance(RepeatMode::Off, true), Some(1));
        for expected in [0, 1, 0, 1, 0, 1] {
            assert_eq!(two.advance(RepeatMode::All, true), Some(expected));
        }
    }

    #[test]
    fn shuffle_toggle_resets_navigation_but_preserves_the_current_item() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c")
            ],
            0,
        ));
        assert!(session.advance(RepeatMode::Off, true).is_some());
        let current = session.current_index;
        assert!(session.shuffle.is_some());

        // Either button transition deliberately starts a fresh traversal.
        session.reset_shuffle_navigation();
        assert_eq!(session.current_index, current);
        assert!(session.shuffle.is_none());
        assert_eq!(session.previous(RepeatMode::All, true), None);
        assert_eq!(session.current_index, current);

        session.reset_shuffle_navigation();
        let current = session.current_index.expect("current");
        let expected = if current + 1 < session.queue.len() {
            current + 1
        } else {
            0
        };
        assert_eq!(session.advance(RepeatMode::All, false), Some(expected));
        assert!(session.shuffle.is_none());
    }

    #[test]
    fn rejected_navigation_before_output_handoff_restores_all_shuffle_state() {
        let mut original = PlaybackSession::default();
        assert!(original.replace_queue(
            vec![
                authoritative_lastfm_item("local", "a"),
                authoritative_lastfm_item("local", "b"),
                authoritative_lastfm_item("local", "c"),
                authoritative_lastfm_item("local", "d"),
            ],
            0,
        ));
        assert!(original.advance(RepeatMode::Off, true).is_some());
        assert!(original.advance(RepeatMode::Off, true).is_some());
        let current = original.current_index.expect("current shuffled item");
        original.queue[current].duration_secs = Some(181);
        sync_test_lastfm_profile(&mut original.queue[current]);
        original.begin_history_occurrence_for_current();
        let accepted_generation = original.begin_event_generation();
        assert!(original.mark_load_accepted(accepted_generation));
        let expected_index = original.current_index;
        let expected_shuffle = original.shuffle.clone();
        let session = RefCell::new(original);

        assert!(!navigate_and_play(
            &session,
            |session| session.advance(RepeatMode::Off, true),
            || false,
        ));
        assert_eq!(session.borrow().current_index, expected_index);
        assert_eq!(session.borrow().shuffle, expected_shuffle);

        assert!(!navigate_and_play(
            &session,
            |session| session.previous(RepeatMode::All, true),
            || false,
        ));
        assert_eq!(session.borrow().current_index, expected_index);
        assert_eq!(session.borrow().shuffle, expected_shuffle);
        assert!(session
            .borrow_mut()
            .take_accepted_lastfm_output_load(accepted_generation)
            .is_some());

        // Once an output handoff is initiated, the selected occurrence is a
        // committed retry target; synchronous output rejection is handled by
        // the existing retry state rather than rolling navigation back.
        assert!(navigate_and_play(
            &session,
            |session| session.advance(RepeatMode::Off, true),
            || true,
        ));
        assert_ne!(session.borrow().current_index, expected_index);
    }

    #[test]
    fn unavailable_navigation_restores_the_move_only_accepted_handoff() {
        let mut item = authoritative_lastfm_item("local", "boundary");
        item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut item);
        let mut playback = PlaybackSession::default();
        assert!(playback.replace_queue(vec![item], 0));
        let generation = playback.begin_event_generation();
        assert!(playback.mark_load_accepted(generation));
        let session = RefCell::new(playback);

        assert!(!navigate_and_play(
            &session,
            |session| session.previous(RepeatMode::Off, false),
            || panic!("an unavailable selection cannot reach playback"),
        ));
        assert!(session
            .borrow_mut()
            .take_accepted_lastfm_output_load(generation)
            .is_some());
    }

    #[test]
    fn committed_navigation_revokes_an_extracted_predecessor_load() {
        let mut predecessor = authoritative_lastfm_item("local", "navigation-predecessor");
        predecessor.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut predecessor);
        let mut successor = authoritative_lastfm_item("local", "navigation-successor");
        successor.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut successor);

        let mut playback = PlaybackSession::default();
        assert!(playback.replace_queue(vec![predecessor, successor], 0));
        let predecessor_generation = playback.begin_event_generation();
        assert!(playback.mark_load_accepted(predecessor_generation));
        let delayed_predecessor = playback
            .take_accepted_lastfm_output_load(predecessor_generation)
            .expect("predecessor proof extracted before navigation");
        let session = RefCell::new(playback);

        assert!(navigate_and_play(
            &session,
            |session| session.advance(RepeatMode::Off, false),
            || {
                let mut session = session.borrow_mut();
                let generation = session.begin_event_generation();
                assert!(session.mark_load_accepted(generation));
                true
            },
        ));
        let successor_generation = session.borrow().current_event_generation();
        let newer = session
            .borrow_mut()
            .take_accepted_lastfm_output_load(successor_generation)
            .expect("committed navigation owns the successor proof");

        assert_newer_lastfm_load_survives_stale_predecessor(
            newer,
            successor_generation,
            delayed_predecessor,
        );
    }

    #[test]
    fn committed_repeat_one_replay_revokes_an_extracted_predecessor_load() {
        let mut item = authoritative_lastfm_item("local", "repeat-one-predecessor");
        item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut item);

        let mut playback = PlaybackSession::default();
        assert!(playback.replace_queue(vec![item], 0));
        let predecessor_generation = playback.begin_event_generation();
        assert!(playback.mark_load_accepted(predecessor_generation));
        let delayed_predecessor = playback
            .take_accepted_lastfm_output_load(predecessor_generation)
            .expect("predecessor proof extracted before repeat-one replay");
        let session = RefCell::new(playback);

        assert!(replay_current_occurrence(&session, || {
            let mut session = session.borrow_mut();
            let generation = session.begin_event_generation();
            assert!(session.mark_load_accepted(generation));
            true
        }));
        let successor_generation = session.borrow().current_event_generation();
        let newer = session
            .borrow_mut()
            .take_accepted_lastfm_output_load(successor_generation)
            .expect("committed repeat-one replay owns the successor proof");

        assert_newer_lastfm_load_survives_stale_predecessor(
            newer,
            successor_generation,
            delayed_predecessor,
        );
    }

    #[test]
    fn generation_changed_failed_navigation_and_replay_revoke_extracted_predecessors() {
        let mut navigation_predecessor =
            authoritative_lastfm_item("local", "failed-navigation-predecessor");
        navigation_predecessor.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut navigation_predecessor);
        let mut navigation_successor =
            authoritative_lastfm_item("local", "failed-navigation-successor");
        navigation_successor.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut navigation_successor);
        let mut navigation = PlaybackSession::default();
        assert!(navigation.replace_queue(vec![navigation_predecessor, navigation_successor], 0,));
        let navigation_generation = navigation.begin_event_generation();
        assert!(navigation.mark_load_accepted(navigation_generation));
        let delayed_navigation = navigation
            .take_accepted_lastfm_output_load(navigation_generation)
            .expect("navigation predecessor proof");
        let navigation = RefCell::new(navigation);

        let navigation_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            navigate_and_play(
                &navigation,
                |session| session.advance(RepeatMode::Off, false),
                || {
                    navigation.borrow_mut().begin_event_generation();
                    false
                },
            )
        }));
        if cfg!(debug_assertions) {
            assert!(navigation_result.is_err());
        } else {
            assert!(matches!(navigation_result, Ok(false)));
        }

        let mut replay_item = authoritative_lastfm_item("local", "failed-repeat-one-predecessor");
        replay_item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut replay_item);
        let mut replay = PlaybackSession::default();
        assert!(replay.replace_queue(vec![replay_item], 0));
        let replay_generation = replay.begin_event_generation();
        assert!(replay.mark_load_accepted(replay_generation));
        let delayed_replay = replay
            .take_accepted_lastfm_output_load(replay_generation)
            .expect("repeat-one predecessor proof");
        let replay = RefCell::new(replay);

        let replay_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            replay_current_occurrence(&replay, || {
                replay.borrow_mut().begin_event_generation();
                false
            })
        }));
        if cfg!(debug_assertions) {
            assert!(replay_result.is_err());
        } else {
            assert!(matches!(replay_result, Ok(false)));
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = crate::source_registry::SourceRegistry::new(runtime.handle().clone());
        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        for delayed in [delayed_navigation, delayed_replay] {
            let admission = owner.accept_output_load(delayed, &registry, &HashSet::new());
            assert!(admission.stale());
            let (handoff, error) = admission.into_update().into_parts();
            assert!(handoff.is_none());
            assert!(error.is_none());
        }
        drop(owner);
        runtime.block_on(registry.shutdown().wait());
    }

    #[test]
    fn queue_lifecycle_resets_and_non_navigation_updates_preserve_shuffle_history() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("local", "a"), item("local", "b"), item("local", "c")],
            0,
        ));
        assert!(session.advance(RepeatMode::Off, true).is_some());
        let history = session.shuffle.clone();

        assert_eq!(refresh(&mut session, "a", refreshed_metadata()), 1);
        assert_eq!(session.shuffle, history);
        let pending = session.begin_pending_resolution();
        assert!(session.accepts_event_generation(pending));
        assert!(session.cancel_pending_resolution_for_retry());
        assert_eq!(
            session.shuffle, history,
            "pending-resolution Pause preserves navigation"
        );
        assert!(!session.clear_if_source("another-source"));
        assert_eq!(session.shuffle, history);

        assert!(session.replace_queue(vec![item("source", "replacement")], 0));
        assert!(session.shuffle.is_none(), "a new queue resets history");

        assert!(session.replace_queue(vec![item("source", "a"), item("source", "b")], 0,));
        assert!(session.advance(RepeatMode::Off, true).is_some());
        assert!(session.clear_if_source("source"));
        assert!(
            session.shuffle.is_none(),
            "source retirement resets history"
        );

        assert!(session.replace_queue(vec![item("source", "a"), item("source", "b")], 0,));
        assert!(session.advance(RepeatMode::Off, true).is_some());
        let (output, output_state) = RecordingOutput::new(0);
        stop_owned_playback(&mut session, &output);
        assert!(session.shuffle.is_none(), "Stop resets history");
        assert_eq!(output_state.borrow().stops, 1);
    }

    #[test]
    fn current_output_control_is_generation_gated_and_ordered_before_output() {
        let mut playback = PlaybackSession::default();
        assert!(playback.replace_queue(vec![item("source", "controlled")], 0));
        let generation = playback.begin_event_generation();
        let session = RefCell::new(playback);
        let order = RefCell::new(Vec::new());

        assert!(apply_current_output_control(
            generation,
            |candidate| {
                order.borrow_mut().push("gate");
                session.borrow().accepts_event_generation(candidate)
            },
            |candidate| {
                assert_eq!(candidate, generation);
                assert!(session.try_borrow_mut().is_ok());
                order.borrow_mut().push("discontinuity");
            },
            || {
                assert!(session.try_borrow_mut().is_ok());
                order.borrow_mut().push("output");
            },
        ));
        assert_eq!(*order.borrow(), ["gate", "discontinuity", "output"]);

        order.borrow_mut().clear();
        session.borrow_mut().clear();
        assert!(!apply_current_output_control(
            generation,
            |candidate| {
                order.borrow_mut().push("gate");
                session.borrow().accepts_event_generation(candidate)
            },
            |_| order.borrow_mut().push("discontinuity"),
            || order.borrow_mut().push("output"),
        ));
        assert_eq!(*order.borrow(), ["gate"]);
    }

    #[test]
    fn toggle_directions_share_the_conservative_discontinuity_boundary() {
        for direction in ["pause", "resume"] {
            let order = RefCell::new(Vec::new());
            assert!(apply_current_output_control(
                PlayerEventGeneration::from_raw(51),
                |_| true,
                |_| order.borrow_mut().push("discontinuity"),
                || order.borrow_mut().push(direction),
            ));
            assert_eq!(*order.borrow(), ["discontinuity", direction]);
        }
    }

    #[test]
    fn external_abandonment_retires_coordinator_and_output_before_source() {
        let order = RefCell::new(Vec::new());
        apply_external_abandonment(
            || order.borrow_mut().push("session"),
            || order.borrow_mut().push("lastfm"),
            || order.borrow_mut().push("output"),
            || order.borrow_mut().push("source"),
        );
        assert_eq!(*order.borrow(), ["session", "lastfm", "output", "source"]);
    }

    #[test]
    fn shared_previous_dispatch_pins_threshold_step_and_boundary_restart() {
        let steps = Cell::new(0);
        let restarts = Cell::new(0);
        assert_eq!(
            dispatch_previous(
                PREVIOUS_RESTART_THRESHOLD_MS + 1,
                || {
                    steps.set(steps.get() + 1);
                    true
                },
                || restarts.set(restarts.get() + 1),
            ),
            PreviousDispatch::Restarted
        );
        assert_eq!(steps.get(), 0);
        assert_eq!(restarts.get(), 1);

        assert_eq!(
            dispatch_previous(
                PREVIOUS_RESTART_THRESHOLD_MS,
                || {
                    steps.set(steps.get() + 1);
                    true
                },
                || restarts.set(restarts.get() + 1),
            ),
            PreviousDispatch::Stepped
        );
        assert_eq!(steps.get(), 1);
        assert_eq!(restarts.get(), 1);

        assert_eq!(
            dispatch_previous(
                0,
                || {
                    steps.set(steps.get() + 1);
                    false
                },
                || restarts.set(restarts.get() + 1),
            ),
            PreviousDispatch::Restarted
        );
        assert_eq!(steps.get(), 2);
        assert_eq!(restarts.get(), 2);
    }

    #[test]
    fn a_restart_then_early_previous_walks_the_real_shuffle_timeline() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c")
            ],
            0,
        ));
        assert!(session.advance(RepeatMode::Off, true).is_some());
        let state = session.shuffle.as_ref().expect("shuffle state");
        let expected_previous = state.history[state.cursor - 1];
        let current = session.current_index;
        let restarts = Cell::new(0);

        assert_eq!(
            dispatch_previous(
                PREVIOUS_RESTART_THRESHOLD_MS + 1,
                || session.previous(RepeatMode::All, true).is_some(),
                || restarts.set(restarts.get() + 1),
            ),
            PreviousDispatch::Restarted
        );
        assert_eq!(session.current_index, current);

        assert_eq!(
            dispatch_previous(
                0,
                || session.previous(RepeatMode::All, true).is_some(),
                || restarts.set(restarts.get() + 1),
            ),
            PreviousDispatch::Stepped
        );
        assert_eq!(session.current_index, Some(expected_previous));
        assert_eq!(restarts.get(), 1);
    }

    #[test]
    fn external_file_is_a_one_item_queue() {
        let mut session = PlaybackSession::default();
        let external = QueueItem::direct_for_test(
            "file:///tmp/example.flac".to_string(),
            "Example".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        let first_source = external.identity.media_key.source_id;
        let first_track = external.identity.media_key.track_id.clone();
        let another = QueueItem::direct_for_test(
            "file:///tmp/example.flac".to_string(),
            "Example".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        assert_ne!(first_source, another.identity.media_key.source_id);
        assert_ne!(first_track, another.identity.media_key.track_id);
        assert_ne!(first_track.as_str(), first_source.to_string());
        assert!(session.replace_queue(vec![external], 0));
        assert_eq!(current_source(&session), first_source);
        assert_ne!(current_source(&session), SourceId::local());
        assert_eq!(session.advance(RepeatMode::Off, false), None);
        assert_eq!(session.advance(RepeatMode::All, false), Some(0));
    }

    #[test]
    fn managed_external_queue_is_pathless_epoch_bound_and_terminally_owned() {
        let source_id = SourceId::external();
        let external = QueueItem::external_for_test(source_id, 73);
        assert!(external.uri().is_empty());
        assert_eq!(external.source_session_epoch(), Some(73));

        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![external], 0));
        assert_eq!(session.current_external_source_id(), Some(source_id));
        let generation = session.begin_pending_resolution();
        assert!(session.accepts_event_generation(generation));
        assert_eq!(
            session.external_source_for_terminal(generation, false),
            Some(source_id)
        );
        assert_eq!(session.external_source_for_terminal(generation, true), None);

        session.clear();
        assert_eq!(session.current_external_source_id(), None);
        assert!(!session.accepts_event_generation(generation));
        assert_eq!(
            session.external_source_for_terminal(generation, false),
            None
        );
    }

    #[test]
    fn clearing_a_session_removes_queue_identity() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a")], 0));
        session.clear();
        assert!(!session.has_current());
        assert!(session.queue.is_empty());
    }

    #[test]
    fn source_retirement_clears_only_its_own_queue() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![protected_item("remote-a", "a")], 0));
        let retired_generation = session.begin_pending_resolution();

        assert!(!session.clear_if_source("remote-b"));
        assert!(session.accepts_event_generation(retired_generation));
        assert!(session.has_current());

        assert!(session.clear_if_source("remote-a"));
        assert!(!session.accepts_event_generation(retired_generation));
        assert!(!session.has_current());
        assert!(session.queue.is_empty());
    }

    #[test]
    fn source_retirement_clears_a_mixed_queue_before_navigation_reaches_it() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item(LOCAL_SOURCE_KEY, "local"), item("remote-a", "remote")],
            0,
        ));
        assert_eq!(current_source(&session), SourceId::local());
        assert!(session.clear_if_source("remote-a"));
        assert!(!session.has_current());
        assert!(session.queue.is_empty());
    }

    #[test]
    fn playlist_view_origin_is_separate_from_local_media_identity() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("playlist:favourites", "local-track")], 0));
        let generation = session.begin_pending_resolution();
        let identity = session.current_identity().expect("identity");
        assert_eq!(identity.media_key.source_id, SourceId::local());
        assert_eq!(
            identity.view_origin,
            Some(ViewOrigin::Playlist("favourites".to_string()))
        );

        assert!(!session.clear_if_source("playlist:other"));
        assert!(!session.clear_if_source("playlist:favourites"));
        assert!(session.accepts_event_generation(generation));
        assert!(session.clear_if_source(LOCAL_SOURCE_KEY));
        assert!(!session.has_current());

        assert!(session.replace_queue(vec![item(LOCAL_SOURCE_KEY, "local-track")], 0));
        assert!(!session.clear_if_source("playlist:favourites"));
        assert!(session.has_current());
    }

    #[test]
    fn radio_queries_share_media_namespace_but_not_view_ownership() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("radio-topvote", "station-uuid")], 0));
        let identity = session.current_identity().expect("identity");
        assert_eq!(identity.media_key.source_id, SourceId::radio_browser());
        assert_eq!(
            identity.view_origin,
            Some(ViewOrigin::Radio("top-voted".to_string()))
        );
        assert!(!session.clear_if_source("radio-nearme"));
        assert!(!session.clear_if_source("radio-topvote"));
        assert!(session.clear_if_source(&SourceId::radio_browser().to_string()));
        assert!(queue_view("radio-attacker-defined").is_none());
    }

    #[test]
    fn removable_queue_identity_is_namespaced_by_logical_device() {
        let device = "device:uuid:01234567-89ab-cdef-0123-456789abcdef";
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item(device, "unix:4172746973742f547261636b2e666c6163")],
            0
        ));
        let identity = session.current_identity().expect("identity");
        assert_eq!(
            identity.media_key.source_id,
            SourceId::removable(device).expect("device source ID")
        );
        assert_eq!(
            identity.media_key.track_id.as_str(),
            "unix:4172746973742f547261636b2e666c6163"
        );
    }

    #[test]
    fn stale_player_events_are_rejected_after_track_change_and_reset() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a"), item("local", "b")], 0));
        let first_generation = session.begin_event_generation();
        let stale_eos = crate::audio::PlayerEvent::ended(first_generation);
        assert!(session.accepts_event_generation(stale_eos.generation()));

        assert_eq!(session.advance(RepeatMode::Off, false), Some(1));
        let second_generation = session.begin_event_generation();
        assert!(!session.accepts_event_generation(stale_eos.generation()));
        assert!(session.accepts_event_generation(second_generation));

        session.clear();
        assert!(!session.accepts_event_generation(second_generation));
    }

    #[test]
    fn protected_resolution_is_owned_by_exact_playback_generation() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("remote", "a"), item("remote", "b")], 0));

        let stale = session.begin_pending_resolution();
        assert!(session.is_resolution_pending());
        assert_eq!(session.advance(RepeatMode::Off, false), Some(1));
        let current = session.begin_pending_resolution();

        assert!(!session.finish_pending_resolution(stale));
        assert!(session.finish_pending_resolution(current));
        assert!(!session.is_resolution_pending());
        assert!(!session.resolution_failed);
    }

    #[test]
    fn failed_resolution_is_retryable_and_stop_invalidates_it() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("remote", "a")], 0));
        let failed = session.begin_pending_resolution();

        assert!(session.fail_pending_resolution(failed));
        assert!(session.resolution_failed);
        assert!(!session.is_resolution_pending());

        let retry = session.begin_pending_resolution();
        assert!(!session.resolution_failed);
        session.clear();
        assert!(!session.finish_pending_resolution(retry));
        assert!(!session.has_current());
    }

    #[test]
    fn pending_pause_cancels_completion_and_keeps_play_retryable() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![protected_item("remote", "a")], 0));
        let cancelled = session.begin_pending_resolution();

        assert!(session.cancel_pending_resolution_for_retry());
        assert!(!session.is_resolution_pending());
        assert!(session.resolution_failed);
        assert!(session.has_current());
        assert!(!session.finish_pending_resolution(cancelled));

        let retry = session.begin_pending_resolution();
        assert_ne!(retry, cancelled);
        assert!(session.finish_pending_resolution(retry));
        assert!(!session.resolution_failed);
    }

    #[test]
    fn accepted_resolved_output_error_retries_identity_but_plain_direct_error_does_not() {
        let mut protected = PlaybackSession::default();
        assert!(protected.replace_queue(vec![protected_item("remote", "a")], 0));
        let failed_load = protected.begin_pending_resolution();
        assert!(protected.finish_pending_resolution(failed_load));

        assert!(protected.mark_resolved_load_failed(failed_load));
        assert!(protected.resolution_failed);
        assert!(!protected.accepts_event_generation(failed_load));
        let retry = protected.begin_pending_resolution();
        assert_ne!(retry, failed_load);

        let mut daap = PlaybackSession::default();
        let mut daap_item = item("daap-source", "a");
        daap_item.uri.clear();
        daap_item.source_session_epoch = Some(9);
        assert!(daap.replace_queue(vec![daap_item], 0));
        let daap_load = daap.begin_event_generation();
        assert!(daap.mark_resolved_load_failed(daap_load));
        assert!(daap.resolution_failed);

        let mut local = PlaybackSession::default();
        assert!(local.replace_queue(vec![item("local", "a")], 0));
        let local_load = local.begin_pending_resolution();
        assert!(local.finish_pending_resolution(local_load));
        assert!(local.mark_resolved_load_failed(local_load));
        assert!(local.resolution_failed);

        let mut direct = PlaybackSession::default();
        assert!(direct.replace_queue(vec![item("radio", "a")], 0));
        let direct_load = direct.begin_event_generation();
        assert!(!direct.mark_resolved_load_failed(direct_load));
        assert!(!direct.resolution_failed);
        assert!(direct.accepts_event_generation(direct_load));
    }

    #[test]
    fn synchronous_load_rejection_keeps_exact_direct_generation_retryable() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a")], 0));
        let rejected = session.begin_event_generation();

        assert!(session.mark_load_rejected(rejected));
        assert!(session.resolution_failed);
        assert!(session.accepts_event_generation(rejected));
        assert!(
            !session.mark_resolved_load_failed(rejected),
            "synchronous refusal has no output state to stop"
        );

        let retry = session.begin_event_generation();
        assert_ne!(retry, rejected);
        assert!(!session.resolution_failed);
        assert!(!session.mark_load_rejected(rejected));
        assert!(session.mark_load_rejected(retry));
    }

    #[test]
    fn buffering_reset_invalidates_timer_and_state() {
        let tracker = BufferingTracker::default();
        let stale_timer = tracker.begin();
        assert!(tracker.is_current(stale_timer));
        assert!(tracker.is_buffering());

        tracker.invalidate();

        assert!(!tracker.is_current(stale_timer));
        assert!(!tracker.is_buffering());
    }

    #[test]
    fn os_stop_then_play_starts_a_fresh_visible_queue() {
        let session = RefCell::new(PlaybackSession::default());
        assert!(session
            .borrow_mut()
            .replace_queue(vec![item("local", "a")], 0));
        assert_eq!(
            resolve_session_play_request(&session, 3, false),
            PlayRequest::Resume
        );

        session.borrow_mut().clear();

        assert_eq!(
            resolve_session_play_request(&session, 3, false),
            PlayRequest::StartAt(0)
        );
        assert!(
            session
                .try_borrow_mut()
                .expect("play-request resolution must release its immutable borrow")
                .replace_queue(vec![item("remote", "fresh")], 0),
            "the StartAt arm must be able to install a fresh queue"
        );
    }

    #[test]
    fn history_is_local_only_and_playlist_rows_keep_exact_library_identity() {
        let mut local = PlaybackSession::default();
        assert!(local.replace_queue(
            vec![history_item("local", "library-track", Some(20_000))],
            0,
        ));
        let local_history = local.history_occurrence.as_ref().expect("local history");
        assert_eq!(local_history.track_id.as_str(), "library-track");
        assert_eq!(local_history.progress.threshold_ms(), 10_000);

        let mut playlist = PlaybackSession::default();
        assert!(playlist.replace_queue(
            vec![history_item(
                "playlist:favourites",
                "projected-library-track",
                Some(9_001),
            )],
            0,
        ));
        let playlist_history = playlist
            .history_occurrence
            .as_ref()
            .expect("playlist projection is local");
        assert_eq!(
            playlist_history.track_id.as_str(),
            "projected-library-track"
        );
        assert_eq!(playlist_history.progress.threshold_ms(), 4_501);

        for remote in [
            item("subsonic-source", "library-track"),
            item("removable-drive", "library-track"),
            QueueItem::external_for_test(SourceId::random(), 9),
        ] {
            let mut session = PlaybackSession::default();
            assert!(session.replace_queue(vec![remote], 0));
            assert!(session.history_occurrence.is_none());
            let generation = session.begin_event_generation();
            assert!(!session.mark_history_load_accepted(generation));
        }
    }

    #[test]
    fn lastfm_snapshot_requires_output_acceptance_and_structured_eligible_source_metadata() {
        let mut local_item = authoritative_lastfm_item("local", "structured-track");
        local_item.album_artist = Some("Exact Album Artist".to_string());
        local_item.track_number = Some(7);
        local_item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut local_item);
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![local_item], 0));

        let generation = session.begin_event_generation();
        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_none());
        assert!(session.mark_load_accepted(generation));
        let accepted = session
            .take_accepted_lastfm_output_load(generation)
            .expect("accepted local source");
        assert_eq!(
            format!("{accepted:?}"),
            "LastFmAcceptedOutputLoad::Eligible(<redacted>)"
        );

        let _ = session.observe_history_event(&PlayerEvent::error(generation, "private error"));
        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_none());

        let (output, _) = RecordingOutput::new(0);
        let mut radio = PlaybackSession::default();
        assert!(radio.replace_queue(vec![item("radio-topvote", "station")], 0));
        let direct = radio
            .load_current_direct(&output)
            .expect("direct radio load attempted");
        assert!(direct.accepted);
        let ineligible = radio
            .take_accepted_lastfm_output_load(direct.generation)
            .expect("an accepted radio load explicitly retires eligible predecessors");
        assert_eq!(
            format!("{ineligible:?}"),
            "LastFmAcceptedOutputLoad::Ineligible"
        );
    }

    #[test]
    fn accepted_lastfm_handoff_is_exact_one_shot_and_retry_generation_reopens_it() {
        let mut item = authoritative_lastfm_item("local", "one-shot");
        item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut item);
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item], 0));

        let generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(generation));
        assert!(session
            .take_accepted_lastfm_output_load(generation.next())
            .is_none());
        assert_eq!(
            session.accepted_lastfm_load.generation(),
            Some(generation),
            "a mismatched generation cannot consume current acceptance proof"
        );
        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_some());
        assert!(session.accepted_lastfm_load.is_consumed());
        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_none());

        let retry_generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(retry_generation));
        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_none());
        assert_eq!(
            session.accepted_lastfm_load.generation(),
            Some(retry_generation)
        );
        assert!(session
            .take_accepted_lastfm_output_load(retry_generation)
            .is_some());
        assert!(session
            .take_accepted_lastfm_output_load(retry_generation)
            .is_none());
    }

    #[test]
    fn metadata_free_discard_consumes_and_revokes_the_exact_accepted_proof() {
        let mut item = authoritative_lastfm_item("local", "dormant-discard");
        item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut item);
        let session = Rc::new(RefCell::new(PlaybackSession::default()));
        assert!(session.borrow_mut().replace_queue(vec![item], 0));

        let generation = session.borrow_mut().begin_event_generation();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let mut coordinator =
            crate::lastfm::playback_coordinator::LastFmPlaybackCoordinatorOwner::isolated_for_test(
            );
        let binding = coordinator
            .bind_window(registry.clone())
            .expect("bind dormant coordinator");

        assert!(finish_coordinated_output_load(
            &session, &binding, generation, true,
        ));
        assert!(session.borrow().accepted_lastfm_load.is_consumed());
        assert!(session
            .borrow_mut()
            .take_accepted_lastfm_output_load(generation)
            .is_none());
        assert!(session.borrow_mut().mark_load_accepted(generation));
        assert!(
            session
                .borrow_mut()
                .take_accepted_lastfm_output_load(generation)
                .is_none(),
            "re-observing acceptance cannot reopen a discarded proof"
        );
        drop(binding);
        drop(coordinator);
        runtime.block_on(registry.shutdown().wait());
    }

    #[test]
    fn direct_attempt_revokes_predecessor_before_reentrant_output_callback() {
        let mut item = authoritative_lastfm_item("local", "ordered-direct-retry");
        item.duration_secs = Some(181);
        item.uri = "https://media.invalid/ordered-direct-retry".to_string();
        sync_test_lastfm_profile(&mut item);
        let session = Rc::new(RefCell::new(PlaybackSession::default()));
        assert!(session.borrow_mut().replace_queue(vec![item], 0));
        let predecessor_generation = session.borrow_mut().begin_event_generation();
        assert!(session
            .borrow_mut()
            .mark_load_accepted(predecessor_generation));
        let delayed_predecessor = session
            .borrow_mut()
            .take_accepted_lastfm_output_load(predecessor_generation)
            .expect("extract predecessor before retry");

        let prepared = session
            .borrow_mut()
            .prepare_current_direct_load()
            .expect("prepare direct retry");
        let PreparedDirectLoad {
            uri,
            generation,
            intent,
        } = prepared;
        assert_eq!(format!("{intent:?}"), "LastFmOutputIntent(<redacted>)");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        assert!(owner
            .accept_output_load(delayed_predecessor, &registry, &HashSet::new())
            .stale());

        let (mut output, _) = RecordingOutput::new(1);
        output.session_borrow_probe = Some(Rc::downgrade(&session));
        output.set_event_generation(generation);
        assert!(!output.load_uri(&uri));
        assert!(session.borrow_mut().finish_output_load(generation, false));
        assert!(session.borrow().resolution_failed);

        runtime.block_on(registry.shutdown().wait());
    }

    #[test]
    fn delayed_eligible_and_ineligible_loads_cannot_replace_newer_owner_state() {
        fn eligible_item(track_id: &str) -> QueueItem {
            let mut item = authoritative_lastfm_item("local", track_id);
            item.duration_secs = Some(181);
            sync_test_lastfm_profile(&mut item);
            item
        }

        fn assert_newer_remains_active(
            owner: &mut crate::lastfm::playback_owner::LastFmPlaybackOwner,
            generation: PlayerEventGeneration,
        ) {
            let update = owner.observe_event(&PlayerEvent::state(generation, PlayerState::Playing));
            let (handoff, error) = update.into_parts();
            assert!(error.is_none());
            assert_eq!(
                handoff.as_ref().map(|handoff| handoff.kind()),
                Some(crate::lastfm::playback_owner::LastFmPlaybackHandoffKind::NowPlaying),
                "a stale accepted-load proof cannot retire or replace the newer occurrence"
            );
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = crate::source_registry::SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();

        let mut eligible_session = PlaybackSession::default();
        assert!(eligible_session.replace_queue(vec![eligible_item("older-eligible")], 0));
        let older_generation = eligible_session.begin_event_generation();
        assert!(eligible_session.mark_load_accepted(older_generation));
        let older = eligible_session
            .take_accepted_lastfm_output_load(older_generation)
            .expect("older eligible accepted load");
        assert!(eligible_session.replace_queue(vec![eligible_item("newer-eligible")], 0));
        let newer_generation = eligible_session.begin_event_generation();
        assert!(eligible_session.mark_load_accepted(newer_generation));
        let newer = eligible_session
            .take_accepted_lastfm_output_load(newer_generation)
            .expect("newer eligible accepted load");

        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        assert!(owner
            .accept_output_load(newer, &registry, &enabled_remote_sources)
            .admitted());
        let stale = owner.accept_output_load(older, &registry, &enabled_remote_sources);
        assert!(matches!(
            &stale,
            crate::lastfm::playback_owner::LastFmPlaybackLoadAdmission::Stale(_)
        ));
        let (handoff, error) = stale.into_update().into_parts();
        assert!(handoff.is_none());
        assert!(error.is_none());
        assert_newer_remains_active(&mut owner, newer_generation);

        let mut ineligible_session = PlaybackSession::default();
        assert!(ineligible_session.replace_queue(vec![item("radio-topvote", "station")], 0));
        let older_generation = ineligible_session.begin_event_generation();
        assert!(ineligible_session.mark_load_accepted(older_generation));
        let older = ineligible_session
            .take_accepted_lastfm_output_load(older_generation)
            .expect("older ineligible accepted load");
        assert!(ineligible_session.replace_queue(vec![eligible_item("newer-after-radio")], 0));
        let newer_generation = ineligible_session.begin_event_generation();
        assert!(ineligible_session.mark_load_accepted(newer_generation));
        let newer = ineligible_session
            .take_accepted_lastfm_output_load(newer_generation)
            .expect("newer eligible accepted load");

        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        assert!(owner
            .accept_output_load(newer, &registry, &enabled_remote_sources)
            .admitted());
        let stale = owner.accept_output_load(older, &registry, &enabled_remote_sources);
        assert!(matches!(
            &stale,
            crate::lastfm::playback_owner::LastFmPlaybackLoadAdmission::Stale(_)
        ));
        let (handoff, error) = stale.into_update().into_parts();
        assert!(handoff.is_none());
        assert!(error.is_none());
        assert_newer_remains_active(&mut owner, newer_generation);
    }

    #[test]
    fn dropping_playback_session_revokes_an_extracted_accepted_load() {
        let mut item = authoritative_lastfm_item("local", "dropped-session");
        item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut item);
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item], 0));
        let generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(generation));
        let delayed = session
            .take_accepted_lastfm_output_load(generation)
            .expect("accepted load extracted before abnormal teardown");
        drop(session);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = crate::source_registry::SourceRegistry::new(runtime.handle().clone());
        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        let admission = owner.accept_output_load(delayed, &registry, &HashSet::new());
        assert!(admission.stale());
        let (handoff, error) = admission.into_update().into_parts();
        assert!(handoff.is_none());
        assert!(error.is_none());
    }

    #[test]
    fn move_only_handoff_stays_one_shot_across_repeated_acceptance_and_retry() {
        let mut item = authoritative_lastfm_item("local", "move-only-one-shot");
        item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut item);
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item], 0));

        let generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(generation));
        let accepted = session
            .take_accepted_lastfm_output_load(generation)
            .expect("the session owns one accepted handoff");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = crate::source_registry::SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        assert!(owner
            .accept_output_load(accepted, &registry, &enabled_remote_sources)
            .into_update()
            .into_parts()
            .1
            .is_none());

        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_none());
        assert!(session.mark_load_accepted(generation));
        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_none());

        let retry_attempt = session.begin_output_attempt();
        let retry_generation = retry_attempt.generation;
        let (handoff, error) = owner
            .observe_output_intent(retry_attempt.intent)
            .into_parts();
        assert!(handoff.is_none());
        assert!(error.is_none());
        assert!(session.mark_load_accepted(retry_generation));
        let retry = session
            .take_accepted_lastfm_output_load(retry_generation)
            .expect("a genuinely new accepted generation receives one fresh handoff");
        assert!(owner
            .accept_output_load(retry, &registry, &enabled_remote_sources)
            .into_update()
            .into_parts()
            .1
            .is_none());
        assert!(session
            .take_accepted_lastfm_output_load(retry_generation)
            .is_none());
    }

    #[test]
    fn invalid_lastfm_candidate_becomes_one_shot_ineligible_output_load() {
        let invalid = authoritative_lastfm_item("local", "missing-duration");
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![invalid], 0));
        let generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(generation));

        let ineligible = session
            .take_accepted_lastfm_output_load(generation)
            .expect("an invalid candidate still replaces the owner's output occurrence");
        assert_eq!(
            format!("{ineligible:?}"),
            "LastFmAcceptedOutputLoad::Ineligible"
        );
        assert!(session.accepted_lastfm_load.is_consumed());
        assert!(session
            .take_accepted_lastfm_output_load(generation)
            .is_none());
    }

    #[test]
    fn accepted_retry_uses_occurrence_frozen_metadata_after_queue_refresh() {
        let mut local_item = authoritative_lastfm_item("local", "frozen-track");
        local_item.title = "Original Title".to_string();
        local_item.artist = "Original Artist".to_string();
        local_item.album = "Original Album".to_string();
        local_item.album_artist = Some("Original Album Artist".to_string());
        local_item.track_number = Some(3);
        local_item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut local_item);

        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![local_item], 0));
        let first_generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(first_generation));
        let first = session
            .take_accepted_lastfm_output_load(first_generation)
            .expect("local occurrence is eligible");

        let retry_generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(retry_generation));
        assert_eq!(
            refresh(
                &mut session,
                "frozen-track",
                QueueTrackRefresh {
                    title: "Refreshed Title".to_string(),
                    artist: "Refreshed Artist".to_string(),
                    album: "Refreshed Album".to_string(),
                    album_artist: Some("Refreshed Album Artist".to_string()),
                    track_number: Some(9),
                    duration_secs: Some(301),
                    cover_art_url: "refreshed-art".to_string(),
                },
            ),
            1
        );
        assert_eq!(
            session.current().expect("current item").title,
            "Refreshed Title"
        );
        let retry = session
            .take_accepted_lastfm_output_load(retry_generation)
            .expect("local retry remains eligible");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = crate::source_registry::SourceRegistry::new(runtime.handle().clone());
        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        let enabled_remote_sources = HashSet::new();
        let first_update = owner
            .accept_output_load(first, &registry, &enabled_remote_sources)
            .into_update();
        assert!(first_update.into_parts().1.is_none());
        let retry_update = owner
            .accept_output_load(retry, &registry, &enabled_remote_sources)
            .into_update();
        assert!(
            retry_update.into_parts().1.is_none(),
            "a queue refresh cannot drift one accepted occurrence's metadata"
        );
    }

    #[test]
    fn tagged_external_file_provenance_reaches_the_frozen_lastfm_candidate() {
        fn adopt_tagged_fixture(
            registry: &crate::source_registry::SourceRegistry,
            directory: &std::path::Path,
            file_name: &str,
            album: Option<&str>,
        ) -> crate::source_registry::ExternalFileSession {
            let path = directory.join(file_name);
            let mut fixture = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/audio/silence.flac"
            ))
            .to_vec();
            assert_eq!(&fixture[..4], b"fLaC");
            // The shared audio fixture is intentionally only 100 ms. Rewrite
            // its STREAMINFO declaration to 31 seconds so this provenance
            // test crosses the real Last.fm duration boundary without adding
            // a large binary fixture. Lofty derives duration from this exact
            // source metadata; playback/decoder behavior is not under test.
            let packed = u64::from_be_bytes(
                fixture[18..26]
                    .try_into()
                    .expect("FLAC STREAMINFO sample word"),
            );
            let sample_rate = (packed >> 44) & 0x0f_ffff;
            assert!(sample_rate > 0);
            let total_samples = sample_rate * 31;
            assert!(total_samples < (1_u64 << 36));
            let declared = (packed & !((1_u64 << 36) - 1)) | total_samples;
            fixture[18..26].copy_from_slice(&declared.to_be_bytes());
            std::fs::write(&path, fixture).expect("copy FLAC fixture");
            crate::local::tag_writer::write_tags(
                &path,
                &crate::local::tag_writer::TagEdits {
                    title: Some("Tagged Title".to_string()),
                    artist: Some("Tagged Artist".to_string()),
                    // Explicitly clear the fixture album in the missing case,
                    // so parser provenance rather than fixture state decides
                    // whether the optional Last.fm field survives.
                    album: Some(album.unwrap_or_default().to_string()),
                    ..Default::default()
                },
            )
            .expect("write fixture tags");

            registry
                .adopt_external_file_if_current(
                    std::fs::File::open(&path).expect("open tagged FLAC"),
                    crate::external_file::ExternalFileHint::new(file_name, Some("flac"))
                        .expect("safe external-file hint"),
                    || true,
                )
                .expect("parse and adopt tagged external file")
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = crate::source_registry::SourceRegistry::new(runtime.handle().clone());
        let directory = tempfile::tempdir().expect("external FLAC fixture directory");

        let missing_album =
            adopt_tagged_fixture(&registry, directory.path(), "missing-album.flac", None);
        assert_eq!(missing_album.track().title, "Tagged Title");
        assert_eq!(missing_album.track().artist_name, "Tagged Artist");
        assert_eq!(missing_album.track().album_title, "Unknown Album");
        assert_eq!(missing_album.track().duration_secs, Some(31));
        assert!(missing_album.playback_source().is_some());
        assert_eq!(
            missing_album
                .playback_source()
                .and_then(|source| source.profile().album()),
            None
        );

        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![QueueItem::external(&missing_album)], 0));
        let candidate = session
            .lastfm_occurrence_candidate
            .as_ref()
            .expect("tagged title and artist make the external occurrence eligible");
        assert_eq!(candidate.title, "Tagged Title");
        assert_eq!(candidate.artist, "Tagged Artist");
        assert_eq!(candidate.album, None, "display fallback album is omitted");

        let tagged_album = adopt_tagged_fixture(
            &registry,
            directory.path(),
            "tagged-album.flac",
            Some("Tagged Album"),
        );
        assert_eq!(
            tagged_album
                .playback_source()
                .and_then(|source| source.profile().album()),
            Some("Tagged Album")
        );
        assert_eq!(tagged_album.track().album_title, "Tagged Album");
        assert!(session.replace_queue(vec![QueueItem::external(&tagged_album)], 0));
        assert_eq!(
            session
                .lastfm_occurrence_candidate
                .as_ref()
                .and_then(|candidate| candidate.album.as_deref()),
            Some("Tagged Album"),
            "an explicit album tag remains authoritative optional metadata"
        );

        let generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(generation));
        let matching = session
            .take_accepted_lastfm_output_load(generation)
            .expect("matching structured tags produce one accepted output proof");
        assert_eq!(
            format!("{matching:?}"),
            "LastFmAcceptedOutputLoad::Eligible(<redacted>)"
        );
        let enabled_remote_sources = HashSet::new();
        let mut owner = crate::lastfm::playback_owner::LastFmPlaybackOwner::new();
        let admission = owner.accept_output_load(matching, &registry, &enabled_remote_sources);
        assert!(admission.admitted());
        let (handoff, error) = admission.into_update().into_parts();
        assert!(handoff.is_none());
        assert!(error.is_none());

        let (handoff, error) = owner
            .observe_event(&PlayerEvent::state(generation, PlayerState::Playing))
            .into_parts();
        assert!(error.is_none());
        let now_playing = handoff.expect("matching managed occurrence emits now-playing");
        assert_eq!(
            now_playing.kind(),
            crate::lastfm::playback_owner::LastFmPlaybackHandoffKind::NowPlaying
        );
        let callback_calls = Cell::new(0);
        let admitted = now_playing.try_admit_with_callbacks_for_test(
            &registry,
            &enabled_remote_sources,
            |_| {
                callback_calls.set(callback_calls.get() + 1);
                crate::lastfm::playback_owner::LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                callback_calls.set(callback_calls.get() + 1);
                crate::lastfm::playback_owner::LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                callback_calls.set(callback_calls.get() + 1);
                crate::lastfm::playback_owner::LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert_eq!(
            admitted,
            Some(crate::lastfm::playback_owner::LastFmPlaybackHandoffKind::NowPlaying)
        );
        assert_eq!(callback_calls.get(), 1);

        let mut mismatched = QueueItem::external(&tagged_album);
        mismatched.lastfm_profile = Some(PlaybackAttributionProfile::for_test(
            "Different Title",
            "Tagged Artist",
            Some("Tagged Album"),
            None,
            tagged_album.track().track_number,
            tagged_album.track().duration_secs,
        ));
        assert!(session.replace_queue(vec![mismatched], 0));
        let generation = session.begin_event_generation();
        assert!(session.mark_load_accepted(generation));
        let ineligible = session
            .take_accepted_lastfm_output_load(generation)
            .expect("the accepted output replacement must still reach ownership");
        assert_eq!(
            format!("{ineligible:?}"),
            "LastFmAcceptedOutputLoad::Ineligible",
            "a profile cannot be paired with metadata different from its registry-minted reference"
        );
        let rejected = owner.accept_output_load(ineligible, &registry, &enabled_remote_sources);
        assert!(!rejected.admitted());
        assert!(!rejected.stale());
        let (handoff, error) = rejected.into_update().into_parts();
        assert!(error.is_none());
        assert_eq!(
            handoff.as_ref().map(|handoff| handoff.kind()),
            Some(crate::lastfm::playback_owner::LastFmPlaybackHandoffKind::ClearNowPlaying),
            "an accepted metadata mismatch terminally clears its eligible predecessor"
        );

        let missing_waiter = registry
            .retire_external(missing_album.source_id())
            .expect("retire missing-album session");
        let album_waiter = registry
            .retire_external(tagged_album.source_id())
            .expect("retire tagged-album session");
        runtime.block_on(async {
            missing_waiter.wait().await;
            album_waiter.wait().await;
            registry.shutdown().wait().await;
        });
    }

    #[test]
    fn retries_keep_but_new_queue_occurrences_replace_lastfm_candidate_identity() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                authoritative_lastfm_item("local", "a"),
                authoritative_lastfm_item("local", "b"),
            ],
            0,
        ));
        let first = session
            .lastfm_occurrence_candidate
            .as_ref()
            .map(|candidate| candidate.identity.clone())
            .expect("first occurrence identity");

        let _retry_generation = session.begin_event_generation();
        assert_eq!(
            session
                .lastfm_occurrence_candidate
                .as_ref()
                .map(|candidate| &candidate.identity),
            Some(&first)
        );

        assert_eq!(session.advance(RepeatMode::Off, false), Some(1));
        let second = session
            .lastfm_occurrence_candidate
            .as_ref()
            .map(|candidate| candidate.identity.clone())
            .expect("second occurrence identity");
        assert_ne!(first, second);

        session.begin_repeat_one_occurrence();
        let repeated = session
            .lastfm_occurrence_candidate
            .as_ref()
            .map(|candidate| &candidate.identity)
            .expect("repeat-one occurrence identity");
        assert_ne!(repeated, &second);
    }

    #[test]
    fn history_requires_a_current_successfully_accepted_delivery() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![history_item("local", "pending", Some(20_000))], 0,));

        let pending = session.begin_pending_resolution();
        assert!(!session.mark_history_load_accepted(pending));
        assert_eq!(
            session.observe_history_event(&PlayerEvent::state(pending, PlayerState::Playing)),
            None
        );
        assert_eq!(
            observe_position(&mut session, pending, 20_000, 20_000),
            None
        );

        assert!(session.finish_pending_resolution(pending));
        assert_eq!(
            observe_position(&mut session, pending, 20_000, 20_000),
            None
        );
        assert!(session.mark_history_load_accepted(pending));
        observe_playing(&mut session, pending);
        assert_eq!(observe_position(&mut session, pending, 0, 20_000), None);
        assert_eq!(
            observe_position(&mut session, pending, 10_000, 20_000)
                .as_ref()
                .map(TrackId::as_str),
            Some("pending")
        );

        let stale = pending;
        let retry = session.begin_event_generation();
        assert_ne!(retry, stale);
        assert_eq!(observe_position(&mut session, stale, 20_000, 20_000), None);

        assert!(session.mark_load_rejected(retry));
        assert_eq!(
            session.observe_history_event(&PlayerEvent::state(retry, PlayerState::Playing)),
            None
        );
        assert_eq!(observe_position(&mut session, retry, 20_000, 20_000), None);
    }

    #[test]
    fn direct_load_acceptance_is_wired_but_rejection_cannot_earn_credit() {
        let mut accepted_item = history_item("local", "accepted", Some(2));
        accepted_item.uri = "file:///music/accepted.flac".to_string();
        let mut accepted = PlaybackSession::default();
        assert!(accepted.replace_queue(vec![accepted_item], 0));
        let (output, _) = RecordingOutput::new(0);
        let load = accepted
            .load_current_direct(&output)
            .expect("direct test load");
        assert!(load.accepted);
        observe_playing(&mut accepted, load.generation);
        assert_eq!(observe_position(&mut accepted, load.generation, 0, 2), None);
        assert_eq!(
            observe_position(&mut accepted, load.generation, 1, 2)
                .as_ref()
                .map(TrackId::as_str),
            Some("accepted")
        );

        let mut rejected_item = history_item("local", "rejected", Some(2));
        rejected_item.uri = "file:///music/rejected.flac".to_string();
        let mut rejected = PlaybackSession::default();
        assert!(rejected.replace_queue(vec![rejected_item], 0));
        let (output, _) = RecordingOutput::new(1);
        let load = rejected
            .load_current_direct(&output)
            .expect("rejected direct test load");
        assert!(!load.accepted);
        assert_eq!(
            rejected
                .observe_history_event(&PlayerEvent::state(load.generation, PlayerState::Playing,)),
            None
        );
        assert_eq!(observe_position(&mut rejected, load.generation, 2, 2), None);
        assert_eq!(
            rejected
                .history_occurrence
                .as_ref()
                .map(|history| history.progress.credited_ms()),
            Some(0)
        );
    }

    #[test]
    fn queue_duration_is_frozen_and_first_output_duration_can_complete_unknown_media() {
        let mut known = PlaybackSession::default();
        assert!(known.replace_queue(vec![history_item("local", "known", Some(20_000))], 0,));
        let generation = accept_history_load(&mut known);
        observe_playing(&mut known, generation);
        assert_eq!(observe_position(&mut known, generation, 0, 1_000), None);
        assert_eq!(observe_position(&mut known, generation, 9_999, 1_000), None);
        assert_eq!(
            observe_position(&mut known, generation, 10_000, 1_000)
                .as_ref()
                .map(TrackId::as_str),
            Some("known"),
            "output duration cannot replace positive queue metadata"
        );

        let mut unknown = PlaybackSession::default();
        assert!(unknown.replace_queue(vec![history_item("local", "unknown", None)], 0));
        let generation = accept_history_load(&mut unknown);
        assert_eq!(observe_position(&mut unknown, generation, 0, 10_000), None);
        assert_eq!(
            observe_position(&mut unknown, generation, 5_000, 10_000)
                .as_ref()
                .map(TrackId::as_str),
            Some("unknown"),
            "first position proves playback and supplies the first positive duration"
        );
    }

    #[test]
    fn pause_buffer_and_missing_playing_state_reanchor_without_jump_credit() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![history_item("local", "remote-like", Some(20_000))], 0,));
        let generation = accept_history_load(&mut session);

        assert_eq!(
            session.observe_history_event(&PlayerEvent::state(generation, PlayerState::Buffering,)),
            None
        );
        assert_eq!(
            observe_position(&mut session, generation, 2_000, 20_000),
            None
        );
        assert_eq!(
            session
                .history_occurrence
                .as_ref()
                .map(|history| history.progress.credited_ms()),
            Some(0),
            "the first tick proves playback but only re-anchors"
        );
        assert_eq!(
            observe_position(&mut session, generation, 6_000, 20_000),
            None
        );

        assert_eq!(
            session.observe_history_event(&PlayerEvent::state(generation, PlayerState::Paused,)),
            None
        );
        assert_eq!(
            observe_position(&mut session, generation, 9_000, 20_000),
            None
        );
        assert_eq!(
            observe_position(&mut session, generation, 10_000, 20_000),
            None
        );
        assert_eq!(
            observe_position(&mut session, generation, 11_000, 20_000),
            None
        );
        assert_eq!(
            session
                .history_occurrence
                .as_ref()
                .map(|history| history.progress.credited_ms()),
            Some(4_000),
            "multiple advancing remote polls while paused earn no credit"
        );

        assert_eq!(
            session.observe_history_event(&PlayerEvent::state(generation, PlayerState::Stopped,)),
            None
        );
        assert_eq!(
            observe_position(&mut session, generation, 12_000, 20_000),
            None
        );
        assert_eq!(
            observe_position(&mut session, generation, 13_000, 20_000),
            None
        );
        assert_eq!(
            session
                .history_occurrence
                .as_ref()
                .map(|history| history.progress.credited_ms()),
            Some(4_000),
            "Stopped also suppresses advancing remote polls"
        );

        assert_eq!(
            session.observe_history_event(&PlayerEvent::state(generation, PlayerState::Playing,)),
            None
        );
        assert_eq!(
            observe_position(&mut session, generation, 14_000, 20_000),
            None,
            "explicit Playing re-anchors before restoring credit"
        );
        assert_eq!(
            observe_position(&mut session, generation, 20_000, 20_000)
                .as_ref()
                .map(TrackId::as_str),
            Some("remote-like")
        );
    }

    #[test]
    fn failed_delivery_retry_retains_credit_but_reanchors_new_generation() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![history_item("local", "retry", Some(20_000))], 0,));
        let first = accept_history_load(&mut session);
        observe_playing(&mut session, first);
        assert_eq!(observe_position(&mut session, first, 0, 20_000), None);
        assert_eq!(observe_position(&mut session, first, 4_000, 20_000), None);
        assert_eq!(
            session.observe_history_event(&PlayerEvent::error(first, "failed")),
            None
        );
        assert_eq!(observe_position(&mut session, first, 20_000, 20_000), None);

        let retry = accept_history_load(&mut session);
        assert_ne!(retry, first);
        assert_eq!(observe_position(&mut session, retry, 9_000, 20_000), None);
        assert_eq!(observe_position(&mut session, retry, 14_999, 20_000), None);
        assert_eq!(
            observe_position(&mut session, retry, 15_000, 20_000)
                .as_ref()
                .map(TrackId::as_str),
            Some("retry"),
            "retry keeps four seconds of credit but not the delivery jump"
        );
    }

    #[test]
    fn explicit_seek_uses_actual_anchor_and_forward_seek_suppresses_unknown_eos() {
        let mut skipped = PlaybackSession::default();
        assert!(skipped.replace_queue(vec![history_item("local", "skipped", None)], 0));
        let generation = accept_history_load(&mut skipped);
        observe_playing(&mut skipped, generation);
        assert_eq!(observe_position(&mut skipped, generation, 0, 0), None);
        assert_eq!(observe_position(&mut skipped, generation, 1_000, 0), None);
        assert!(skipped.observe_history_seek(generation, 1_500, 10_000));
        assert!(skipped
            .history_occurrence
            .as_ref()
            .is_some_and(|history| history.progress.observed_forward_skip()));
        assert_eq!(
            skipped.observe_history_event(&PlayerEvent::ended(generation)),
            None
        );

        let mut restarted = PlaybackSession::default();
        assert!(restarted.replace_queue(vec![history_item("local", "restarted", None)], 0));
        let generation = accept_history_load(&mut restarted);
        assert!(restarted.observe_history_seek(generation, 2_000, 0));
        assert_eq!(
            restarted
                .observe_history_event(&PlayerEvent::ended(generation))
                .as_ref()
                .map(TrackId::as_str),
            Some("restarted"),
            "a backward Previous restart is not skip evidence"
        );
        assert!(!restarted.observe_history_seek(generation.next(), 0, 1_000));
    }

    #[test]
    fn repeat_one_failure_rolls_back_only_its_tentative_history_occurrence() {
        let mut playback = PlaybackSession::default();
        let mut item = authoritative_lastfm_item("local", "repeat-rollback");
        item.duration_ms = Some(20_000);
        item.duration_secs = Some(181);
        sync_test_lastfm_profile(&mut item);
        assert!(playback.replace_queue(vec![item], 0));
        let generation = playback.begin_event_generation();
        assert!(playback.mark_load_accepted(generation));
        observe_playing(&mut playback, generation);
        assert_eq!(observe_position(&mut playback, generation, 0, 20_000), None);
        assert_eq!(
            observe_position(&mut playback, generation, 4_000, 20_000),
            None
        );
        let previous_occurrence = playback
            .history_occurrence
            .clone()
            .expect("current local occurrence");
        let session = RefCell::new(playback);

        assert!(!replay_current_occurrence(&session, || {
            let session = session.borrow();
            let tentative = session
                .history_occurrence
                .as_ref()
                .expect("tentative repeat occurrence");
            assert_ne!(tentative, &previous_occurrence);
            assert_eq!(session.event_generation, generation);
            false
        }));

        let restored = session.borrow();
        assert_eq!(restored.event_generation, generation);
        assert_eq!(
            restored
                .current()
                .map(|item| item.identity.media_key.track_id.as_str()),
            Some("repeat-rollback")
        );
        assert_eq!(
            restored.history_occurrence.as_ref(),
            Some(&previous_occurrence)
        );
        drop(restored);
        assert!(session
            .borrow_mut()
            .take_accepted_lastfm_output_load(generation)
            .is_some());
    }

    #[test]
    fn next_previous_repeat_all_and_repeat_one_create_fresh_occurrences() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                history_item("local", "a", Some(2)),
                history_item("local", "b", Some(2)),
            ],
            0,
        ));
        let first_a = accept_history_load(&mut session);
        assert_eq!(observe_position(&mut session, first_a, 0, 2), None);
        assert_eq!(
            observe_position(&mut session, first_a, 1, 2)
                .as_ref()
                .map(TrackId::as_str),
            Some("a")
        );

        assert_eq!(session.advance(RepeatMode::Off, false), Some(1));
        assert_eq!(
            session
                .history_occurrence
                .as_ref()
                .map(|h| h.track_id.as_str()),
            Some("b")
        );
        assert_eq!(
            session.observe_history_event(&PlayerEvent::ended(first_a)),
            None
        );
        let first_b = accept_history_load(&mut session);
        assert_eq!(observe_position(&mut session, first_b, 0, 2), None);
        assert_eq!(
            observe_position(&mut session, first_b, 1, 2)
                .as_ref()
                .map(TrackId::as_str),
            Some("b")
        );

        assert_eq!(session.previous(RepeatMode::Off, false), Some(0));
        let second_a = accept_history_load(&mut session);
        assert_eq!(observe_position(&mut session, second_a, 0, 2), None);
        assert_eq!(
            observe_position(&mut session, second_a, 1, 2)
                .as_ref()
                .map(TrackId::as_str),
            Some("a")
        );

        assert_eq!(session.previous(RepeatMode::All, false), Some(1));
        let wrapped_b = accept_history_load(&mut session);
        assert_eq!(observe_position(&mut session, wrapped_b, 0, 2), None);
        assert_eq!(
            observe_position(&mut session, wrapped_b, 1, 2)
                .as_ref()
                .map(TrackId::as_str),
            Some("b")
        );

        session.begin_repeat_one_occurrence();
        assert_eq!(
            session.observe_history_event(&PlayerEvent::ended(wrapped_b)),
            None
        );
        let repeated_b = accept_history_load(&mut session);
        assert_eq!(observe_position(&mut session, repeated_b, 0, 2), None);
        assert_eq!(
            observe_position(&mut session, repeated_b, 1, 2)
                .as_ref()
                .map(TrackId::as_str),
            Some("b")
        );

        let mut one = PlaybackSession::default();
        assert!(one.replace_queue(vec![history_item("local", "one", Some(2))], 0));
        let first = accept_history_load(&mut one);
        assert_eq!(observe_position(&mut one, first, 0, 2), None);
        assert!(observe_position(&mut one, first, 1, 2).is_some());
        assert_eq!(one.advance(RepeatMode::All, false), Some(0));
        let repeated = accept_history_load(&mut one);
        assert_eq!(observe_position(&mut one, repeated, 0, 2), None);
        assert_eq!(
            observe_position(&mut one, repeated, 1, 2)
                .as_ref()
                .map(TrackId::as_str),
            Some("one")
        );
    }

    #[test]
    fn known_and_unknown_eos_follow_contract_and_count_only_once() {
        let mut known = PlaybackSession::default();
        assert!(known.replace_queue(vec![history_item("local", "known-eos", Some(20_000))], 0,));
        let known_generation = accept_history_load(&mut known);
        assert_eq!(
            known.observe_history_event(&PlayerEvent::ended(known_generation)),
            None,
            "known EOS never invents tail credit"
        );

        let mut unknown = PlaybackSession::default();
        assert!(unknown.replace_queue(vec![history_item("local", "unknown-eos", None)], 0));
        let unknown_generation = accept_history_load(&mut unknown);
        assert_eq!(
            unknown
                .observe_history_event(&PlayerEvent::ended(unknown_generation))
                .as_ref()
                .map(TrackId::as_str),
            Some("unknown-eos")
        );
        assert_eq!(
            unknown.observe_history_event(&PlayerEvent::ended(unknown_generation)),
            None,
            "duplicate EOS cannot count one occurrence twice"
        );
    }

    #[test]
    fn clear_retires_history_and_rejects_late_events_and_seeks() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![history_item("local", "cleared", Some(2))], 0,));
        let generation = accept_history_load(&mut session);
        session.clear();

        assert!(session.history_occurrence.is_none());
        assert_eq!(
            session.observe_history_event(&PlayerEvent::position(generation, 2, 2)),
            None
        );
        assert!(!session.observe_history_seek(generation, 0, 1));
    }

    #[test]
    fn duplicate_track_occurrences_keep_stable_identity_but_select_distinct_rows() {
        let mut first = item("playlist:one", "same-track");
        first.row_instance_id = Some(11);
        let mut second = first.clone();
        second.occurrence = 1;
        second.row_instance_id = Some(22);
        let media_key = first.identity.media_key.clone();
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![first, second], 1));

        assert_eq!(current_id(&session), "same-track");
        assert_eq!(session.current().map(|item| item.occurrence), Some(1));
        assert_eq!(
            find_queue_item_position(4, &media_key, 1, Some(22), |index| {
                [
                    (11, "same-track"),
                    (12, "other"),
                    (22, "same-track"),
                    (33, "same-track"),
                ]
                .get(index as usize)
                .map(|(row_id, track_id)| {
                    (
                        *row_id,
                        MediaKey::new(
                            media_key.source_id,
                            TrackId::new(*track_id).expect("test track ID"),
                        ),
                    )
                })
            }),
            Some(2)
        );
    }

    #[test]
    fn rebuilt_row_fallback_matches_source_and_track_identity() {
        let shared_track_id = TrackId::new("same-native-id").expect("shared track ID");
        let local_key = MediaKey::new(SourceId::local(), shared_track_id.clone());
        let remote_key = MediaKey::new(SourceId::random(), shared_track_id);
        let rebuilt_rows = [
            Some((101, remote_key.clone())),
            None, // unavailable or malformed rows do not participate
            Some((102, local_key.clone())),
            Some((103, remote_key.clone())),
            Some((104, local_key.clone())),
        ];

        assert_eq!(
            find_queue_item_position(5, &local_key, 0, None, |index| {
                rebuilt_rows[index as usize].clone()
            }),
            Some(2),
            "a remote row with the same native ID must not capture the local occurrence"
        );
        assert_eq!(
            find_queue_item_position(5, &remote_key, 1, None, |index| {
                rebuilt_rows[index as usize].clone()
            }),
            Some(3),
            "occurrence counting must be scoped to the complete media key"
        );
    }

    #[test]
    fn rebuilt_row_candidates_skip_unavailable_and_malformed_playlist_rows() {
        use crate::ui::objects::{PlaylistOccurrenceBinding, PlaylistRowUnavailableReason};

        let view = queue_view("playlist:mixed").expect("playlist view");
        let available_id = TrackId::new("available").expect("available track ID");
        let available = projected_row(available_id.as_str(), "file:///music/available.flac");
        available.set_playlist_occurrence_binding(
            PlaylistOccurrenceBinding::available_local("entry-available", available_id.clone())
                .expect("available occurrence"),
        );
        let unavailable = projected_row("missing", "");
        unavailable.set_playlist_occurrence_binding(
            PlaylistOccurrenceBinding::unavailable(
                "entry-unavailable",
                SourceId::local(),
                Some(TrackId::new("missing").expect("missing track ID")),
                PlaylistRowUnavailableReason::LocalTrackMissing,
            )
            .expect("unavailable occurrence"),
        );
        let malformed = projected_row("initially-valid", "");
        malformed.set_track_id("");

        assert_eq!(
            row_position_identity(&view, &available).map(|(_, key)| key),
            Some(MediaKey::new(SourceId::local(), available_id))
        );
        assert_eq!(row_position_identity(&view, &unavailable), None);
        assert_eq!(row_position_identity(&view, &malformed), None);
    }
}
