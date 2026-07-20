//! Versioned, content-redacted playlist-sidebar projection.
//!
//! SQLite owns the monotonic revision. This module turns one revision and its
//! ordered playlist/link rows into a complete snapshot inside one explicit
//! read transaction, then publishes only strictly newer snapshots. Refresh
//! requests are hints: their capacity-one channel deliberately coalesces, and
//! periodic revision polling recovers from lost hints and direct SQL writers.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbErr, QueryResult, Statement, TransactionTrait,
    TryGetable,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::db::entities::server_playlist_link::{
    self, ServerPlaylistLocalState, ServerPlaylistRemoteState, StoredServerPlaylistLink,
    MAX_SERVER_PLAYLIST_LINK_NAME_BYTES,
};

const REVISION_QUERY: &str = "SELECT revision FROM playlist_sidebar_revision WHERE singleton = 1";
const SNAPSHOT_QUERY: &str = r"
SELECT
    p.id AS playlist_id,
    p.name AS playlist_name,
    p.is_smart AS playlist_is_smart,
    l.playlist_id AS link_playlist_id,
    l.source_id AS link_source_id,
    l.native_playlist_id AS link_native_playlist_id,
    l.mode AS link_mode,
    l.last_synced_name AS link_last_synced_name,
    l.digest_version AS link_digest_version,
    l.membership_digest AS link_membership_digest,
    l.last_success_at_ms AS link_last_success_at_ms,
    l.local_state AS link_local_state,
    l.remote_state AS link_remote_state,
    l.state_revision AS link_state_revision
FROM playlists AS p
LEFT JOIN server_playlist_links AS l ON l.playlist_id = p.id
ORDER BY p.created_at ASC, p.id ASC
";

/// Fallback cadence for direct SQL mutations or a lost refresh hint.
const PLAYLIST_SIDEBAR_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Authoritative mutability and link-state presentation for one playlist row.
///
/// A pull mirror remains a regular-playlist navigation target, but this typed
/// value prevents consumers from inferring editability from compatibility UI
/// strings. Native server-playlist identity is deliberately absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistSidebarKind {
    EditableRegular,
    EditableSmart,
    PullMirror {
        local_state: ServerPlaylistLocalState,
        remote_state: ServerPlaylistRemoteState,
    },
}

/// One content-redacted playlist sidebar row.
///
/// The local playlist ID and effective display name remain private. Debug
/// output exposes only their byte lengths, and native server identity is never
/// represented by this type at all.
#[derive(Clone, Eq, PartialEq)]
pub struct PlaylistSidebarEntry {
    playlist_id: String,
    name: String,
    kind: PlaylistSidebarKind,
}

impl PlaylistSidebarEntry {
    pub fn new(
        playlist_id: impl Into<String>,
        name: impl Into<String>,
        kind: PlaylistSidebarKind,
    ) -> Self {
        Self {
            playlist_id: playlist_id.into(),
            name: name.into(),
            kind,
        }
    }

    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn kind(&self) -> PlaylistSidebarKind {
        self.kind
    }
}

impl fmt::Debug for PlaylistSidebarEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaylistSidebarEntry")
            .field("playlist_id_byte_len", &self.playlist_id.len())
            .field("name_byte_len", &self.name.len())
            .field("kind", &self.kind)
            .finish()
    }
}

/// Validated durable playlist-sidebar revision.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PlaylistSidebarRevision(i64);

impl PlaylistSidebarRevision {
    pub const fn new(value: i64) -> Result<Self, PlaylistSidebarRevisionError> {
        if value < 0 {
            Err(PlaylistSidebarRevisionError)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A revision cannot be negative.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("playlist sidebar revision is invalid")]
pub struct PlaylistSidebarRevisionError;

/// Complete presentation state for one durable revision.
///
/// `Unavailable` is closed and content-free. It retracts previously published
/// editable rows when a revision can be read coherently but its joined model
/// cannot be decoded or validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistSidebarState {
    Ready(Vec<PlaylistSidebarEntry>),
    Unavailable,
}

/// One coherent, versioned playlist-sidebar snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistSidebarSnapshot {
    revision: PlaylistSidebarRevision,
    state: PlaylistSidebarState,
}

