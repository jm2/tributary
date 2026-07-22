//! GTK-free owner for authoritative Last.fm playback occurrences.
//!
//! This boundary is the only bridge from accepted output generations to the
//! standalone playback-evidence state machine. It freezes structured metadata
//! and source attribution before the first load, preserves one genuine queue
//! occurrence across output retries within one owner lifetime, maps the
//! complete [`PlayerEvent`] surface explicitly, and produces move-only runtime
//! handoffs. Managed-source work is admitted only while [`SourceRegistry`]
//! revalidates the exact live source; no lease, adapter, locator, or credential
//! crosses this module.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use crate::architecture::MediaKey;
use crate::architecture::SourceId;
use crate::audio::{PlayerEvent, PlayerEventGeneration, PlayerState};
use crate::source_registry::{PlaybackSourceReference, SourceRegistry};
use crate::ui::playback::LastFmAcceptedOutputMint;

use super::playback::{
    LastFmPlaybackAction, LastFmPlaybackClock, LastFmPlaybackEvidenceError, LastFmPlaybackMetadata,
    LastFmPlaybackOccurrence, LastFmPlaybackState, SystemLastFmPlaybackClock,
};
use super::runtime::{
    LastFmNowPlaying, LastFmNowPlayingOutcome, LastFmRuntimeAdmissionError,
    LastFmRuntimeCommandError, LastFmRuntimeHandle, LastFmRuntimeOperation,
};
use super::storage::{LastFmEnqueueOutcome, UnboundLastFmScrobble};

/// Opaque identity of one genuine queue occurrence.
///
/// Clones name the same occurrence by allocation identity. Constructing a new
/// value always names a different occurrence even if its source and metadata
/// are byte-for-byte identical. This value is intentionally neither `Copy`
/// nor serializable.
#[derive(Clone)]
#[allow(clippy::redundant_pub_crate)] // Explicit construction-authority boundary.
pub(crate) struct LastFmPlaybackOccurrenceIdentity(Arc<()>);

impl LastFmPlaybackOccurrenceIdentity {
    /// Mint identity for a newly selected queue occurrence.
    #[allow(clippy::redundant_pub_crate)] // Only playback coordination may mint occurrences.
    pub(crate) fn fresh() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for LastFmPlaybackOccurrenceIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for LastFmPlaybackOccurrenceIdentity {}

impl fmt::Debug for LastFmPlaybackOccurrenceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPlaybackOccurrenceIdentity(<opaque>)")
    }
}

/// Revocable one-shot freshness for one output-accepted load.
///
/// The playback coordinator keeps a clone until a newer output generation
/// supersedes this one. Claim and revocation linearize on the same lock, and a
/// successful claim keeps that lock through the complete owner mutation. The
/// value exposes neither its state nor its generation in diagnostics.
#[derive(Clone)]
#[must_use = "accepted-output freshness must be bound to a load or explicitly revoked"]
#[allow(clippy::redundant_pub_crate)] // Shared only with the playback coordinator.
pub(crate) struct LastFmAcceptedOutputFreshness(Arc<Mutex<bool>>);

impl LastFmAcceptedOutputFreshness {
    /// Mint freshness for one newly accepted output generation.
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn fresh() -> Self {
        Self(Arc::new(Mutex::new(true)))
    }

    /// Revoke this output generation before a successor can mutate the owner.
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn revoke(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
    }

    fn try_claim<T>(&self, claim: impl FnOnce() -> T) -> Option<T> {
        let mut fresh = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*fresh {
            return None;
        }
        *fresh = false;
        let result = claim();
        drop(fresh);
        Some(result)
    }
}

impl fmt::Debug for LastFmAcceptedOutputFreshness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmAcceptedOutputFreshness(<redacted>)")
    }
}

struct LastFmEphemeralHandoffLaneState {
    current_identity: Option<Arc<()>>,
    unclaimed: bool,
}

type LastFmEphemeralHandoffLane = Arc<Mutex<LastFmEphemeralHandoffLaneState>>;

struct LastFmEphemeralHandoffFreshness {
    lane: LastFmEphemeralHandoffLane,
    identity: Arc<()>,
}

impl LastFmEphemeralHandoffFreshness {
    fn try_claim<T>(self, claim: impl FnOnce() -> T) -> Option<T> {
        let mut lane = self
            .lane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = lane
            .current_identity
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.identity));
        if !current || !lane.unclaimed {
            return None;
        }
        lane.unclaimed = false;
        let result = claim();
        drop(lane);
        Some(result)
    }
}

/// Frozen source attribution for one accepted playback occurrence.
///
/// Local media is intrinsically owned by Tributary. Every other supported
/// source carries only a non-authoritative reference which must be presented
/// back to [`SourceRegistry`] at runtime ingress.
#[derive(Clone, Eq, PartialEq)]
enum LastFmPlaybackSourceKind {
    #[cfg(test)]
    Local(MediaKey),
    Managed(PlaybackSourceReference),
}

#[derive(Clone, Eq, PartialEq)]
#[allow(clippy::redundant_pub_crate)] // Explicit source-attribution authority boundary.
pub(crate) struct LastFmPlaybackSource(LastFmPlaybackSourceKind);

impl LastFmPlaybackSource {
    /// Capture one exact local-library item in boundary tests.
    ///
    /// Production local attribution remains fail-closed until the local
    /// parser/database path can mint exact per-track provenance.
    #[cfg(test)]
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn local(media_key: MediaKey) -> Option<Self> {
        (media_key.source_id == SourceId::local())
            .then_some(Self(LastFmPlaybackSourceKind::Local(media_key)))
    }

    /// Capture one non-authoritative managed-source reference.
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn managed(reference: PlaybackSourceReference) -> Self {
        Self(LastFmPlaybackSourceKind::Managed(reference))
    }

    #[allow(clippy::too_many_arguments)]
    fn matches_attribution(
        &self,
        artist: &str,
        title: &str,
        album: Option<&str>,
        album_artist: Option<&str>,
        track_number: Option<u32>,
        duration_secs: Option<u64>,
    ) -> bool {
        match &self.0 {
            #[cfg(test)]
            LastFmPlaybackSourceKind::Local(_) => true,
            LastFmPlaybackSourceKind::Managed(reference) => reference.matches_attribution(
                title,
                artist,
                album,
                album_artist,
                track_number,
                duration_secs,
            ),
        }
    }

    fn admit<T>(
        self,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
        admit: impl FnOnce() -> T,
    ) -> Option<T> {
        match self.0 {
            #[cfg(test)]
            LastFmPlaybackSourceKind::Local(_) => Some(admit()),
            LastFmPlaybackSourceKind::Managed(reference) => {
                registry.try_admit_playback_action(&reference, enabled_remote_sources, admit)
            }
        }
    }
}

impl fmt::Debug for LastFmPlaybackSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            #[cfg(test)]
            LastFmPlaybackSourceKind::Local(_) => {
                formatter.write_str("LastFmPlaybackSource::Local(<redacted>)")
            }
            LastFmPlaybackSourceKind::Managed(_) => {
                formatter.write_str("LastFmPlaybackSource::Managed(<redacted>)")
            }
        }
    }
}

/// Move-only, validated input for one successfully accepted output load.
///
/// Construction freezes the authoritative structured fields before any
/// playback event can arrive. The value intentionally provides no field
/// accessors: it can only be consumed by
/// [`LastFmAcceptedOutputLoad::eligible`].
#[allow(clippy::redundant_pub_crate)] // Move-only input stays inside application coordination.
pub(crate) struct LastFmAcceptedPlayback {
    identity: LastFmPlaybackOccurrenceIdentity,
    source: LastFmPlaybackSource,
    metadata: LastFmPlaybackMetadata,
}

impl LastFmAcceptedPlayback {
    /// Validate and freeze structured metadata for one accepted load.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn try_new(
        identity: LastFmPlaybackOccurrenceIdentity,
        source: LastFmPlaybackSource,
        artist: String,
        title: String,
        album: Option<String>,
        album_artist: Option<String>,
        track_number: Option<u32>,
        duration_secs: Option<u64>,
    ) -> Result<Self, LastFmPlaybackEvidenceError> {
        if !source.matches_attribution(
            &artist,
            &title,
            album.as_deref(),
            album_artist.as_deref(),
            track_number,
            duration_secs,
        ) {
            return Err(LastFmPlaybackEvidenceError::InvalidMetadata);
        }
        let metadata = LastFmPlaybackMetadata::try_new(
            artist,
            title,
            album,
            album_artist,
            track_number,
            duration_secs,
        )?;
        Ok(Self {
            identity,
            source,
            metadata,
        })
    }
}

impl fmt::Debug for LastFmAcceptedPlayback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmAcceptedPlayback(<redacted>)")
    }
}

enum LastFmAcceptedOutputLoadKind {
    Eligible(Box<LastFmAcceptedPlayback>),
    Ineligible,
}

/// Move-only proof of one exact output-accepted generation and its immutable
/// Last.fm eligibility decision.
///
/// Binding the generation here prevents a caller from validating metadata for
/// one accepted load and attaching it to another output generation. An
/// ineligible accepted load remains an explicit owner event because replacing
/// output media must terminally retire and clear any predecessor.
#[must_use = "accepted output loads must update or explicitly retire playback ownership"]
#[allow(clippy::redundant_pub_crate)] // Explicit accepted-output authority boundary.
pub(crate) struct LastFmAcceptedOutputLoad {
    generation: PlayerEventGeneration,
    freshness: LastFmAcceptedOutputFreshness,
    kind: LastFmAcceptedOutputLoadKind,
}

