//! Pure progress accounting for durable local playback history.
//!
//! A [`PlaybackHistoryProgress`] belongs to one playback occurrence. It does
//! not read a clock or perform persistence: the playback coordinator feeds it
//! advancing position samples, reports user seeks explicitly, and neutrally
//! re-anchors it for retries or resumes. The single `true` returned by an
//! observation is the point at which the coordinator may persist one counted
//! play.

/// The longest listening time required to count one playback occurrence.
pub const MAX_COUNT_THRESHOLD_MS: u64 = 4 * 60 * 1_000;

/// Progress toward counting one local playback occurrence.
///
/// Consecutive position samples are assumed to represent uninterrupted,
/// forward playback. A caller must use [`Self::observe_seek`] before the next
/// sample after a seek or restart and [`Self::observe_reanchor`] after a retry
/// or resume. Paused and buffering pipelines must not provide advancing
/// samples.
///
/// A positive known duration uses half the duration, rounded up to the next
/// millisecond and capped at four minutes. A missing or zero duration begins
/// with the four-minute fallback and may accept the first positive duration
/// later. Once a positive duration is accepted, its threshold is frozen.
/// Create a new value only for a genuinely new queue occurrence; retrying,
/// resuming, restarting, or seeking the current occurrence must retain this
/// value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackHistoryProgress {
    positive_duration_is_frozen: bool,
    threshold_ms: u64,
    credited_ms: u64,
    position_ms: u64,
    observed_forward_skip: bool,
    counted: bool,
}

impl PlaybackHistoryProgress {
    /// Start accounting for an occurrence at position zero.
    ///
    /// A positive `duration_ms` freezes the threshold immediately. `None` and
    /// zero use the unknown-duration fallback until
    /// [`Self::observe_duration`] receives the first positive duration.
    #[must_use]
    pub fn new(duration_ms: Option<u64>) -> Self {
        let positive_duration = duration_ms.filter(|duration_ms| *duration_ms > 0);
        let threshold_ms = positive_duration.map_or(MAX_COUNT_THRESHOLD_MS, count_threshold_ms);

        Self {
            positive_duration_is_frozen: positive_duration.is_some(),
            threshold_ms,
            credited_ms: 0,
            position_ms: 0,
            observed_forward_skip: false,
            counted: false,
        }
    }

    /// Supply duration discovered by the playback output.
    ///
    /// For an occurrence that began with a missing or zero duration, the
    /// first positive value replaces the four-minute fallback and freezes the
    /// threshold. Zero and all later values are ignored. Returns `true`
    /// exactly once if already accumulated credit reaches the newly reduced
    /// threshold.
    #[must_use]
    pub fn observe_duration(&mut self, duration_ms: u64) -> bool {
        if !self.positive_duration_is_frozen && duration_ms > 0 {
            self.threshold_ms = count_threshold_ms(duration_ms);
            self.credited_ms = self.credited_ms.min(self.threshold_ms);
            self.positive_duration_is_frozen = true;
            return self.latch_threshold();
        }

        false
    }

    /// Observe an uninterrupted playback position.
    ///
    /// Only a strictly advancing sample earns credit. Duplicate and regressed
    /// samples are ignored as stale or implausible; a real backward seek or
    /// restart must first be reported with [`Self::observe_seek`]. A retry or
    /// resume must similarly use [`Self::observe_reanchor`].
    /// Returns `true` exactly once, when accumulated credit first reaches the
    /// effective threshold.
    #[must_use]
    pub fn observe_position(&mut self, position_ms: u64) -> bool {
        if position_ms > self.position_ms {
            let advance_ms = position_ms - self.position_ms;
            self.credited_ms = self
                .credited_ms
                .saturating_add(advance_ms)
                .min(self.threshold_ms);
            self.position_ms = position_ms;
        }

        self.latch_threshold()
    }

    /// Re-anchor after a seek or restart without earning jump credit.
    ///
    /// A forward seek records that content may have been skipped. That
    /// evidence prevents an unknown-duration natural end from taking its
    /// otherwise permitted early-count path. Backward and equal seeks do not
    /// erase credit or prior forward-skip evidence. This operation can never
    /// itself count the occurrence.
    pub fn observe_seek(&mut self, position_ms: u64) {
        if position_ms > self.position_ms {
            self.observed_forward_skip = true;
        }
        self.position_ms = position_ms;
    }

