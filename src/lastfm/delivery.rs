//! Pure Last.fm queue-delivery policy and testable runtime boundaries.
//!
//! The lifecycle owner remains responsible for serializing durable queue
//! mutations and proving that a network result still belongs to its active
//! delivery generation. This module converts already-validated queue rows
//! into protocol values, classifies every protocol result without retaining
//! private response data, and computes the durable retry deadline.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use uuid::{Variant, Version};

use crate::db::entities::lastfm_scrobble::{
    StoredLastFmScrobble, MAX_LASTFM_ATTEMPT_COUNT, MAX_LASTFM_METADATA_BYTES,
    MAX_LASTFM_RETRY_AT_MS, MAX_LASTFM_STARTED_AT_SECS,
};

use super::client::{
    LastFmClient, LastFmClientError, LastFmTrack, Scrobble, ScrobbleBatchResult, SubmissionResult,
    MAX_SCROBBLES_PER_BATCH,
};
use super::credentials::StoredSession;
use super::storage::LastFmBatchReceipt;

const INITIAL_RETRY_DELAY_MS: i64 = 30_000;
const MAXIMUM_RETRY_DELAY_MS: i64 = 60 * 60 * 1_000;
// 30 seconds shifted seven times is 64 minutes, so every later attempt is
// already at the one-hour cap.
const FIRST_CAPPED_RETRY_ATTEMPT: u32 = 7;

/// Network boundary used by the single Last.fm delivery worker.
///
/// Tests implement this trait with a scripted, gateable transport. Production
/// delegates to the bounded, exact-origin Last.fm client.
#[async_trait::async_trait]
pub trait LastFmTransport: Send + Sync {
    async fn update_now_playing(
        &self,
        session: &StoredSession,
        track: &LastFmTrack,
    ) -> Result<SubmissionResult, LastFmClientError>;

    async fn submit_scrobbles(
        &self,
        session: &StoredSession,
        scrobbles: &[Scrobble],
    ) -> Result<ScrobbleBatchResult, LastFmClientError>;
}

#[async_trait::async_trait]
impl LastFmTransport for LastFmClient {
    async fn update_now_playing(
        &self,
        session: &StoredSession,
        track: &LastFmTrack,
    ) -> Result<SubmissionResult, LastFmClientError> {
        Self::update_now_playing(self, session, track).await
    }

    async fn submit_scrobbles(
        &self,
        session: &StoredSession,
        scrobbles: &[Scrobble],
    ) -> Result<ScrobbleBatchResult, LastFmClientError> {
        self.scrobble(session, scrobbles).await
    }
}

/// Absolute wall-clock boundary used for persisted retry timestamps.
///
/// `wait_until_unix_ms` is cancellation-safe: dropping its future abandons
/// only the in-memory wait and never changes durable retry state.
#[async_trait::async_trait]
pub trait LastFmClock: Send + Sync {
    fn now_unix_ms(&self) -> Result<i64, LastFmDeliveryPrimitiveError>;

    async fn wait_until_unix_ms(
        &self,
        deadline_unix_ms: i64,
    ) -> Result<(), LastFmDeliveryPrimitiveError>;
}

/// Production clock for durable Unix-millisecond retry deadlines.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLastFmClock;

#[async_trait::async_trait]
impl LastFmClock for SystemLastFmClock {
    fn now_unix_ms(&self) -> Result<i64, LastFmDeliveryPrimitiveError> {
        system_time_unix_ms(SystemTime::now())
    }

    async fn wait_until_unix_ms(
        &self,
        deadline_unix_ms: i64,
    ) -> Result<(), LastFmDeliveryPrimitiveError> {
        validate_retry_timestamp(deadline_unix_ms)?;
        let now_unix_ms = self.now_unix_ms()?;
        let remaining_ms = deadline_unix_ms.saturating_sub(now_unix_ms);
        if remaining_ms > 0 {
            let remaining_ms = u64::try_from(remaining_ms)
                .map_err(|_| LastFmDeliveryPrimitiveError::ClockOutOfRange)?;
            tokio::time::sleep(Duration::from_millis(remaining_ms)).await;
        }
        Ok(())
    }
}

/// Content-free failure at a pure delivery boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmDeliveryPrimitiveError {
    #[error("Last.fm queue delivery data is invalid")]
    InvalidStoredRow,
    #[error("Last.fm retry attempt state is invalid")]
    AttemptCountOutOfRange,
    #[error("Last.fm retry clock is out of range")]
    ClockOutOfRange,
}