impl LastFmAcceptedOutputLoad {
    /// Bind one exact output generation to validated eligible metadata.
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn eligible(
        _mint: LastFmAcceptedOutputMint,
        generation: PlayerEventGeneration,
        freshness: LastFmAcceptedOutputFreshness,
        accepted: LastFmAcceptedPlayback,
    ) -> Self {
        Self {
            generation,
            freshness,
            kind: LastFmAcceptedOutputLoadKind::Eligible(Box::new(accepted)),
        }
    }

    /// Record that one exact accepted output load is not Last.fm-eligible.
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) const fn ineligible(
        _mint: LastFmAcceptedOutputMint,
        generation: PlayerEventGeneration,
        freshness: LastFmAcceptedOutputFreshness,
    ) -> Self {
        Self {
            generation,
            freshness,
            kind: LastFmAcceptedOutputLoadKind::Ineligible,
        }
    }
}

impl fmt::Debug for LastFmAcceptedOutputLoad {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LastFmAcceptedOutputLoadKind::Eligible(_) => {
                formatter.write_str("LastFmAcceptedOutputLoad::Eligible(<redacted>)")
            }
            LastFmAcceptedOutputLoadKind::Ineligible => {
                formatter.write_str("LastFmAcceptedOutputLoad::Ineligible")
            }
        }
    }
}

/// Fixed category emitted when the coordinator must disable an occurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmPlaybackOwnerError {
    #[error("Last.fm playback evidence failed")]
    Evidence(#[from] LastFmPlaybackEvidenceError),
    #[error("Last.fm playback occurrence changed after acceptance")]
    InconsistentOccurrence,
}

/// Fixed kind of one move-only runtime handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmPlaybackHandoffKind {
    NowPlaying,
    Enqueue,
    ClearNowPlaying,
}

enum LastFmPlaybackHandoffPayload {
    NowPlaying {
        freshness: LastFmEphemeralHandoffFreshness,
        source: LastFmPlaybackSource,
        now_playing: LastFmNowPlaying,
    },
    Enqueue {
        source: LastFmPlaybackSource,
        scrobble: UnboundLastFmScrobble,
    },
    ClearNowPlaying {
        freshness: LastFmEphemeralHandoffFreshness,
    },
}

/// One move-only action at the exact source/runtime admission boundary.
///
/// Payload fields are private so no caller can extract a managed-source action
/// and accidentally submit it without the registry's synchronous policy
/// recheck. Clear is deliberately source-independent: it must be able to
/// retire an already-published value after its source authority disappears.
#[must_use = "playback handoffs must be admitted or explicitly discarded"]
pub struct LastFmPlaybackHandoff(LastFmPlaybackHandoffPayload);

impl LastFmPlaybackHandoff {
    fn now_playing(
        freshness: LastFmEphemeralHandoffFreshness,
        source: LastFmPlaybackSource,
        now_playing: LastFmNowPlaying,
    ) -> Self {
        Self(LastFmPlaybackHandoffPayload::NowPlaying {
            freshness,
            source,
            now_playing,
        })
    }

    fn enqueue(source: LastFmPlaybackSource, scrobble: UnboundLastFmScrobble) -> Self {
        Self(LastFmPlaybackHandoffPayload::Enqueue { source, scrobble })
    }

    fn clear_now_playing(freshness: LastFmEphemeralHandoffFreshness) -> Self {
        Self(LastFmPlaybackHandoffPayload::ClearNowPlaying { freshness })
    }

    #[must_use]
    pub const fn kind(&self) -> LastFmPlaybackHandoffKind {
        match &self.0 {
            LastFmPlaybackHandoffPayload::NowPlaying { .. } => {
                LastFmPlaybackHandoffKind::NowPlaying
            }
            LastFmPlaybackHandoffPayload::Enqueue { .. } => LastFmPlaybackHandoffKind::Enqueue,
            LastFmPlaybackHandoffPayload::ClearNowPlaying { .. } => {
                LastFmPlaybackHandoffKind::ClearNowPlaying
            }
        }
    }

    /// Perform the bounded runtime ingress while exact managed-source policy
    /// is held by the registry. `None` means source authority/policy rejected
    /// the action before runtime ingress; `Some(Err(_))` is a fixed runtime
    /// admission failure.
    pub fn try_admit(
        self,
        runtime: &LastFmRuntimeHandle,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
    ) -> Option<Result<LastFmPlaybackRuntimeOperation, LastFmRuntimeAdmissionError>> {
        self.try_admit_with(
            registry,
            enabled_remote_sources,
            |now_playing| {
                runtime
                    .try_update_now_playing(now_playing)
                    .map(LastFmPlaybackRuntimeOperation::NowPlaying)
            },
            |scrobble| {
                runtime
                    .try_enqueue(scrobble)
                    .map(LastFmPlaybackRuntimeOperation::Enqueue)
            },
            || {
                runtime
                    .try_clear_now_playing()
                    .map(LastFmPlaybackRuntimeOperation::ClearNowPlaying)
            },
        )
    }

    /// Test-only callback ingress for cross-module authority regressions.
    ///
    /// This preserves the production freshness and source-policy gates while
    /// avoiding construction of a credential-owning runtime in UI tests.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn try_admit_with_callbacks_for_test<T>(
        self,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
        admit_now_playing: impl FnOnce(LastFmNowPlaying) -> T,
        admit_enqueue: impl FnOnce(UnboundLastFmScrobble) -> T,
        admit_clear: impl FnOnce() -> T,
    ) -> Option<T> {
        self.try_admit_with(
            registry,
            enabled_remote_sources,
            admit_now_playing,
            admit_enqueue,
            admit_clear,
        )
    }

    fn try_admit_with<T>(
        self,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
        admit_now_playing: impl FnOnce(LastFmNowPlaying) -> T,
        admit_enqueue: impl FnOnce(UnboundLastFmScrobble) -> T,
        admit_clear: impl FnOnce() -> T,
    ) -> Option<T> {
        match self.0 {
            LastFmPlaybackHandoffPayload::NowPlaying {
                freshness,
                source,
                now_playing,
            } => freshness
                .try_claim(|| {
                    source.admit(registry, enabled_remote_sources, || {
                        admit_now_playing(now_playing)
                    })
                })
                .flatten(),
            LastFmPlaybackHandoffPayload::Enqueue { source, scrobble } => {
                source.admit(registry, enabled_remote_sources, || admit_enqueue(scrobble))
            }
            LastFmPlaybackHandoffPayload::ClearNowPlaying { freshness } => {
                freshness.try_claim(admit_clear)
            }
        }
    }
}

impl fmt::Debug for LastFmPlaybackHandoff {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("LastFmPlaybackHandoff")
            .field(&self.kind())
            .finish()
    }
}

/// Admitted runtime work, retaining only its fixed result channel.
#[must_use = "admitted Last.fm runtime work must be awaited or deliberately cancelled"]
pub enum LastFmPlaybackRuntimeOperation {
    NowPlaying(LastFmRuntimeOperation<LastFmNowPlayingOutcome>),
    Enqueue(LastFmRuntimeOperation<LastFmEnqueueOutcome>),
    ClearNowPlaying(LastFmRuntimeOperation<()>),
}

impl LastFmPlaybackRuntimeOperation {
    pub async fn wait(self) -> Result<LastFmPlaybackRuntimeOutcome, LastFmRuntimeCommandError> {
        match self {
            Self::NowPlaying(operation) => operation
                .wait()
                .await
                .map(LastFmPlaybackRuntimeOutcome::NowPlaying),
            Self::Enqueue(operation) => operation
                .wait()
                .await
                .map(LastFmPlaybackRuntimeOutcome::Enqueue),
            Self::ClearNowPlaying(operation) => operation
                .wait()
                .await
                .map(|()| LastFmPlaybackRuntimeOutcome::ClearNowPlaying),
        }
    }
}

impl fmt::Debug for LastFmPlaybackRuntimeOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::NowPlaying(_) => LastFmPlaybackHandoffKind::NowPlaying,
            Self::Enqueue(_) => LastFmPlaybackHandoffKind::Enqueue,
            Self::ClearNowPlaying(_) => LastFmPlaybackHandoffKind::ClearNowPlaying,
        };
        formatter
            .debug_tuple("LastFmPlaybackRuntimeOperation")
            .field(&kind)
            .finish()
    }
}

/// Content-free completion of one admitted runtime handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmPlaybackRuntimeOutcome {
    NowPlaying(LastFmNowPlayingOutcome),
    Enqueue(LastFmEnqueueOutcome),
    ClearNowPlaying,
}

/// At most one handoff plus one fixed owner failure from a synchronous event.
///
/// An inconsistent occurrence or evidence failure can require an explicit
/// clear and a visible fixed-category error at the same time, so these values
/// intentionally are independent options rather than a `Result`.
#[must_use = "playback owner updates may contain a required clear or fixed failure"]
pub struct LastFmPlaybackOwnerUpdate {
    handoff: Option<LastFmPlaybackHandoff>,
    error: Option<LastFmPlaybackOwnerError>,
}

/// Result of source-policy admission for one output-accepted load.
///
/// Rejection never creates the proposed occurrence, but it does terminally
/// retire any predecessor because the output has already replaced that media.
/// Its update therefore may contain one required clear-now-playing handoff.
#[must_use = "load admission must handle rejection and any predecessor clear"]
pub enum LastFmPlaybackLoadAdmission {
    Admitted(LastFmPlaybackOwnerUpdate),
    Rejected(LastFmPlaybackOwnerUpdate),
    Stale(LastFmPlaybackOwnerUpdate),
}