    /// Re-anchor after a retry, resume, or same-occurrence output handoff.
    ///
    /// The position jump earns no credit, but it is not user skip evidence and
    /// therefore does not suppress the unknown-duration natural-end rule.
    /// Existing credit and prior forward-seek evidence remain unchanged.
    pub fn observe_reanchor(&mut self, position_ms: u64) {
        self.position_ms = position_ms;
    }

    /// Observe a natural end of stream.
    ///
    /// Known-duration media receive no tail credit at end of stream and can
    /// count only when the normal threshold has already been observed.
    /// Unknown-duration media may count early at a natural end only when no
    /// forward seek has ever provided evidence of skipped content.
    /// Returns `true` only for the occurrence's first count decision.
    #[must_use]
    pub fn observe_natural_end(&mut self) -> bool {
        if self.positive_duration_is_frozen {
            return self.latch_threshold();
        }

        if !self.counted && !self.observed_forward_skip {
            self.counted = true;
            return true;
        }

        false
    }

    /// The current listening threshold for this occurrence.
    ///
    /// It becomes immutable when the first positive duration is accepted.
    #[must_use]
    pub const fn threshold_ms(&self) -> u64 {
        self.threshold_ms
    }

    /// Credited forward playback, capped at the threshold.
    #[must_use]
    pub const fn credited_ms(&self) -> u64 {
        self.credited_ms
    }

    /// Whether a forward seek has supplied skip evidence.
    #[must_use]
    pub const fn observed_forward_skip(&self) -> bool {
        self.observed_forward_skip
    }

    /// Whether this occurrence has already produced its one count signal.
    #[must_use]
    pub const fn is_counted(&self) -> bool {
        self.counted
    }

    fn latch_threshold(&mut self) -> bool {
        if !self.counted && self.credited_ms >= self.threshold_ms {
            self.counted = true;
            return true;
        }

        false
    }
}

const fn ceil_half(value: u64) -> u64 {
    value / 2 + value % 2
}

