//! Migration: create the bounded durable Last.fm scrobble queue.
//!
//! The queue is an offline-delivery boundary, not a playback-history mirror.
//! It stores only bounded submission metadata, an opaque occurrence identity,
//! a one-way account-binding digest, and retry state. Credentials, source identities,
//! locators, paths, endpoints, and server responses are structurally absent.

use std::fmt;

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{
    ConnectionTrait, DatabaseConnection, Statement, TransactionTrait,
};

const TABLE: &str = "lastfm_scrobble_queue";
const OCCURRENCE_INDEX: &str = "idx_lastfm_scrobble_queue_occurrence_id";
const ACCOUNT_FIFO_INDEX: &str = "idx_lastfm_scrobble_queue_account_binding_fifo";
const OCCURRENCE_ID_BYTES: usize = 16;
const ACCOUNT_BINDING_BYTES: usize = 32;
const MAX_TEXT_BYTES: usize = 1024;
const MAX_STARTED_AT_SECS: i64 = 253_402_300_799;
const MAX_RETRY_AT_MS: i64 = 253_402_300_799_999;
const MAX_ATTEMPT_COUNT: i32 = 31;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_queue(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_queue(manager, false).await
    }
}

/// Revalidate the privacy and ordering boundary on every startup even when
/// the migration ledger is already current.
pub(super) async fn revalidate(connection: &DatabaseConnection) -> Result<(), DbErr> {
    validate_schema(&SchemaManager::new(connection)).await
}

async fn migrate_queue(manager: &SchemaManager<'_>, create: bool) -> Result<(), DbErr> {
    let transaction = manager.get_connection().begin().await?;
    let result = {
        let manager = SchemaManager::new(&transaction);
        if create {
            create_or_validate(&manager).await
        } else {
            drop_if_lossless(&manager).await
        }
    };

    match result {
        Ok(()) => transaction.commit().await,
        Err(error) => {
            let rollback = transaction.rollback().await;
            Err(preserve_original_error(error, rollback))
        }
    }
}

fn preserve_original_error(original: DbErr, rollback: Result<(), DbErr>) -> DbErr {
    match rollback {
        Ok(()) => original,
        Err(rollback_error) => DbErr::Migration(format!(
            "{original}; additionally failed to roll back Last.fm queue migration: {rollback_error}"
        )),
    }
}

async fn create_or_validate(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    match object_type(manager, TABLE).await? {
        None => {
            manager
                .get_connection()
                .execute_unprepared(&canonical_table_sql())
                .await?;
            manager
                .get_connection()
                .execute_unprepared(&canonical_occurrence_index_sql())
                .await?;
            manager
                .get_connection()
                .execute_unprepared(&canonical_account_fifo_index_sql())
                .await?;
        }
        Some(object_type) if object_type == "table" => {}
        Some(object_type) => {
            return Err(DbErr::Migration(format!(
                "{TABLE} must be a table, found {object_type}"
            )));
        }
    }
    validate_schema(manager).await
}