/// Durable action selected from one complete transport result.
///
/// The variants carry no provider message, submitted metadata, account data,
/// or credentials and are therefore safe to publish in runtime status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmDeliveryDisposition {
    /// Delete the exact receipt after a valid success or recognized terminal
    /// provider rejection.
    SettleTerminal,
    /// Retain the exact receipt and durably apply exponential backoff.
    RetryTransient,
    /// Retain the queue and wait for same-account reauthorization.
    PauseForReauthentication,
    /// Retain the queue and require a compatibility recovery action.
    QuarantineCompatibility,
    /// Retain the queue and stop automatic work because the build, client, or
    /// caller violated a local capability/invariant boundary.
    PauseCapabilityOrInternal,
}

/// Classify one delivery result, including the response cardinality proof.
///
/// [`LastFmClient`] already validates response ordering and cardinality. The
/// explicit check here keeps scripted transports and future implementations
/// from authorizing deletion with an incoherent success result.
#[must_use]
pub fn delivery_disposition(
    receipt: &LastFmBatchReceipt,
    result: &Result<ScrobbleBatchResult, LastFmClientError>,
) -> LastFmDeliveryDisposition {
    let expected_item_count = receipt.len();
    if !(1..=MAX_SCROBBLES_PER_BATCH).contains(&expected_item_count) {
        return LastFmDeliveryDisposition::PauseCapabilityOrInternal;
    }

    match result {
        Ok(batch) if batch.items.len() == expected_item_count => {
            // Keep this match explicit rather than treating every present or
            // future response variant as terminal by default. Extending
            // `SubmissionResult` must force a reviewed delivery-policy choice.
            for item in &batch.items {
                match item {
                    SubmissionResult::Accepted { .. } | SubmissionResult::Ignored { .. } => {}
                }
            }
            LastFmDeliveryDisposition::SettleTerminal
        }
        Ok(_) => LastFmDeliveryDisposition::QuarantineCompatibility,
        Err(error) => disposition_for_client_error(*error),
    }
}

/// Exhaustively map the closed client error set to durable delivery policy.
#[must_use]
pub const fn disposition_for_client_error(error: LastFmClientError) -> LastFmDeliveryDisposition {
    // Keep the client's closed retry classification authoritative. In
    // particular, an ordinary HTTP failure or an oversized response is not a
    // network failure and must not cause an unbounded resubmission loop.
    if error.is_retryable() {
        return LastFmDeliveryDisposition::RetryTransient;
    }

    match error {
        LastFmClientError::ReauthenticationRequired => {
            LastFmDeliveryDisposition::PauseForReauthentication
        }
        LastFmClientError::ServiceRejected { .. } => LastFmDeliveryDisposition::SettleTerminal,
        // None of these responses proves a trustworthy terminal mapping for
        // the submitted rows. Retain the exact receipt and require explicit
        // compatibility recovery instead of deleting or retrying it.
        LastFmClientError::HttpStatus
        | LastFmClientError::BodyLimit
        | LastFmClientError::InvalidResponse => LastFmDeliveryDisposition::QuarantineCompatibility,
        LastFmClientError::AppCredentialsUnavailable
        | LastFmClientError::ClientConstruction
        | LastFmClientError::InvalidInput => LastFmDeliveryDisposition::PauseCapabilityOrInternal,
        // Exhaustive only because a match guard does not refine enum variants.
        // The authoritative `is_retryable` branch above handles these values.
        LastFmClientError::Timeout
        | LastFmClientError::Transport
        | LastFmClientError::ServiceUnavailable
        | LastFmClientError::RateLimited => LastFmDeliveryDisposition::PauseCapabilityOrInternal,
    }
}

