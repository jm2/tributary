//! Authoritative, generation-owned Last.fm playback evidence.
//!
//! This module deliberately knows nothing about GTK, queue models, source
//! policy, credentials, or network delivery. A playback coordinator first
//! freezes an already-authorized structured metadata snapshot, creates one
//! occurrence, and attaches only output generations whose media load was
//! accepted. The occurrence then emits at most one ephemeral now-playing
//! action and at most one durable scrobble-admission action.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use crate::db::entities::lastfm_scrobble::{MAX_LASTFM_METADATA_BYTES, MAX_LASTFM_STARTED_AT_SECS};

use super::client::LastFmTrack;
use super::storage::{LastFmQueueError, UnboundLastFmScrobble};

/// Longest observed listening time required for an eligible scrobble.
pub const MAX_LASTFM_SCROBBLE_THRESHOLD_MS: u64 = 4 * 60 * 1_000;

/// Frozen, validated metadata for one eligible Last.fm playback occurrence.
///
/// Callers must obtain these fields from the structured track snapshot owned
/// by the real media source. This boundary validates the Last.fm contract but
/// cannot authorize filename parsing, display fallbacks, or source-policy
/// changes.
#[derive(Clone, Eq, PartialEq)]
pub struct LastFmPlaybackMetadata {
    artist: String,
    title: String,
    album: Option<String>,
    album_artist: Option<String>,
    track_number: Option<i32>,
    duration_secs: i32,
}

impl LastFmPlaybackMetadata {
    /// Freeze one structured metadata snapshot.
    ///
    /// Required text is preserved byte-for-byte. Whitespace-only optional
    /// text is omitted; other optional text is also preserved byte-for-byte.
    /// A duration must be known, representable by the durable queue, and
    /// greater than 30 whole seconds.
    pub fn try_new(
        artist: String,
        title: String,
        album: Option<String>,
        album_artist: Option<String>,
        track_number: Option<u32>,
        duration_secs: Option<u64>,
    ) -> Result<Self, LastFmPlaybackEvidenceError> {
        if !valid_required_text(&artist) || !valid_required_text(&title) {
            return Err(LastFmPlaybackEvidenceError::InvalidMetadata);
        }

        let album = canonical_optional_text(album)?;
        let album_artist = canonical_optional_text(album_artist)?;
        let track_number = track_number
            .map(i32::try_from)
            .transpose()
            .map_err(|_| LastFmPlaybackEvidenceError::InvalidMetadata)?;
        if track_number == Some(0) {
            return Err(LastFmPlaybackEvidenceError::InvalidMetadata);
        }
        let duration_secs = duration_secs
            .and_then(|value| i32::try_from(value).ok())
            .filter(|value| *value > 30)
            .ok_or(LastFmPlaybackEvidenceError::InvalidMetadata)?;

        Ok(Self {
            artist,
            title,
            album,
            album_artist,
            track_number,
            duration_secs,
        })
    }

    fn now_playing_track(&self) -> LastFmTrack {
        LastFmTrack {
            artist: self.artist.clone(),
            title: self.title.clone(),
            album: self.album.clone(),
            album_artist: self.album_artist.clone(),
            track_number: self.track_number.map(|value| value as u32),
            duration_seconds: self.duration_secs as u32,
        }
    }
}

impl fmt::Debug for LastFmPlaybackMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPlaybackMetadata(<redacted>)")
    }
}

/// Coarse state evidence accepted from the current output generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmPlaybackState {
    Buffering,
    Playing,
    Paused,
    Stopped,
}

/// Wall-clock boundary used only when an occurrence first proves playback.
///
/// A stale event, a repeated Playing event, or non-playing state does not
/// consult this clock. Tests can therefore prove that the UTC start is
/// captured once rather than inferred from later wall-clock time.
pub trait LastFmPlaybackClock {
    fn now_unix_seconds(&self) -> Result<i64, LastFmPlaybackEvidenceError>;
}

/// Production UTC wall clock for Last.fm start evidence.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLastFmPlaybackClock;

impl LastFmPlaybackClock for SystemLastFmPlaybackClock {
    fn now_unix_seconds(&self) -> Result<i64, LastFmPlaybackEvidenceError> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| LastFmPlaybackEvidenceError::ClockOutOfRange)?;
        i64::try_from(elapsed.as_secs())
            .ok()
            .filter(|value| (1..=MAX_LASTFM_STARTED_AT_SECS).contains(value))
            .ok_or(LastFmPlaybackEvidenceError::ClockOutOfRange)
    }
}

/// One one-shot action proven by current-generation playback.
pub enum LastFmPlaybackAction {
    /// Attempt `track.updateNowPlaying` once; never persist or retry it.
    NowPlaying(LastFmTrack),
    /// Submit this validated account-independent value to runtime admission.
    Scrobble(UnboundLastFmScrobble),
}

impl fmt::Debug for LastFmPlaybackAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NowPlaying(_) => {
                formatter.write_str("LastFmPlaybackAction::NowPlaying(<redacted>)")
            }
            Self::Scrobble(_) => formatter.write_str("LastFmPlaybackAction::Scrobble(<redacted>)"),
        }
    }
}

