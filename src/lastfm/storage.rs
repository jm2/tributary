//! Durable, account-isolated Last.fm scrobble queue.
//!
//! Enqueue is a single SQLite write statement that applies the global bound
//! before insertion and uses the opaque occurrence UUID as an idempotency key.
//! Delivery reads only the oldest account prefix: a delayed first row blocks
//! newer rows so cached scrobbles are always submitted before later plays.
//! One lifecycle-owned worker must remain the sole batch consumer. Storage does
//! not lease rows across processes; instead, its opaque receipt makes every
//! terminal or retry mutation an exact, all-or-none compare-and-swap against
//! the batch that worker read.

use std::fmt;

use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use uuid::{Uuid, Variant, Version};

use crate::db::entities::lastfm_scrobble::{
    self, StoredLastFmScrobble, StoredMetadataText, MAX_LASTFM_ATTEMPT_COUNT,
    MAX_LASTFM_METADATA_BYTES, MAX_LASTFM_RETRY_AT_MS, MAX_LASTFM_STARTED_AT_SECS,
};

use super::credentials::LastFmAccountBinding;

/// Hard global bound on pending listening records.
pub const MAX_LASTFM_QUEUE_ROWS: u64 = 10_000;
/// Last.fm's protocol batch ceiling.
pub const MAX_LASTFM_BATCH_ROWS: usize = 50;

/// Account-independent, fully validated playback evidence awaiting runtime
/// admission.
///
/// Playback producers can construct this value without receiving the
/// vault-derived account binding. Only the Last.fm runtime may bind it to the
/// exact current account before durable queue admission.
#[derive(Clone, PartialEq, Eq)]
pub struct UnboundLastFmScrobble {
    payload: ValidatedLastFmScrobble,
}

impl UnboundLastFmScrobble {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        occurrence_id: Uuid,
        artist: String,
        track_title: String,
        album: Option<String>,
        album_artist: Option<String>,
        track_number: Option<i32>,
        duration_secs: i32,
        started_at_unix_secs: i64,
    ) -> Result<Self, LastFmQueueError> {
        ValidatedLastFmScrobble::try_new(
            occurrence_id,
            artist,
            track_title,
            album,
            album_artist,
            track_number,
            duration_secs,
            started_at_unix_secs,
        )
        .map(|payload| Self { payload })
    }

    #[must_use]
    pub const fn occurrence_id(&self) -> Uuid {
        self.payload.occurrence_id
    }

    #[must_use]
    pub fn artist(&self) -> &str {
        &self.payload.artist
    }

    #[must_use]
    pub fn track_title(&self) -> &str {
        &self.payload.track_title
    }

    #[must_use]
    pub fn album(&self) -> Option<&str> {
        self.payload.album.as_deref()
    }

    #[must_use]
    pub fn album_artist(&self) -> Option<&str> {
        self.payload.album_artist.as_deref()
    }

    #[must_use]
    pub const fn track_number(&self) -> Option<i32> {
        self.payload.track_number
    }

    #[must_use]
    pub const fn duration_secs(&self) -> i32 {
        self.payload.duration_secs
    }

    #[must_use]
    pub const fn started_at_unix_secs(&self) -> i64 {
        self.payload.started_at_unix_secs
    }

    /// Attach the exact current vault-derived account authority.
    ///
    /// Keeping this inside `crate::lastfm` prevents playback producers from
    /// selecting or retaining account identity themselves.
    pub(in crate::lastfm) fn bind(
        self,
        account_binding: LastFmAccountBinding,
    ) -> PendingLastFmScrobble {
        PendingLastFmScrobble {
            account_binding,
            payload: self.payload,
        }
    }
}

impl fmt::Debug for UnboundLastFmScrobble {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.payload
            .fmt_redacted("UnboundLastFmScrobble", formatter)
    }
}

/// Canonical account-bound payload admitted before network delivery.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingLastFmScrobble {
    account_binding: LastFmAccountBinding,
    payload: ValidatedLastFmScrobble,
}

#[derive(Clone, PartialEq, Eq)]
struct ValidatedLastFmScrobble {
    occurrence_id: Uuid,
    artist: String,
    track_title: String,
    album: Option<String>,
    album_artist: Option<String>,
    track_number: Option<i32>,
    duration_secs: i32,
    started_at_unix_secs: i64,
}

impl ValidatedLastFmScrobble {
    #[allow(clippy::too_many_arguments)]
    fn try_new(
        occurrence_id: Uuid,
        artist: String,
        track_title: String,
        album: Option<String>,
        album_artist: Option<String>,
        track_number: Option<i32>,
        duration_secs: i32,
        started_at_unix_secs: i64,
    ) -> Result<Self, LastFmQueueError> {
        if !is_random_uuid(occurrence_id) {
            return Err(LastFmQueueError::InvalidInput);
        }
        let album = canonical_optional_text(album)?;
        let album_artist = canonical_optional_text(album_artist)?;
        if !valid_required_text(&artist)
            || !valid_required_text(&track_title)
            || track_number.is_some_and(|value| value <= 0)
            || duration_secs <= 30
            || !(1..=MAX_LASTFM_STARTED_AT_SECS).contains(&started_at_unix_secs)
        {
            return Err(LastFmQueueError::InvalidInput);
        }

        Ok(Self {
            occurrence_id,
            artist,
            track_title,
            album,
            album_artist,
            track_number,
            duration_secs,
            started_at_unix_secs,
        })
    }

    fn fmt_redacted(&self, type_name: &str, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct(type_name)
            .field("artist_byte_len", &self.artist.len())
            .field("track_title_byte_len", &self.track_title.len())
            .field("has_album", &self.album.is_some())
            .field("has_album_artist", &self.album_artist.is_some())
            .field("has_track_number", &self.track_number.is_some())
            .field("duration_secs", &self.duration_secs)
            .finish_non_exhaustive()
    }
}

impl PendingLastFmScrobble {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::lastfm) fn try_new(
        occurrence_id: Uuid,
        account_binding: LastFmAccountBinding,
        artist: String,
        track_title: String,
        album: Option<String>,
        album_artist: Option<String>,
        track_number: Option<i32>,
        duration_secs: i32,
        started_at_unix_secs: i64,
    ) -> Result<Self, LastFmQueueError> {
        UnboundLastFmScrobble::try_new(
            occurrence_id,
            artist,
            track_title,
            album,
            album_artist,
            track_number,
            duration_secs,
            started_at_unix_secs,
        )
        .map(|input| input.bind(account_binding))
    }

    #[must_use]
    pub const fn occurrence_id(&self) -> Uuid {
        self.payload.occurrence_id
    }

    #[must_use]
    pub const fn account_binding(&self) -> LastFmAccountBinding {
        self.account_binding
    }

    #[must_use]
    pub fn artist(&self) -> &str {
        &self.payload.artist
    }

    #[must_use]
    pub fn track_title(&self) -> &str {
        &self.payload.track_title
    }

    #[must_use]
    pub fn album(&self) -> Option<&str> {
        self.payload.album.as_deref()
    }

    #[must_use]
    pub fn album_artist(&self) -> Option<&str> {
        self.payload.album_artist.as_deref()
    }

    #[must_use]
    pub const fn track_number(&self) -> Option<i32> {
        self.payload.track_number
    }

    #[must_use]
    pub const fn duration_secs(&self) -> i32 {
        self.payload.duration_secs
    }

    #[must_use]
    pub const fn started_at_unix_secs(&self) -> i64 {
        self.payload.started_at_unix_secs
    }
}