impl PlaylistSidebarSnapshot {
    /// Build a full state at an already validated revision.
    ///
    /// Runtime publication should use [`load_playlist_sidebar_snapshot`]; the
    /// constructor is also useful for pure ordering/reducer tests that never
    /// claim database authority.
    pub fn new(revision: PlaylistSidebarRevision, state: PlaylistSidebarState) -> Self {
        Self { revision, state }
    }

    pub const fn revision(&self) -> PlaylistSidebarRevision {
        self.revision
    }

    pub const fn state(&self) -> &PlaylistSidebarState {
        &self.state
    }

    pub fn into_state(self) -> PlaylistSidebarState {
        self.state
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlaylistSidebarModelError;

/// Load the revision and complete ordered joined projection in one explicit
/// read transaction.
///
/// Revision, schema, SQL, connection, and transaction failures return `Err`,
/// so callers retain their previous applicable snapshot and retry. Once a
/// valid revision has been observed, joined row decoding or model validation
/// instead produces a versioned `Unavailable` snapshot, retracting stale UI
/// authority without revealing corrupt or server-controlled content.
pub async fn load_playlist_sidebar_snapshot<C>(db: &C) -> Result<PlaylistSidebarSnapshot, DbErr>
where
    C: ConnectionTrait + TransactionTrait,
{
    load_playlist_sidebar_snapshot_inner(db, None).await
}

async fn load_playlist_sidebar_snapshot_inner<C>(
    db: &C,
    #[cfg(test)] pause_after_revision: Option<&SnapshotReadPause>,
    #[cfg(not(test))] _pause_after_revision: Option<&()>,
) -> Result<PlaylistSidebarSnapshot, DbErr>
where
    C: ConnectionTrait + TransactionTrait,
{
    let transaction = db.begin().await?;
    let backend = transaction.get_database_backend();
    let revision = query_revision(&transaction).await?;

    #[cfg(test)]
    if let Some(pause) = pause_after_revision {
        pause
            .entered
            .send(())
            .await
            .expect("snapshot read pause observer must remain open");
        pause
            .resume
            .recv()
            .await
            .expect("snapshot read pause controller must remain open");
    }

    let rows = transaction
        .query_all(Statement::from_string(backend, SNAPSHOT_QUERY))
        .await?;
    let state = match decode_snapshot_rows(rows) {
        Ok(entries) => PlaylistSidebarState::Ready(entries),
        Err(PlaylistSidebarModelError) => PlaylistSidebarState::Unavailable,
    };
    transaction.commit().await?;
    Ok(PlaylistSidebarSnapshot::new(revision, state))
}

async fn query_revision<C>(db: &C) -> Result<PlaylistSidebarRevision, DbErr>
where
    C: ConnectionTrait,
{
    let mut rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            REVISION_QUERY,
        ))
        .await?;
    if rows.len() != 1 {
        return Err(DbErr::Custom(
            "Playlist sidebar revision singleton is invalid".to_string(),
        ));
    }
    let row = rows.pop().ok_or_else(|| {
        DbErr::Custom("Playlist sidebar revision singleton is invalid".to_string())
    })?;
    let value = row
        .try_get::<i64>("", "revision")
        .map_err(|_| DbErr::Custom("Playlist sidebar revision value is malformed".to_string()))?;
    PlaylistSidebarRevision::new(value)
        .map_err(|_| DbErr::Custom("Playlist sidebar revision value is invalid".to_string()))
}

fn decode_snapshot_rows(
    rows: Vec<QueryResult>,
) -> Result<Vec<PlaylistSidebarEntry>, PlaylistSidebarModelError> {
    rows.into_iter().map(decode_snapshot_row).collect()
}