/// Revalidate and convert one private durable row into a Last.fm request item.
///
/// Queue receipts are opaque and contain validated rows, but this boundary is
/// intentionally defensive because the worker is the final point before
/// private metadata reaches the network.
pub fn scrobble_from_stored(
    row: &StoredLastFmScrobble,
) -> Result<Scrobble, LastFmDeliveryPrimitiveError> {
    if row.id <= 0
        || row.occurrence_id.get_variant() != Variant::RFC4122
        || row.occurrence_id.get_version() != Some(Version::Random)
        || !valid_required_text(&row.artist)
        || !valid_required_text(&row.track_title)
        || !valid_optional_text(row.album.as_deref())
        || !valid_optional_text(row.album_artist.as_deref())
        || row.track_number.is_some_and(|number| number <= 0)
        || row.duration_secs <= 30
        || !(1..=MAX_LASTFM_STARTED_AT_SECS).contains(&row.started_at_unix_secs)
        || !(0..=MAX_LASTFM_ATTEMPT_COUNT).contains(&row.attempt_count)
        || !(0..=MAX_LASTFM_RETRY_AT_MS).contains(&row.next_attempt_at_ms)
    {
        return Err(LastFmDeliveryPrimitiveError::InvalidStoredRow);
    }

    let track_number = row
        .track_number
        .map(u32::try_from)
        .transpose()
        .map_err(|_| LastFmDeliveryPrimitiveError::InvalidStoredRow)?;
    let duration_seconds = u32::try_from(row.duration_secs)
        .map_err(|_| LastFmDeliveryPrimitiveError::InvalidStoredRow)?;
    let started_at_unix_seconds = u64::try_from(row.started_at_unix_secs)
        .map_err(|_| LastFmDeliveryPrimitiveError::InvalidStoredRow)?;

    Ok(Scrobble {
        track: LastFmTrack {
            artist: row.artist.clone(),
            title: row.track_title.clone(),
            album: row.album.clone(),
            album_artist: row.album_artist.clone(),
            track_number,
            duration_seconds,
        },
        started_at_unix_seconds,
    })
}

/// Convert an opaque FIFO receipt to one ordered bounded protocol batch.
pub fn scrobbles_from_receipt(
    receipt: &LastFmBatchReceipt,
) -> Result<Vec<Scrobble>, LastFmDeliveryPrimitiveError> {
    if receipt.rows().is_empty() || receipt.rows().len() > MAX_SCROBBLES_PER_BATCH {
        return Err(LastFmDeliveryPrimitiveError::InvalidStoredRow);
    }
    receipt.rows().iter().map(scrobble_from_stored).collect()
}

/// Deterministic delay for the receipt's largest pre-reschedule attempt count.
///
/// Attempt zero is the first transient failure and waits 30 seconds. Attempts
/// one through six double that delay; attempt seven and later wait one hour.
pub fn retry_delay_ms(attempt_count: i32) -> Result<i64, LastFmDeliveryPrimitiveError> {
    if !(0..=MAX_LASTFM_ATTEMPT_COUNT).contains(&attempt_count) {
        return Err(LastFmDeliveryPrimitiveError::AttemptCountOutOfRange);
    }
    let exponent = u32::try_from(attempt_count)
        .map_err(|_| LastFmDeliveryPrimitiveError::AttemptCountOutOfRange)?
        .min(FIRST_CAPPED_RETRY_ATTEMPT);
    let exponential = INITIAL_RETRY_DELAY_MS
        .checked_shl(exponent)
        .ok_or(LastFmDeliveryPrimitiveError::AttemptCountOutOfRange)?;
    Ok(exponential.min(MAXIMUM_RETRY_DELAY_MS))
}

/// Compute the bounded absolute retry timestamp for an opaque batch receipt.
pub fn next_retry_at_ms(
    now_unix_ms: i64,
    receipt: &LastFmBatchReceipt,
) -> Result<i64, LastFmDeliveryPrimitiveError> {
    next_retry_at_for_attempt(now_unix_ms, receipt.maximum_attempt_count())
}

fn next_retry_at_for_attempt(
    now_unix_ms: i64,
    attempt_count: i32,
) -> Result<i64, LastFmDeliveryPrimitiveError> {
    validate_retry_timestamp(now_unix_ms)?;
    let delay_ms = retry_delay_ms(attempt_count)?;
    Ok(now_unix_ms
        .checked_add(delay_ms)
        .unwrap_or(MAX_LASTFM_RETRY_AT_MS)
        .min(MAX_LASTFM_RETRY_AT_MS))
}

fn system_time_unix_ms(now: SystemTime) -> Result<i64, LastFmDeliveryPrimitiveError> {
    let elapsed = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| LastFmDeliveryPrimitiveError::ClockOutOfRange)?;
    let milliseconds = i64::try_from(elapsed.as_millis())
        .map_err(|_| LastFmDeliveryPrimitiveError::ClockOutOfRange)?;
    validate_retry_timestamp(milliseconds)?;
    Ok(milliseconds)
}

fn validate_retry_timestamp(value: i64) -> Result<(), LastFmDeliveryPrimitiveError> {
    if (0..=MAX_LASTFM_RETRY_AT_MS).contains(&value) {
        Ok(())
    } else {
        Err(LastFmDeliveryPrimitiveError::ClockOutOfRange)
    }
}