impl LastFmPlaybackLoadAdmission {
    #[must_use]
    pub const fn admitted(&self) -> bool {
        matches!(self, Self::Admitted(_))
    }

    #[must_use]
    pub const fn stale(&self) -> bool {
        matches!(self, Self::Stale(_))
    }

    pub fn into_update(self) -> LastFmPlaybackOwnerUpdate {
        match self {
            Self::Admitted(update) | Self::Rejected(update) | Self::Stale(update) => update,
        }
    }
}

impl fmt::Debug for LastFmPlaybackLoadAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admitted(update) => formatter.debug_tuple("Admitted").field(update).finish(),
            Self::Rejected(update) => formatter.debug_tuple("Rejected").field(update).finish(),
            Self::Stale(update) => formatter.debug_tuple("Stale").field(update).finish(),
        }
    }
}

impl LastFmPlaybackOwnerUpdate {
    const fn none() -> Self {
        Self {
            handoff: None,
            error: None,
        }
    }

    fn handoff(handoff: LastFmPlaybackHandoff) -> Self {
        Self {
            handoff: Some(handoff),
            error: None,
        }
    }

    fn error(error: LastFmPlaybackOwnerError) -> Self {
        Self {
            handoff: None,
            error: Some(error),
        }
    }

    fn handoff_and_error(handoff: LastFmPlaybackHandoff, error: LastFmPlaybackOwnerError) -> Self {
        Self {
            handoff: Some(handoff),
            error: Some(error),
        }
    }

    #[must_use = "both the handoff and fixed failure must be handled"]
    pub fn into_parts(
        self,
    ) -> (
        Option<LastFmPlaybackHandoff>,
        Option<LastFmPlaybackOwnerError>,
    ) {
        (self.handoff, self.error)
    }
}

impl fmt::Debug for LastFmPlaybackOwnerUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmPlaybackOwnerUpdate")
            .field(
                "handoff_kind",
                &self.handoff.as_ref().map(|value| value.kind()),
            )
            .field("error", &self.error)
            .finish()
    }
}

struct ActiveOccurrence {
    identity: LastFmPlaybackOccurrenceIdentity,
    source: LastFmPlaybackSource,
    metadata: LastFmPlaybackMetadata,
    occurrence: LastFmPlaybackOccurrence<PlayerEventGeneration>,
    now_playing_needs_clear: bool,
}

impl ActiveOccurrence {
    fn retire(&mut self) -> bool {
        self.occurrence.retire();
        self.take_clear()
    }

    fn take_clear(&mut self) -> bool {
        if !self.now_playing_needs_clear {
            return false;
        }
        self.now_playing_needs_clear = false;
        true
    }
}

/// Sole owner of the active Last.fm playback-evidence occurrence.
pub struct LastFmPlaybackOwner<C = SystemLastFmPlaybackClock>
where
    C: LastFmPlaybackClock,
{
    clock: C,
    active: Option<ActiveOccurrence>,
    ephemeral_handoff_lane: LastFmEphemeralHandoffLane,
}

impl LastFmPlaybackOwner<SystemLastFmPlaybackClock> {
    #[cfg(test)]
    #[allow(clippy::redundant_pub_crate)] // Production ownership is application-internal.
    pub(crate) fn new() -> Self {
        Self::with_clock(SystemLastFmPlaybackClock)
    }
}

impl<C> LastFmPlaybackOwner<C>
where
    C: LastFmPlaybackClock,
{
    #[cfg(test)]
    fn with_clock(clock: C) -> Self {
        Self {
            clock,
            active: None,
            ephemeral_handoff_lane: Arc::new(Mutex::new(LastFmEphemeralHandoffLaneState {
                current_identity: None,
                unclaimed: false,
            })),
        }
    }

    /// Consume one exact accepted output load and update occurrence ownership.
    ///
    /// Eligible metadata still crosses exact synchronous source-policy
    /// admission. An explicitly ineligible load, or one rejected by managed
    /// source policy, creates no occurrence and terminally retires the
    /// predecessor because the output has already replaced that media.
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn accept_output_load(
        &mut self,
        load: LastFmAcceptedOutputLoad,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
    ) -> LastFmPlaybackLoadAdmission {
        self.accept_output_load_observing(load, registry, enabled_remote_sources, || {})
    }

    fn accept_output_load_observing(
        &mut self,
        load: LastFmAcceptedOutputLoad,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
        after_owner_mutation: impl FnOnce(),
    ) -> LastFmPlaybackLoadAdmission {
        let LastFmAcceptedOutputLoad {
            generation,
            freshness,
            kind,
        } = load;
        freshness
            .try_claim(|| {
                let admission = self.with_ephemeral_handoff_lane(|owner, lane| match kind {
                    LastFmAcceptedOutputLoadKind::Eligible(accepted) => owner
                        .accept_eligible_output_load_with_lane(
                            *accepted,
                            generation,
                            registry,
                            enabled_remote_sources,
                            lane,
                        ),
                    LastFmAcceptedOutputLoadKind::Ineligible => {
                        owner.reject_output_load_with_lane(lane)
                    }
                });
                after_owner_mutation();
                admission
            })
            .unwrap_or_else(
                || LastFmPlaybackLoadAdmission::Stale(LastFmPlaybackOwnerUpdate::none()),
            )
    }

    fn accept_eligible_output_load_with_lane(
        &mut self,
        accepted: LastFmAcceptedPlayback,
        generation: PlayerEventGeneration,
        registry: &SourceRegistry,
        enabled_remote_sources: &HashSet<SourceId>,
        lane: &mut LastFmEphemeralHandoffLaneState,
    ) -> LastFmPlaybackLoadAdmission {
        let source = accepted.source.clone();
        match source.admit(registry, enabled_remote_sources, || {
            self.accept_load_with_lane(accepted, generation, lane)
        }) {
            Some(update) => LastFmPlaybackLoadAdmission::Admitted(update),
            None => self.reject_output_load_with_lane(lane),
        }
    }

    fn reject_output_load_with_lane(
        &mut self,
        lane: &mut LastFmEphemeralHandoffLaneState,
    ) -> LastFmPlaybackLoadAdmission {
        let update = self.retire_with_lane(lane).map_or_else(
            LastFmPlaybackOwnerUpdate::none,
            LastFmPlaybackOwnerUpdate::handoff,
        );
        LastFmPlaybackLoadAdmission::Rejected(update)
    }

    /// Attach an already source-admitted load.
    ///
    /// Within this owner lifetime, the same opaque identity and frozen
    /// snapshot denotes a retry and keeps its original UUID, start, credit,
    /// and one-shot latches. A different identity retires the predecessor.
    /// Reusing an identity with changed source or metadata permanently retires
    /// that identity until a genuinely new identity arrives.
    fn accept_load(
        &mut self,
        accepted: LastFmAcceptedPlayback,
        generation: PlayerEventGeneration,
    ) -> LastFmPlaybackOwnerUpdate {
        self.with_ephemeral_handoff_lane(move |owner, lane| {
            owner.accept_load_with_lane(accepted, generation, lane)
        })
    }

    fn accept_load_with_lane(
        &mut self,
        accepted: LastFmAcceptedPlayback,
        generation: PlayerEventGeneration,
        lane: &mut LastFmEphemeralHandoffLaneState,
    ) -> LastFmPlaybackOwnerUpdate {
        let LastFmAcceptedPlayback {
            identity,
            source,
            metadata,
        } = accepted;

        if let Some(active) = self.active.as_mut() {
            if active.identity == identity {
                if active.source != source || active.metadata != metadata {
                    let needs_clear = active.retire();
                    let clear = self.issue_clear_if_needed_with_lane(needs_clear, lane);
                    return update_with_optional_handoff_and_error(
                        clear,
                        LastFmPlaybackOwnerError::InconsistentOccurrence,
                    );
                }
                let _accepted = active.occurrence.accept_generation(generation);
                return LastFmPlaybackOwnerUpdate::none();
            }
        }

        let needs_clear = self
            .active
            .take()
            .is_some_and(|mut predecessor| predecessor.retire());
        let clear = self.issue_clear_if_needed_with_lane(needs_clear, lane);
        let mut occurrence = LastFmPlaybackOccurrence::new(metadata.clone());
        if !occurrence.accept_generation(generation) {
            return update_with_optional_handoff_and_error(
                clear,
                LastFmPlaybackOwnerError::Evidence(LastFmPlaybackEvidenceError::Invariant),
            );
        }
        self.active = Some(ActiveOccurrence {
            identity,
            source,
            metadata,
            occurrence,
            now_playing_needs_clear: false,
        });

        clear.map_or_else(
            LastFmPlaybackOwnerUpdate::none,
            LastFmPlaybackOwnerUpdate::handoff,
        )
    }

    /// Map one concrete output event to playback evidence.
    ///
    /// Position-event duration is intentionally ignored: qualification uses
    /// only the structured duration frozen at accepted-load construction.
    /// Error text is likewise ignored and can never enter owner diagnostics.
    pub fn observe_event(&mut self, event: &PlayerEvent) -> LastFmPlaybackOwnerUpdate {
        let action = match event {
            PlayerEvent::StateChanged { generation, state } => {
                let Some(active) = self.active.as_mut() else {
                    return LastFmPlaybackOwnerUpdate::none();
                };
                active
                    .occurrence
                    .observe_state(*generation, map_player_state(*state), &self.clock)
            }
            PlayerEvent::PositionChanged {
                generation,
                position_ms,
                duration_ms: _,
            } => {
                let Some(active) = self.active.as_mut() else {
                    return LastFmPlaybackOwnerUpdate::none();
                };
                active
                    .occurrence
                    .observe_position(*generation, *position_ms, &self.clock)
            }
            PlayerEvent::TrackEnded { generation } => {
                let Some(active) = self.active.as_mut() else {
                    return LastFmPlaybackOwnerUpdate::none();
                };
                if !active.occurrence.observe_natural_end(*generation) {
                    return LastFmPlaybackOwnerUpdate::none();
                }
                let clear = self
                    .active
                    .take()
                    .is_some_and(|mut terminal| terminal.take_clear());
                let clear = self.issue_clear_if_needed(clear);
                return clear.map_or_else(
                    LastFmPlaybackOwnerUpdate::none,
                    LastFmPlaybackOwnerUpdate::handoff,
                );
            }
            PlayerEvent::Error {
                generation,
                message: _,
            } => {
                let Some(active) = self.active.as_mut() else {
                    return LastFmPlaybackOwnerUpdate::none();
                };
                if !active.occurrence.observe_error(*generation) {
                    return LastFmPlaybackOwnerUpdate::none();
                }
                let needs_clear = active.take_clear();
                let clear = self.issue_clear_if_needed(needs_clear);
                return clear.map_or_else(
                    LastFmPlaybackOwnerUpdate::none,
                    LastFmPlaybackOwnerUpdate::handoff,
                );
            }
        };

        match action {
            Ok(Some(action)) => self.handoff_action(action),
            Ok(None) => LastFmPlaybackOwnerUpdate::none(),
            Err(error) => self.disable_with_error(error),
        }
    }

    /// Re-anchor current-generation position sampling after seek/restart or a
    /// same-output resume handoff. This method cannot emit an action.
    pub fn observe_discontinuity(&mut self, generation: PlayerEventGeneration) -> bool {
        self.active
            .as_mut()
            .is_some_and(|active| active.occurrence.observe_discontinuity(generation))
    }

    /// End the active occurrence for Stop, queue/source retirement, output
    /// replacement, or application shutdown. Clear is emitted at most once.
    pub fn retire(&mut self) -> Option<LastFmPlaybackHandoff> {
        self.with_ephemeral_handoff_lane(|owner, lane| owner.retire_with_lane(lane))
    }

    fn retire_with_lane(
        &mut self,
        lane: &mut LastFmEphemeralHandoffLaneState,
    ) -> Option<LastFmPlaybackHandoff> {
        let needs_clear = self.active.take().is_some_and(|mut active| active.retire());
        self.issue_clear_if_needed_with_lane(needs_clear, lane)
    }

    fn handoff_action(&mut self, action: LastFmPlaybackAction) -> LastFmPlaybackOwnerUpdate {
        let Some(active) = self.active.as_mut() else {
            return LastFmPlaybackOwnerUpdate::error(LastFmPlaybackOwnerError::Evidence(
                LastFmPlaybackEvidenceError::Invariant,
            ));
        };
        match action {
            LastFmPlaybackAction::NowPlaying(track) => {
                let Ok(now_playing) = LastFmNowPlaying::try_new(track) else {
                    active.occurrence.retire();
                    let needs_clear = active.take_clear();
                    let clear = self.issue_clear_if_needed(needs_clear);
                    return update_with_optional_handoff_and_error(
                        clear,
                        LastFmPlaybackOwnerError::Evidence(LastFmPlaybackEvidenceError::Invariant),
                    );
                };
                active.now_playing_needs_clear = true;
                let source = active.source.clone();
                LastFmPlaybackOwnerUpdate::handoff(self.issue_now_playing(source, now_playing))
            }
            LastFmPlaybackAction::Scrobble(scrobble) => LastFmPlaybackOwnerUpdate::handoff(
                LastFmPlaybackHandoff::enqueue(active.source.clone(), scrobble),
            ),
        }
    }

    fn disable_with_error(
        &mut self,
        error: LastFmPlaybackEvidenceError,
    ) -> LastFmPlaybackOwnerUpdate {
        let needs_clear = self
            .active
            .as_mut()
            .is_some_and(ActiveOccurrence::take_clear);
        let clear = self.issue_clear_if_needed(needs_clear);
        update_with_optional_handoff_and_error(clear, LastFmPlaybackOwnerError::Evidence(error))
    }

    fn issue_now_playing(
        &mut self,
        source: LastFmPlaybackSource,
        now_playing: LastFmNowPlaying,
    ) -> LastFmPlaybackHandoff {
        self.with_ephemeral_handoff_lane(move |owner, lane| {
            let freshness = owner.replace_ephemeral_handoff_freshness_with_lane(lane);
            LastFmPlaybackHandoff::now_playing(freshness, source, now_playing)
        })
    }

    fn issue_clear_if_needed(&mut self, needs_clear: bool) -> Option<LastFmPlaybackHandoff> {
        self.with_ephemeral_handoff_lane(|owner, lane| {
            owner.issue_clear_if_needed_with_lane(needs_clear, lane)
        })
    }

    fn issue_clear_if_needed_with_lane(
        &self,
        needs_clear: bool,
        lane: &mut LastFmEphemeralHandoffLaneState,
    ) -> Option<LastFmPlaybackHandoff> {
        needs_clear.then(|| {
            let freshness = self.replace_ephemeral_handoff_freshness_with_lane(lane);
            LastFmPlaybackHandoff::clear_now_playing(freshness)
        })
    }

    fn replace_ephemeral_handoff_freshness_with_lane(
        &self,
        lane: &mut LastFmEphemeralHandoffLaneState,
    ) -> LastFmEphemeralHandoffFreshness {
        let identity = Arc::new(());
        lane.current_identity = Some(Arc::clone(&identity));
        lane.unclaimed = true;
        LastFmEphemeralHandoffFreshness {
            lane: Arc::clone(&self.ephemeral_handoff_lane),
            identity,
        }
    }

    fn with_ephemeral_handoff_lane<T>(
        &mut self,
        mutate: impl FnOnce(&mut Self, &mut LastFmEphemeralHandoffLaneState) -> T,
    ) -> T {
        let lane = Arc::clone(&self.ephemeral_handoff_lane);
        let mut lane = lane.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = mutate(self, &mut lane);
        drop(lane);
        result
    }
}