fn decode_snapshot_row(
    row: QueryResult,
) -> Result<PlaylistSidebarEntry, PlaylistSidebarModelError> {
    let playlist_id: String = decode(&row, "playlist_id")?;
    let name: String = decode(&row, "playlist_name")?;
    let link_playlist_id: Option<String> = decode(&row, "link_playlist_id")?;

    // Link presence is checked before the legacy smart flag. Even a damaged
    // parent `is_smart` value therefore cannot make a pull mirror editable.
    let kind = if let Some(link_playlist_id) = link_playlist_id {
        let link = server_playlist_link::Model {
            playlist_id: link_playlist_id,
            source_id: decode(&row, "link_source_id")?,
            native_playlist_id: decode(&row, "link_native_playlist_id")?,
            mode: decode(&row, "link_mode")?,
            last_synced_name: decode(&row, "link_last_synced_name")?,
            digest_version: decode(&row, "link_digest_version")?,
            membership_digest: decode(&row, "link_membership_digest")?,
            last_success_at_ms: decode(&row, "link_last_success_at_ms")?,
            local_state: decode(&row, "link_local_state")?,
            remote_state: decode(&row, "link_remote_state")?,
            state_revision: decode(&row, "link_state_revision")?,
        };
        let link =
            StoredServerPlaylistLink::try_from(link).map_err(|_| PlaylistSidebarModelError)?;
        if link.playlist_id != playlist_id || name.len() > MAX_SERVER_PLAYLIST_LINK_NAME_BYTES {
            return Err(PlaylistSidebarModelError);
        }
        PlaylistSidebarKind::PullMirror {
            local_state: link.local_state,
            remote_state: link.remote_state,
        }
    } else {
        match decode::<i64>(&row, "playlist_is_smart")? {
            0 => PlaylistSidebarKind::EditableRegular,
            1 => PlaylistSidebarKind::EditableSmart,
            _ => return Err(PlaylistSidebarModelError),
        }
    };

    Ok(PlaylistSidebarEntry::new(playlist_id, name, kind))
}

fn decode<T>(row: &QueryResult, column: &str) -> Result<T, PlaylistSidebarModelError>
where
    T: TryGetable,
{
    row.try_get("", column)
        .map_err(|_| PlaylistSidebarModelError)
}

/// Result of one nonblocking refresh request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistSidebarRefreshRequest {
    Requested,
    /// A request is already pending; that request covers this hint.
    Coalesced,
    /// The publisher has stopped or its owner explicitly closed the lane.
    Closed,
}

struct PlaylistSidebarRefreshInner {
    sender: async_channel::Sender<()>,
    shutdown: CancellationToken,
}

impl Drop for PlaylistSidebarRefreshInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Cloneable, capacity-one, nonblocking refresh-hint sender.
#[derive(Clone)]
pub struct PlaylistSidebarRefresh {
    inner: Arc<PlaylistSidebarRefreshInner>,
}

impl PlaylistSidebarRefresh {
    /// Request a complete refresh without waiting on database or channel work.
    /// A full lane is successful coalescing rather than an error.
    pub fn request(&self) -> PlaylistSidebarRefreshRequest {
        match self.inner.sender.try_send(()) {
            Ok(()) => PlaylistSidebarRefreshRequest::Requested,
            Err(async_channel::TrySendError::Full(())) => PlaylistSidebarRefreshRequest::Coalesced,
            Err(async_channel::TrySendError::Closed(())) => PlaylistSidebarRefreshRequest::Closed,
        }
    }

    /// Close the shared lane and cancel the publisher, including while it is
    /// waiting to deliver into a bounded output channel.
    pub fn close(&self) -> bool {
        let closed = self.inner.sender.close();
        self.inner.shutdown.cancel();
        closed
    }

    pub fn is_closed(&self) -> bool {
        self.inner.sender.is_closed()
    }
}