fn valid_required_text(value: &str) -> bool {
    value.len() <= MAX_LASTFM_METADATA_BYTES
        && value.chars().any(|character| !character.is_whitespace())
        && !value.chars().any(char::is_control)
}

fn valid_optional_text(value: Option<&str>) -> bool {
    value.is_none_or(valid_required_text)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use uuid::Uuid;

    use super::*;
    use crate::db::migration::Migrator;
    use crate::lastfm::client::{IgnoredReason, SubmissionResult};
    use crate::lastfm::credentials::{LastFmAccountBinding, ProtectedString};
    use crate::lastfm::storage::{self, LastFmBatchAvailability, PendingLastFmScrobble};

    const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";

    async fn database() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        database
    }

    fn account_binding() -> LastFmAccountBinding {
        StoredSession::new("listener", ProtectedString::new(SESSION_KEY))
            .unwrap()
            .account_binding()
    }

    fn pending_scrobble(
        account_binding: LastFmAccountBinding,
        index: usize,
    ) -> PendingLastFmScrobble {
        let number = i32::try_from(index + 1).unwrap();
        PendingLastFmScrobble::try_new(
            Uuid::new_v4(),
            account_binding,
            format!("Artist {index}"),
            format!("Track {index}"),
            Some(format!("Album {index}")),
            Some(format!("Album Artist {index}")),
            Some(number),
            241 + number,
            1_700_000_000 + i64::from(number),
        )
        .unwrap()
    }

    async fn ready_receipt(
        database: &DatabaseConnection,
        account_binding: LastFmAccountBinding,
        now_ms: i64,
        limit: usize,
    ) -> LastFmBatchReceipt {
        match storage::batch_availability(database, account_binding, now_ms, limit)
            .await
            .unwrap()
        {
            LastFmBatchAvailability::Ready(receipt) => receipt,
            availability => panic!("expected ready receipt, got {availability:?}"),
        }
    }

    fn stored_row() -> StoredLastFmScrobble {
        StoredLastFmScrobble {
            id: 1,
            occurrence_id: Uuid::new_v4(),
            account_binding: [0xa5; 32],
            artist: "Artist".to_owned(),
            track_title: "Track".to_owned(),
            album: Some("Album".to_owned()),
            album_artist: Some("Album Artist".to_owned()),
            track_number: Some(7),
            duration_secs: 241,
            started_at_unix_secs: 1_700_000_000,
            attempt_count: 0,
            next_attempt_at_ms: 0,
        }
    }

    fn accepted_batch(count: usize) -> ScrobbleBatchResult {
        ScrobbleBatchResult {
            items: vec![SubmissionResult::Accepted { corrected: false }; count],
        }
    }

    #[test]
    fn concrete_client_implements_the_transport_boundary() {
        fn assert_transport<T: LastFmTransport>() {}
        assert_transport::<LastFmClient>();
    }

    #[test]
    fn stored_conversion_preserves_exact_structured_metadata() {
        let row = stored_row();
        let converted = scrobble_from_stored(&row).expect("canonical row converts");
        assert_eq!(converted.track.artist, "Artist");
        assert_eq!(converted.track.title, "Track");
        assert_eq!(converted.track.album.as_deref(), Some("Album"));
        assert_eq!(
            converted.track.album_artist.as_deref(),
            Some("Album Artist")
        );
        assert_eq!(converted.track.track_number, Some(7));
        assert_eq!(converted.track.duration_seconds, 241);
        assert_eq!(converted.started_at_unix_seconds, 1_700_000_000);
    }

    #[test]
    fn stored_conversion_revalidates_every_mutable_payload_class() {
        let mut cases = Vec::new();
        let mut invalid = stored_row();
        invalid.id = 0;
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.occurrence_id = Uuid::nil();
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.artist = " \t ".to_owned();
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.track_title = "line\nbreak".to_owned();
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.album = Some(String::new());
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.album_artist = Some("x".repeat(MAX_LASTFM_METADATA_BYTES + 1));
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.track_number = Some(0);
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.duration_secs = 30;
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.started_at_unix_secs = 0;
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.attempt_count = -1;
        cases.push(invalid);
        let mut invalid = stored_row();
        invalid.next_attempt_at_ms = -1;
        cases.push(invalid);

        for invalid in cases {
            assert_eq!(
                scrobble_from_stored(&invalid),
                Err(LastFmDeliveryPrimitiveError::InvalidStoredRow)
            );
        }
    }

    #[tokio::test]
    async fn result_mapping_is_exhaustive_and_receipt_cardinality_safe() {
        let database = database().await;
        let account_binding = account_binding();
        for index in 0..2 {
            storage::enqueue(&database, &pending_scrobble(account_binding, index))
                .await
                .unwrap();
        }
        let receipt = ready_receipt(&database, account_binding, 0, 50).await;

        assert_eq!(
            delivery_disposition(&receipt, &Ok(accepted_batch(2))),
            LastFmDeliveryDisposition::SettleTerminal
        );
        assert_eq!(
            delivery_disposition(
                &receipt,
                &Ok(ScrobbleBatchResult {
                    items: vec![
                        SubmissionResult::Accepted { corrected: true },
                        SubmissionResult::Ignored {
                            reason: IgnoredReason::Other(65_535),
                        },
                    ],
                })
            ),
            LastFmDeliveryDisposition::SettleTerminal
        );
        assert_eq!(
            delivery_disposition(&receipt, &Ok(accepted_batch(1))),
            LastFmDeliveryDisposition::QuarantineCompatibility
        );

        for error in [
            LastFmClientError::Timeout,
            LastFmClientError::Transport,
            LastFmClientError::ServiceUnavailable,
            LastFmClientError::RateLimited,
        ] {
            assert_eq!(
                disposition_for_client_error(error),
                LastFmDeliveryDisposition::RetryTransient
            );
        }
        assert_eq!(
            disposition_for_client_error(LastFmClientError::ReauthenticationRequired),
            LastFmDeliveryDisposition::PauseForReauthentication
        );
        assert_eq!(
            disposition_for_client_error(LastFmClientError::ServiceRejected { code: 13 }),
            LastFmDeliveryDisposition::SettleTerminal
        );
        for error in [
            LastFmClientError::HttpStatus,
            LastFmClientError::BodyLimit,
            LastFmClientError::InvalidResponse,
        ] {
            assert_eq!(
                disposition_for_client_error(error),
                LastFmDeliveryDisposition::QuarantineCompatibility
            );
        }
        for error in [
            LastFmClientError::AppCredentialsUnavailable,
            LastFmClientError::ClientConstruction,
            LastFmClientError::InvalidInput,
        ] {
            assert_eq!(
                disposition_for_client_error(error),
                LastFmDeliveryDisposition::PauseCapabilityOrInternal
            );
        }

        for error in [
            LastFmClientError::AppCredentialsUnavailable,
            LastFmClientError::InvalidInput,
            LastFmClientError::ClientConstruction,
            LastFmClientError::Timeout,
            LastFmClientError::Transport,
            LastFmClientError::HttpStatus,
            LastFmClientError::ServiceUnavailable,
            LastFmClientError::RateLimited,
            LastFmClientError::ReauthenticationRequired,
            LastFmClientError::ServiceRejected { code: 13 },
            LastFmClientError::BodyLimit,
            LastFmClientError::InvalidResponse,
        ] {
            assert_eq!(
                matches!(
                    disposition_for_client_error(error),
                    LastFmDeliveryDisposition::RetryTransient
                ),
                error.is_retryable(),
                "durable retry classification drifted for {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn real_receipt_conversion_preserves_fifo_and_exact_fifty_row_boundary() {
        let database = database().await;
        let account_binding = account_binding();
        for index in 0..=MAX_SCROBBLES_PER_BATCH {
            storage::enqueue(&database, &pending_scrobble(account_binding, index))
                .await
                .unwrap();
        }

        let receipt = ready_receipt(&database, account_binding, 0, MAX_SCROBBLES_PER_BATCH).await;
        let converted = scrobbles_from_receipt(&receipt).unwrap();
        assert_eq!(receipt.len(), MAX_SCROBBLES_PER_BATCH);
        assert_eq!(converted.len(), MAX_SCROBBLES_PER_BATCH);
        for (index, scrobble) in converted.iter().enumerate() {
            let number = u32::try_from(index + 1).unwrap();
            assert_eq!(scrobble.track.artist, format!("Artist {index}"));
            assert_eq!(scrobble.track.title, format!("Track {index}"));
            assert_eq!(
                scrobble.track.album.as_deref(),
                Some(format!("Album {index}").as_str())
            );
            assert_eq!(
                scrobble.track.album_artist.as_deref(),
                Some(format!("Album Artist {index}").as_str())
            );
            assert_eq!(scrobble.track.track_number, Some(number));
            assert_eq!(scrobble.track.duration_seconds, 241 + number);
            assert_eq!(
                scrobble.started_at_unix_seconds,
                1_700_000_000 + u64::from(number)
            );
        }
        assert_eq!(storage::queue_len(&database).await.unwrap(), 51);
    }

    #[tokio::test]
    async fn mixed_attempt_receipt_uses_largest_durable_backoff() {
        let database = database().await;
        let account_binding = account_binding();
        storage::enqueue(&database, &pending_scrobble(account_binding, 0))
            .await
            .unwrap();
        let first = ready_receipt(&database, account_binding, 0, 50).await;
        storage::reschedule_batch(&database, &first, 500)
            .await
            .unwrap();
        storage::enqueue(&database, &pending_scrobble(account_binding, 1))
            .await
            .unwrap();

        let mixed = ready_receipt(&database, account_binding, 500, 50).await;
        assert_eq!(
            mixed
                .rows()
                .iter()
                .map(|row| row.attempt_count)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert_eq!(mixed.maximum_attempt_count(), 1);
        assert_eq!(next_retry_at_ms(1_000, &mixed).unwrap(), 61_000);
    }

    #[test]
    fn retry_schedule_doubles_from_thirty_seconds_and_caps_at_one_hour() {
        let expected_seconds = [30, 60, 120, 240, 480, 960, 1_920, 3_600];
        for (attempt, expected_seconds) in expected_seconds.into_iter().enumerate() {
            assert_eq!(
                retry_delay_ms(i32::try_from(attempt).unwrap()).unwrap(),
                expected_seconds * 1_000
            );
        }
        assert_eq!(
            retry_delay_ms(MAX_LASTFM_ATTEMPT_COUNT).unwrap(),
            MAXIMUM_RETRY_DELAY_MS
        );
        for invalid in [-1, MAX_LASTFM_ATTEMPT_COUNT + 1] {
            assert_eq!(
                retry_delay_ms(invalid),
                Err(LastFmDeliveryPrimitiveError::AttemptCountOutOfRange)
            );
        }
    }

    #[test]
    fn retry_timestamp_is_checked_and_clamped_to_the_storage_boundary() {
        assert_eq!(next_retry_at_for_attempt(1_000, 0).unwrap(), 31_000);
        assert_eq!(
            next_retry_at_for_attempt(MAX_LASTFM_RETRY_AT_MS - 1, 0).unwrap(),
            MAX_LASTFM_RETRY_AT_MS
        );
        assert_eq!(
            next_retry_at_for_attempt(MAX_LASTFM_RETRY_AT_MS, 31).unwrap(),
            MAX_LASTFM_RETRY_AT_MS
        );
        for invalid_now in [-1, MAX_LASTFM_RETRY_AT_MS + 1] {
            assert_eq!(
                next_retry_at_for_attempt(invalid_now, 0),
                Err(LastFmDeliveryPrimitiveError::ClockOutOfRange)
            );
        }
    }

    struct ImmediateClock {
        now: i64,
        waited: Mutex<Vec<i64>>,
    }

    #[async_trait::async_trait]
    impl LastFmClock for ImmediateClock {
        fn now_unix_ms(&self) -> Result<i64, LastFmDeliveryPrimitiveError> {
            Ok(self.now)
        }

        async fn wait_until_unix_ms(
            &self,
            deadline_unix_ms: i64,
        ) -> Result<(), LastFmDeliveryPrimitiveError> {
            validate_retry_timestamp(deadline_unix_ms)?;
            self.waited
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(deadline_unix_ms);
            Ok(())
        }
    }

    #[tokio::test]
    async fn clock_boundary_is_injectable_without_network_or_sleep() {
        let clock = ImmediateClock {
            now: 123,
            waited: Mutex::new(Vec::new()),
        };
        assert_eq!(clock.now_unix_ms().unwrap(), 123);
        clock.wait_until_unix_ms(456).await.unwrap();
        assert_eq!(
            *clock
                .waited
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![456]
        );
    }

    #[test]
    fn primitive_diagnostics_never_include_private_payloads() {
        let private = "private-title-never-print";
        let row = StoredLastFmScrobble {
            track_title: private.to_owned(),
            ..stored_row()
        };
        let diagnostics = format!(
            "{:?} {:?}",
            scrobble_from_stored(&row).unwrap(),
            LastFmDeliveryPrimitiveError::InvalidStoredRow
        );
        assert!(!diagnostics.contains(private));
    }
}