async fn drop_if_lossless(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let Some(target_type) = object_type(manager, TABLE).await? else {
        return Ok(());
    };
    if target_type != "table" {
        return Err(DbErr::Migration(format!(
            "{TABLE} must be a table, found {target_type}"
        )));
    }
    validate_schema(manager).await?;

    let row = manager
        .get_connection()
        .query_one(Statement::from_string(
            manager.get_database_backend(),
            format!("SELECT COUNT(*) AS count FROM {TABLE}"),
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("failed to inspect Last.fm queue".to_owned()))?;
    let count: i64 = row.try_get("", "count")?;
    if count != 0 {
        return Err(DbErr::Migration(format!(
            "cannot downgrade {count} pending Last.fm scrobble row(s) losslessly"
        )));
    }

    manager
        .get_connection()
        .execute_unprepared(&format!("DROP TABLE {TABLE}"))
        .await?;
    if object_type(manager, TABLE).await?.is_some() {
        return Err(DbErr::Migration(format!(
            "{TABLE} remained after downgrade"
        )));
    }
    Ok(())
}

fn canonical_table_sql() -> String {
    format!(
        "CREATE TABLE {TABLE} (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             occurrence_id BLOB NOT NULL,
             account_binding BLOB NOT NULL,
             artist TEXT NOT NULL,
             track_title TEXT NOT NULL,
             album TEXT,
             album_artist TEXT,
             track_number INTEGER,
             duration_secs INTEGER NOT NULL,
             started_at_unix_secs INTEGER NOT NULL,
             attempt_count INTEGER NOT NULL DEFAULT 0,
             next_attempt_at_ms INTEGER NOT NULL DEFAULT 0,
             CONSTRAINT ck_lastfm_queue_occurrence_id CHECK (
                 typeof(occurrence_id) = 'blob'
                 AND length(occurrence_id) = {OCCURRENCE_ID_BYTES}
             ),
             CONSTRAINT ck_lastfm_queue_account_binding CHECK (
                 typeof(account_binding) = 'blob'
                 AND length(account_binding) = {ACCOUNT_BINDING_BYTES}
             ),
             CONSTRAINT ck_lastfm_queue_artist CHECK (
                 typeof(artist) = 'text'
                 AND length(CAST(artist AS BLOB)) BETWEEN 1 AND {MAX_TEXT_BYTES}
             ),
             CONSTRAINT ck_lastfm_queue_track_title CHECK (
                 typeof(track_title) = 'text'
                 AND length(CAST(track_title AS BLOB)) BETWEEN 1 AND {MAX_TEXT_BYTES}
             ),
             CONSTRAINT ck_lastfm_queue_album CHECK (
                 album IS NULL OR (
                     typeof(album) = 'text'
                     AND length(CAST(album AS BLOB)) BETWEEN 1 AND {MAX_TEXT_BYTES}
                 )
             ),
             CONSTRAINT ck_lastfm_queue_album_artist CHECK (
                 album_artist IS NULL OR (
                     typeof(album_artist) = 'text'
                     AND length(CAST(album_artist AS BLOB)) BETWEEN 1 AND {MAX_TEXT_BYTES}
                 )
             ),
             CONSTRAINT ck_lastfm_queue_track_number CHECK (
                 track_number IS NULL OR (
                     typeof(track_number) = 'integer'
                     AND track_number BETWEEN 1 AND 2147483647
                 )
             ),
             CONSTRAINT ck_lastfm_queue_duration CHECK (
                 typeof(duration_secs) = 'integer'
                 AND duration_secs BETWEEN 31 AND 2147483647
             ),
             CONSTRAINT ck_lastfm_queue_started_at CHECK (
                 typeof(started_at_unix_secs) = 'integer'
                 AND started_at_unix_secs BETWEEN 1 AND {MAX_STARTED_AT_SECS}
             ),
             CONSTRAINT ck_lastfm_queue_attempt_count CHECK (
                 typeof(attempt_count) = 'integer'
                 AND attempt_count BETWEEN 0 AND {MAX_ATTEMPT_COUNT}
             ),
             CONSTRAINT ck_lastfm_queue_next_attempt CHECK (
                 typeof(next_attempt_at_ms) = 'integer'
                 AND next_attempt_at_ms BETWEEN 0 AND {MAX_RETRY_AT_MS}
             )
         )"
    )
}

fn canonical_occurrence_index_sql() -> String {
    format!("CREATE UNIQUE INDEX {OCCURRENCE_INDEX} ON {TABLE} (occurrence_id)")
}

fn canonical_account_fifo_index_sql() -> String {
    format!("CREATE INDEX {ACCOUNT_FIFO_INDEX} ON {TABLE} (account_binding, id)")
}

async fn object_type(manager: &SchemaManager<'_>, name: &str) -> Result<Option<String>, DbErr> {
    manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT type FROM sqlite_master WHERE name = ?",
            [name.into()],
        ))
        .await?
        .map(|row| row.try_get("", "type"))
        .transpose()
}

type ColumnSchema = (i32, String, String, i32, Option<String>, i32);

async fn validate_schema(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    validate_columns(manager).await?;
    validate_table_sql(manager).await?;
    validate_indexes(manager).await?;
    validate_no_foreign_keys(manager).await?;
    validate_no_triggers(manager).await
}

async fn validate_columns(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let columns = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA table_info('{TABLE}')"),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok::<ColumnSchema, DbErr>((
                row.try_get("", "cid")?,
                row.try_get("", "name")?,
                row.try_get::<String>("", "type")?.to_ascii_lowercase(),
                row.try_get("", "notnull")?,
                row.try_get("", "dflt_value")?,
                row.try_get("", "pk")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let expected = vec![
        (0, "id".to_owned(), "integer".to_owned(), 0, None, 1),
        (1, "occurrence_id".to_owned(), "blob".to_owned(), 1, None, 0),
        (
            2,
            "account_binding".to_owned(),
            "blob".to_owned(),
            1,
            None,
            0,
        ),
        (3, "artist".to_owned(), "text".to_owned(), 1, None, 0),
        (4, "track_title".to_owned(), "text".to_owned(), 1, None, 0),
        (5, "album".to_owned(), "text".to_owned(), 0, None, 0),
        (6, "album_artist".to_owned(), "text".to_owned(), 0, None, 0),
        (
            7,
            "track_number".to_owned(),
            "integer".to_owned(),
            0,
            None,
            0,
        ),
        (
            8,
            "duration_secs".to_owned(),
            "integer".to_owned(),
            1,
            None,
            0,
        ),
        (
            9,
            "started_at_unix_secs".to_owned(),
            "integer".to_owned(),
            1,
            None,
            0,
        ),
        (
            10,
            "attempt_count".to_owned(),
            "integer".to_owned(),
            1,
            Some("0".to_owned()),
            0,
        ),
        (
            11,
            "next_attempt_at_ms".to_owned(),
            "integer".to_owned(),
            1,
            Some("0".to_owned()),
            0,
        ),
    ];
    if columns != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has an unexpected column schema: {columns:?}"
        )));
    }
    Ok(())
}