impl fmt::Debug for PlaylistSidebarRefresh {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaylistSidebarRefresh")
            .field("closed", &self.is_closed())
            .field("pending", &!self.inner.sender.is_empty())
            .finish()
    }
}

/// Single-consumer half of the playlist-sidebar refresh lane.
pub struct PlaylistSidebarRefreshReceiver {
    receiver: async_channel::Receiver<()>,
    shutdown: CancellationToken,
}

impl fmt::Debug for PlaylistSidebarRefreshReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaylistSidebarRefreshReceiver")
            .field("closed", &self.receiver.is_closed())
            .finish_non_exhaustive()
    }
}

/// Create the capacity-one refresh lane consumed by one publisher task.
pub fn playlist_sidebar_refresh_channel() -> (PlaylistSidebarRefresh, PlaylistSidebarRefreshReceiver)
{
    let (sender, receiver) = async_channel::bounded(1);
    let shutdown = CancellationToken::new();
    (
        PlaylistSidebarRefresh {
            inner: Arc::new(PlaylistSidebarRefreshInner {
                sender,
                shutdown: shutdown.clone(),
            }),
        },
        PlaylistSidebarRefreshReceiver { receiver, shutdown },
    )
}

/// Immediately load and then maintain a strictly increasing full-snapshot
/// stream until its refresh owner or output is closed.
pub async fn run_playlist_sidebar_publisher(
    db: DatabaseConnection,
    refresh: PlaylistSidebarRefreshReceiver,
    output: async_channel::Sender<PlaylistSidebarSnapshot>,
) {
    run_playlist_sidebar_publisher_with_interval(
        db,
        refresh,
        output,
        PLAYLIST_SIDEBAR_POLL_INTERVAL,
    )
    .await;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublishAttempt {
    Continue,
    Retry,
    Stop,
}

async fn run_playlist_sidebar_publisher_with_interval(
    db: DatabaseConnection,
    refresh: PlaylistSidebarRefreshReceiver,
    output: async_channel::Sender<PlaylistSidebarSnapshot>,
    poll_interval: Duration,
) {
    let mut last_published = None;
    let mut retry_pending = match publish_current_snapshot(
        &db,
        &refresh.shutdown,
        &output,
        &mut last_published,
    )
    .await
    {
        PublishAttempt::Continue => false,
        PublishAttempt::Retry => true,
        PublishAttempt::Stop => return,
    };

    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // `interval`'s first tick is immediate; the explicit load above already
    // owns that initial observation.
    poll.tick().await;

    loop {
        let should_load = tokio::select! {
            biased;
            () = refresh.shutdown.cancelled() => return,
            request = refresh.receiver.recv() => {
                if request.is_err() {
                    return;
                }
                true
            }
            _ = poll.tick() => {
                retry_pending || poll_observed_newer_revision(&db, last_published).await
            }
        };
        if !should_load {
            continue;
        }
        retry_pending =
            match publish_current_snapshot(&db, &refresh.shutdown, &output, &mut last_published)
                .await
            {
                PublishAttempt::Continue => false,
                PublishAttempt::Retry => true,
                PublishAttempt::Stop => return,
            };
    }
}

async fn poll_observed_newer_revision(
    db: &DatabaseConnection,
    last_published: Option<PlaylistSidebarRevision>,
) -> bool {
    match query_revision(db).await {
        Ok(revision) => last_published.is_none_or(|last| revision > last),
        Err(_) => {
            warn!("Playlist sidebar revision poll failed; retrying later");
            false
        }
    }
}

async fn publish_current_snapshot(
    db: &DatabaseConnection,
    shutdown: &CancellationToken,
    output: &async_channel::Sender<PlaylistSidebarSnapshot>,
    last_published: &mut Option<PlaylistSidebarRevision>,
) -> PublishAttempt {
    let snapshot = tokio::select! {
        biased;
        () = shutdown.cancelled() => return PublishAttempt::Stop,
        result = load_playlist_sidebar_snapshot(db) => match result {
            Ok(snapshot) => snapshot,
            Err(_) => {
                warn!("Playlist sidebar snapshot load failed; retrying later");
                return PublishAttempt::Retry;
            }
        },
    };
    if last_published.is_some_and(|last| snapshot.revision() <= last) {
        return PublishAttempt::Continue;
    }

    let revision = snapshot.revision();
    let sent = tokio::select! {
        biased;
        () = shutdown.cancelled() => return PublishAttempt::Stop,
        result = output.send(snapshot) => result.is_ok(),
    };
    if !sent {
        return PublishAttempt::Stop;
    }
    *last_published = Some(revision);
    PublishAttempt::Continue
}

#[cfg(test)]
struct SnapshotReadPause {
    entered: async_channel::Sender<()>,
    resume: async_channel::Receiver<()>,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};

    use super::*;

    async fn migrated_database() -> DatabaseConnection {
        use sea_orm_migration::MigratorTrait;

        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open playlist sidebar database");
        crate::db::migration::Migrator::up(&db, None)
            .await
            .expect("run playlist sidebar migrations");
        db
    }

    async fn migrated_file_database() -> (tempfile::TempDir, DatabaseConnection) {
        use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
        use sea_orm::SqlxSqliteConnector;
        use sea_orm_migration::MigratorTrait;

        let directory = tempfile::tempdir().expect("create playlist sidebar database directory");
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(directory.path().join("sidebar.db"))
                    .create_if_missing(true)
                    .foreign_keys(true)
                    .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .expect("open pooled playlist sidebar database");
        let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
        crate::db::migration::Migrator::up(&db, None)
            .await
            .expect("run pooled playlist sidebar migrations");
        (directory, db)
    }

    async fn execute(db: &DatabaseConnection, sql: impl Into<String>) {
        db.execute(Statement::from_string(DbBackend::Sqlite, sql.into()))
            .await
            .expect("execute playlist sidebar fixture SQL");
    }

    fn sql_string(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    async fn insert_playlist(
        db: &DatabaseConnection,
        id: &str,
        name: &str,
        is_smart: i64,
        created_at: &str,
    ) {
        execute(
            db,
            format!(
                "INSERT INTO playlists (id,name,is_smart,smart_rules_json,limit_enabled,limit_value,limit_unit,limit_sort,match_mode,live_updating,created_at,updated_at) VALUES ({},{},{},NULL,0,NULL,NULL,NULL,'all',1,{},{})",
                sql_string(id),
                sql_string(name),
                is_smart,
                sql_string(created_at),
                sql_string(created_at),
            ),
        )
        .await;
    }

    async fn insert_link(db: &DatabaseConnection, playlist_id: &str, native_id: &str) {
        let source_id = crate::architecture::SourceId::random().to_string();
        execute(
            db,
            format!(
                "INSERT INTO server_playlist_links (playlist_id,source_id,native_playlist_id,mode,last_synced_name,digest_version,membership_digest,last_success_at_ms,local_state,remote_state,state_revision) VALUES ({},{},{},'pull_read_only_v1','Server copy',1,zeroblob(32),1,'conflict','missing',4)",
                sql_string(playlist_id),
                sql_string(&source_id),
                sql_string(native_id),
            ),
        )
        .await;
    }

    #[test]
    fn revision_validation_is_closed_and_nonnegative() {
        assert_eq!(PlaylistSidebarRevision::new(0).unwrap().value(), 0);
        assert_eq!(
            PlaylistSidebarRevision::new(i64::MAX).unwrap().value(),
            i64::MAX
        );
        assert_eq!(
            PlaylistSidebarRevision::new(-1),
            Err(PlaylistSidebarRevisionError)
        );
    }

    #[test]
    fn entry_and_snapshot_debug_are_content_redacted() {
        let entry = PlaylistSidebarEntry::new(
            "private-local-id",
            "private server name",
            PlaylistSidebarKind::EditableRegular,
        );
        let diagnostic = format!(
            "{:?}",
            PlaylistSidebarSnapshot::new(
                PlaylistSidebarRevision::new(8).unwrap(),
                PlaylistSidebarState::Ready(vec![entry]),
            )
        );
        assert!(!diagnostic.contains("private-local-id"));
        assert!(!diagnostic.contains("private server name"));
        assert!(diagnostic.contains("playlist_id_byte_len"));
    }

    #[tokio::test]
    async fn first_revision_zero_is_a_ready_ordered_typed_snapshot() {
        let db = migrated_database().await;
        // Restore the migration's initial value after fixture writes so this
        // test covers revision zero as a valid first publication.
        insert_playlist(&db, "z", "Later by ID", 0, "2026-07-20T00:00:01Z").await;
        insert_playlist(&db, "b", "Second", 1, "2026-07-20T00:00:00Z").await;
        insert_playlist(&db, "a", "First", 0, "2026-07-20T00:00:00Z").await;
        execute(
            &db,
            "UPDATE playlist_sidebar_revision SET revision = 0 WHERE singleton = 1",
        )
        .await;

        let snapshot = load_playlist_sidebar_snapshot(&db).await.unwrap();
        assert_eq!(snapshot.revision().value(), 0);
        let PlaylistSidebarState::Ready(entries) = snapshot.state() else {
            panic!("expected ready sidebar projection");
        };
        assert_eq!(
            entries
                .iter()
                .map(PlaylistSidebarEntry::playlist_id)
                .collect::<Vec<_>>(),
            ["a", "b", "z"]
        );
        assert_eq!(entries[0].kind(), PlaylistSidebarKind::EditableRegular);
        assert_eq!(entries[1].kind(), PlaylistSidebarKind::EditableSmart);
    }

    #[tokio::test]
    async fn revision_and_rows_share_one_coherent_concurrent_read_snapshot() {
        let (_directory, db) = migrated_file_database().await;
        let (entered_tx, entered_rx) = async_channel::bounded(1);
        let (resume_tx, resume_rx) = async_channel::bounded(1);
        let pause = SnapshotReadPause {
            entered: entered_tx,
            resume: resume_rx,
        };
        let reader_db = db.clone();
        let reader = tokio::spawn(async move {
            load_playlist_sidebar_snapshot_inner(&reader_db, Some(&pause)).await
        });

        entered_rx
            .recv()
            .await
            .expect("reader observed the initial revision");
        tokio::time::timeout(
            Duration::from_secs(2),
            insert_playlist(
                &db,
                "concurrent",
                "Committed after revision read",
                0,
                "2026-07-20T00:00:00Z",
            ),
        )
        .await
        .expect("WAL writer commits while the older read snapshot is open");
        resume_tx.send(()).await.expect("resume snapshot reader");

        let before = reader
            .await
            .expect("snapshot task remains healthy")
            .unwrap();
        assert_eq!(before.revision().value(), 0);
        assert!(matches!(
            before.state(),
            PlaylistSidebarState::Ready(entries) if entries.is_empty()
        ));

        let after = load_playlist_sidebar_snapshot(&db).await.unwrap();
        assert!(after.revision() > before.revision());
        assert!(matches!(
            after.state(),
            PlaylistSidebarState::Ready(entries)
                if entries.len() == 1 && entries[0].playlist_id() == "concurrent"
        ));
    }

    #[tokio::test]
    async fn valid_link_presence_wins_corrupt_parent_and_native_id_never_escapes() {
        let db = migrated_database().await;
        insert_playlist(
            &db,
            "linked",
            "Private mirrored name",
            1,
            "2026-07-20T00:00:00Z",
        )
        .await;
        insert_link(&db, "linked", "private-native-id").await;
        // A non-boolean parent value would be unavailable for an ordinary row.
        // The linked row must not inspect it or become editable.
        execute(&db, "PRAGMA ignore_check_constraints = ON").await;
        execute(&db, "UPDATE playlists SET is_smart = 7 WHERE id = 'linked'").await;
        execute(&db, "PRAGMA ignore_check_constraints = OFF").await;

        let snapshot = load_playlist_sidebar_snapshot(&db).await.unwrap();
        let PlaylistSidebarState::Ready(entries) = snapshot.state() else {
            panic!("valid linked row must remain ready");
        };
        assert_eq!(
            entries[0].kind(),
            PlaylistSidebarKind::PullMirror {
                local_state: ServerPlaylistLocalState::Conflict,
                remote_state: ServerPlaylistRemoteState::Missing,
            }
        );
        let diagnostic = format!("{snapshot:?}");
        assert!(!diagnostic.contains("private-native-id"));
        assert!(!diagnostic.contains("Private mirrored name"));
    }

    #[tokio::test]
    async fn malformed_join_and_oversized_linked_name_are_versioned_unavailable() {
        let db = migrated_database().await;
        insert_playlist(&db, "linked", "Server copy", 0, "2026-07-20T00:00:00Z").await;
        insert_link(&db, "linked", "native-id").await;
        execute(&db, "PRAGMA ignore_check_constraints = ON").await;
        execute(
            &db,
            "UPDATE server_playlist_links SET local_state = 'broken' WHERE playlist_id = 'linked'",
        )
        .await;
        execute(&db, "PRAGMA ignore_check_constraints = OFF").await;
        let malformed = load_playlist_sidebar_snapshot(&db).await.unwrap();
        assert!(matches!(
            malformed.state(),
            PlaylistSidebarState::Unavailable
        ));

        execute(&db, "PRAGMA ignore_check_constraints = ON").await;
        execute(
            &db,
            "UPDATE server_playlist_links SET local_state = 'clean' WHERE playlist_id = 'linked'",
        )
        .await;
        let oversized = "x".repeat(MAX_SERVER_PLAYLIST_LINK_NAME_BYTES + 1);
        execute(
            &db,
            format!(
                "UPDATE playlists SET name = {} WHERE id = 'linked'",
                sql_string(&oversized)
            ),
        )
        .await;
        execute(&db, "PRAGMA ignore_check_constraints = OFF").await;
        let oversized = load_playlist_sidebar_snapshot(&db).await.unwrap();
        assert!(matches!(
            oversized.state(),
            PlaylistSidebarState::Unavailable
        ));
        assert!(oversized.revision() > malformed.revision());
    }

    #[tokio::test]
    async fn revision_and_schema_failures_return_errors_without_snapshots() {
        let db = migrated_database().await;
        execute(&db, "PRAGMA ignore_check_constraints = ON").await;
        execute(
            &db,
            "UPDATE playlist_sidebar_revision SET revision = -1 WHERE singleton = 1",
        )
        .await;
        execute(&db, "PRAGMA ignore_check_constraints = OFF").await;
        assert!(load_playlist_sidebar_snapshot(&db).await.is_err());

        execute(&db, "DROP TABLE playlist_sidebar_revision").await;
        assert!(load_playlist_sidebar_snapshot(&db).await.is_err());
    }

    #[test]
    fn refresh_channel_is_capacity_one_nonblocking_and_closes_globally() {
        let (refresh, _receiver) = playlist_sidebar_refresh_channel();
        let clone = refresh.clone();
        assert_eq!(refresh.request(), PlaylistSidebarRefreshRequest::Requested);
        assert_eq!(clone.request(), PlaylistSidebarRefreshRequest::Coalesced);
        assert!(refresh.close());
        assert!(!clone.close());
        assert_eq!(clone.request(), PlaylistSidebarRefreshRequest::Closed);
    }

    #[tokio::test]
    async fn publisher_immediately_emits_zero_coalesces_and_stops_cleanly() {
        let db = migrated_database().await;
        let (refresh, receiver) = playlist_sidebar_refresh_channel();
        let (output, snapshots) = async_channel::bounded(1);
        let task = tokio::spawn(run_playlist_sidebar_publisher_with_interval(
            db,
            receiver,
            output,
            Duration::from_secs(30),
        ));

        let first = snapshots.recv().await.unwrap();
        assert_eq!(first.revision().value(), 0);
        assert_eq!(refresh.request(), PlaylistSidebarRefreshRequest::Requested);
        assert!(matches!(
            refresh.request(),
            PlaylistSidebarRefreshRequest::Requested | PlaylistSidebarRefreshRequest::Coalesced
        ));
        assert!(refresh.close());
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("publisher stops promptly")
            .expect("publisher task remains healthy");
    }

    #[tokio::test]
    async fn owner_close_cancels_a_publisher_blocked_on_bounded_output() {
        let db = migrated_database().await;
        let (refresh, receiver) = playlist_sidebar_refresh_channel();
        let (output, _snapshots) = async_channel::bounded(1);
        output
            .send(PlaylistSidebarSnapshot::new(
                PlaylistSidebarRevision::new(0).unwrap(),
                PlaylistSidebarState::Unavailable,
            ))
            .await
            .unwrap();
        let task = tokio::spawn(run_playlist_sidebar_publisher_with_interval(
            db,
            receiver,
            output,
            Duration::from_secs(30),
        ));
        tokio::task::yield_now().await;
        assert!(refresh.close());
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("owner close cancels a blocked delivery")
            .expect("publisher task remains healthy");
    }

    #[tokio::test]
    async fn publisher_poll_recovers_raw_mutation_and_emits_only_newer_snapshots() {
        let db = migrated_database().await;
        let (refresh, receiver) = playlist_sidebar_refresh_channel();
        let (output, snapshots) = async_channel::unbounded();
        let task = tokio::spawn(run_playlist_sidebar_publisher_with_interval(
            db.clone(),
            receiver,
            output,
            Duration::from_millis(20),
        ));
        let first = snapshots.recv().await.unwrap();
        assert_eq!(first.revision().value(), 0);

        insert_playlist(&db, "raw", "Raw writer", 0, "2026-07-20T00:00:00Z").await;
        let updated = tokio::time::timeout(Duration::from_secs(2), snapshots.recv())
            .await
            .expect("poll observes raw SQL mutation")
            .unwrap();
        assert!(updated.revision() > first.revision());
        assert!(matches!(
            updated.state(),
            PlaylistSidebarState::Ready(entries) if entries.len() == 1
        ));

        assert_eq!(refresh.request(), PlaylistSidebarRefreshRequest::Requested);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(snapshots.is_empty(), "equal revisions are not republished");
        refresh.close();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn publisher_retries_error_without_advancing_then_publishes_repair() {
        let db = migrated_database().await;
        execute(&db, "PRAGMA ignore_check_constraints = ON").await;
        execute(
            &db,
            "UPDATE playlist_sidebar_revision SET revision = -1 WHERE singleton = 1",
        )
        .await;
        execute(&db, "PRAGMA ignore_check_constraints = OFF").await;

        let (refresh, receiver) = playlist_sidebar_refresh_channel();
        let (output, snapshots) = async_channel::unbounded();
        let task = tokio::spawn(run_playlist_sidebar_publisher_with_interval(
            db.clone(),
            receiver,
            output,
            Duration::from_millis(20),
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(snapshots.is_empty());

        execute(&db, "PRAGMA ignore_check_constraints = ON").await;
        execute(
            &db,
            "UPDATE playlist_sidebar_revision SET revision = 3 WHERE singleton = 1",
        )
        .await;
        execute(&db, "PRAGMA ignore_check_constraints = OFF").await;
        let repaired = tokio::time::timeout(Duration::from_secs(2), snapshots.recv())
            .await
            .expect("publisher retries after revision repair")
            .unwrap();
        assert_eq!(repaired.revision().value(), 3);
        refresh.close();
        task.await.unwrap();
    }
}