/// Content-free failure at the playback-evidence boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmPlaybackEvidenceError {
    #[error("Last.fm playback metadata is invalid")]
    InvalidMetadata,
    #[error("Last.fm playback start clock is out of range")]
    ClockOutOfRange,
    #[error("Last.fm playback evidence is internally inconsistent")]
    Invariant,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PlaybackSamplingState {
    AwaitingGeneration,
    PositionMayProvePlaying,
    PlayingNeedsAnchor,
    PlayingAnchored { position_ms: u64 },
    PositionProofRevoked,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum NowPlayingLatch {
    Open,
    Captured { started_at_unix_secs: i64 },
    Failed,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScrobbleLatch {
    Open,
    Closed,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OccurrenceDisposition {
    Enabled,
    Disabled,
    Terminated,
}

/// Evidence retained for one genuine queue occurrence.
///
/// `G` is the playback coordinator's opaque generation type. Keeping it
/// generic lets GTK-side playback use `PlayerEventGeneration` directly while
/// this module remains independent of GTK and every concrete output backend.
/// The occurrence is deliberately not cloneable: its UUID and one-shot
/// latches are executable authority. A coordinator that needs speculative
/// rollback must move the value out of its `Option` and restore that same
/// value rather than forking it.
pub struct LastFmPlaybackOccurrence<G>
where
    G: Copy + Eq,
{
    occurrence_id: Uuid,
    metadata: LastFmPlaybackMetadata,
    accepted_generation: Option<G>,
    threshold_ms: u64,
    credited_ms: u64,
    sampling: PlaybackSamplingState,
    now_playing: NowPlayingLatch,
    scrobble: ScrobbleLatch,
    disposition: OccurrenceDisposition,
}

impl<G> LastFmPlaybackOccurrence<G>
where
    G: Copy + Eq,
{
    /// Create a fresh opaque occurrence before its first output load.
    #[must_use]
    pub fn new(metadata: LastFmPlaybackMetadata) -> Self {
        let duration_ms = u64::try_from(metadata.duration_secs)
            .expect("validated positive Last.fm duration")
            .saturating_mul(1_000);
        Self {
            occurrence_id: Uuid::new_v4(),
            metadata,
            accepted_generation: None,
            threshold_ms: lastfm_scrobble_threshold_ms(duration_ms),
            credited_ms: 0,
            sampling: PlaybackSamplingState::AwaitingGeneration,
            now_playing: NowPlayingLatch::Open,
            scrobble: ScrobbleLatch::Open,
            disposition: OccurrenceDisposition::Enabled,
        }
    }

    /// Attach a successfully accepted output load to this occurrence.
    ///
    /// A different accepted generation is a same-occurrence retry/handoff:
    /// accumulated credit and one-shot latches remain frozen, while its first
    /// position sample must re-anchor without credit. Repeating acceptance of
    /// the same generation is inert.
    pub fn accept_generation(&mut self, generation: G) -> bool {
        if self.disposition != OccurrenceDisposition::Enabled
            || self.accepted_generation == Some(generation)
        {
            return false;
        }
        self.accepted_generation = Some(generation);
        self.sampling = PlaybackSamplingState::PositionMayProvePlaying;
        true
    }

    /// Retire one rejected/failed delivery generation without ending the
    /// occurrence, allowing a same-occurrence retry to retain real credit.
    pub fn retire_generation(&mut self, generation: G) -> bool {
        if self.disposition != OccurrenceDisposition::Enabled
            || self.accepted_generation != Some(generation)
        {
            return false;
        }
        self.accepted_generation = None;
        self.sampling = PlaybackSamplingState::AwaitingGeneration;
        true
    }

    /// Observe a coarse state from one accepted current output generation.
    ///
    /// Playing is authoritative playback evidence. Buffering permits a later
    /// position sample to prove recovery for outputs without a clean Playing
    /// transition. Paused and Stopped explicitly revoke that fallback.
    pub fn observe_state<C>(
        &mut self,
        generation: G,
        state: LastFmPlaybackState,
        clock: &C,
    ) -> Result<Option<LastFmPlaybackAction>, LastFmPlaybackEvidenceError>
    where
        C: LastFmPlaybackClock + ?Sized,
    {
        if !self.accepts(generation) {
            return Ok(None);
        }

        match state {
            LastFmPlaybackState::Playing => {
                if !matches!(
                    self.sampling,
                    PlaybackSamplingState::PlayingNeedsAnchor
                        | PlaybackSamplingState::PlayingAnchored { .. }
                ) {
                    self.sampling = PlaybackSamplingState::PlayingNeedsAnchor;
                }
                self.begin_playing_evidence(clock)
            }
            LastFmPlaybackState::Buffering => {
                // A later Buffering notification must not undo the explicit
                // revocation established by Paused or Stopped. Only Playing
                // can restore evidence after those states.
                if self.sampling != PlaybackSamplingState::PositionProofRevoked {
                    self.sampling = PlaybackSamplingState::PositionMayProvePlaying;
                }
                Ok(None)
            }
            LastFmPlaybackState::Paused | LastFmPlaybackState::Stopped => {
                self.sampling = PlaybackSamplingState::PositionProofRevoked;
                Ok(None)
            }
        }
    }

    /// Observe a current-generation output position in milliseconds.
    ///
    /// The initial sample and the first sample after every accepted load,
    /// Playing transition, Buffering recovery, pause/resume, seek, restart, or
    /// retry only establish an anchor. Thereafter only strictly forward
    /// sampled deltas earn credit. Duplicate and regressed samples are inert.
    pub fn observe_position<C>(
        &mut self,
        generation: G,
        position_ms: u64,
        clock: &C,
    ) -> Result<Option<LastFmPlaybackAction>, LastFmPlaybackEvidenceError>
    where
        C: LastFmPlaybackClock + ?Sized,
    {
        if !self.accepts(generation) {
            return Ok(None);
        }

        let anchor_ms = match self.sampling {
            PlaybackSamplingState::PositionMayProvePlaying => {
                self.sampling = PlaybackSamplingState::PlayingAnchored { position_ms };
                return self.begin_playing_evidence(clock);
            }
            PlaybackSamplingState::PlayingNeedsAnchor => {
                self.sampling = PlaybackSamplingState::PlayingAnchored { position_ms };
                return Ok(None);
            }
            PlaybackSamplingState::PlayingAnchored {
                position_ms: anchor_ms,
            } => anchor_ms,
            PlaybackSamplingState::AwaitingGeneration
            | PlaybackSamplingState::PositionProofRevoked => return Ok(None),
        };
        if position_ms > anchor_ms {
            let advance_ms = position_ms - anchor_ms;
            self.credited_ms = self
                .credited_ms
                .saturating_add(advance_ms)
                .min(self.threshold_ms);
            self.sampling = PlaybackSamplingState::PlayingAnchored { position_ms };
        }

        if self.scrobble == ScrobbleLatch::Closed || self.credited_ms < self.threshold_ms {
            return Ok(None);
        }

        // Close the latch before constructing or handing off the queue item.
        // Admission failure must not let later playback recreate it.
        self.scrobble = ScrobbleLatch::Closed;
        let NowPlayingLatch::Captured {
            started_at_unix_secs,
        } = self.now_playing
        else {
            self.disposition = OccurrenceDisposition::Disabled;
            return Err(LastFmPlaybackEvidenceError::Invariant);
        };
        let scrobble = UnboundLastFmScrobble::try_new(
            self.occurrence_id,
            self.metadata.artist.clone(),
            self.metadata.title.clone(),
            self.metadata.album.clone(),
            self.metadata.album_artist.clone(),
            self.metadata.track_number,
            self.metadata.duration_secs,
            started_at_unix_secs,
        )
        .map_err(|error| {
            self.disposition = OccurrenceDisposition::Disabled;
            map_queue_construction_error(error)
        })?;
        Ok(Some(LastFmPlaybackAction::Scrobble(scrobble)))
    }

    /// Mark a seek, Previous restart, resume handoff, or other discontinuity.
    ///
    /// The next accepted position establishes a new no-credit anchor. This
    /// operation itself cannot emit or add listening credit.
    pub fn observe_discontinuity(&mut self, generation: G) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        self.sampling = match self.sampling {
            PlaybackSamplingState::PlayingAnchored { .. }
            | PlaybackSamplingState::PlayingNeedsAnchor => {
                PlaybackSamplingState::PlayingNeedsAnchor
            }
            PlaybackSamplingState::PositionMayProvePlaying => {
                PlaybackSamplingState::PositionMayProvePlaying
            }
            PlaybackSamplingState::AwaitingGeneration
            | PlaybackSamplingState::PositionProofRevoked => {
                PlaybackSamplingState::PositionProofRevoked
            }
        };
        true
    }

    /// End this occurrence on a current-generation natural EOS.
    ///
    /// EOS never supplies an unobserved tail or independently qualifies a
    /// scrobble.
    pub fn observe_natural_end(&mut self, generation: G) -> bool {
        self.terminate_generation(generation)
    }

    /// Retire the failed output generation without fabricating credit.
    ///
    /// Managed playback may retry the same queue occurrence, so an output
    /// error preserves already-observed credit, the original captured start,
    /// and both one-shot latches. If the coordinator instead decides the
    /// occurrence is terminal, it must subsequently call [`Self::retire`].
    pub fn observe_error(&mut self, generation: G) -> bool {
        self.retire_generation(generation)
    }

    /// End this occurrence unconditionally for Stop, source retirement,
    /// queue replacement, or application shutdown.
    pub fn retire(&mut self) {
        self.disposition = OccurrenceDisposition::Terminated;
        self.accepted_generation = None;
        self.sampling = PlaybackSamplingState::AwaitingGeneration;
    }

    /// Current exact qualification threshold, useful to the coordinator's
    /// deterministic tests but never included in diagnostics.
    #[must_use]
    pub const fn threshold_ms(&self) -> u64 {
        self.threshold_ms
    }

    /// Observed forward playback credit, capped at the threshold.
    #[must_use]
    pub const fn credited_ms(&self) -> u64 {
        self.credited_ms
    }

    /// Whether this occurrence has closed its now-playing action latch.
    #[must_use]
    pub const fn now_playing_latch_closed(&self) -> bool {
        !matches!(self.now_playing, NowPlayingLatch::Open)
    }

    /// Whether this occurrence has closed its scrobble-action latch.
    #[must_use]
    pub const fn scrobble_latch_closed(&self) -> bool {
        matches!(self.scrobble, ScrobbleLatch::Closed)
    }

    fn accepts(&self, generation: G) -> bool {
        self.disposition == OccurrenceDisposition::Enabled
            && self.accepted_generation == Some(generation)
    }

    fn begin_playing_evidence<C>(
        &mut self,
        clock: &C,
    ) -> Result<Option<LastFmPlaybackAction>, LastFmPlaybackEvidenceError>
    where
        C: LastFmPlaybackClock + ?Sized,
    {
        if self.now_playing != NowPlayingLatch::Open {
            return Ok(None);
        }

        // Close before reading the clock or returning network work. A clock
        // failure and a failed/cancelled now-playing request are both one-shot.
        self.now_playing = NowPlayingLatch::Failed;
        let started_at_unix_secs = clock.now_unix_seconds().and_then(|value| {
            if (1..=MAX_LASTFM_STARTED_AT_SECS).contains(&value) {
                Ok(value)
            } else {
                Err(LastFmPlaybackEvidenceError::ClockOutOfRange)
            }
        });
        let started_at_unix_secs = match started_at_unix_secs {
            Ok(value) => value,
            Err(error) => {
                self.disposition = OccurrenceDisposition::Disabled;
                self.scrobble = ScrobbleLatch::Closed;
                return Err(error);
            }
        };
        self.now_playing = NowPlayingLatch::Captured {
            started_at_unix_secs,
        };
        Ok(Some(LastFmPlaybackAction::NowPlaying(
            self.metadata.now_playing_track(),
        )))
    }

    fn terminate_generation(&mut self, generation: G) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        self.retire();
        true
    }
}

impl<G> fmt::Debug for LastFmPlaybackOccurrence<G>
where
    G: Copy + Eq,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmPlaybackOccurrence")
            .field(
                "has_accepted_generation",
                &self.accepted_generation.is_some(),
            )
            .field("now_playing_latch_closed", &self.now_playing_latch_closed())
            .field("scrobble_latch_closed", &self.scrobble_latch_closed())
            .field(
                "disabled",
                &(self.disposition == OccurrenceDisposition::Disabled),
            )
            .field(
                "terminated",
                &(self.disposition == OccurrenceDisposition::Terminated),
            )
            .finish_non_exhaustive()
    }
}