async fn validate_table_sql(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let row = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
            [TABLE.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("{TABLE} SQL is missing")))?;
    let actual: String = row.try_get("", "sql")?;
    if canonical_sql(&actual) != canonical_sql(&canonical_table_sql()) {
        return Err(DbErr::Migration(format!(
            "{TABLE} does not have the exact canonical table definition"
        )));
    }
    Ok(())
}

async fn validate_indexes(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let mut indexes = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_list('{TABLE}')"),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>((
                row.try_get::<String>("", "name")?,
                row.try_get::<i32>("", "unique")? == 1,
                row.try_get::<String>("", "origin")?,
                row.try_get::<i32>("", "partial")? == 1,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    indexes.sort_by(|left, right| left.0.cmp(&right.0));
    let expected = vec![
        (ACCOUNT_FIFO_INDEX.to_owned(), false, "c".to_owned(), false),
        (OCCURRENCE_INDEX.to_owned(), true, "c".to_owned(), false),
    ];
    if indexes != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has unexpected indexes: {indexes:?}"
        )));
    }

    validate_index(manager, OCCURRENCE_INDEX, &["occurrence_id"]).await?;
    validate_index(manager, ACCOUNT_FIFO_INDEX, &["account_binding", "id"]).await?;
    validate_index_sql(manager, OCCURRENCE_INDEX, &canonical_occurrence_index_sql()).await?;
    validate_index_sql(
        manager,
        ACCOUNT_FIFO_INDEX,
        &canonical_account_fifo_index_sql(),
    )
    .await
}

async fn validate_index(
    manager: &SchemaManager<'_>,
    index: &str,
    expected: &[&str],
) -> Result<(), DbErr> {
    let columns = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_info('{}')", index.replace('\'', "''")),
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    if columns
        .iter()
        .map(String::as_str)
        .ne(expected.iter().copied())
    {
        return Err(DbErr::Migration(format!(
            "{index} has unexpected columns: {columns:?}"
        )));
    }
    Ok(())
}

async fn validate_index_sql(
    manager: &SchemaManager<'_>,
    index: &str,
    expected: &str,
) -> Result<(), DbErr> {
    let row = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?",
            [index.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("{index} SQL is missing")))?;
    let actual: String = row.try_get("", "sql")?;
    if canonical_sql(&actual) != canonical_sql(expected) {
        return Err(DbErr::Migration(format!(
            "{index} does not have the exact canonical definition"
        )));
    }
    Ok(())
}

async fn validate_no_foreign_keys(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA foreign_key_list('{TABLE}')"),
        ))
        .await?;
    if !rows.is_empty() {
        return Err(DbErr::Migration(format!(
            "{TABLE} has {} unexpected foreign key(s)",
            rows.len()
        )));
    }
    Ok(())
}

async fn validate_no_triggers(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let row = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT COUNT(*) AS count FROM sqlite_master
             WHERE type = 'trigger' AND tbl_name = ?",
            [TABLE.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("failed to inspect {TABLE} triggers")))?;
    let count: i64 = row.try_get("", "count")?;
    if count != 0 {
        return Err(DbErr::Migration(format!(
            "{TABLE} has {count} unexpected trigger(s)"
        )));
    }
    Ok(())
}

/// Normalize harmless SQLite formatting and identifier quoting while keeping
/// string literals byte-exact.
fn canonical_sql(sql: &str) -> String {
    let mut canonical = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\'' => {
                canonical.push('\'');
                while let Some(literal_character) = characters.next() {
                    canonical.push(literal_character);
                    if literal_character == '\'' {
                        if characters.peek() == Some(&'\'') {
                            canonical.push(characters.next().expect("peeked quote exists"));
                        } else {
                            break;
                        }
                    }
                }
            }
            '"' => append_quoted_identifier(&mut canonical, &mut characters, '"'),
            '`' => append_quoted_identifier(&mut canonical, &mut characters, '`'),
            '[' => append_quoted_identifier(&mut canonical, &mut characters, ']'),
            character if character.is_ascii_whitespace() => {}
            character => canonical.extend(character.to_lowercase()),
        }
    }
    canonical
}