impl<C> fmt::Debug for LastFmPlaybackOwner<C>
where
    C: LastFmPlaybackClock,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmPlaybackOwner")
            .field("has_active_occurrence", &self.active.is_some())
            .field(
                "now_playing_needs_clear",
                &self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.now_playing_needs_clear),
            )
            .finish_non_exhaustive()
    }
}

impl<C> Drop for LastFmPlaybackOwner<C>
where
    C: LastFmPlaybackClock,
{
    fn drop(&mut self) {
        let mut lane = self
            .ephemeral_handoff_lane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lane.current_identity = None;
        lane.unclaimed = false;
    }
}

const fn map_player_state(state: PlayerState) -> LastFmPlaybackState {
    match state {
        PlayerState::Stopped => LastFmPlaybackState::Stopped,
        PlayerState::Buffering => LastFmPlaybackState::Buffering,
        PlayerState::Playing => LastFmPlaybackState::Playing,
        PlayerState::Paused => LastFmPlaybackState::Paused,
    }
}

fn update_with_optional_handoff_and_error(
    handoff: Option<LastFmPlaybackHandoff>,
    error: LastFmPlaybackOwnerError,
) -> LastFmPlaybackOwnerUpdate {
    handoff.map_or_else(
        || LastFmPlaybackOwnerUpdate::error(error),
        |handoff| LastFmPlaybackOwnerUpdate::handoff_and_error(handoff, error),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use crate::architecture::TrackId;
    use crate::lastfm::client::LastFmTrack;
    use uuid::Uuid;

    use super::*;

    const STARTED_AT: i64 = 1_752_000_123;

    #[derive(Clone)]
    struct TestClock {
        result: Result<i64, LastFmPlaybackEvidenceError>,
        calls: Arc<AtomicUsize>,
    }

    impl TestClock {
        fn successful() -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    result: Ok(STARTED_AT),
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }

        fn failing() -> Self {
            Self {
                result: Err(LastFmPlaybackEvidenceError::ClockOutOfRange),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl LastFmPlaybackClock for TestClock {
        fn now_unix_seconds(&self) -> Result<i64, LastFmPlaybackEvidenceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result
        }
    }

    fn generation(value: u64) -> PlayerEventGeneration {
        PlayerEventGeneration::from_raw(value)
    }

    fn local_source(track_id: &str) -> LastFmPlaybackSource {
        LastFmPlaybackSource::local(MediaKey::new(
            SourceId::local(),
            TrackId::new(track_id).expect("valid test track id"),
        ))
        .expect("local source")
    }

    fn accepted(
        identity: LastFmPlaybackOccurrenceIdentity,
        source: LastFmPlaybackSource,
    ) -> LastFmAcceptedPlayback {
        accepted_with(identity, source, "artist-private", "title-private", 100)
    }

    fn accepted_with(
        identity: LastFmPlaybackOccurrenceIdentity,
        source: LastFmPlaybackSource,
        artist: &str,
        title: &str,
        duration_secs: u64,
    ) -> LastFmAcceptedPlayback {
        LastFmAcceptedPlayback::try_new(
            identity,
            source,
            artist.to_owned(),
            title.to_owned(),
            Some("album-private".to_owned()),
            Some("album-artist-private".to_owned()),
            Some(7),
            Some(duration_secs),
        )
        .expect("valid accepted playback")
    }

    fn assert_empty(update: LastFmPlaybackOwnerUpdate) {
        let (handoff, error) = update.into_parts();
        assert!(handoff.is_none());
        assert!(error.is_none());
    }

    fn expect_admission_update(
        admission: LastFmPlaybackLoadAdmission,
    ) -> LastFmPlaybackOwnerUpdate {
        admission.into_update()
    }

    fn output_freshness() -> LastFmAcceptedOutputFreshness {
        LastFmAcceptedOutputFreshness::fresh()
    }

    fn ephemeral_freshness() -> LastFmEphemeralHandoffFreshness {
        let identity = Arc::new(());
        LastFmEphemeralHandoffFreshness {
            lane: Arc::new(Mutex::new(LastFmEphemeralHandoffLaneState {
                current_identity: Some(Arc::clone(&identity)),
                unclaimed: true,
            })),
            identity,
        }
    }

    fn expect_handoff(
        update: LastFmPlaybackOwnerUpdate,
        expected: LastFmPlaybackHandoffKind,
    ) -> LastFmPlaybackHandoff {
        let (handoff, error) = update.into_parts();
        assert!(error.is_none());
        let handoff = handoff.expect("expected handoff");
        assert_eq!(handoff.kind(), expected);
        handoff
    }

    fn expect_now_playing(update: LastFmPlaybackOwnerUpdate) {
        let handoff = expect_handoff(update, LastFmPlaybackHandoffKind::NowPlaying);
        match handoff.0 {
            LastFmPlaybackHandoffPayload::NowPlaying { source, .. } => {
                assert!(matches!(
                    source,
                    LastFmPlaybackSource(LastFmPlaybackSourceKind::Local(_))
                ));
            }
            _ => panic!("expected now-playing payload"),
        }
    }

    fn expect_enqueue(update: LastFmPlaybackOwnerUpdate) -> UnboundLastFmScrobble {
        let handoff = expect_handoff(update, LastFmPlaybackHandoffKind::Enqueue);
        match handoff.0 {
            LastFmPlaybackHandoffPayload::Enqueue { source, scrobble } => {
                assert!(matches!(
                    source,
                    LastFmPlaybackSource(LastFmPlaybackSourceKind::Local(_))
                ));
                scrobble
            }
            _ => panic!("expected enqueue payload"),
        }
    }

    fn expect_clear(update: LastFmPlaybackOwnerUpdate) {
        let handoff = expect_handoff(update, LastFmPlaybackHandoffKind::ClearNowPlaying);
        assert!(matches!(
            handoff.0,
            LastFmPlaybackHandoffPayload::ClearNowPlaying { .. }
        ));
    }

    fn load_and_prove_playing(
        owner: &mut LastFmPlaybackOwner<TestClock>,
        identity: LastFmPlaybackOccurrenceIdentity,
        source: LastFmPlaybackSource,
        generation: PlayerEventGeneration,
    ) {
        assert_empty(owner.accept_load(accepted(identity, source), generation));
        expect_now_playing(
            owner.observe_event(&PlayerEvent::state(generation, PlayerState::Playing)),
        );
    }

    fn qualify_after_playing(
        owner: &mut LastFmPlaybackOwner<TestClock>,
        generation: PlayerEventGeneration,
    ) -> UnboundLastFmScrobble {
        assert_empty(owner.observe_event(&PlayerEvent::position(generation, 10_000, 100_000)));
        expect_enqueue(owner.observe_event(&PlayerEvent::position(generation, 60_000, 100_000)))
    }

    #[test]
    fn occurrence_identity_uses_exact_allocation_identity_and_redacts() {
        let sentinel = LastFmPlaybackOccurrenceIdentity::fresh();
        let same = sentinel.clone();
        let different = LastFmPlaybackOccurrenceIdentity::fresh();

        assert_eq!(sentinel, same);
        assert_ne!(sentinel, different);
        let debug = format!("{sentinel:?}");
        assert_eq!(debug, "LastFmPlaybackOccurrenceIdentity(<opaque>)");
    }

    #[test]
    fn source_construction_rejects_nonlocal_local_claim_and_redacts_identity() {
        let secret_track = "SOURCE-TRACK-SENTINEL";
        let local = local_source(secret_track);
        assert!(!format!("{local:?}").contains(secret_track));

        let remote_key = MediaKey::new(
            SourceId::random(),
            TrackId::new(secret_track).expect("valid track id"),
        );
        assert!(LastFmPlaybackSource::local(remote_key.clone()).is_none());
        let reference = PlaybackSourceReference::session(remote_key, 9).expect("valid reference");
        let managed = LastFmPlaybackSource::managed(reference);
        assert_eq!(
            format!("{managed:?}"),
            "LastFmPlaybackSource::Managed(<redacted>)"
        );
    }

    #[test]
    fn accepted_snapshot_validates_metadata_and_redacts_every_field() {
        let invalid = LastFmAcceptedPlayback::try_new(
            LastFmPlaybackOccurrenceIdentity::fresh(),
            local_source("invalid"),
            "   ".to_owned(),
            "title".to_owned(),
            None,
            None,
            None,
            Some(100),
        );
        assert_eq!(
            invalid.unwrap_err(),
            LastFmPlaybackEvidenceError::InvalidMetadata
        );

        let snapshot = accepted_with(
            LastFmPlaybackOccurrenceIdentity::fresh(),
            local_source("SNAPSHOT-SOURCE-SENTINEL"),
            "SNAPSHOT-ARTIST-SENTINEL",
            "SNAPSHOT-TITLE-SENTINEL",
            100,
        );
        assert_eq!(
            format!("{snapshot:?}"),
            "LastFmAcceptedPlayback(<redacted>)"
        );
    }

    #[test]
    fn accepted_load_alone_emits_nothing_and_playing_emits_once() {
        let (clock, calls) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let identity = LastFmPlaybackOccurrenceIdentity::fresh();
        let source = local_source("one");

        assert_empty(owner.accept_load(accepted(identity.clone(), source.clone()), generation(1)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        expect_now_playing(
            owner.observe_event(&PlayerEvent::state(generation(1), PlayerState::Playing)),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_empty(owner.observe_event(&PlayerEvent::state(generation(1), PlayerState::Playing)));
        assert_empty(owner.accept_load(accepted(identity, source), generation(1)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn first_position_is_playing_proof_but_only_an_anchor() {
        let (clock, calls) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        assert_empty(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("position-proof"),
            ),
            generation(3),
        ));

        expect_now_playing(owner.observe_event(&PlayerEvent::position(
            generation(3),
            90_000,
            u64::MAX,
        )));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(3), 139_999, 1)));
        let scrobble =
            expect_enqueue(owner.observe_event(&PlayerEvent::position(generation(3), 140_000, 0)));
        assert_eq!(scrobble.duration_secs(), 100);
    }

    #[test]
    fn buffering_permits_position_proof_while_pause_and_stop_revoke_it() {
        for revoked in [PlayerState::Paused, PlayerState::Stopped] {
            let (clock, calls) = TestClock::successful();
            let mut owner = LastFmPlaybackOwner::with_clock(clock);
            assert_empty(owner.accept_load(
                accepted(
                    LastFmPlaybackOccurrenceIdentity::fresh(),
                    local_source("state-map"),
                ),
                generation(4),
            ));
            assert_empty(owner.observe_event(&PlayerEvent::state(generation(4), revoked)));
            assert_empty(
                owner.observe_event(&PlayerEvent::state(generation(4), PlayerState::Buffering)),
            );
            assert_empty(owner.observe_event(&PlayerEvent::position(
                generation(4),
                20_000,
                100_000,
            )));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            expect_now_playing(
                owner.observe_event(&PlayerEvent::state(generation(4), PlayerState::Playing)),
            );
        }

        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        assert_empty(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("buffering"),
            ),
            generation(5),
        ));
        assert_empty(
            owner.observe_event(&PlayerEvent::state(generation(5), PlayerState::Buffering)),
        );
        expect_now_playing(owner.observe_event(&PlayerEvent::position(generation(5), 1, 100_000)));
    }

    #[test]
    fn frozen_metadata_and_duration_drive_exact_scrobble_payload() {
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let identity = LastFmPlaybackOccurrenceIdentity::fresh();
        assert_empty(owner.accept_load(
            accepted_with(
                identity,
                local_source("frozen"),
                "  exact artist  ",
                "exact title",
                100,
            ),
            generation(6),
        ));
        expect_now_playing(owner.observe_event(&PlayerEvent::position(generation(6), 0, 1)));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(6), 49_999, u64::MAX)));
        let scrobble =
            expect_enqueue(owner.observe_event(&PlayerEvent::position(generation(6), 50_000, 0)));

        assert_eq!(scrobble.artist(), "  exact artist  ");
        assert_eq!(scrobble.track_title(), "exact title");
        assert_eq!(scrobble.album(), Some("album-private"));
        assert_eq!(scrobble.album_artist(), Some("album-artist-private"));
        assert_eq!(scrobble.track_number(), Some(7));
        assert_eq!(scrobble.duration_secs(), 100);
        assert_eq!(scrobble.started_at_unix_secs(), STARTED_AT);
    }

    #[test]
    fn same_identity_retry_preserves_start_credit_and_closed_now_playing_latch() {
        let (clock, calls) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let identity = LastFmPlaybackOccurrenceIdentity::fresh();
        let source = local_source("retry");
        load_and_prove_playing(&mut owner, identity.clone(), source.clone(), generation(7));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(7), 10_000, 100_000)));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(7), 30_000, 100_000)));
        expect_clear(owner.observe_event(&PlayerEvent::error(
            generation(7),
            "RETRY-ERROR-PRIVATE-SENTINEL",
        )));

        assert_empty(owner.accept_load(accepted(identity, source), generation(8)));
        assert_empty(owner.observe_event(&PlayerEvent::state(generation(8), PlayerState::Playing)));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(8), 100_000, u64::MAX)));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(8), 129_999, 1)));
        let scrobble =
            expect_enqueue(owner.observe_event(&PlayerEvent::position(generation(8), 130_000, 1)));

        assert_eq!(scrobble.started_at_unix_secs(), STARTED_AT);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn error_before_playing_needs_no_clear_and_retry_can_still_begin() {
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let identity = LastFmPlaybackOccurrenceIdentity::fresh();
        let source = local_source("early-error");
        assert_empty(owner.accept_load(accepted(identity.clone(), source.clone()), generation(9)));
        assert_empty(
            owner.observe_event(&PlayerEvent::error(generation(9), "PRIVATE-EARLY-ERROR")),
        );

        assert_empty(owner.accept_load(accepted(identity, source), generation(10)));
        expect_now_playing(owner.observe_event(&PlayerEvent::position(generation(10), 0, 100_000)));
    }

    #[test]
    fn new_identity_retires_predecessor_and_mints_a_new_scrobble_uuid() {
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        load_and_prove_playing(
            &mut owner,
            LastFmPlaybackOccurrenceIdentity::fresh(),
            local_source("same-track"),
            generation(11),
        );
        let first = qualify_after_playing(&mut owner, generation(11));

        expect_clear(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("same-track"),
            ),
            generation(12),
        ));
        expect_now_playing(
            owner.observe_event(&PlayerEvent::state(generation(12), PlayerState::Playing)),
        );
        let second = qualify_after_playing(&mut owner, generation(12));
        assert_ne!(first.occurrence_id(), second.occurrence_id());
    }

    #[test]
    fn same_identity_metadata_drift_fails_closed_and_clears_once() {
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let identity = LastFmPlaybackOccurrenceIdentity::fresh();
        let source = local_source("drift");
        load_and_prove_playing(&mut owner, identity.clone(), source.clone(), generation(13));

        let update = owner.accept_load(
            accepted_with(
                identity.clone(),
                source.clone(),
                "artist-private",
                "changed-title-private",
                100,
            ),
            generation(14),
        );
        let (handoff, error) = update.into_parts();
        assert_eq!(
            error,
            Some(LastFmPlaybackOwnerError::InconsistentOccurrence)
        );
        assert_eq!(
            handoff.as_ref().map(LastFmPlaybackHandoff::kind),
            Some(LastFmPlaybackHandoffKind::ClearNowPlaying)
        );

        assert_empty(owner.accept_load(accepted(identity, source), generation(15)));
        assert_empty(
            owner.observe_event(&PlayerEvent::state(generation(15), PlayerState::Playing)),
        );
        assert!(owner.retire().is_none());
    }

    #[test]
    fn same_identity_source_drift_fails_closed_without_metadata_exposure() {
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let identity = LastFmPlaybackOccurrenceIdentity::fresh();
        assert_empty(owner.accept_load(
            accepted(identity.clone(), local_source("source-one")),
            generation(16),
        ));

        let update = owner.accept_load(
            accepted(identity, local_source("source-two")),
            generation(17),
        );
        let debug = format!("{update:?}");
        let (handoff, error) = update.into_parts();
        assert!(handoff.is_none());
        assert_eq!(
            error,
            Some(LastFmPlaybackOwnerError::InconsistentOccurrence)
        );
        assert!(!debug.contains("source-one"));
        assert!(!debug.contains("source-two"));
    }

    #[test]
    fn stale_generation_events_are_inert() {
        let (clock, calls) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        assert_empty(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("stale"),
            ),
            generation(19),
        ));

        assert_empty(
            owner.observe_event(&PlayerEvent::state(generation(18), PlayerState::Playing)),
        );
        assert_empty(owner.observe_event(&PlayerEvent::position(
            generation(18),
            u64::MAX,
            u64::MAX,
        )));
        assert_empty(owner.observe_event(&PlayerEvent::ended(generation(18))));
        assert_empty(
            owner.observe_event(&PlayerEvent::error(generation(18), "STALE-PRIVATE-ERROR")),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        expect_now_playing(
            owner.observe_event(&PlayerEvent::state(generation(19), PlayerState::Playing)),
        );
    }

    #[test]
    fn natural_end_never_supplies_tail_credit_and_is_terminal() {
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        load_and_prove_playing(
            &mut owner,
            LastFmPlaybackOccurrenceIdentity::fresh(),
            local_source("eos"),
            generation(20),
        );
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(20), 0, 100_000)));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(20), 49_999, 100_000)));
        expect_clear(owner.observe_event(&PlayerEvent::ended(generation(20))));
        assert_empty(owner.observe_event(&PlayerEvent::position(
            generation(20),
            u64::MAX,
            100_000,
        )));
        assert!(owner.retire().is_none());
    }

    #[test]
    fn discontinuity_reanchors_without_jump_credit() {
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        assert_empty(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("seek"),
            ),
            generation(21),
        ));
        expect_now_playing(owner.observe_event(&PlayerEvent::position(generation(21), 0, 100_000)));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(21), 20_000, 100_000)));
        assert!(owner.observe_discontinuity(generation(21)));
        assert!(!owner.observe_discontinuity(generation(99)));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(21), 90_000, 100_000)));
        assert_empty(owner.observe_event(&PlayerEvent::position(generation(21), 119_999, 100_000)));
        expect_enqueue(owner.observe_event(&PlayerEvent::position(
            generation(21),
            120_000,
            100_000,
        )));
    }

    #[test]
    fn explicit_retire_clears_once_and_is_idempotent() {
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        load_and_prove_playing(
            &mut owner,
            LastFmPlaybackOccurrenceIdentity::fresh(),
            local_source("retire"),
            generation(22),
        );

        let clear = owner.retire().expect("clear after now playing");
        assert_eq!(clear.kind(), LastFmPlaybackHandoffKind::ClearNowPlaying);
        assert!(owner.retire().is_none());
        assert_empty(owner.observe_event(&PlayerEvent::position(
            generation(22),
            u64::MAX,
            u64::MAX,
        )));
    }

    #[test]
    fn clock_failure_is_fixed_redacted_and_disables_same_identity() {
        let mut owner = LastFmPlaybackOwner::with_clock(TestClock::failing());
        let identity = LastFmPlaybackOccurrenceIdentity::fresh();
        let source = local_source("CLOCK-SOURCE-SENTINEL");
        assert_empty(owner.accept_load(
            accepted_with(
                identity.clone(),
                source.clone(),
                "CLOCK-ARTIST-SENTINEL",
                "CLOCK-TITLE-SENTINEL",
                100,
            ),
            generation(23),
        ));
        let update = owner.observe_event(&PlayerEvent::state(generation(23), PlayerState::Playing));
        let debug = format!("{owner:?} {update:?}");
        let (handoff, error) = update.into_parts();
        assert!(handoff.is_none());
        assert_eq!(
            error,
            Some(LastFmPlaybackOwnerError::Evidence(
                LastFmPlaybackEvidenceError::ClockOutOfRange
            ))
        );
        assert!(!debug.contains("CLOCK-SOURCE-SENTINEL"));
        assert!(!debug.contains("CLOCK-ARTIST-SENTINEL"));
        assert!(!debug.contains("CLOCK-TITLE-SENTINEL"));

        assert_empty(owner.accept_load(
            accepted_with(
                identity,
                source,
                "CLOCK-ARTIST-SENTINEL",
                "CLOCK-TITLE-SENTINEL",
                100,
            ),
            generation(24),
        ));
        assert_empty(
            owner.observe_event(&PlayerEvent::state(generation(24), PlayerState::Playing)),
        );
    }

    #[test]
    fn diagnostics_never_include_event_or_metadata_content() {
        let sentinel = "LASTFM-PRIVATE-DIAGNOSTIC-SENTINEL";
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        assert_empty(owner.accept_load(
            accepted_with(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source(sentinel),
                sentinel,
                sentinel,
                100,
            ),
            generation(25),
        ));
        let update = owner.observe_event(&PlayerEvent::state(generation(25), PlayerState::Playing));
        let debug = format!("{owner:?} {update:?}");
        assert!(!debug.contains(sentinel));
        let (handoff, error) = update.into_parts();
        assert!(error.is_none());
        let handoff = handoff.expect("now playing");
        assert!(!format!("{handoff:?}").contains(sentinel));

        let error_update = owner.observe_event(&PlayerEvent::error(generation(25), sentinel));
        assert!(!format!("{error_update:?} {owner:?}").contains(sentinel));
    }

    #[test]
    fn system_owner_constructor_starts_empty() {
        let owner = LastFmPlaybackOwner::new();
        assert_eq!(
            format!("{owner:?}"),
            "LastFmPlaybackOwner { has_active_occurrence: false, now_playing_needs_clear: false, .. }"
        );
    }

    #[test]
    fn load_admission_accepts_local_and_rejection_retires_predecessor_with_clear() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);

        let local_admission = owner.accept_output_load(
            LastFmAcceptedOutputLoad::eligible(
                LastFmAcceptedOutputMint::for_test(),
                generation(26),
                output_freshness(),
                accepted(
                    LastFmPlaybackOccurrenceIdentity::fresh(),
                    local_source("admitted-local"),
                ),
            ),
            &registry,
            &enabled_remote_sources,
        );
        assert!(local_admission.admitted());
        assert_empty(expect_admission_update(local_admission));
        expect_now_playing(
            owner.observe_event(&PlayerEvent::state(generation(26), PlayerState::Playing)),
        );

        let remote_key = MediaKey::new(
            SourceId::random(),
            TrackId::remote("not-installed").expect("valid remote track id"),
        );
        let remote_source = LastFmPlaybackSource::managed(
            PlaybackSourceReference::session(remote_key, 1).expect("valid remote reference"),
        );
        let denied = owner.accept_output_load(
            LastFmAcceptedOutputLoad::eligible(
                LastFmAcceptedOutputMint::for_test(),
                generation(27),
                output_freshness(),
                accepted(LastFmPlaybackOccurrenceIdentity::fresh(), remote_source),
            ),
            &registry,
            &enabled_remote_sources,
        );
        assert!(!denied.admitted());
        expect_clear(expect_admission_update(denied));

        assert_empty(owner.observe_event(&PlayerEvent::position(generation(26), 10_000, 100_000)));
        assert!(owner.retire().is_none());
    }

    #[test]
    fn ineligible_accepted_successor_clears_predecessor_once_and_makes_it_stale() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let (clock, calls) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let first_generation = generation(30);

        let admitted = owner.accept_output_load(
            LastFmAcceptedOutputLoad::eligible(
                LastFmAcceptedOutputMint::for_test(),
                first_generation,
                output_freshness(),
                accepted(
                    LastFmPlaybackOccurrenceIdentity::fresh(),
                    local_source("eligible-a"),
                ),
            ),
            &registry,
            &enabled_remote_sources,
        );
        assert!(admitted.admitted());
        assert_empty(expect_admission_update(admitted));
        expect_now_playing(
            owner.observe_event(&PlayerEvent::state(first_generation, PlayerState::Playing)),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let rejected = owner.accept_output_load(
            LastFmAcceptedOutputLoad::ineligible(
                LastFmAcceptedOutputMint::for_test(),
                generation(31),
                output_freshness(),
            ),
            &registry,
            &enabled_remote_sources,
        );
        assert!(!rejected.admitted());
        expect_clear(expect_admission_update(rejected));

        let repeated = owner.accept_output_load(
            LastFmAcceptedOutputLoad::ineligible(
                LastFmAcceptedOutputMint::for_test(),
                generation(32),
                output_freshness(),
            ),
            &registry,
            &enabled_remote_sources,
        );
        assert!(!repeated.admitted());
        assert_empty(expect_admission_update(repeated));
        assert_empty(
            owner.observe_event(&PlayerEvent::state(first_generation, PlayerState::Playing)),
        );
        assert_empty(owner.observe_event(&PlayerEvent::position(
            first_generation,
            u64::MAX,
            u64::MAX,
        )));
        assert_empty(owner.observe_event(&PlayerEvent::ended(first_generation)));
        assert_empty(owner.observe_event(&PlayerEvent::error(
            first_generation,
            "STALE-A-PRIVATE-ERROR",
        )));
        assert!(owner.retire().is_none());
    }

    #[test]
    fn accepted_output_load_debug_redacts_generation_source_and_metadata() {
        let sentinel = "ACCEPTED-OUTPUT-LOAD-PRIVATE-SENTINEL";
        let freshness = output_freshness();
        assert_eq!(
            format!("{freshness:?}"),
            "LastFmAcceptedOutputFreshness(<redacted>)"
        );
        let eligible = LastFmAcceptedOutputLoad::eligible(
            LastFmAcceptedOutputMint::for_test(),
            generation(8_736_451),
            freshness,
            accepted_with(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source(sentinel),
                sentinel,
                sentinel,
                100,
            ),
        );
        let debug = format!("{eligible:?}");
        assert_eq!(debug, "LastFmAcceptedOutputLoad::Eligible(<redacted>)");
        assert!(!debug.contains(sentinel));
        assert!(!debug.contains("8736451"));
        assert_eq!(
            format!(
                "{:?}",
                LastFmAcceptedOutputLoad::ineligible(
                    LastFmAcceptedOutputMint::for_test(),
                    generation(8_736_451),
                    output_freshness(),
                )
            ),
            "LastFmAcceptedOutputLoad::Ineligible"
        );
    }

    #[test]
    fn revoked_eligible_and_ineligible_output_loads_are_stale_and_inert() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let active_generation = generation(33);

        load_and_prove_playing(
            &mut owner,
            LastFmPlaybackOccurrenceIdentity::fresh(),
            local_source("freshness-predecessor"),
            active_generation,
        );

        let eligible_freshness = output_freshness();
        let stale_eligible = LastFmAcceptedOutputLoad::eligible(
            LastFmAcceptedOutputMint::for_test(),
            generation(34),
            eligible_freshness.clone(),
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("stale-eligible"),
            ),
        );
        eligible_freshness.revoke();
        let eligible_admission =
            owner.accept_output_load(stale_eligible, &registry, &enabled_remote_sources);
        assert!(eligible_admission.stale());
        assert_empty(eligible_admission.into_update());

        let ineligible_freshness = output_freshness();
        let stale_ineligible = LastFmAcceptedOutputLoad::ineligible(
            LastFmAcceptedOutputMint::for_test(),
            generation(35),
            ineligible_freshness.clone(),
        );
        ineligible_freshness.revoke();
        let ineligible_admission =
            owner.accept_output_load(stale_ineligible, &registry, &enabled_remote_sources);
        assert!(ineligible_admission.stale());
        assert_empty(ineligible_admission.into_update());

        let scrobble = qualify_after_playing(&mut owner, active_generation);
        assert_eq!(scrobble.duration_secs(), 100);
    }

    #[test]
    fn accepted_output_freshness_stays_locked_through_owner_mutation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let admitting_registry = registry.clone();
        let enabled_remote_sources = HashSet::new();
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let freshness = output_freshness();
        let freshness_probe = freshness.clone();
        let load = LastFmAcceptedOutputLoad::eligible(
            LastFmAcceptedOutputMint::for_test(),
            generation(36),
            freshness,
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("linearized-accepted-load"),
            ),
        );
        let (mutation_observed_tx, mutation_observed_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);

        let accepting = thread::spawn(move || {
            let admission = owner.accept_output_load_observing(
                load,
                &admitting_registry,
                &enabled_remote_sources,
                || {
                    mutation_observed_tx
                        .send(())
                        .expect("freshness observer remains alive");
                    release_rx
                        .recv()
                        .expect("release accepted-output freshness claim");
                },
            );
            (owner, admission)
        });

        mutation_observed_rx
            .recv()
            .expect("owner mutation reached while freshness is claimed");
        assert!(matches!(
            freshness_probe.0.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        release_tx
            .send(())
            .expect("accepted-output claimant remains alive");

        let (mut owner, admission) = accepting.join().expect("accepted-output thread");
        assert!(admission.admitted());
        assert_empty(admission.into_update());
        assert!(freshness_probe.try_claim(|| ()).is_none());
        expect_now_playing(
            owner.observe_event(&PlayerEvent::state(generation(36), PlayerState::Playing)),
        );
        drop(owner);
        runtime.block_on(registry.shutdown().wait());
    }

    #[test]
    fn rejected_managed_load_with_empty_owner_reports_rejection_without_clear() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let remote_key = MediaKey::new(
            SourceId::random(),
            TrackId::remote("EMPTY-OWNER-PRIVATE-SENTINEL").expect("valid remote track id"),
        );
        let remote_source = LastFmPlaybackSource::managed(
            PlaybackSourceReference::session(remote_key, 1).expect("valid remote reference"),
        );
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let denied = owner.accept_output_load(
            LastFmAcceptedOutputLoad::eligible(
                LastFmAcceptedOutputMint::for_test(),
                generation(28),
                output_freshness(),
                accepted(LastFmPlaybackOccurrenceIdentity::fresh(), remote_source),
            ),
            &registry,
            &HashSet::new(),
        );

        assert!(!denied.admitted());
        assert!(!format!("{denied:?}").contains("EMPTY-OWNER-PRIVATE-SENTINEL"));
        assert_empty(expect_admission_update(denied));
        assert!(owner.retire().is_none());
    }

    #[test]
    fn source_bound_handoffs_revalidate_policy_before_invoking_runtime_ingress() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let source_id = SourceId::random();
        let remote_key = MediaKey::new(
            source_id,
            TrackId::remote("handoff-policy").expect("valid remote track id"),
        );
        let reference =
            PlaybackSourceReference::session(remote_key, 7).expect("valid remote reference");
        let enabled_remote_sources = HashSet::from([source_id]);

        let now_playing = LastFmNowPlaying::try_new(LastFmTrack {
            artist: "private artist".to_owned(),
            title: "private title".to_owned(),
            album: None,
            album_artist: None,
            track_number: Some(1),
            duration_seconds: 100,
        })
        .expect("valid now-playing metadata");
        let now_playing_handoff = LastFmPlaybackHandoff::now_playing(
            ephemeral_freshness(),
            LastFmPlaybackSource::managed(reference.clone()),
            now_playing,
        );
        let now_playing_calls = Cell::new(0);
        let result = now_playing_handoff.try_admit_with(
            &registry,
            &enabled_remote_sources,
            |_| {
                now_playing_calls.set(now_playing_calls.get() + 1);
                LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                now_playing_calls.set(now_playing_calls.get() + 1);
                LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                now_playing_calls.set(now_playing_calls.get() + 1);
                LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert!(result.is_none());
        assert_eq!(now_playing_calls.get(), 0);

        let scrobble = UnboundLastFmScrobble::try_new(
            Uuid::new_v4(),
            "private artist".to_owned(),
            "private title".to_owned(),
            None,
            None,
            Some(1),
            100,
            STARTED_AT,
        )
        .expect("valid scrobble");
        let enqueue_handoff =
            LastFmPlaybackHandoff::enqueue(LastFmPlaybackSource::managed(reference), scrobble);
        let enqueue_calls = Cell::new(0);
        let result = enqueue_handoff.try_admit_with(
            &registry,
            &enabled_remote_sources,
            |_| {
                enqueue_calls.set(enqueue_calls.get() + 1);
                LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                enqueue_calls.set(enqueue_calls.get() + 1);
                LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                enqueue_calls.set(enqueue_calls.get() + 1);
                LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert!(result.is_none());
        assert_eq!(enqueue_calls.get(), 0);
    }

    #[test]
    fn local_and_clear_handoffs_reach_their_exact_runtime_ingress_callbacks() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let now_playing = LastFmNowPlaying::try_new(LastFmTrack {
            artist: "artist".to_owned(),
            title: "title".to_owned(),
            album: None,
            album_artist: None,
            track_number: None,
            duration_seconds: 100,
        })
        .expect("valid now-playing metadata");

        let local = LastFmPlaybackHandoff::now_playing(
            ephemeral_freshness(),
            local_source("callback-local"),
            now_playing,
        );
        let local_calls = Cell::new(0);
        let result = local.try_admit_with(
            &registry,
            &enabled_remote_sources,
            |_| {
                local_calls.set(local_calls.get() + 1);
                LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                local_calls.set(local_calls.get() + 1);
                LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                local_calls.set(local_calls.get() + 1);
                LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert_eq!(result, Some(LastFmPlaybackHandoffKind::NowPlaying));
        assert_eq!(local_calls.get(), 1);

        let clear = LastFmPlaybackHandoff::clear_now_playing(ephemeral_freshness());
        let clear_calls = Cell::new(0);
        let result = clear.try_admit_with(
            &registry,
            &enabled_remote_sources,
            |_| {
                clear_calls.set(clear_calls.get() + 1);
                LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                clear_calls.set(clear_calls.get() + 1);
                LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                clear_calls.set(clear_calls.get() + 1);
                LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert_eq!(result, Some(LastFmPlaybackHandoffKind::ClearNowPlaying));
        assert_eq!(clear_calls.get(), 1);
    }

    #[test]
    fn delayed_now_playing_is_stale_after_its_successor_clear_is_issued() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);

        assert_empty(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("delayed-now-playing"),
            ),
            generation(40),
        ));
        let delayed_now_playing = expect_handoff(
            owner.observe_event(&PlayerEvent::state(generation(40), PlayerState::Playing)),
            LastFmPlaybackHandoffKind::NowPlaying,
        );
        let clear_admission = owner.accept_output_load(
            LastFmAcceptedOutputLoad::ineligible(
                LastFmAcceptedOutputMint::for_test(),
                generation(41),
                output_freshness(),
            ),
            &registry,
            &enabled_remote_sources,
        );
        assert!(!clear_admission.admitted());
        assert!(!clear_admission.stale());
        let successor_clear = expect_handoff(
            clear_admission.into_update(),
            LastFmPlaybackHandoffKind::ClearNowPlaying,
        );

        let stale_calls = Cell::new(0);
        let stale_result = delayed_now_playing.try_admit_with_callbacks_for_test(
            &registry,
            &enabled_remote_sources,
            |_| {
                stale_calls.set(stale_calls.get() + 1);
                LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                stale_calls.set(stale_calls.get() + 1);
                LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                stale_calls.set(stale_calls.get() + 1);
                LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert!(stale_result.is_none());
        assert_eq!(stale_calls.get(), 0);

        let clear_calls = Cell::new(0);
        let clear_result = successor_clear.try_admit_with_callbacks_for_test(
            &registry,
            &enabled_remote_sources,
            |_| {
                clear_calls.set(clear_calls.get() + 1);
                LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                clear_calls.set(clear_calls.get() + 1);
                LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                clear_calls.set(clear_calls.get() + 1);
                LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert_eq!(
            clear_result,
            Some(LastFmPlaybackHandoffKind::ClearNowPlaying)
        );
        assert_eq!(clear_calls.get(), 1);
    }

    #[test]
    fn delayed_clear_is_stale_after_its_successor_now_playing_is_issued() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);

        assert_empty(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("clear-predecessor"),
            ),
            generation(42),
        ));
        let predecessor_now_playing = expect_handoff(
            owner.observe_event(&PlayerEvent::state(generation(42), PlayerState::Playing)),
            LastFmPlaybackHandoffKind::NowPlaying,
        );
        let delayed_clear = expect_handoff(
            owner.accept_load(
                accepted(
                    LastFmPlaybackOccurrenceIdentity::fresh(),
                    local_source("now-playing-successor"),
                ),
                generation(43),
            ),
            LastFmPlaybackHandoffKind::ClearNowPlaying,
        );
        let successor_now_playing = expect_handoff(
            owner.observe_event(&PlayerEvent::state(generation(43), PlayerState::Playing)),
            LastFmPlaybackHandoffKind::NowPlaying,
        );
        drop(predecessor_now_playing);

        let stale_calls = Cell::new(0);
        let stale_result = delayed_clear.try_admit_with_callbacks_for_test(
            &registry,
            &enabled_remote_sources,
            |_| {
                stale_calls.set(stale_calls.get() + 1);
                LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                stale_calls.set(stale_calls.get() + 1);
                LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                stale_calls.set(stale_calls.get() + 1);
                LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert!(stale_result.is_none());
        assert_eq!(stale_calls.get(), 0);

        let successor_calls = Cell::new(0);
        let successor_result = successor_now_playing.try_admit_with_callbacks_for_test(
            &registry,
            &enabled_remote_sources,
            |_| {
                successor_calls.set(successor_calls.get() + 1);
                LastFmPlaybackHandoffKind::NowPlaying
            },
            |_| {
                successor_calls.set(successor_calls.get() + 1);
                LastFmPlaybackHandoffKind::Enqueue
            },
            || {
                successor_calls.set(successor_calls.get() + 1);
                LastFmPlaybackHandoffKind::ClearNowPlaying
            },
        );
        assert_eq!(
            successor_result,
            Some(LastFmPlaybackHandoffKind::NowPlaying)
        );
        assert_eq!(successor_calls.get(), 1);
    }

    #[test]
    fn enqueue_does_not_revoke_a_delayed_now_playing_handoff() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let enabled_remote_sources = HashSet::new();
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);
        let active_generation = generation(44);

        assert_empty(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("enqueue-keeps-lane"),
            ),
            active_generation,
        ));
        let delayed_now_playing = expect_handoff(
            owner.observe_event(&PlayerEvent::state(active_generation, PlayerState::Playing)),
            LastFmPlaybackHandoffKind::NowPlaying,
        );
        assert_empty(owner.observe_event(&PlayerEvent::position(
            active_generation,
            10_000,
            100_000,
        )));
        let enqueue = expect_handoff(
            owner.observe_event(&PlayerEvent::position(active_generation, 60_000, 100_000)),
            LastFmPlaybackHandoffKind::Enqueue,
        );

        let enqueue_result = enqueue.try_admit_with_callbacks_for_test(
            &registry,
            &enabled_remote_sources,
            |_| LastFmPlaybackHandoffKind::NowPlaying,
            |_| LastFmPlaybackHandoffKind::Enqueue,
            || LastFmPlaybackHandoffKind::ClearNowPlaying,
        );
        assert_eq!(enqueue_result, Some(LastFmPlaybackHandoffKind::Enqueue));

        let now_playing_result = delayed_now_playing.try_admit_with_callbacks_for_test(
            &registry,
            &enabled_remote_sources,
            |_| LastFmPlaybackHandoffKind::NowPlaying,
            |_| LastFmPlaybackHandoffKind::Enqueue,
            || LastFmPlaybackHandoffKind::ClearNowPlaying,
        );
        assert_eq!(
            now_playing_result,
            Some(LastFmPlaybackHandoffKind::NowPlaying)
        );
    }

    #[test]
    fn ephemeral_freshness_lane_stays_locked_through_ingress_callback() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        let admitting_registry = registry.clone();
        let enabled_remote_sources = HashSet::new();
        let (clock, _) = TestClock::successful();
        let mut owner = LastFmPlaybackOwner::with_clock(clock);

        assert_empty(owner.accept_load(
            accepted(
                LastFmPlaybackOccurrenceIdentity::fresh(),
                local_source("linearized-now-playing"),
            ),
            generation(45),
        ));
        let handoff = expect_handoff(
            owner.observe_event(&PlayerEvent::state(generation(45), PlayerState::Playing)),
            LastFmPlaybackHandoffKind::NowPlaying,
        );
        let lane_probe = Arc::clone(&owner.ephemeral_handoff_lane);
        let (ingress_observed_tx, ingress_observed_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);

        let admitting = thread::spawn(move || {
            handoff.try_admit_with_callbacks_for_test(
                &admitting_registry,
                &enabled_remote_sources,
                |_| {
                    ingress_observed_tx
                        .send(())
                        .expect("ingress observer remains alive");
                    release_rx.recv().expect("release ephemeral lane claim");
                    LastFmPlaybackHandoffKind::NowPlaying
                },
                |_| panic!("now-playing handoff cannot reach enqueue ingress"),
                || panic!("now-playing handoff cannot reach clear ingress"),
            )
        });

        ingress_observed_rx
            .recv()
            .expect("runtime ingress reached while ephemeral lane is claimed");
        assert!(matches!(
            lane_probe.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        release_tx
            .send(())
            .expect("ephemeral handoff claimant remains alive");

        assert_eq!(
            admitting.join().expect("handoff admission thread"),
            Some(LastFmPlaybackHandoffKind::NowPlaying)
        );
        let lane = lane_probe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(!lane.unclaimed);
        drop(lane);
        drop(owner);
        runtime.block_on(registry.shutdown().wait());
    }
}