/// Exact Last.fm threshold for an already-known positive duration.
#[must_use]
pub const fn lastfm_scrobble_threshold_ms(duration_ms: u64) -> u64 {
    let half_rounded_up = duration_ms / 2 + duration_ms % 2;
    if half_rounded_up < MAX_LASTFM_SCROBBLE_THRESHOLD_MS {
        half_rounded_up
    } else {
        MAX_LASTFM_SCROBBLE_THRESHOLD_MS
    }
}

fn valid_required_text(value: &str) -> bool {
    value.len() <= MAX_LASTFM_METADATA_BYTES
        && value.chars().any(|character| !character.is_whitespace())
        && !value.chars().any(char::is_control)
}

fn canonical_optional_text(
    value: Option<String>,
) -> Result<Option<String>, LastFmPlaybackEvidenceError> {
    match value {
        None => Ok(None),
        Some(value) if !value.chars().any(|character| !character.is_whitespace()) => Ok(None),
        Some(value)
            if value.len() <= MAX_LASTFM_METADATA_BYTES && !value.chars().any(char::is_control) =>
        {
            Ok(Some(value))
        }
        Some(_) => Err(LastFmPlaybackEvidenceError::InvalidMetadata),
    }
}

const fn map_queue_construction_error(error: LastFmQueueError) -> LastFmPlaybackEvidenceError {
    match error {
        LastFmQueueError::InvalidInput => LastFmPlaybackEvidenceError::Invariant,
        LastFmQueueError::InvalidBatch
        | LastFmQueueError::Full
        | LastFmQueueError::AccountMismatch
        | LastFmQueueError::OccurrenceConflict
        | LastFmQueueError::StaleBatch
        | LastFmQueueError::CorruptStorage
        | LastFmQueueError::Storage => LastFmPlaybackEvidenceError::Invariant,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use proptest::prelude::*;
    use uuid::{Variant, Version};

    use super::*;

    const START: i64 = 1_700_000_123;

    struct ScriptedClock {
        values: RefCell<Vec<Result<i64, LastFmPlaybackEvidenceError>>>,
        calls: Cell<usize>,
    }

    impl ScriptedClock {
        fn fixed(value: i64) -> Self {
            Self::new(vec![Ok(value)])
        }

        fn new(mut values: Vec<Result<i64, LastFmPlaybackEvidenceError>>) -> Self {
            values.reverse();
            Self {
                values: RefCell::new(values),
                calls: Cell::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.get()
        }
    }

    impl LastFmPlaybackClock for ScriptedClock {
        fn now_unix_seconds(&self) -> Result<i64, LastFmPlaybackEvidenceError> {
            self.calls.set(self.calls.get() + 1);
            self.values
                .borrow_mut()
                .pop()
                .unwrap_or(Err(LastFmPlaybackEvidenceError::ClockOutOfRange))
        }
    }

    fn metadata(duration_secs: u64) -> LastFmPlaybackMetadata {
        LastFmPlaybackMetadata::try_new(
            "Exact Artist".to_owned(),
            "Exact Title".to_owned(),
            Some("Exact Album".to_owned()),
            Some("Exact Album Artist".to_owned()),
            Some(7),
            Some(duration_secs),
        )
        .expect("valid metadata")
    }

    fn occurrence(duration_secs: u64) -> LastFmPlaybackOccurrence<u64> {
        LastFmPlaybackOccurrence::new(metadata(duration_secs))
    }

    fn assert_no_action(action: Result<Option<LastFmPlaybackAction>, LastFmPlaybackEvidenceError>) {
        assert!(matches!(action, Ok(None)));
    }

    fn expect_now_playing(action: Option<LastFmPlaybackAction>) -> LastFmTrack {
        match action {
            Some(LastFmPlaybackAction::NowPlaying(track)) => track,
            other => panic!("expected now-playing action, got {other:?}"),
        }
    }

    fn expect_scrobble(action: Option<LastFmPlaybackAction>) -> UnboundLastFmScrobble {
        match action {
            Some(LastFmPlaybackAction::Scrobble(scrobble)) => scrobble,
            other => panic!("expected scrobble action, got {other:?}"),
        }
    }

    fn begin_playing(
        occurrence: &mut LastFmPlaybackOccurrence<u64>,
        generation: u64,
        clock: &ScriptedClock,
    ) {
        assert!(occurrence.accept_generation(generation));
        expect_now_playing(
            occurrence
                .observe_state(generation, LastFmPlaybackState::Playing, clock)
                .expect("playing evidence"),
        );
        assert_no_action(occurrence.observe_position(generation, 0, clock));
    }

    #[test]
    fn metadata_preserves_exact_structured_values() {
        let frozen = metadata(301);
        let mut occurrence = LastFmPlaybackOccurrence::<u64>::new(frozen);
        let clock = ScriptedClock::fixed(START);
        assert!(occurrence.accept_generation(1));

        let track = expect_now_playing(
            occurrence
                .observe_state(1, LastFmPlaybackState::Playing, &clock)
                .unwrap(),
        );
        assert_eq!(track.artist, "Exact Artist");
        assert_eq!(track.title, "Exact Title");
        assert_eq!(track.album.as_deref(), Some("Exact Album"));
        assert_eq!(track.album_artist.as_deref(), Some("Exact Album Artist"));
        assert_eq!(track.track_number, Some(7));
        assert_eq!(track.duration_seconds, 301);
    }

    #[test]
    fn metadata_is_frozen_against_later_source_mutation() {
        let mut artist = "Frozen Artist".to_owned();
        let mut title = "Frozen Title".to_owned();
        let frozen = LastFmPlaybackMetadata::try_new(
            artist.clone(),
            title.clone(),
            None,
            None,
            None,
            Some(31),
        )
        .unwrap();
        artist.replace_range(.., "Changed Artist");
        title.replace_range(.., "Changed Title");

        let mut occurrence = LastFmPlaybackOccurrence::<u64>::new(frozen);
        let clock = ScriptedClock::fixed(START);
        assert!(occurrence.accept_generation(1));
        let track = expect_now_playing(
            occurrence
                .observe_state(1, LastFmPlaybackState::Playing, &clock)
                .unwrap(),
        );
        assert_eq!(track.artist, "Frozen Artist");
        assert_eq!(track.title, "Frozen Title");
        assert_ne!(track.artist, artist);
        assert_ne!(track.title, title);
    }

    #[test]
    fn optional_whitespace_is_omitted_but_nonempty_bytes_are_exact() {
        let frozen = LastFmPlaybackMetadata::try_new(
            " Artist ".to_owned(),
            " Title ".to_owned(),
            Some(" \t ".to_owned()),
            Some(" Album Artist ".to_owned()),
            None,
            Some(31),
        )
        .unwrap();
        let track = frozen.now_playing_track();
        assert_eq!(track.artist, " Artist ");
        assert_eq!(track.title, " Title ");
        assert_eq!(track.album, None);
        assert_eq!(track.album_artist.as_deref(), Some(" Album Artist "));
    }

    #[test]
    fn metadata_rejects_missing_short_or_unrepresentable_values() {
        let invalid_required = ["", " \t ", "line\nbreak"];
        for invalid in invalid_required {
            assert_eq!(
                LastFmPlaybackMetadata::try_new(
                    invalid.to_owned(),
                    "Title".to_owned(),
                    None,
                    None,
                    None,
                    Some(31),
                ),
                Err(LastFmPlaybackEvidenceError::InvalidMetadata)
            );
        }

        for duration in [None, Some(0), Some(30), Some(i32::MAX as u64 + 1)] {
            assert_eq!(
                LastFmPlaybackMetadata::try_new(
                    "Artist".to_owned(),
                    "Title".to_owned(),
                    None,
                    None,
                    None,
                    duration,
                ),
                Err(LastFmPlaybackEvidenceError::InvalidMetadata)
            );
        }
        for number in [Some(0), Some(i32::MAX as u32 + 1)] {
            assert_eq!(
                LastFmPlaybackMetadata::try_new(
                    "Artist".to_owned(),
                    "Title".to_owned(),
                    None,
                    None,
                    number,
                    Some(31),
                ),
                Err(LastFmPlaybackEvidenceError::InvalidMetadata)
            );
        }
    }

    #[test]
    fn metadata_enforces_exact_utf8_byte_limit_and_controls() {
        let exact = "🎵".repeat(MAX_LASTFM_METADATA_BYTES / 4);
        assert_eq!(exact.len(), MAX_LASTFM_METADATA_BYTES);
        assert!(LastFmPlaybackMetadata::try_new(
            exact,
            "Title".to_owned(),
            None,
            None,
            None,
            Some(31),
        )
        .is_ok());
        assert_eq!(
            LastFmPlaybackMetadata::try_new(
                "x".repeat(MAX_LASTFM_METADATA_BYTES + 1),
                "Title".to_owned(),
                None,
                None,
                None,
                Some(31),
            ),
            Err(LastFmPlaybackEvidenceError::InvalidMetadata)
        );
        assert_eq!(
            LastFmPlaybackMetadata::try_new(
                "Artist".to_owned(),
                "Title".to_owned(),
                Some("bad\u{7f}".to_owned()),
                None,
                None,
                Some(31),
            ),
            Err(LastFmPlaybackEvidenceError::InvalidMetadata)
        );
    }

    #[test]
    fn duration_eligibility_and_threshold_edges_are_exact() {
        assert!(LastFmPlaybackMetadata::try_new(
            "Artist".to_owned(),
            "Title".to_owned(),
            None,
            None,
            None,
            Some(30),
        )
        .is_err());
        let occurrence = occurrence(31);
        assert_eq!(occurrence.threshold_ms(), 15_500);
        assert_eq!(lastfm_scrobble_threshold_ms(479_999), 240_000);
        assert_eq!(lastfm_scrobble_threshold_ms(480_000), 240_000);
        assert_eq!(lastfm_scrobble_threshold_ms(480_001), 240_000);
    }

    #[test]
    fn occurrences_generate_distinct_opaque_random_uuids() {
        let first = occurrence(31);
        let second = occurrence(31);
        assert_ne!(first.occurrence_id, second.occurrence_id);
        for id in [first.occurrence_id, second.occurrence_id] {
            assert_eq!(id.get_variant(), Variant::RFC4122);
            assert_eq!(id.get_version(), Some(Version::Random));
        }
    }

    #[test]
    fn unaccepted_and_stale_generations_are_inert_and_do_not_read_clock() {
        let mut occurrence = occurrence(31);
        let clock = ScriptedClock::fixed(START);
        assert_no_action(occurrence.observe_state(7, LastFmPlaybackState::Playing, &clock));
        assert_no_action(occurrence.observe_position(7, 20_000, &clock));
        assert!(occurrence.accept_generation(8));
        assert_no_action(occurrence.observe_state(7, LastFmPlaybackState::Playing, &clock));
        assert_no_action(occurrence.observe_position(7, 20_000, &clock));
        assert!(!occurrence.observe_discontinuity(7));
        assert!(!occurrence.observe_natural_end(7));
        assert_eq!(clock.calls(), 0);
        assert_eq!(occurrence.credited_ms(), 0);
    }

    #[test]
    fn first_playing_evidence_captures_clock_and_emits_now_playing_once() {
        let clock = ScriptedClock::new(vec![Ok(START), Ok(START + 999)]);
        let mut occurrence = occurrence(31);
        assert!(occurrence.accept_generation(1));
        expect_now_playing(
            occurrence
                .observe_state(1, LastFmPlaybackState::Playing, &clock)
                .unwrap(),
        );
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Playing, &clock));
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Paused, &clock));
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Playing, &clock));
        assert_eq!(clock.calls(), 1);
        assert!(occurrence.now_playing_latch_closed());
        assert!(matches!(
            occurrence.now_playing,
            NowPlayingLatch::Captured {
                started_at_unix_secs: START
            }
        ));
    }

    #[test]
    fn first_position_can_prove_playing_but_only_anchors() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(31);
        assert!(occurrence.accept_generation(1));
        expect_now_playing(occurrence.observe_position(1, 9_000, &clock).unwrap());
        assert_eq!(occurrence.credited_ms(), 0);
        assert_no_action(occurrence.observe_position(1, 24_499, &clock));
        assert_eq!(occurrence.credited_ms(), 15_499);
        expect_scrobble(occurrence.observe_position(1, 24_500, &clock).unwrap());
    }

    #[test]
    fn exact_threshold_emits_one_scrobble_with_frozen_start_and_metadata() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(31);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 15_499, &clock));
        let scrobble = expect_scrobble(occurrence.observe_position(1, 15_500, &clock).unwrap());
        assert_eq!(scrobble.occurrence_id(), occurrence.occurrence_id);
        assert_eq!(scrobble.artist(), "Exact Artist");
        assert_eq!(scrobble.track_title(), "Exact Title");
        assert_eq!(scrobble.album(), Some("Exact Album"));
        assert_eq!(scrobble.album_artist(), Some("Exact Album Artist"));
        assert_eq!(scrobble.track_number(), Some(7));
        assert_eq!(scrobble.duration_secs(), 31);
        assert_eq!(scrobble.started_at_unix_secs(), START);
        assert!(occurrence.scrobble_latch_closed());
        assert_no_action(occurrence.observe_position(1, 31_000, &clock));
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Playing, &clock));
    }

    #[test]
    fn duplicate_and_regressed_positions_never_replay_credit() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(100);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 20_000, &clock));
        assert_no_action(occurrence.observe_position(1, 20_000, &clock));
        assert_no_action(occurrence.observe_position(1, 5_000, &clock));
        assert_no_action(occurrence.observe_position(1, 21_000, &clock));
        assert_eq!(occurrence.credited_ms(), 21_000);
    }

    #[test]
    fn paused_and_stopped_positions_cannot_prove_playing_or_earn_credit() {
        for state in [LastFmPlaybackState::Paused, LastFmPlaybackState::Stopped] {
            let clock = ScriptedClock::fixed(START);
            let mut occurrence = occurrence(31);
            assert!(occurrence.accept_generation(1));
            assert_no_action(occurrence.observe_state(1, state, &clock));
            assert_no_action(occurrence.observe_position(1, 20_000, &clock));
            assert_no_action(occurrence.observe_position(1, 31_000, &clock));
            assert_eq!(occurrence.credited_ms(), 0);
            assert_eq!(clock.calls(), 0);
        }
    }

    #[test]
    fn buffering_cannot_restore_position_proof_after_pause_or_stop() {
        for revoked_state in [LastFmPlaybackState::Paused, LastFmPlaybackState::Stopped] {
            let clock = ScriptedClock::fixed(START);
            let mut occurrence = occurrence(100);
            begin_playing(&mut occurrence, 1, &clock);
            assert_no_action(occurrence.observe_position(1, 10_000, &clock));

            assert_no_action(occurrence.observe_state(1, revoked_state, &clock));
            assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Buffering, &clock));
            assert_no_action(occurrence.observe_position(1, 80_000, &clock));
            assert_no_action(occurrence.observe_position(1, 90_000, &clock));
            assert_eq!(occurrence.credited_ms(), 10_000);

            assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Playing, &clock));
            assert_no_action(occurrence.observe_position(1, 100_000, &clock));
            assert_no_action(occurrence.observe_position(1, 139_999, &clock));
            assert_eq!(occurrence.credited_ms(), 49_999);
            expect_scrobble(occurrence.observe_position(1, 140_000, &clock).unwrap());
            assert_eq!(clock.calls(), 1);
        }
    }

    #[test]
    fn pause_resume_reanchors_without_wall_clock_or_position_jump_credit() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(100);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 20_000, &clock));
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Paused, &clock));
        assert_no_action(occurrence.observe_position(1, 60_000, &clock));
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Playing, &clock));
        assert_no_action(occurrence.observe_position(1, 70_000, &clock));
        assert_eq!(occurrence.credited_ms(), 20_000);
        assert_no_action(occurrence.observe_position(1, 99_999, &clock));
        expect_scrobble(occurrence.observe_position(1, 100_000, &clock).unwrap());
    }

    #[test]
    fn buffering_recovery_position_reanchors_without_credit() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(100);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 20_000, &clock));
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Buffering, &clock));
        assert_no_action(occurrence.observe_position(1, 80_000, &clock));
        assert_eq!(occurrence.credited_ms(), 20_000);
        assert_no_action(occurrence.observe_position(1, 109_999, &clock));
        expect_scrobble(occurrence.observe_position(1, 110_000, &clock).unwrap());
    }

    #[test]
    fn seek_and_previous_restart_reanchor_without_jump_credit() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(100);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 10_000, &clock));
        assert!(occurrence.observe_discontinuity(1));
        assert_no_action(occurrence.observe_position(1, 90_000, &clock));
        assert!(occurrence.observe_discontinuity(1));
        assert_no_action(occurrence.observe_position(1, 0, &clock));
        assert_no_action(occurrence.observe_position(1, 39_999, &clock));
        assert_eq!(occurrence.credited_ms(), 49_999);
        expect_scrobble(occurrence.observe_position(1, 40_000, &clock).unwrap());
    }

    #[test]
    fn same_occurrence_retry_retains_credit_but_reanchors_new_generation() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(100);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 20_000, &clock));
        assert!(occurrence.retire_generation(1));
        assert!(occurrence.accept_generation(2));
        assert_no_action(occurrence.observe_position(1, 50_000, &clock));
        assert_no_action(occurrence.observe_position(2, 70_000, &clock));
        assert_eq!(occurrence.credited_ms(), 20_000);
        assert_no_action(occurrence.observe_position(2, 99_999, &clock));
        expect_scrobble(occurrence.observe_position(2, 100_000, &clock).unwrap());
        assert_eq!(clock.calls(), 1);
    }

    #[test]
    fn natural_end_never_grants_tail_credit_and_is_terminal() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(31);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 15_499, &clock));
        assert!(occurrence.observe_natural_end(1));
        assert_eq!(occurrence.credited_ms(), 15_499);
        assert!(!occurrence.accept_generation(2));
        assert_no_action(occurrence.observe_position(1, u64::MAX, &clock));
        assert!(!occurrence.scrobble_latch_closed());
    }

    #[test]
    fn output_error_preserves_same_occurrence_evidence_for_retry() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(31);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 10_000, &clock));
        assert!(occurrence.observe_error(1));
        assert_no_action(occurrence.observe_position(1, u64::MAX, &clock));
        assert!(occurrence.accept_generation(2));
        assert_no_action(occurrence.observe_position(2, 50_000, &clock));
        let scrobble = expect_scrobble(occurrence.observe_position(2, 55_500, &clock).unwrap());
        assert_eq!(scrobble.started_at_unix_secs(), START);
        assert_eq!(occurrence.credited_ms(), 15_500);
        assert_eq!(clock.calls(), 1);
    }

    #[test]
    fn duplicate_playing_state_does_not_force_another_anchor() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(31);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 10_000, &clock));
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Playing, &clock));
        expect_scrobble(occurrence.observe_position(1, 15_500, &clock).unwrap());
        assert_eq!(clock.calls(), 1);
    }

    #[test]
    fn invalid_start_clock_fails_closed_once() {
        for value in [0, -1, MAX_LASTFM_STARTED_AT_SECS + 1] {
            let clock = ScriptedClock::fixed(value);
            let mut occurrence = occurrence(31);
            assert!(occurrence.accept_generation(1));
            assert!(matches!(
                occurrence.observe_state(1, LastFmPlaybackState::Playing, &clock),
                Err(LastFmPlaybackEvidenceError::ClockOutOfRange)
            ));
            assert!(occurrence.now_playing_latch_closed());
            assert!(occurrence.scrobble_latch_closed());
            assert_no_action(occurrence.observe_position(1, u64::MAX, &clock));
            assert_eq!(clock.calls(), 1);
        }
    }

    #[test]
    fn clock_error_is_not_retried() {
        let clock = ScriptedClock::new(vec![
            Err(LastFmPlaybackEvidenceError::ClockOutOfRange),
            Ok(START),
        ]);
        let mut occurrence = occurrence(31);
        assert!(occurrence.accept_generation(1));
        assert!(matches!(
            occurrence.observe_position(1, 0, &clock),
            Err(LastFmPlaybackEvidenceError::ClockOutOfRange)
        ));
        assert_no_action(occurrence.observe_state(1, LastFmPlaybackState::Playing, &clock));
        assert_eq!(clock.calls(), 1);
    }

    #[test]
    fn occurrence_authority_can_be_moved_out_and_restored_without_cloning() {
        let clock = ScriptedClock::fixed(START);
        let mut occurrence = occurrence(100);
        begin_playing(&mut occurrence, 1, &clock);
        assert_no_action(occurrence.observe_position(1, 20_000, &clock));
        let occurrence_id = occurrence.occurrence_id;
        let mut slot = Some(occurrence);

        let rollback = slot.take().expect("move rollback authority out");
        assert!(slot.is_none());
        let mut tentative = LastFmPlaybackOccurrence::<u64>::new(metadata(100));
        assert_ne!(tentative.occurrence_id, occurrence_id);
        tentative.retire();
        slot = Some(rollback);

        let restored = slot.as_mut().expect("restore exact moved authority");
        assert_eq!(restored.occurrence_id, occurrence_id);
        assert_eq!(restored.credited_ms(), 20_000);
        assert!(restored.now_playing_latch_closed());
        assert!(!restored.scrobble_latch_closed());
        assert!(matches!(
            restored.now_playing,
            NowPlayingLatch::Captured {
                started_at_unix_secs: START
            }
        ));
        assert_no_action(restored.observe_position(1, 49_999, &clock));
        expect_scrobble(restored.observe_position(1, 50_000, &clock).unwrap());
    }

    #[test]
    fn diagnostics_redact_metadata_duration_timestamp_and_occurrence_identity() {
        let artist = "SENTINEL_ARTIST_1f25";
        let title = "SENTINEL_TITLE_7b93";
        let album = "SENTINEL_ALBUM_3481";
        let album_artist = "SENTINEL_ALBUM_ARTIST_9562";
        let frozen = LastFmPlaybackMetadata::try_new(
            artist.to_owned(),
            title.to_owned(),
            Some(album.to_owned()),
            Some(album_artist.to_owned()),
            Some(1_927_463),
            Some(1_927_464),
        )
        .unwrap();
        let mut occurrence = LastFmPlaybackOccurrence::<u64>::new(frozen.clone());
        let occurrence_id = occurrence.occurrence_id.to_string();
        let clock = ScriptedClock::fixed(191_827_364);
        assert!(occurrence.accept_generation(8_736_451));
        let now_playing_action = occurrence
            .observe_state(8_736_451, LastFmPlaybackState::Playing, &clock)
            .unwrap()
            .unwrap();
        assert_no_action(occurrence.observe_position(8_736_451, 0, &clock));
        let scrobble_action = occurrence
            .observe_position(8_736_451, MAX_LASTFM_SCROBBLE_THRESHOLD_MS, &clock)
            .unwrap()
            .unwrap();

        for diagnostics in [
            format!("{frozen:?}"),
            format!("{occurrence:?}"),
            format!("{now_playing_action:?}"),
            format!("{scrobble_action:?}"),
        ] {
            for private in [
                artist,
                title,
                album,
                album_artist,
                "1927463",
                "1927464",
                "191827364",
                occurrence_id.as_str(),
                "8736451",
            ] {
                assert!(
                    !diagnostics.contains(private),
                    "private sentinel leaked: {private} in {diagnostics}"
                );
            }
        }
    }

    proptest! {
        #[test]
        fn threshold_is_ceil_half_capped_for_every_duration(duration_ms in any::<u64>()) {
            let expected = (duration_ms / 2 + duration_ms % 2)
                .min(MAX_LASTFM_SCROBBLE_THRESHOLD_MS);
            prop_assert_eq!(lastfm_scrobble_threshold_ms(duration_ms), expected);
        }

        #[test]
        fn monotonic_forward_samples_emit_at_most_one_scrobble_at_threshold(
            duration_secs in 31_u64..=3_600,
            deltas in proptest::collection::vec(0_u64..50_000, 0..80),
        ) {
            let clock = ScriptedClock::fixed(START);
            let mut occurrence = occurrence(duration_secs);
            begin_playing(&mut occurrence, 1, &clock);
            let threshold = occurrence.threshold_ms();
            let mut position = 0_u64;
            let mut scrobbles = 0_usize;
            for delta in deltas {
                position = position.saturating_add(delta);
                let action = occurrence.observe_position(1, position, &clock).unwrap();
                if matches!(action, Some(LastFmPlaybackAction::Scrobble(_))) {
                    scrobbles += 1;
                }
                prop_assert!(scrobbles <= 1);
                prop_assert_eq!(occurrence.scrobble_latch_closed(), position >= threshold);
                prop_assert_eq!(occurrence.credited_ms(), position.min(threshold));
            }
        }

        #[test]
        fn discontinuities_never_change_accumulated_credit(
            duration_secs in 31_u64..=3_600,
            first_position in any::<u64>(),
            anchor_position in any::<u64>(),
        ) {
            let clock = ScriptedClock::fixed(START);
            let mut occurrence = occurrence(duration_secs);
            begin_playing(&mut occurrence, 1, &clock);
            let _ = occurrence.observe_position(1, first_position, &clock).unwrap();
            let before = occurrence.credited_ms();
            prop_assert!(occurrence.observe_discontinuity(1));
            let _ = occurrence.observe_position(1, anchor_position, &clock).unwrap();
            prop_assert_eq!(occurrence.credited_ms(), before);
        }

        #[test]
        fn stale_positions_never_change_evidence(
            positions in proptest::collection::vec(any::<u64>(), 0..100),
        ) {
            let clock = ScriptedClock::fixed(START);
            let mut occurrence = occurrence(600);
            prop_assert!(occurrence.accept_generation(9));
            let before = (
                occurrence.credited_ms(),
                occurrence.now_playing_latch_closed(),
                occurrence.scrobble_latch_closed(),
            );
            for position in positions {
                let action = occurrence.observe_position(8, position, &clock).unwrap();
                prop_assert!(action.is_none());
            }
            prop_assert_eq!(before, (
                occurrence.credited_ms(),
                occurrence.now_playing_latch_closed(),
                occurrence.scrobble_latch_closed(),
            ));
            prop_assert_eq!(clock.calls(), 0);
        }
    }
}
