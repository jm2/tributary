//! Migration: enforce `UNIQUE(playlist_id, position)` on `playlist_entries`.
//!
//! A prior bug could let two entries share a position within the same
//! playlist. Before adding the UNIQUE index we first renumber every
//! playlist's entries to a clean `0..N` sequence (ordered by current
//! position, ties broken by id) so any pre-existing duplicates are
//! resolved and the index can be created safely.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{Statement, TransactionTrait};

const UNIQUE_POSITION_INDEX: &str = "idx_playlist_entries_playlist_position_unique";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // SeaORM wraps PostgreSQL migration batches in a transaction, but not
        // SQLite ones. Own this transaction so a failed index creation cannot
        // leave the preceding position rewrite applied.
        let transaction = manager.get_connection().begin().await?;
        let result = {
            let manager = SchemaManager::new(&transaction);
            normalize_positions_and_create_index(&manager).await
        };

        match result {
            Ok(()) => transaction.commit().await,
            Err(error) => {
                transaction.rollback().await?;
                Err(error)
            }
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name(UNIQUE_POSITION_INDEX)
                    .table(PlaylistEntries::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}

/// Normalize positions from an immutable snapshot, then enforce uniqueness.
///
/// SQLite's `UPDATE` statement does not provide snapshot isolation between a
/// target row and a correlated subquery that reads the same table. Computing
/// every rank in a temporary table first prevents earlier row updates from
/// changing the rank of later rows. The `(position, id)` ordering preserves
/// the existing playlist order and resolves duplicate positions
/// deterministically.
async fn normalize_positions_and_create_index(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    match existing_unique_position_index(manager).await? {
        ExistingIndex::Missing => {}
        ExistingIndex::Correct => return Ok(()),
        ExistingIndex::Conflicting(detail) => {
            return Err(DbErr::Migration(format!(
                "Index {UNIQUE_POSITION_INDEX} already exists but does not match the required \
                 UNIQUE(playlist_id, position) index: {detail}"
            )));
        }
    }

    manager
        .get_connection()
        .execute_unprepared(
            "CREATE TEMP TABLE tributary_playlist_position_snapshot (
                 entry_id TEXT PRIMARY KEY NOT NULL,
                 normalized_position INTEGER NOT NULL
             )",
        )
        .await?;

    manager
        .get_connection()
        .execute_unprepared(
            "INSERT INTO tributary_playlist_position_snapshot (
                 entry_id,
                 normalized_position
             )
             SELECT current.id,
                    (
                        SELECT COUNT(*)
                        FROM playlist_entries AS sibling
                        WHERE sibling.playlist_id = current.playlist_id
                          AND (
                                sibling.position < current.position
                             OR (sibling.position = current.position
                                 AND sibling.id < current.id)
                          )
                    )
             FROM playlist_entries AS current",
        )
        .await?;

    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE playlist_entries
             SET position = (
                 SELECT snapshot.normalized_position
                 FROM tributary_playlist_position_snapshot AS snapshot
                 WHERE snapshot.entry_id = playlist_entries.id
             )",
        )
        .await?;

    manager
        .create_index(
            Index::create()
                .name(UNIQUE_POSITION_INDEX)
                .table(PlaylistEntries::Table)
                .col(PlaylistEntries::PlaylistId)
                .col(PlaylistEntries::Position)
                .unique()
                .to_owned(),
        )
        .await?;

    manager
        .get_connection()
        .execute_unprepared("DROP TABLE tributary_playlist_position_snapshot")
        .await?;

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum ExistingIndex {
    Missing,
    Correct,
    Conflicting(String),
}