const fn count_threshold_ms(duration_ms: u64) -> u64 {
    let half_duration_ms = ceil_half(duration_ms);
    if half_duration_ms < MAX_COUNT_THRESHOLD_MS {
        half_duration_ms
    } else {
        MAX_COUNT_THRESHOLD_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_duration_threshold_is_half_rounded_up() {
        assert_eq!(PlaybackHistoryProgress::new(Some(10)).threshold_ms(), 5);
        assert_eq!(PlaybackHistoryProgress::new(Some(9)).threshold_ms(), 5);
        assert_eq!(PlaybackHistoryProgress::new(Some(1)).threshold_ms(), 1);
    }

    #[test]
    fn known_duration_threshold_is_capped_at_four_minutes() {
        assert_eq!(
            PlaybackHistoryProgress::new(Some(479_999)).threshold_ms(),
            240_000
        );
        assert_eq!(
            PlaybackHistoryProgress::new(Some(480_000)).threshold_ms(),
            240_000
        );
        assert_eq!(
            PlaybackHistoryProgress::new(Some(480_001)).threshold_ms(),
            240_000
        );
        assert_eq!(
            PlaybackHistoryProgress::new(Some(u64::MAX)).threshold_ms(),
            240_000
        );
    }

    #[test]
    fn unknown_duration_uses_four_minute_threshold() {
        assert_eq!(
            PlaybackHistoryProgress::new(None).threshold_ms(),
            MAX_COUNT_THRESHOLD_MS
        );
        assert_eq!(
            PlaybackHistoryProgress::new(Some(0)).threshold_ms(),
            MAX_COUNT_THRESHOLD_MS
        );
    }

    #[test]
    fn initial_positive_duration_freezes_threshold_against_disagreement() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_duration(2_000));
        assert_eq!(progress.threshold_ms(), 10_000);
        assert!(!progress.observe_duration(80_000));
        assert_eq!(progress.threshold_ms(), 10_000);
    }

    #[test]
    fn first_later_positive_duration_replaces_unknown_fallback_and_freezes() {
        let mut progress = PlaybackHistoryProgress::new(None);

        assert!(!progress.observe_duration(0));
        assert_eq!(progress.threshold_ms(), MAX_COUNT_THRESHOLD_MS);
        assert!(!progress.observe_duration(20_001));
        assert_eq!(progress.threshold_ms(), 10_001);
        assert!(!progress.observe_duration(2_000));
        assert_eq!(progress.threshold_ms(), 10_001);
    }

    #[test]
    fn later_positive_duration_can_latch_already_accumulated_credit() {
        let mut progress = PlaybackHistoryProgress::new(Some(0));

        assert!(!progress.observe_position(12_000));
        assert!(progress.observe_duration(20_000));
        assert_eq!(progress.threshold_ms(), 10_000);
        assert_eq!(progress.credited_ms(), 10_000);
        assert!(!progress.observe_duration(1));
        assert!(!progress.observe_position(13_000));
    }

    #[test]
    fn later_positive_duration_does_not_latch_before_reduced_threshold() {
        let mut progress = PlaybackHistoryProgress::new(None);

        assert!(!progress.observe_position(9_999));
        assert!(!progress.observe_duration(20_000));
        assert_eq!(progress.threshold_ms(), 10_000);
        assert!(progress.observe_position(10_000));
    }

    #[test]
    fn short_track_counts_at_exact_threshold_not_before() {
        let mut progress = PlaybackHistoryProgress::new(Some(3_001));
        assert_eq!(progress.threshold_ms(), 1_501);

        assert!(!progress.observe_position(1_500));
        assert_eq!(progress.credited_ms(), 1_500);
        assert!(progress.observe_position(1_501));
        assert_eq!(progress.credited_ms(), 1_501);
        assert!(progress.is_counted());
    }

    #[test]
    fn multiple_samples_accumulate_observed_forward_playback() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_position(2_000));
        assert!(!progress.observe_position(6_000));
        assert_eq!(progress.credited_ms(), 6_000);
        assert!(progress.observe_position(10_000));
    }

    #[test]
    fn duplicate_samples_provide_no_credit() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_position(4_000));
        assert!(!progress.observe_position(4_000));
        assert!(!progress.observe_position(4_000));
        assert_eq!(progress.credited_ms(), 4_000);
    }

    #[test]
    fn regressed_sample_is_ignored_without_manufacturing_replay_credit() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_position(7_000));
        assert!(!progress.observe_position(2_000));
        assert_eq!(progress.credited_ms(), 7_000);
        assert!(!progress.observe_position(8_000));
        assert_eq!(progress.credited_ms(), 8_000);
        assert!(progress.observe_position(10_000));
    }

    #[test]
    fn forward_seek_provides_no_credit_and_records_skip_evidence() {
        let mut progress = PlaybackHistoryProgress::new(Some(30_000));

        assert!(!progress.observe_position(4_000));
        progress.observe_seek(13_000);
        assert_eq!(progress.credited_ms(), 4_000);
        assert!(progress.observed_forward_skip());

        assert!(!progress.observe_position(14_000));
        assert_eq!(progress.credited_ms(), 5_000);
    }

    #[test]
    fn backward_seek_does_not_erase_credit_or_create_skip_evidence() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_position(7_000));
        progress.observe_seek(2_000);
        assert_eq!(progress.credited_ms(), 7_000);
        assert!(!progress.observed_forward_skip());

        assert!(progress.observe_position(5_000));
        assert_eq!(progress.credited_ms(), 10_000);
    }

    #[test]
    fn restart_at_zero_preserves_credit_for_same_occurrence() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_position(6_000));
        progress.observe_seek(0);
        assert!(!progress.observe_position(3_999));
        assert!(progress.observe_position(4_000));
        assert_eq!(progress.credited_ms(), 10_000);
    }

    #[test]
    fn retry_and_resume_reanchor_without_resetting_occurrence() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_position(4_000));
        // A retry resumes from the last reported position.
        progress.observe_reanchor(4_000);
        assert!(!progress.observe_position(7_000));
        // Another retry restarts decoding from an earlier point.
        progress.observe_reanchor(5_000);
        assert!(progress.observe_position(8_000));
        assert_eq!(progress.credited_ms(), 10_000);
    }

    #[test]
    fn forward_retry_reanchor_is_not_user_skip_evidence() {
        let mut progress = PlaybackHistoryProgress::new(None);

        assert!(!progress.observe_position(10_000));
        progress.observe_reanchor(12_000);

        assert_eq!(progress.credited_ms(), 10_000);
        assert!(!progress.observed_forward_skip());
        assert!(progress.observe_natural_end());
    }

    #[test]
    fn counted_signal_is_latched_exactly_once() {
        let mut progress = PlaybackHistoryProgress::new(Some(10_000));

        assert!(progress.observe_position(5_000));
        assert!(!progress.observe_position(6_000));
        assert!(!progress.observe_position(10_000));
        assert!(!progress.observe_natural_end());
        assert!(progress.is_counted());
    }

    #[test]
    fn repeat_one_replay_uses_a_fresh_count_latch() {
        let mut first_occurrence = PlaybackHistoryProgress::new(Some(10_000));
        assert!(first_occurrence.observe_position(5_000));
        assert!(!first_occurrence.observe_natural_end());

        let mut replay_occurrence = PlaybackHistoryProgress::new(Some(10_000));
        assert!(!replay_occurrence.is_counted());
        assert!(replay_occurrence.observe_position(5_000));
    }

    #[test]
    fn credit_is_capped_at_threshold_even_for_large_samples() {
        let mut progress = PlaybackHistoryProgress::new(None);

        assert!(progress.observe_position(u64::MAX));
        assert_eq!(progress.credited_ms(), MAX_COUNT_THRESHOLD_MS);
    }

    #[test]
    fn known_duration_natural_end_does_not_credit_unobserved_tail() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_position(9_999));
        assert!(!progress.observe_natural_end());
        assert!(!progress.is_counted());
    }

    #[test]
    fn known_duration_natural_end_cannot_hide_a_forward_skip() {
        let mut progress = PlaybackHistoryProgress::new(Some(20_000));

        assert!(!progress.observe_position(4_000));
        progress.observe_seek(19_000);
        assert!(!progress.observe_natural_end());
        assert!(!progress.is_counted());
    }

    #[test]
    fn zero_duration_is_unknown_and_can_count_at_unskipped_natural_end() {
        let mut progress = PlaybackHistoryProgress::new(Some(0));

        assert!(progress.observe_natural_end());
        assert_eq!(progress.credited_ms(), 0);
        assert!(!progress.observe_natural_end());
    }

    #[test]
    fn unknown_duration_natural_end_counts_without_forward_skip() {
        let mut progress = PlaybackHistoryProgress::new(None);

        assert!(!progress.observe_position(12_000));
        assert!(progress.observe_natural_end());
        assert!(!progress.observe_natural_end());
    }

    #[test]
    fn unknown_duration_natural_end_allows_backward_seek() {
        let mut progress = PlaybackHistoryProgress::new(None);

        assert!(!progress.observe_position(12_000));
        progress.observe_seek(1_000);
        assert!(!progress.observed_forward_skip());
        assert!(progress.observe_natural_end());
    }

    #[test]
    fn unknown_duration_natural_end_rejects_any_prior_forward_skip() {
        let mut progress = PlaybackHistoryProgress::new(None);

        assert!(!progress.observe_position(12_000));
        progress.observe_seek(20_000);
        progress.observe_seek(0);
        assert!(!progress.observe_position(30_000));
        assert!(progress.observed_forward_skip());
        assert!(!progress.observe_natural_end());
    }

    #[test]
    fn unknown_duration_can_reach_normal_threshold_despite_forward_skip() {
        let mut progress = PlaybackHistoryProgress::new(None);

        assert!(!progress.observe_position(120_000));
        progress.observe_seek(180_000);
        assert!(!progress.observe_position(239_999));
        progress.observe_seek(0);
        assert!(progress.observe_position(60_001));
        assert_eq!(progress.credited_ms(), MAX_COUNT_THRESHOLD_MS);
        assert!(!progress.observe_natural_end());
    }
}