impl fmt::Debug for PendingLastFmScrobble {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.payload
            .fmt_redacted("PendingLastFmScrobble", formatter)
    }
}

/// Result of an atomic idempotent enqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmEnqueueOutcome {
    Inserted { row_id: i64 },
    AlreadyQueued { row_id: i64 },
}

/// Opaque proof of the exact FIFO prefix selected for one network request.
///
/// The payload is readable so the delivery worker can build the Last.fm form,
/// but callers cannot construct, reorder, or retarget a receipt. Retry and
/// terminal settlement compare every frozen row field before mutating SQLite.
pub struct LastFmBatchReceipt {
    account_binding: LastFmAccountBinding,
    rows: Vec<StoredLastFmScrobble>,
}

impl LastFmBatchReceipt {
    fn try_new(
        account_binding: LastFmAccountBinding,
        rows: Vec<StoredLastFmScrobble>,
    ) -> Result<Self, LastFmQueueError> {
        if rows.is_empty()
            || rows.len() > MAX_LASTFM_BATCH_ROWS
            || rows
                .iter()
                .any(|row| row.account_binding != *account_binding.as_bytes())
            || rows.windows(2).any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(LastFmQueueError::InvalidBatch);
        }
        Ok(Self {
            account_binding,
            rows,
        })
    }

    #[must_use]
    pub fn rows(&self) -> &[StoredLastFmScrobble] {
        &self.rows
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Largest durable retry count in this exact FIFO prefix.
    ///
    /// A batch may contain newly admitted rows behind rows that have already
    /// failed, so the delivery worker must advance from the most conservative
    /// retained attempt when it computes one shared not-before value.
    #[must_use]
    pub fn maximum_attempt_count(&self) -> i32 {
        self.rows
            .iter()
            .fold(0, |maximum, row| maximum.max(row.attempt_count))
    }
}

impl fmt::Debug for LastFmBatchReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmBatchReceipt")
            .field("row_count", &self.rows.len())
            .finish_non_exhaustive()
    }
}

/// Availability of the oldest account-bound FIFO prefix.
///
/// A delayed oldest row is distinct from an empty queue so the delivery
/// worker can wait for the exact durable not-before time without polling.
/// Newer rows never bypass that delayed head.
#[derive(Debug)]
pub enum LastFmBatchAvailability {
    Empty,
    DelayedUntil { next_attempt_at_ms: i64 },
    Ready(LastFmBatchReceipt),
}

/// Content-free queue failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmQueueError {
    InvalidInput,
    InvalidBatch,
    Full,
    AccountMismatch,
    OccurrenceConflict,
    StaleBatch,
    CorruptStorage,
    Storage,
}

impl fmt::Display for LastFmQueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidInput => "Last.fm queue input is invalid",
            Self::InvalidBatch => "Last.fm queue batch is invalid",
            Self::Full => "Last.fm offline queue is full",
            Self::AccountMismatch => "Last.fm queue belongs to another account",
            Self::OccurrenceConflict => "Last.fm occurrence identity conflicts with queued data",
            Self::StaleBatch => "Last.fm queue batch is no longer current",
            Self::CorruptStorage => "Last.fm queue storage is not canonical",
            Self::Storage => "Last.fm queue storage failed",
        })
    }
}

impl std::error::Error for LastFmQueueError {}

/// Capability issued only after Last.fm queue admission is closed and every
/// write admitted before that close has crossed the lifecycle FIFO barrier.
///
/// The future runtime coordinator owns issuance. Keeping construction inside
/// the `lastfm` module makes the destructive recovery primitive unavailable to
/// unrelated application code.
pub struct LastFmClosedAndDrainedQueue {
    _private: (),
}

impl LastFmClosedAndDrainedQueue {
    pub(in crate::lastfm) const fn issue_after_barrier() -> Self {
        Self { _private: () }
    }
}

impl fmt::Debug for LastFmClosedAndDrainedQueue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmClosedAndDrainedQueue")
    }
}

/// Atomically persist a scrobble before any network submission.
pub async fn enqueue(
    db: &DatabaseConnection,
    input: &PendingLastFmScrobble,
) -> Result<LastFmEnqueueOutcome, LastFmQueueError> {
    enqueue_with_cap(db, input, MAX_LASTFM_QUEUE_ROWS).await
}