/// Inspect the named SQLite index without relying on `CREATE INDEX IF NOT
/// EXISTS`, which would silently accept a same-name non-unique or wrong-column
/// index. A correctly shaped index is accepted only when the data also has the
/// normalized `0..N` invariant produced by this migration.
async fn existing_unique_position_index(
    manager: &SchemaManager<'_>,
) -> Result<ExistingIndex, DbErr> {
    let backend = manager.get_database_backend();
    let connection = manager.get_connection();

    let owner = connection
        .query_one(Statement::from_string(
            backend,
            format!(
                "SELECT tbl_name
                 FROM sqlite_master
                 WHERE type = 'index'
                   AND name = '{UNIQUE_POSITION_INDEX}'"
            ),
        ))
        .await?;
    let Some(owner) = owner else {
        return Ok(ExistingIndex::Missing);
    };

    let table: String = owner.try_get("", "tbl_name")?;
    if table != "playlist_entries" {
        return Ok(ExistingIndex::Conflicting(format!(
            "it belongs to table {table}"
        )));
    }

    let index_rows = connection
        .query_all(Statement::from_string(
            backend,
            "PRAGMA index_list('playlist_entries')".to_string(),
        ))
        .await?;
    let Some(index_row) = index_rows.into_iter().find(|row| {
        row.try_get::<String>("", "name")
            .is_ok_and(|name| name == UNIQUE_POSITION_INDEX)
    }) else {
        return Ok(ExistingIndex::Conflicting(
            "SQLite did not expose metadata for the named index".to_string(),
        ));
    };

    let is_unique: i32 = index_row.try_get("", "unique")?;
    let is_partial: i32 = index_row.try_get("", "partial").unwrap_or_default();

    let mut columns = connection
        .query_all(Statement::from_string(
            backend,
            format!("PRAGMA index_info('{UNIQUE_POSITION_INDEX}')"),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>((
                row.try_get::<i32>("", "seqno")?,
                row.try_get::<Option<String>>("", "name")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    columns.sort_by_key(|(sequence, _)| *sequence);
    let column_names = columns
        .into_iter()
        .map(|(_, name)| name)
        .collect::<Vec<_>>();
    let expected_columns = vec![
        Some("playlist_id".to_string()),
        Some("position".to_string()),
    ];

    if is_unique != 1 || is_partial != 0 || column_names != expected_columns {
        return Ok(ExistingIndex::Conflicting(format!(
            "unique={is_unique}, partial={is_partial}, columns={column_names:?}"
        )));
    }

    let invalid_positions = connection
        .query_one(Statement::from_string(
            backend,
            "SELECT 1 AS invalid
             FROM playlist_entries
             GROUP BY playlist_id
             HAVING MIN(position) <> 0
                 OR MAX(position) <> COUNT(*) - 1
                 OR COUNT(DISTINCT position) <> COUNT(*)
             LIMIT 1"
                .to_string(),
        ))
        .await?;
    if invalid_positions.is_some() {
        return Ok(ExistingIndex::Conflicting(
            "playlist positions are not normalized to contiguous 0..N sequences".to_string(),
        ));
    }

    Ok(ExistingIndex::Correct)
}

/// Column identifiers for the `playlist_entries` table.
#[derive(DeriveIden)]
enum PlaylistEntries {
    Table,
    PlaylistId,
    Position,
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement,
    };

    use super::*;
    use crate::db::migration::Migrator;

    async fn database_before_position_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, Some(3))
            .await
            .expect("apply migrations preceding position normalization");
        db
    }

    async fn insert_playlist(db: &DatabaseConnection, id: &str) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlists (id, name, created_at, updated_at)
             VALUES (?, ?, '2026-07-10T00:00:00Z', '2026-07-10T00:00:00Z')",
            [id.into(), format!("Playlist {id}").into()],
        ))
        .await
        .expect("insert playlist");
    }

    async fn try_insert_entry(
        db: &DatabaseConnection,
        playlist_id: &str,
        id: &str,
        position: i32,
    ) -> Result<(), DbErr> {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlist_entries (id, playlist_id, position)
             VALUES (?, ?, ?)",
            [id.into(), playlist_id.into(), position.into()],
        ))
        .await
        .map(|_| ())
    }

    async fn insert_entry(db: &DatabaseConnection, playlist_id: &str, id: &str, position: i32) {
        try_insert_entry(db, playlist_id, id, position)
            .await
            .expect("insert playlist entry");
    }

    async fn positions(db: &DatabaseConnection, playlist_id: &str) -> Vec<(String, i32)> {
        db.query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id, position
             FROM playlist_entries
             WHERE playlist_id = ?
             ORDER BY position, id",
            [playlist_id.into()],
        ))
        .await
        .expect("query playlist positions")
        .into_iter()
        .map(|row| {
            (
                row.try_get("", "id").expect("entry id"),
                row.try_get("", "position").expect("entry position"),
            )
        })
        .collect()
    }

    async fn apply_position_migration(db: &DatabaseConnection) -> Result<(), DbErr> {
        Migrator::up(db, Some(1)).await
    }

    async fn migration_is_applied(db: &DatabaseConnection) -> bool {
        let migration_name = Migration.name().to_string();
        Migrator::get_migration_models(db)
            .await
            .expect("query migration ledger")
            .iter()
            .any(|migration| migration.version == migration_name)
    }

    #[tokio::test]
    async fn normalizes_gaps_without_following_row_insertion_order() {
        let db = database_before_position_migration().await;
        insert_playlist(&db, "playlist-a").await;

        // Insert in reverse order to reproduce the original migration's
        // read-after-write failure. Position, not row insertion order, is the
        // intended playlist order.
        insert_entry(&db, "playlist-a", "entry-c", 30).await;
        insert_entry(&db, "playlist-a", "entry-b", 20).await;
        insert_entry(&db, "playlist-a", "entry-a", 10).await;

        apply_position_migration(&db)
            .await
            .expect("position migration succeeds");

        assert_eq!(
            positions(&db, "playlist-a").await,
            vec![
                ("entry-a".to_owned(), 0),
                ("entry-b".to_owned(), 1),
                ("entry-c".to_owned(), 2),
            ]
        );
    }

    #[tokio::test]
    async fn resolves_duplicate_positions_with_a_deterministic_tie_breaker() {
        let db = database_before_position_migration().await;
        insert_playlist(&db, "playlist-a").await;

        insert_entry(&db, "playlist-a", "entry-c", 10).await;
        insert_entry(&db, "playlist-a", "entry-a", 10).await;
        insert_entry(&db, "playlist-a", "entry-b", 3).await;

        apply_position_migration(&db)
            .await
            .expect("position migration succeeds");

        assert_eq!(
            positions(&db, "playlist-a").await,
            vec![
                ("entry-b".to_owned(), 0),
                ("entry-a".to_owned(), 1),
                ("entry-c".to_owned(), 2),
            ]
        );
    }

    #[tokio::test]
    async fn normalizes_each_playlist_independently() {
        let db = database_before_position_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_playlist(&db, "playlist-b").await;

        insert_entry(&db, "playlist-a", "a-second", 9).await;
        insert_entry(&db, "playlist-b", "b-second", 100).await;
        insert_entry(&db, "playlist-a", "a-first", -4).await;
        insert_entry(&db, "playlist-b", "b-first", 100).await;

        apply_position_migration(&db)
            .await
            .expect("position migration succeeds");

        assert_eq!(
            positions(&db, "playlist-a").await,
            vec![("a-first".to_owned(), 0), ("a-second".to_owned(), 1)]
        );
        assert_eq!(
            positions(&db, "playlist-b").await,
            vec![("b-first".to_owned(), 0), ("b-second".to_owned(), 1)]
        );
    }

    #[tokio::test]
    async fn migrates_an_empty_table_and_enforces_unique_positions() {
        let db = database_before_position_migration().await;

        apply_position_migration(&db)
            .await
            .expect("empty position migration succeeds");

        insert_playlist(&db, "playlist-a").await;
        insert_entry(&db, "playlist-a", "entry-a", 0).await;
        let duplicate = try_insert_entry(&db, "playlist-a", "entry-b", 0).await;
        assert!(duplicate.is_err(), "unique position index must be active");
    }

    #[tokio::test]
    async fn retries_after_schema_commit_before_migration_ledger_insert() {
        let db = database_before_position_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_entry(&db, "playlist-a", "entry-c", 30).await;
        insert_entry(&db, "playlist-a", "entry-a", 10).await;
        insert_entry(&db, "playlist-a", "entry-b", 20).await;

        // Apply the schema change directly, simulating a process that commits
        // Migration::up and exits before SeaORM inserts its ledger row.
        let manager = SchemaManager::new(&db);
        Migration
            .up(&manager)
            .await
            .expect("commit migration schema change directly");
        assert!(!migration_is_applied(&db).await);
        assert_eq!(
            positions(&db, "playlist-a").await,
            vec![
                ("entry-a".to_owned(), 0),
                ("entry-b".to_owned(), 1),
                ("entry-c".to_owned(), 2),
            ]
        );

        // The normal migrator retries `up`, recognizes the exact index and
        // normalized data, and records the previously missing ledger entry.
        apply_position_migration(&db)
            .await
            .expect("retry already-committed migration");
        assert!(migration_is_applied(&db).await);
        assert!(
            try_insert_entry(&db, "playlist-a", "entry-d", 2)
                .await
                .is_err(),
            "the accepted index must still enforce uniqueness"
        );
    }

    #[tokio::test]
    async fn correctly_shaped_index_does_not_hide_unnormalized_positions() {
        let db = database_before_position_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_entry(&db, "playlist-a", "entry-a", 10).await;
        insert_entry(&db, "playlist-a", "entry-b", 20).await;
        let original_positions = positions(&db, "playlist-a").await;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX idx_playlist_entries_playlist_position_unique
             ON playlist_entries (playlist_id, position)",
        )
        .await
        .expect("create correctly shaped index over gapped data");

        let error = apply_position_migration(&db)
            .await
            .expect_err("gapped positions must not be accepted as already migrated");
        assert!(
            error.to_string().contains("positions are not normalized"),
            "unexpected migration error: {error}"
        );
        assert_eq!(positions(&db, "playlist-a").await, original_positions);
        assert!(!migration_is_applied(&db).await);
    }

    #[tokio::test]
    async fn failed_index_creation_rolls_back_position_updates_and_can_be_retried() {
        let db = database_before_position_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_entry(&db, "playlist-a", "entry-c", 30).await;
        insert_entry(&db, "playlist-a", "entry-b", 20).await;
        insert_entry(&db, "playlist-a", "entry-a", 10).await;
        let original_positions = positions(&db, "playlist-a").await;

        // Reserve the migration's index name with a different, non-unique
        // index. Strict metadata inspection must reject it without rewriting
        // positions or silently treating it as the required index.
        db.execute_unprepared(
            "CREATE INDEX idx_playlist_entries_playlist_position_unique
             ON playlist_entries (id)",
        )
        .await
        .expect("create conflicting index");

        let error = apply_position_migration(&db)
            .await
            .expect_err("conflicting index must fail the migration");
        assert!(
            error.to_string().contains("does not match the required"),
            "unexpected migration error: {error}"
        );
        assert_eq!(positions(&db, "playlist-a").await, original_positions);

        let snapshot_count: i64 = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS count
                 FROM sqlite_temp_master
                 WHERE type = 'table'
                   AND name = 'tributary_playlist_position_snapshot'",
            ))
            .await
            .expect("query temporary schema")
            .expect("count row")
            .try_get("", "count")
            .expect("snapshot count");
        assert_eq!(snapshot_count, 0, "rollback must remove the snapshot table");

        db.execute_unprepared("DROP INDEX idx_playlist_entries_playlist_position_unique")
            .await
            .expect("remove conflicting index");
        apply_position_migration(&db)
            .await
            .expect("migration can be retried after a rollback");
        assert_eq!(
            positions(&db, "playlist-a").await,
            vec![
                ("entry-a".to_owned(), 0),
                ("entry-b".to_owned(), 1),
                ("entry-c".to_owned(), 2),
            ]
        );
    }

    #[tokio::test]
    async fn down_succeeds_when_index_is_already_missing_and_when_repeated() {
        let db = database_before_position_migration().await;
        apply_position_migration(&db)
            .await
            .expect("apply position migration");
        assert!(migration_is_applied(&db).await);

        // Simulate a committed DROP INDEX followed by a missing ledger delete.
        db.execute_unprepared("DROP INDEX idx_playlist_entries_playlist_position_unique")
            .await
            .expect("pre-drop migration index");
        Migrator::down(&db, Some(1))
            .await
            .expect("finish partially applied down migration");
        assert!(!migration_is_applied(&db).await);

        // Direct repeated teardown is also a no-op.
        let manager = SchemaManager::new(&db);
        Migration
            .down(&manager)
            .await
            .expect("repeat already-completed down migration");
        Migration
            .down(&manager)
            .await
            .expect("repeat down migration again");
    }
}