fn append_quoted_identifier<I>(
    canonical: &mut String,
    characters: &mut std::iter::Peekable<I>,
    closing_quote: char,
) where
    I: Iterator<Item = char>,
{
    while let Some(character) = characters.next() {
        if character == closing_quote {
            if characters.peek() == Some(&closing_quote) {
                canonical.extend(character.to_lowercase());
                characters.next();
            } else {
                break;
            }
        } else {
            canonical.extend(character.to_lowercase());
        }
    }
}

impl fmt::Debug for Migration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmScrobbleQueueMigration")
    }
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ActiveModelTrait, Database, DatabaseConnection, DbBackend, EntityTrait, PaginatorTrait,
    };
    use uuid::Uuid;

    use super::*;
    use crate::db::entities::lastfm_scrobble::{self, StoredLastFmScrobble};
    use crate::db::migration::Migrator;

    async fn database_before_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory SQLite database");
        Migrator::up(&db, Some(16))
            .await
            .expect("apply migrations preceding Last.fm queue");
        db
    }

    async fn migrated_database() -> DatabaseConnection {
        let db = database_before_migration().await;
        Migrator::up(&db, Some(1))
            .await
            .expect("apply Last.fm queue migration");
        db
    }

    fn model() -> lastfm_scrobble::Model {
        lastfm_scrobble::Model {
            id: 0,
            occurrence_id: Uuid::new_v4().as_bytes().to_vec().into(),
            account_binding: vec![0xa5; ACCOUNT_BINDING_BYTES].into(),
            artist: "Artist".to_owned().into(),
            track_title: "Track".to_owned().into(),
            album: Some("Album".to_owned().into()),
            album_artist: None,
            track_number: Some(1.into()),
            duration_secs: 31.into(),
            started_at_unix_secs: 1.into(),
            attempt_count: 0,
            next_attempt_at_ms: 0,
        }
    }

    async fn insert_model(
        db: &DatabaseConnection,
        model: lastfm_scrobble::Model,
    ) -> Result<lastfm_scrobble::Model, DbErr> {
        let mut active: lastfm_scrobble::ActiveModel = model.into();
        active.id = sea_orm_migration::sea_orm::ActiveValue::NotSet;
        active.insert(db).await
    }

    async fn target_exists(db: &DatabaseConnection) -> bool {
        object_type(&SchemaManager::new(db), TABLE).await.unwrap() == Some("table".to_owned())
    }

    #[tokio::test]
    async fn fresh_up_creates_exact_schema_indexes_and_entity_round_trips() {
        let db = migrated_database().await;
        validate_schema(&SchemaManager::new(&db))
            .await
            .expect("canonical Last.fm queue schema");
        assert_eq!(
            OCCURRENCE_ID_BYTES,
            crate::db::entities::lastfm_scrobble::LASTFM_QUEUE_OCCURRENCE_ID_BYTES
        );
        assert_eq!(
            ACCOUNT_BINDING_BYTES,
            crate::db::entities::lastfm_scrobble::LASTFM_ACCOUNT_BINDING_BYTES
        );
        assert_eq!(
            MAX_TEXT_BYTES,
            crate::db::entities::lastfm_scrobble::MAX_LASTFM_METADATA_BYTES
        );

        let inserted = insert_model(&db, model()).await.expect("insert queue row");
        assert!(inserted.id > 0);
        StoredLastFmScrobble::try_from(inserted).expect("validate stored row");
    }

    #[tokio::test]
    async fn exact_interrupted_target_is_retryable_but_near_matches_fail_closed() {
        let compatible = database_before_migration().await;
        Migration
            .up(&SchemaManager::new(&compatible))
            .await
            .expect("install exact interrupted target");
        Migrator::up(&compatible, Some(1))
            .await
            .expect("record exact interrupted target");
        Migration
            .up(&SchemaManager::new(&compatible))
            .await
            .expect("exact retry is idempotent");

        let partial = database_before_migration().await;
        partial
            .execute_unprepared("CREATE TABLE lastfm_scrobble_queue (id INTEGER PRIMARY KEY)")
            .await
            .unwrap();
        let error = Migrator::up(&partial, Some(1))
            .await
            .expect_err("partial target must fail");
        assert!(error.to_string().contains("column schema"));
        assert!(target_exists(&partial).await);

        let altered = database_before_migration().await;
        altered
            .execute_unprepared(&canonical_table_sql().replace(
                "duration_secs BETWEEN 31 AND 2147483647",
                "duration_secs BETWEEN 30 AND 2147483647",
            ))
            .await
            .unwrap();
        altered
            .execute_unprepared(&canonical_occurrence_index_sql())
            .await
            .unwrap();
        altered
            .execute_unprepared(&canonical_account_fifo_index_sql())
            .await
            .unwrap();
        let error = Migrator::up(&altered, Some(1))
            .await
            .expect_err("weakened threshold must fail");
        assert!(error.to_string().contains("exact canonical"));
    }

    #[tokio::test]
    async fn database_constraints_reject_private_shape_and_numeric_drift() {
        let db = migrated_database().await;
        insert_model(&db, model()).await.expect("canonical row");

        for sql in [
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (zeroblob(15), zeroblob(32), 'Artist', 'Track', 31, 1, 0, 0)",
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (randomblob(16), zeroblob(31), 'Artist', 'Track', 31, 1, 0, 0)",
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (randomblob(16), randomblob(32), '', 'Track', 31, 1, 0, 0)",
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (randomblob(16), randomblob(32), 'Artist', '', 31, 1, 0, 0)",
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (randomblob(16), randomblob(32), 'Artist', 'Track', 30, 1, 0, 0)",
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (randomblob(16), randomblob(32), 'Artist', 'Track', 31, 1, 32, 0)",
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, track_number, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (randomblob(16), randomblob(32), 'Artist', 'Track', 0, 31, 1, 0, 0)",
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (randomblob(16), randomblob(32), 'Artist', 'Track', 31, 0, 0, 0)",
            "INSERT INTO lastfm_scrobble_queue
             (occurrence_id, account_binding, artist, track_title, duration_secs,
              started_at_unix_secs, attempt_count, next_attempt_at_ms)
             VALUES (randomblob(16), randomblob(32), 'Artist', 'Track', 31, 1, 0, -1)",
        ] {
            db.execute_unprepared(sql)
                .await
                .expect_err("noncanonical queue row must violate a CHECK");
        }
        assert_eq!(lastfm_scrobble::Entity::find().count(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn metadata_constraints_measure_utf8_bytes_at_the_exact_boundary() {
        let db = migrated_database().await;
        let mut exact = model();
        exact.artist = "🎵".repeat(MAX_TEXT_BYTES / 4).into();
        insert_model(&db, exact.clone())
            .await
            .expect("1,024-byte metadata is accepted");

        exact.id = 0;
        exact.occurrence_id = Uuid::new_v4().as_bytes().to_vec().into();
        exact.artist = format!("{}a", exact.artist.as_str()).into();
        insert_model(&db, exact)
            .await
            .expect_err("1,025-byte metadata is rejected");
    }

    #[tokio::test]
    async fn occurrence_identity_is_unique_and_fifo_index_is_exact() {
        let db = migrated_database().await;
        let first = insert_model(&db, model()).await.unwrap();
        let mut duplicate = model();
        duplicate.occurrence_id = first.occurrence_id;
        insert_model(&db, duplicate)
            .await
            .expect_err("occurrence identity must be idempotent");

        let rows = db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT id FROM lastfm_scrobble_queue WHERE account_binding = ? ORDER BY id",
                [first.account_binding.into()],
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn downgrade_is_lossless_or_refused_and_unknown_objects_are_preserved() {
        let empty = migrated_database().await;
        Migration
            .down(&SchemaManager::new(&empty))
            .await
            .expect("empty downgrade is lossless");
        assert!(!target_exists(&empty).await);

        let nonempty = migrated_database().await;
        insert_model(&nonempty, model()).await.unwrap();
        let error = Migration
            .down(&SchemaManager::new(&nonempty))
            .await
            .expect_err("pending scrobble prevents downgrade");
        assert!(error.to_string().contains("cannot downgrade 1"));
        assert!(target_exists(&nonempty).await);

        nonempty
            .execute_unprepared(
                "CREATE TRIGGER unexpected_lastfm_trigger
                 AFTER INSERT ON lastfm_scrobble_queue BEGIN SELECT 1; END",
            )
            .await
            .unwrap();
        let error = Migration
            .up(&SchemaManager::new(&nonempty))
            .await
            .expect_err("unexpected trigger must fail closed");
        assert!(error.to_string().contains("unexpected trigger"));
        assert!(target_exists(&nonempty).await);
    }
}