async fn enqueue_with_cap(
    db: &DatabaseConnection,
    input: &PendingLastFmScrobble,
    cap: u64,
) -> Result<LastFmEnqueueOutcome, LastFmQueueError> {
    if cap == 0 || cap > i64::MAX as u64 {
        return Err(LastFmQueueError::InvalidInput);
    }
    let transaction = db.begin().await.map_err(|_| LastFmQueueError::Storage)?;
    match enqueue_in_transaction(&transaction, input, cap).await {
        Ok(outcome) => {
            transaction
                .commit()
                .await
                .map_err(|_| LastFmQueueError::Storage)?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

async fn enqueue_in_transaction<C>(
    db: &C,
    input: &PendingLastFmScrobble,
    cap: u64,
) -> Result<LastFmEnqueueOutcome, LastFmQueueError>
where
    C: ConnectionTrait,
{
    let result = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO lastfm_scrobble_queue (
                 occurrence_id, account_binding, artist, track_title, album, album_artist,
                 track_number, duration_secs, started_at_unix_secs,
                 attempt_count, next_attempt_at_ms
             )
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0
             WHERE NOT EXISTS (
                 SELECT 1 FROM lastfm_scrobble_queue WHERE account_binding <> ?
             )
             AND (SELECT COUNT(*) FROM lastfm_scrobble_queue) < ?
             ON CONFLICT(occurrence_id) DO NOTHING",
            [
                input.payload.occurrence_id.as_bytes().to_vec().into(),
                input.account_binding.as_bytes().to_vec().into(),
                input.payload.artist.clone().into(),
                input.payload.track_title.clone().into(),
                input.payload.album.clone().into(),
                input.payload.album_artist.clone().into(),
                input.payload.track_number.into(),
                input.payload.duration_secs.into(),
                input.payload.started_at_unix_secs.into(),
                input.account_binding.as_bytes().to_vec().into(),
                i64::try_from(cap)
                    .map_err(|_| LastFmQueueError::InvalidInput)?
                    .into(),
            ],
        ))
        .await
        .map_err(|_| LastFmQueueError::Storage)?;

    if result.rows_affected() == 1 {
        let row_id =
            i64::try_from(result.last_insert_id()).map_err(|_| LastFmQueueError::CorruptStorage)?;
        if row_id <= 0 {
            return Err(LastFmQueueError::CorruptStorage);
        }
        return Ok(LastFmEnqueueOutcome::Inserted { row_id });
    }

    // Any mixed-account state is quarantined before interpreting even an
    // otherwise idempotent occurrence. This keeps corrupted or externally
    // modified storage from being mistaken for a healthy single-account queue.
    if queue_has_other_binding(db, input.account_binding).await? {
        return Err(LastFmQueueError::AccountMismatch);
    }

    if let Some(existing) = lastfm_scrobble::Entity::find()
        .filter(
            lastfm_scrobble::Column::OccurrenceId.eq(input
                .payload
                .occurrence_id
                .as_bytes()
                .to_vec()),
        )
        .one(db)
        .await
        .map_err(|_| LastFmQueueError::Storage)?
    {
        let row_id = StoredLastFmScrobble::try_from(existing.clone())
            .map_err(|_| LastFmQueueError::CorruptStorage)?
            .id;
        return if same_payload(&existing, input) {
            Ok(LastFmEnqueueOutcome::AlreadyQueued { row_id })
        } else {
            Err(LastFmQueueError::OccurrenceConflict)
        };
    }

    let count = lastfm_scrobble::Entity::find()
        .count(db)
        .await
        .map_err(|_| LastFmQueueError::Storage)?;
    if count >= cap {
        Err(LastFmQueueError::Full)
    } else {
        // A valid row can be omitted only by the atomic cap or occurrence
        // conflict predicates. Anything else means the storage boundary drifted.
        Err(LastFmQueueError::Storage)
    }
}

/// Inspect at most one Last.fm batch from the oldest FIFO prefix.
///
/// The result distinguishes an empty queue from a delayed head. When the head
/// is due, only its contiguous due prefix is returned; a future row blocks
/// every row behind it even when those later rows carry an earlier timestamp.
pub async fn batch_availability(
    db: &DatabaseConnection,
    account_binding: LastFmAccountBinding,
    now_ms: i64,
    limit: usize,
) -> Result<LastFmBatchAvailability, LastFmQueueError> {
    if !(0..=MAX_LASTFM_RETRY_AT_MS).contains(&now_ms)
        || !(1..=MAX_LASTFM_BATCH_ROWS).contains(&limit)
    {
        return Err(LastFmQueueError::InvalidBatch);
    }

    if queue_has_other_binding(db, account_binding).await? {
        return Err(LastFmQueueError::AccountMismatch);
    }
    let rows = load_prefix(db, account_binding, limit).await?;
    let Some(head) = rows.first() else {
        return Ok(LastFmBatchAvailability::Empty);
    };
    if head.next_attempt_at_ms > now_ms {
        return Ok(LastFmBatchAvailability::DelayedUntil {
            next_attempt_at_ms: head.next_attempt_at_ms,
        });
    }

    let due = rows
        .into_iter()
        .take_while(|row| row.next_attempt_at_ms <= now_ms)
        .collect::<Vec<_>>();
    LastFmBatchReceipt::try_new(account_binding, due).map(LastFmBatchAvailability::Ready)
}

/// Compatibility wrapper for callers that do not need the delayed deadline.
pub async fn due_batch(
    db: &DatabaseConnection,
    account_binding: LastFmAccountBinding,
    now_ms: i64,
    limit: usize,
) -> Result<Option<LastFmBatchReceipt>, LastFmQueueError> {
    match batch_availability(db, account_binding, now_ms, limit).await? {
        LastFmBatchAvailability::Ready(receipt) => Ok(Some(receipt)),
        LastFmBatchAvailability::Empty | LastFmBatchAvailability::DelayedUntil { .. } => Ok(None),
    }
}

/// Delete one complete accepted/ignored batch if its exact FIFO snapshot is
/// still current. A stale or partially missing receipt changes no row.
pub async fn settle_terminal(
    db: &DatabaseConnection,
    receipt: &LastFmBatchReceipt,
) -> Result<(), LastFmQueueError> {
    let transaction = db.begin().await.map_err(|_| LastFmQueueError::Storage)?;
    if let Err(error) = validate_receipt(&transaction, receipt).await {
        let _ = transaction.rollback().await;
        return Err(error);
    }

    let placeholders = std::iter::repeat_n("?", receipt.rows.len())
        .collect::<Vec<_>>()
        .join(", ");
    let mut values = Vec::with_capacity(receipt.rows.len() + 1);
    values.push(receipt.account_binding.as_bytes().to_vec().into());
    values.extend(receipt.rows.iter().map(|row| row.id.into()));
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!(
                "DELETE FROM lastfm_scrobble_queue
                 WHERE account_binding = ? AND id IN ({placeholders})"
            ),
            values,
        ))
        .await;
    let Ok(result) = result else {
        let _ = transaction.rollback().await;
        return Err(LastFmQueueError::Storage);
    };
    if result.rows_affected() != receipt.rows.len() as u64 {
        let _ = transaction.rollback().await;
        return Err(LastFmQueueError::StaleBatch);
    }
    transaction
        .commit()
        .await
        .map_err(|_| LastFmQueueError::Storage)
}

/// Retain one complete failed batch and move it to one bounded retry time if
/// its exact FIFO snapshot is still current. A stale receipt changes no row.
pub async fn reschedule_batch(
    db: &DatabaseConnection,
    receipt: &LastFmBatchReceipt,
    next_attempt_at_ms: i64,
) -> Result<(), LastFmQueueError> {
    if !(0..=MAX_LASTFM_RETRY_AT_MS).contains(&next_attempt_at_ms) {
        return Err(LastFmQueueError::InvalidBatch);
    }
    let transaction = db.begin().await.map_err(|_| LastFmQueueError::Storage)?;
    if let Err(error) = validate_receipt(&transaction, receipt).await {
        let _ = transaction.rollback().await;
        return Err(error);
    }

    let placeholders = std::iter::repeat_n("?", receipt.rows.len())
        .collect::<Vec<_>>()
        .join(", ");
    let mut values = Vec::with_capacity(receipt.rows.len() + 2);
    values.push(next_attempt_at_ms.into());
    values.push(receipt.account_binding.as_bytes().to_vec().into());
    values.extend(receipt.rows.iter().map(|row| row.id.into()));
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!(
                "UPDATE lastfm_scrobble_queue
                 SET attempt_count = MIN(attempt_count + 1, {MAX_LASTFM_ATTEMPT_COUNT}),
                     next_attempt_at_ms = ?
                 WHERE account_binding = ? AND id IN ({placeholders})"
            ),
            values,
        ))
        .await;
    let Ok(result) = result else {
        let _ = transaction.rollback().await;
        return Err(LastFmQueueError::Storage);
    };
    if result.rows_affected() != receipt.rows.len() as u64 {
        let _ = transaction.rollback().await;
        return Err(LastFmQueueError::StaleBatch);
    }
    transaction
        .commit()
        .await
        .map_err(|_| LastFmQueueError::Storage)
}

/// Purge private queued metadata only for the expected current account.
///
/// A stale disconnect cannot erase a successor account's rows. The lifecycle
/// coordinator must close enqueue admission before calling this operation.
pub async fn purge_account(
    db: &DatabaseConnection,
    account_binding: LastFmAccountBinding,
) -> Result<u64, LastFmQueueError> {
    let transaction = db.begin().await.map_err(|_| LastFmQueueError::Storage)?;
    match queue_has_other_binding(&transaction, account_binding).await {
        Ok(false) => {}
        Ok(true) => {
            let _ = transaction.rollback().await;
            return Err(LastFmQueueError::AccountMismatch);
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    }
    let result = lastfm_scrobble::Entity::delete_many()
        .filter(lastfm_scrobble::Column::AccountBinding.eq(account_binding.as_bytes().to_vec()))
        .exec(&transaction)
        .await;
    let Ok(result) = result else {
        let _ = transaction.rollback().await;
        return Err(LastFmQueueError::Storage);
    };
    transaction
        .commit()
        .await
        .map_err(|_| LastFmQueueError::Storage)?;
    Ok(result.rows_affected)
}

/// Purge the queue snapshot retained when the native vault cannot identify it.
///
/// This recovery path is intentionally separate from [`purge_account`]. The
/// lifecycle coordinator may issue `authority` only after closing queue
/// admission, draining every previously admitted queue write, stopping the
/// delivery worker, and preventing creation of a successor account until this
/// transaction commits. It snapshots the current maximum row identity and
/// deletes only through that boundary, so a successor row admitted after the
/// snapshot is never selected for deletion.
pub async fn purge_quarantined_after_admission_closed(
    db: &DatabaseConnection,
    _authority: &LastFmClosedAndDrainedQueue,
) -> Result<u64, LastFmQueueError> {
    let transaction = db.begin().await.map_err(|_| LastFmQueueError::Storage)?;
    let cutoff = lastfm_scrobble::Entity::find()
        .select_only()
        .column(lastfm_scrobble::Column::Id)
        .order_by_desc(lastfm_scrobble::Column::Id)
        .into_tuple::<i64>()
        .one(&transaction)
        .await
        .map_err(|_| LastFmQueueError::Storage);
    let cutoff = match cutoff {
        Ok(Some(cutoff)) => cutoff,
        Ok(None) => {
            transaction
                .commit()
                .await
                .map_err(|_| LastFmQueueError::Storage)?;
            return Ok(0);
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    };

    let deleted = match purge_through_cutoff(&transaction, cutoff).await {
        Ok(deleted) => deleted,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    };
    transaction
        .commit()
        .await
        .map_err(|_| LastFmQueueError::Storage)?;
    Ok(deleted)
}

/// Number of private rows retained in the single-account queue.
pub async fn queue_len(db: &DatabaseConnection) -> Result<u64, LastFmQueueError> {
    lastfm_scrobble::Entity::find()
        .count(db)
        .await
        .map_err(|_| LastFmQueueError::Storage)
}

/// Validate the complete retained queue for one expected account.
///
/// Startup uses this stronger boundary before opening admission. Unlike a
/// delivery read, validation cannot stop at the 50-row protocol batch limit:
/// every retained row must be canonical, belong to the expected account, and
/// fit within the global queue bound before the exact count is returned.
pub async fn validate_account_queue(
    db: &DatabaseConnection,
    account_binding: LastFmAccountBinding,
) -> Result<u64, LastFmQueueError> {
    let transaction = db.begin().await.map_err(|_| LastFmQueueError::Storage)?;
    let result = async {
        if queue_has_other_binding(&transaction, account_binding).await? {
            return Err(LastFmQueueError::AccountMismatch);
        }

        let count = lastfm_scrobble::Entity::find()
            .count(&transaction)
            .await
            .map_err(|_| LastFmQueueError::Storage)?;
        if count > MAX_LASTFM_QUEUE_ROWS {
            return Err(LastFmQueueError::CorruptStorage);
        }

        let rows = lastfm_scrobble::Entity::find()
            .order_by_asc(lastfm_scrobble::Column::Id)
            .all(&transaction)
            .await
            .map_err(|_| LastFmQueueError::Storage)?;
        let loaded_count =
            u64::try_from(rows.len()).map_err(|_| LastFmQueueError::CorruptStorage)?;
        if loaded_count != count
            || rows
                .into_iter()
                .any(|row| StoredLastFmScrobble::try_from(row).is_err())
        {
            return Err(LastFmQueueError::CorruptStorage);
        }
        Ok(count)
    }
    .await;

    match result {
        Ok(count) => {
            transaction
                .commit()
                .await
                .map_err(|_| LastFmQueueError::Storage)?;
            Ok(count)
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

async fn validate_receipt<C>(db: &C, receipt: &LastFmBatchReceipt) -> Result<(), LastFmQueueError>
where
    C: ConnectionTrait,
{
    if queue_has_other_binding(db, receipt.account_binding).await? {
        return Err(LastFmQueueError::AccountMismatch);
    }
    let current = load_prefix(db, receipt.account_binding, receipt.rows.len()).await?;
    if current != receipt.rows {
        return Err(LastFmQueueError::StaleBatch);
    }
    Ok(())
}

async fn load_prefix<C>(
    db: &C,
    account_binding: LastFmAccountBinding,
    limit: usize,
) -> Result<Vec<StoredLastFmScrobble>, LastFmQueueError>
where
    C: ConnectionTrait,
{
    let rows = lastfm_scrobble::Entity::find()
        .filter(lastfm_scrobble::Column::AccountBinding.eq(account_binding.as_bytes().to_vec()))
        .order_by_asc(lastfm_scrobble::Column::Id)
        .limit(u64::try_from(limit).map_err(|_| LastFmQueueError::InvalidBatch)?)
        .all(db)
        .await
        .map_err(|_| LastFmQueueError::Storage)?;
    rows.into_iter()
        .map(StoredLastFmScrobble::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| LastFmQueueError::CorruptStorage)
}

fn same_payload(model: &lastfm_scrobble::Model, input: &PendingLastFmScrobble) -> bool {
    model.account_binding.as_slice() == input.account_binding.as_bytes()
        && model.artist.as_str() == input.payload.artist
        && model.track_title.as_str() == input.payload.track_title
        && model.album.as_ref().map(StoredMetadataText::as_str) == input.payload.album.as_deref()
        && model.album_artist.as_ref().map(StoredMetadataText::as_str)
            == input.payload.album_artist.as_deref()
        && model.track_number == input.payload.track_number
        && model.duration_secs == input.payload.duration_secs
        && model.started_at_unix_secs.get() == input.payload.started_at_unix_secs
}

async fn queue_has_other_binding<C>(
    db: &C,
    account_binding: LastFmAccountBinding,
) -> Result<bool, LastFmQueueError>
where
    C: ConnectionTrait,
{
    lastfm_scrobble::Entity::find()
        .filter(lastfm_scrobble::Column::AccountBinding.ne(account_binding.as_bytes().to_vec()))
        .count(db)
        .await
        .map(|count| count != 0)
        .map_err(|_| LastFmQueueError::Storage)
}

async fn purge_through_cutoff<C>(db: &C, cutoff: i64) -> Result<u64, LastFmQueueError>
where
    C: ConnectionTrait,
{
    lastfm_scrobble::Entity::delete_many()
        .filter(lastfm_scrobble::Column::Id.lte(cutoff))
        .exec(db)
        .await
        .map(|result| result.rows_affected)
        .map_err(|_| LastFmQueueError::Storage)
}

fn is_random_uuid(uuid: Uuid) -> bool {
    uuid.get_variant() == Variant::RFC4122 && uuid.get_version() == Some(Version::Random)
}

fn valid_required_text(value: &str) -> bool {
    value.len() <= MAX_LASTFM_METADATA_BYTES
        && value.chars().any(|character| !character.is_whitespace())
        && !value.chars().any(char::is_control)
}

fn canonical_optional_text(value: Option<String>) -> Result<Option<String>, LastFmQueueError> {
    match value {
        None => Ok(None),
        Some(value) if !value.chars().any(|character| !character.is_whitespace()) => Ok(None),
        Some(value)
            if value.len() <= MAX_LASTFM_METADATA_BYTES && !value.chars().any(char::is_control) =>
        {
            Ok(Some(value))
        }
        Some(_) => Err(LastFmQueueError::InvalidInput),
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{Database, EntityTrait};

    use super::*;
    use sea_orm_migration::MigratorTrait;

    use crate::db::migration::Migrator;
    use crate::lastfm::credentials::{ProtectedString, StoredSession};

    const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    fn binding(username: &str) -> LastFmAccountBinding {
        StoredSession::new(username, ProtectedString::new(SESSION_KEY))
            .unwrap()
            .account_binding()
    }

    fn input(account_binding: LastFmAccountBinding, title: &str) -> PendingLastFmScrobble {
        PendingLastFmScrobble::try_new(
            Uuid::new_v4(),
            account_binding,
            "Artist".to_owned(),
            title.to_owned(),
            Some("Album".to_owned()),
            None,
            Some(1),
            60,
            1_700_000_000,
        )
        .unwrap()
    }

    async fn batch(
        db: &DatabaseConnection,
        account_binding: LastFmAccountBinding,
        now_ms: i64,
        limit: usize,
    ) -> LastFmBatchReceipt {
        due_batch(db, account_binding, now_ms, limit)
            .await
            .unwrap()
            .expect("due batch")
    }

    async fn models(db: &DatabaseConnection) -> Vec<lastfm_scrobble::Model> {
        lastfm_scrobble::Entity::find()
            .order_by_asc(lastfm_scrobble::Column::Id)
            .all(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn complete_account_queue_validation_returns_exact_count() {
        let db = database().await;
        let account = binding("listener");
        assert_eq!(validate_account_queue(&db, account).await.unwrap(), 0);

        for title in ["First", "Second", "Third"] {
            enqueue(&db, &input(account, title)).await.unwrap();
        }
        assert_eq!(validate_account_queue(&db, account).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn complete_account_queue_validation_rejects_another_binding() {
        let db = database().await;
        let retained_account = binding("retained-listener");
        let expected_account = binding("expected-listener");
        enqueue(&db, &input(retained_account, "Private row"))
            .await
            .unwrap();

        assert_eq!(
            validate_account_queue(&db, expected_account)
                .await
                .unwrap_err(),
            LastFmQueueError::AccountMismatch
        );
        assert_eq!(queue_len(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn complete_account_queue_validation_finds_corruption_beyond_one_batch() {
        let db = database().await;
        let account = binding("listener");
        for index in 0..=MAX_LASTFM_BATCH_ROWS {
            enqueue(&db, &input(account, &format!("Track {index}")))
                .await
                .unwrap();
        }
        let retained = models(&db).await;
        let corrupt_id = retained[MAX_LASTFM_BATCH_ROWS].id;
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE lastfm_scrobble_queue SET occurrence_id = ? WHERE id = ?",
            [Uuid::nil().as_bytes().to_vec().into(), corrupt_id.into()],
        ))
        .await
        .unwrap();

        let first_batch = batch_availability(&db, account, 0, MAX_LASTFM_BATCH_ROWS)
            .await
            .unwrap();
        assert!(matches!(
            first_batch,
            LastFmBatchAvailability::Ready(receipt)
                if receipt.len() == MAX_LASTFM_BATCH_ROWS
        ));
        let error = validate_account_queue(&db, account).await.unwrap_err();
        assert_eq!(error, LastFmQueueError::CorruptStorage);
        assert_eq!(format!("{error:?}"), "CorruptStorage");
        assert_eq!(queue_len(&db).await.unwrap(), 51);
    }

    #[tokio::test]
    async fn enqueue_is_atomic_bounded_idempotent_and_collision_safe() {
        let db = database().await;
        let account = binding("listener");
        let first = input(account, "First");
        let inserted = enqueue_with_cap(&db, &first, 2).await.unwrap();
        let LastFmEnqueueOutcome::Inserted { row_id } = inserted else {
            panic!("first row must be inserted");
        };
        assert_eq!(
            enqueue_with_cap(&db, &first, 2).await.unwrap(),
            LastFmEnqueueOutcome::AlreadyQueued { row_id }
        );

        let mut collision = input(account, "Changed");
        collision.payload.occurrence_id = first.payload.occurrence_id;
        assert_eq!(
            enqueue_with_cap(&db, &collision, 2).await.unwrap_err(),
            LastFmQueueError::OccurrenceConflict
        );

        enqueue_with_cap(&db, &input(account, "Second"), 2)
            .await
            .unwrap();
        assert_eq!(
            enqueue_with_cap(&db, &first, 2).await.unwrap(),
            LastFmEnqueueOutcome::AlreadyQueued { row_id }
        );
        assert_eq!(
            enqueue_with_cap(&db, &input(account, "Third"), 2)
                .await
                .unwrap_err(),
            LastFmQueueError::Full
        );
        assert_eq!(queue_len(&db).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn idempotent_enqueue_rejects_malformed_existing_row_identities() {
        for malformed_id in [-1_i64, 0] {
            let db = database().await;
            let account = binding("listener");
            let pending = input(account, "Corrupt retry");
            db.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO lastfm_scrobble_queue (
                     id, occurrence_id, account_binding, artist, track_title,
                     album, track_number, duration_secs, started_at_unix_secs,
                     attempt_count, next_attempt_at_ms
                 ) VALUES (?, ?, ?, 'Artist', 'Corrupt retry', 'Album', 1, 60,
                           1700000000, 0, 0)",
                [
                    malformed_id.into(),
                    pending.occurrence_id().as_bytes().to_vec().into(),
                    account.as_bytes().to_vec().into(),
                ],
            ))
            .await
            .unwrap();

            assert_eq!(
                enqueue(&db, &pending).await.unwrap_err(),
                LastFmQueueError::CorruptStorage
            );
            assert_eq!(models(&db).await[0].id, malformed_id);
        }
    }

    #[tokio::test]
    async fn batch_availability_reports_empty_and_rejects_invalid_bounds() {
        let db = database().await;
        let account = binding("listener");
        assert!(matches!(
            batch_availability(&db, account, 0, 1).await.unwrap(),
            LastFmBatchAvailability::Empty
        ));

        for (now_ms, limit) in [
            (-1, 1),
            (MAX_LASTFM_RETRY_AT_MS + 1, 1),
            (0, 0),
            (0, MAX_LASTFM_BATCH_ROWS + 1),
        ] {
            assert_eq!(
                batch_availability(&db, account, now_ms, limit)
                    .await
                    .unwrap_err(),
                LastFmQueueError::InvalidBatch
            );
        }
    }

    #[tokio::test]
    async fn delayed_oldest_row_blocks_newer_due_rows_and_retry_is_durable() {
        let db = database().await;
        let account = binding("listener");
        enqueue(&db, &input(account, "First")).await.unwrap();
        let initial = batch(&db, account, 0, 50).await;
        assert_eq!(initial.len(), 1);
        reschedule_batch(&db, &initial, 500).await.unwrap();
        for title in ["Second", "Third"] {
            enqueue(&db, &input(account, title)).await.unwrap();
        }
        assert!(matches!(
            batch_availability(&db, account, 499, 50).await.unwrap(),
            LastFmBatchAvailability::DelayedUntil {
                next_attempt_at_ms: 500
            }
        ));

        let due = match batch_availability(&db, account, 500, 50).await.unwrap() {
            LastFmBatchAvailability::Ready(receipt) => receipt,
            availability => panic!("exact not-before boundary must be ready: {availability:?}"),
        };
        assert_eq!(due.len(), 3);
        assert_eq!(due.maximum_attempt_count(), 1);
        assert_eq!(due.rows()[0].attempt_count, 1);
        assert_eq!(due.rows()[0].next_attempt_at_ms, 500);
        assert!(due.rows()[1..]
            .iter()
            .all(|row| row.attempt_count == 0 && row.next_attempt_at_ms == 0));
        assert_eq!(due.rows().iter().map(|row| row.id).collect::<Vec<_>>(), {
            let mut ids = due.rows().iter().map(|row| row.id).collect::<Vec<_>>();
            ids.sort_unstable();
            ids
        });
    }

    #[tokio::test]
    async fn ready_batch_stops_before_a_delayed_row_that_blocks_a_due_tail() {
        let db = database().await;
        let account = binding("listener");
        for title in ["First", "Second", "Third"] {
            enqueue(&db, &input(account, title)).await.unwrap();
        }
        let stored = models(&db).await;
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE lastfm_scrobble_queue SET next_attempt_at_ms = 500 WHERE id = ?",
            [stored[1].id.into()],
        ))
        .await
        .unwrap();

        let ready = match batch_availability(&db, account, 0, 50).await.unwrap() {
            LastFmBatchAvailability::Ready(receipt) => receipt,
            availability => panic!("oldest due row must be ready: {availability:?}"),
        };
        assert_eq!(ready.len(), 1);
        assert_eq!(ready.rows()[0].id, stored[0].id);
        settle_terminal(&db, &ready).await.unwrap();

        assert!(matches!(
            batch_availability(&db, account, 0, 50).await.unwrap(),
            LastFmBatchAvailability::DelayedUntil {
                next_attempt_at_ms: 500
            }
        ));
        let remaining = models(&db).await;
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[1].next_attempt_at_ms, 0);
    }

    #[tokio::test]
    async fn binding_mismatch_quarantines_and_disconnect_purges_every_row() {
        let db = database().await;
        let first_account = binding("first-listener");
        let second_account = binding("second-listener");
        enqueue(&db, &input(first_account, "First")).await.unwrap();
        assert_eq!(
            batch_availability(&db, second_account, 0, 50)
                .await
                .unwrap_err(),
            LastFmQueueError::AccountMismatch
        );
        assert_eq!(
            enqueue(&db, &input(second_account, "Second"))
                .await
                .unwrap_err(),
            LastFmQueueError::AccountMismatch
        );
        let first = batch(&db, first_account, 0, 50).await;
        assert_eq!(first.len(), 1);
        settle_terminal(&db, &first).await.unwrap();
        assert_eq!(queue_len(&db).await.unwrap(), 0);

        enqueue(&db, &input(first_account, "Private pending row"))
            .await
            .unwrap();
        assert_eq!(
            purge_account(&db, second_account).await.unwrap_err(),
            LastFmQueueError::AccountMismatch
        );
        assert_eq!(queue_len(&db).await.unwrap(), 1);
        assert_eq!(purge_account(&db, first_account).await.unwrap(), 1);
        assert_eq!(
            lastfm_scrobble::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .len(),
            0
        );

        enqueue(&db, &input(second_account, "Successor private row"))
            .await
            .unwrap();
        assert_eq!(
            purge_account(&db, first_account).await.unwrap_err(),
            LastFmQueueError::AccountMismatch
        );
        assert_eq!(
            models(&db).await[0].track_title.as_str(),
            "Successor private row"
        );
        assert_eq!(purge_account(&db, second_account).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn unavailable_vault_recovery_purges_only_its_closed_snapshot() {
        let db = database().await;
        let retired_account = binding("retired-listener");
        let successor_account = binding("successor-listener");
        let retired = enqueue(&db, &input(retired_account, "Retired private row"))
            .await
            .unwrap();
        let LastFmEnqueueOutcome::Inserted { row_id: cutoff } = retired else {
            panic!("retired row must be inserted");
        };

        // Model a successor admitted strictly after a recovery snapshot. The
        // private helper's ID predicate is the same one used transactionally
        // by the public closed-admission operation.
        purge_through_cutoff(&db, cutoff).await.unwrap();
        enqueue(&db, &input(successor_account, "Successor private row"))
            .await
            .unwrap();
        assert_eq!(purge_through_cutoff(&db, cutoff).await.unwrap(), 0);
        assert_eq!(
            models(&db).await[0].track_title.as_str(),
            "Successor private row"
        );

        assert_eq!(purge_account(&db, successor_account).await.unwrap(), 1);
        enqueue(&db, &input(retired_account, "Unrecoverable private row"))
            .await
            .unwrap();

        // A real recovery snapshots and purges all rows that exist while
        // admission is closed, including a binding that cannot be recreated
        // because its vault record is missing or corrupt.
        let authority = LastFmClosedAndDrainedQueue::issue_after_barrier();
        assert_eq!(
            purge_quarantined_after_admission_closed(&db, &authority)
                .await
                .unwrap(),
            1
        );
        assert_eq!(queue_len(&db).await.unwrap(), 0);

        // Recovery remains destructive for malformed private rows that normal
        // delivery correctly refuses, including an explicitly injected
        // non-positive SQLite row identity.
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO lastfm_scrobble_queue (
                 id, occurrence_id, account_binding, artist, track_title,
                 duration_secs, started_at_unix_secs, attempt_count,
                 next_attempt_at_ms
             ) VALUES (-1, ?, ?, 'Artist', 'Track', 60, 1, 0, 0)",
            [
                Uuid::new_v4().as_bytes().to_vec().into(),
                retired_account.as_bytes().to_vec().into(),
            ],
        ))
        .await
        .unwrap();
        let authority = LastFmClosedAndDrainedQueue::issue_after_barrier();
        assert_eq!(
            purge_quarantined_after_admission_closed(&db, &authority)
                .await
                .unwrap(),
            1
        );
        assert_eq!(queue_len(&db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn malformed_private_rows_fail_closed_without_content_diagnostics() {
        let db = database().await;
        let account = binding("listener");
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO lastfm_scrobble_queue (
                 occurrence_id, account_binding, artist, track_title, duration_secs,
                 started_at_unix_secs, attempt_count, next_attempt_at_ms
             ) VALUES (?, ?, 'Artist', 'Track', 60, 1, 0, 0)",
            [
                Uuid::nil().as_bytes().to_vec().into(),
                account.as_bytes().to_vec().into(),
            ],
        ))
        .await
        .unwrap();
        let error = batch_availability(&db, account, 0, 50).await.unwrap_err();
        assert_eq!(error, LastFmQueueError::CorruptStorage);
        assert_eq!(format!("{error:?}"), "CorruptStorage");
    }

    #[test]
    fn unbound_and_bound_construction_share_exact_validation() {
        let account = binding("listener");
        let invalid_inputs = [
            (
                Uuid::nil(),
                "Artist".to_owned(),
                "Track".to_owned(),
                None,
                None,
                Some(1),
                60,
                1,
            ),
            (
                Uuid::new_v4(),
                "Artist\0".to_owned(),
                "Track".to_owned(),
                None,
                None,
                Some(1),
                60,
                1,
            ),
            (
                Uuid::new_v4(),
                "Artist".to_owned(),
                " \t".to_owned(),
                None,
                None,
                Some(1),
                60,
                1,
            ),
            (
                Uuid::new_v4(),
                "Artist".to_owned(),
                "Track".to_owned(),
                Some("Album\n".to_owned()),
                None,
                Some(1),
                60,
                1,
            ),
            (
                Uuid::new_v4(),
                "Artist".to_owned(),
                "Track".to_owned(),
                None,
                None,
                Some(0),
                60,
                1,
            ),
            (
                Uuid::new_v4(),
                "Artist".to_owned(),
                "Track".to_owned(),
                None,
                None,
                Some(1),
                30,
                1,
            ),
            (
                Uuid::new_v4(),
                "Artist".to_owned(),
                "Track".to_owned(),
                None,
                None,
                Some(1),
                60,
                0,
            ),
        ];

        for (
            occurrence_id,
            artist,
            track_title,
            album,
            album_artist,
            track_number,
            duration_secs,
            started_at_unix_secs,
        ) in invalid_inputs
        {
            let unbound_error = UnboundLastFmScrobble::try_new(
                occurrence_id,
                artist.clone(),
                track_title.clone(),
                album.clone(),
                album_artist.clone(),
                track_number,
                duration_secs,
                started_at_unix_secs,
            )
            .unwrap_err();
            let bound_error = PendingLastFmScrobble::try_new(
                occurrence_id,
                account,
                artist,
                track_title,
                album,
                album_artist,
                track_number,
                duration_secs,
                started_at_unix_secs,
            )
            .unwrap_err();
            assert_eq!(unbound_error, LastFmQueueError::InvalidInput);
            assert_eq!(bound_error, unbound_error);
        }
    }

    #[test]
    fn binding_adds_only_account_authority_to_the_validated_payload() {
        let account = binding("listener");
        let occurrence_id = Uuid::new_v4();
        let unbound = UnboundLastFmScrobble::try_new(
            occurrence_id,
            " Artist ".to_owned(),
            " Track ".to_owned(),
            Some(" Album ".to_owned()),
            Some(" \t".to_owned()),
            Some(7),
            301,
            1_700_000_123,
        )
        .unwrap();
        assert_eq!(unbound.occurrence_id(), occurrence_id);
        assert_eq!(unbound.artist(), " Artist ");
        assert_eq!(unbound.track_title(), " Track ");
        assert_eq!(unbound.album(), Some(" Album "));
        assert_eq!(unbound.album_artist(), None);
        assert_eq!(unbound.track_number(), Some(7));
        assert_eq!(unbound.duration_secs(), 301);
        assert_eq!(unbound.started_at_unix_secs(), 1_700_000_123);

        let bound = unbound.clone().bind(account);
        let legacy_constructor = PendingLastFmScrobble::try_new(
            occurrence_id,
            account,
            " Artist ".to_owned(),
            " Track ".to_owned(),
            Some(" Album ".to_owned()),
            Some(" \t".to_owned()),
            Some(7),
            301,
            1_700_000_123,
        )
        .unwrap();
        assert_eq!(bound, legacy_constructor);
        assert_eq!(bound.account_binding(), account);
        assert_eq!(bound.occurrence_id(), unbound.occurrence_id());
        assert_eq!(bound.artist(), unbound.artist());
        assert_eq!(bound.track_title(), unbound.track_title());
        assert_eq!(bound.album(), unbound.album());
        assert_eq!(bound.album_artist(), unbound.album_artist());
        assert_eq!(bound.track_number(), unbound.track_number());
        assert_eq!(bound.duration_secs(), unbound.duration_secs());
        assert_eq!(bound.started_at_unix_secs(), unbound.started_at_unix_secs());
    }

    #[test]
    fn unbound_and_bound_diagnostics_remain_content_free() {
        let account = binding("private-listener");
        let occurrence_id = Uuid::new_v4();
        let unbound = UnboundLastFmScrobble::try_new(
            occurrence_id,
            "Private Artist".to_owned(),
            "Private Track".to_owned(),
            Some("Private Album".to_owned()),
            Some("Private Album Artist".to_owned()),
            Some(9),
            321,
            1_700_987_654,
        )
        .unwrap();
        let bound = unbound.clone().bind(account);

        for diagnostics in [format!("{unbound:?}"), format!("{bound:?}")] {
            assert!(!diagnostics.contains("Private"));
            assert!(!diagnostics.contains(&occurrence_id.to_string()));
            assert!(!diagnostics.contains("1700987654"));
            assert!(!diagnostics.contains("LastFmAccountBinding"));
            assert!(diagnostics.contains("duration_secs: 321"));
        }
    }

    #[test]
    fn blank_optional_metadata_is_omitted_without_rewriting_nonblank_bytes() {
        let account = binding("listener");
        let blank = PendingLastFmScrobble::try_new(
            Uuid::new_v4(),
            account,
            "Artist".to_owned(),
            "Track".to_owned(),
            Some(String::new()),
            Some(" \t\n".to_owned()),
            None,
            31,
            1,
        )
        .unwrap();
        assert_eq!(blank.album(), None);
        assert_eq!(blank.album_artist(), None);

        let exact = PendingLastFmScrobble::try_new(
            Uuid::new_v4(),
            account,
            "Artist".to_owned(),
            "Track".to_owned(),
            Some(" Album ".to_owned()),
            Some(" Album Artist ".to_owned()),
            None,
            31,
            1,
        )
        .unwrap();
        assert_eq!(exact.album(), Some(" Album "));
        assert_eq!(exact.album_artist(), Some(" Album Artist "));

        assert_eq!(
            PendingLastFmScrobble::try_new(
                Uuid::new_v4(),
                account,
                "Artist\0".to_owned(),
                "Track".to_owned(),
                None,
                None,
                None,
                31,
                1,
            )
            .unwrap_err(),
            LastFmQueueError::InvalidInput
        );
    }

    #[tokio::test]
    async fn batch_bound_is_exact_and_settlement_advances_the_fifo() {
        let db = database().await;
        let account = binding("listener");
        for index in 0..=MAX_LASTFM_BATCH_ROWS {
            enqueue(&db, &input(account, &format!("Track {index}")))
                .await
                .unwrap();
        }
        assert_eq!(
            due_batch(&db, account, 0, MAX_LASTFM_BATCH_ROWS + 1)
                .await
                .unwrap_err(),
            LastFmQueueError::InvalidBatch
        );
        let first = batch(&db, account, 0, MAX_LASTFM_BATCH_ROWS).await;
        assert_eq!(first.len(), MAX_LASTFM_BATCH_ROWS);
        assert!(first.rows().windows(2).all(|rows| rows[0].id < rows[1].id));
        settle_terminal(&db, &first).await.unwrap();
        let last = batch(&db, account, 0, MAX_LASTFM_BATCH_ROWS).await;
        assert_eq!(last.len(), 1);
    }

    #[tokio::test]
    async fn stale_partial_receipts_never_partially_settle_or_reschedule() {
        for settlement in [true, false] {
            let db = database().await;
            let account = binding("listener");
            for title in ["First", "Second", "Third"] {
                enqueue(&db, &input(account, title)).await.unwrap();
            }
            let receipt = batch(&db, account, 0, 50).await;
            let removed_id = receipt.rows()[1].id;
            lastfm_scrobble::Entity::delete_by_id(removed_id)
                .exec(&db)
                .await
                .unwrap();

            let error = if settlement {
                settle_terminal(&db, &receipt).await.unwrap_err()
            } else {
                reschedule_batch(&db, &receipt, 500).await.unwrap_err()
            };
            assert_eq!(error, LastFmQueueError::StaleBatch);
            let remaining = models(&db).await;
            assert_eq!(remaining.len(), 2);
            assert!(remaining.iter().all(|row| row.id != removed_id));
            assert!(remaining.iter().all(|row| row.attempt_count == 0));
            assert!(remaining.iter().all(|row| row.next_attempt_at_ms == 0));
        }
    }

    #[tokio::test]
    async fn receipt_rejects_nonprefix_mixed_reordered_and_changed_state() {
        let db = database().await;
        let account = binding("listener");
        for title in ["First", "Second", "Third"] {
            enqueue(&db, &input(account, title)).await.unwrap();
        }
        let current = batch(&db, account, 0, 50).await;

        let nonprefix = LastFmBatchReceipt::try_new(account, current.rows()[1..].to_vec()).unwrap();
        assert_eq!(
            settle_terminal(&db, &nonprefix).await.unwrap_err(),
            LastFmQueueError::StaleBatch
        );
        assert_eq!(queue_len(&db).await.unwrap(), 3);

        let mut reordered = current.rows()[..2].to_vec();
        reordered.reverse();
        assert_eq!(
            LastFmBatchReceipt::try_new(account, reordered).unwrap_err(),
            LastFmQueueError::InvalidBatch
        );

        let other = binding("other-listener");
        let mut mixed = current.rows()[..2].to_vec();
        mixed[1].account_binding = *other.as_bytes();
        assert_eq!(
            LastFmBatchReceipt::try_new(account, mixed).unwrap_err(),
            LastFmQueueError::InvalidBatch
        );

        reschedule_batch(&db, &current, 500).await.unwrap();
        assert_eq!(
            settle_terminal(&db, &current).await.unwrap_err(),
            LastFmQueueError::StaleBatch
        );
        assert_eq!(queue_len(&db).await.unwrap(), 3);
        let refreshed = batch(&db, account, 500, 50).await;
        assert!(refreshed.rows().iter().all(|row| row.attempt_count == 1));
        settle_terminal(&db, &refreshed).await.unwrap();
        assert_eq!(queue_len(&db).await.unwrap(), 0);
    }

    #[test]
    fn input_and_receipt_diagnostics_are_content_free() {
        let account = binding("listener");
        assert_eq!(
            PendingLastFmScrobble::try_new(
                Uuid::new_v4(),
                account,
                "Artist".to_owned(),
                "Track".to_owned(),
                None,
                None,
                None,
                30,
                0,
            )
            .unwrap_err(),
            LastFmQueueError::InvalidInput
        );
        let private = input(account, "Private Title");
        let diagnostics = format!("{private:?}");
        assert!(!diagnostics.contains("Private"));
        assert!(!diagnostics.contains("LastFmAccountBinding"));
    }
}
