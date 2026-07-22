//! Migration: add one durable fixed-category Last.fm delivery pause.
//!
//! The singleton stores only the one-way account binding and a closed numeric
//! category. It deliberately contains no credentials, metadata, endpoint,
//! response, or diagnostic text.

use std::fmt;

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{
    ConnectionTrait, DatabaseConnection, Statement, TransactionTrait,
};

const TABLE: &str = "lastfm_delivery_pause";
const QUEUE_TABLE: &str = "lastfm_scrobble_queue";
const ACCOUNT_BINDING_BYTES: usize = 32;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_pause(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_pause(manager, false).await
    }
}

/// Revalidate the durable closed-retry boundary on every startup.
pub(super) async fn revalidate(connection: &DatabaseConnection) -> Result<(), DbErr> {
    validate_schema(&SchemaManager::new(connection)).await
}

async fn migrate_pause(manager: &SchemaManager<'_>, create: bool) -> Result<(), DbErr> {
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
            Err(match rollback {
                Ok(()) => error,
                Err(rollback_error) => DbErr::Migration(format!(
                    "{error}; additionally failed to roll back Last.fm pause migration: {rollback_error}"
                )),
            })
        }
    }
}

async fn create_or_validate(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    match named_object_type(manager, QUEUE_TABLE).await? {
        Some(object_type) if object_type == "table" => {}
        Some(object_type) => {
            return Err(DbErr::Migration(format!(
                "{QUEUE_TABLE} must be a table, found {object_type}"
            )));
        }
        None => {
            return Err(DbErr::Migration(format!(
                "{QUEUE_TABLE} must exist before {TABLE}"
            )));
        }
    }
    match object_type(manager).await? {
        None => {
            manager
                .get_connection()
                .execute_unprepared(&canonical_table_sql())
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
    let Some(target_type) = object_type(manager).await? else {
        return Ok(());
    };
    if target_type != "table" {
        return Err(DbErr::Migration(format!(
            "{TABLE} must be a table, found {target_type}"
        )));
    }
    match named_object_type(manager, QUEUE_TABLE).await? {
        Some(queue_type) if queue_type == "table" => {}
        Some(queue_type) => {
            return Err(DbErr::Migration(format!(
                "{QUEUE_TABLE} must be a table, found {queue_type}"
            )));
        }
        None => {
            return Err(DbErr::Migration(format!(
                "{QUEUE_TABLE} is missing while {TABLE} remains"
            )));
        }
    }
    validate_schema(manager).await?;
    let pause_count = manager
        .get_connection()
        .query_one(Statement::from_string(
            manager.get_database_backend(),
            format!("SELECT COUNT(*) AS count FROM {TABLE}"),
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("failed to inspect Last.fm pause state".to_owned()))?
        .try_get::<i64>("", "count")?;
    let queue_count = manager
        .get_connection()
        .query_one(Statement::from_string(
            manager.get_database_backend(),
            format!("SELECT COUNT(*) AS count FROM {QUEUE_TABLE}"),
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("failed to inspect Last.fm queue".to_owned()))?
        .try_get::<i64>("", "count")?;
    if pause_count != 0 || queue_count != 0 {
        return Err(DbErr::Migration(format!(
            "cannot downgrade {queue_count} pending Last.fm scrobble row(s) and {pause_count} durable pause row(s) safely"
        )));
    }
    manager
        .get_connection()
        .execute_unprepared(&format!("DROP TABLE {TABLE}"))
        .await?;
    if object_type(manager).await?.is_some() {
        return Err(DbErr::Migration(format!(
            "{TABLE} remained after downgrade"
        )));
    }
    Ok(())
}

fn canonical_table_sql() -> String {
    format!(
        "CREATE TABLE {TABLE} (
             slot INTEGER PRIMARY KEY,
             account_binding BLOB NOT NULL,
             pause_category INTEGER NOT NULL,
             CONSTRAINT ck_lastfm_pause_slot CHECK (
                 typeof(slot) = 'integer' AND slot = 1
             ),
             CONSTRAINT ck_lastfm_pause_account_binding CHECK (
                 typeof(account_binding) = 'blob'
                 AND length(account_binding) = {ACCOUNT_BINDING_BYTES}
             ),
             CONSTRAINT ck_lastfm_pause_category CHECK (
                 typeof(pause_category) = 'integer'
                 AND pause_category BETWEEN 1 AND 4
             )
         )"
    )
}

async fn object_type(manager: &SchemaManager<'_>) -> Result<Option<String>, DbErr> {
    named_object_type(manager, TABLE).await
}

async fn named_object_type(
    manager: &SchemaManager<'_>,
    name: &str,
) -> Result<Option<String>, DbErr> {
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
        (0, "slot".to_owned(), "integer".to_owned(), 0, None, 1),
        (
            1,
            "account_binding".to_owned(),
            "blob".to_owned(),
            1,
            None,
            0,
        ),
        (
            2,
            "pause_category".to_owned(),
            "integer".to_owned(),
            1,
            None,
            0,
        ),
    ];
    if columns != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has an unexpected column schema: {columns:?}"
        )));
    }

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

    for (kind, pragma) in [
        ("index", format!("PRAGMA index_list('{TABLE}')")),
        ("foreign key", format!("PRAGMA foreign_key_list('{TABLE}')")),
    ] {
        let rows = manager
            .get_connection()
            .query_all(Statement::from_string(
                manager.get_database_backend(),
                pragma,
            ))
            .await?;
        if !rows.is_empty() {
            return Err(DbErr::Migration(format!(
                "{TABLE} has unexpected {kind} objects"
            )));
        }
    }
    let trigger_count = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT COUNT(*) AS count FROM sqlite_master
             WHERE type = 'trigger' AND tbl_name = ?",
            [TABLE.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("failed to inspect {TABLE} triggers")))?
        .try_get::<i64>("", "count")?;
    if trigger_count != 0 {
        return Err(DbErr::Migration(format!(
            "{TABLE} has {trigger_count} unexpected trigger(s)"
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
        formatter.write_str("LastFmDeliveryPauseMigration")
    }
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{Database, DatabaseConnection, DbBackend};

    use super::*;
    use crate::db::migration::Migrator;

    async fn database_through_queue_migration() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, Some(17)).await.unwrap();
        database
    }

    async fn migrated_database() -> DatabaseConnection {
        let database = database_through_queue_migration().await;
        Migrator::up(&database, Some(1)).await.unwrap();
        database
    }

    async fn target_exists(database: &DatabaseConnection) -> bool {
        object_type(&SchemaManager::new(database)).await.unwrap() == Some("table".to_owned())
    }

    async fn queue_snapshot(
        database: &DatabaseConnection,
    ) -> Vec<(
        i64,
        Vec<u8>,
        Vec<u8>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i32>,
        i32,
        i64,
        i32,
        i64,
    )> {
        database
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, occurrence_id, account_binding, artist, track_title,
                        album, album_artist, track_number, duration_secs,
                        started_at_unix_secs, attempt_count, next_attempt_at_ms
                 FROM lastfm_scrobble_queue ORDER BY id"
                    .to_owned(),
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.try_get("", "id").unwrap(),
                    row.try_get("", "occurrence_id").unwrap(),
                    row.try_get("", "account_binding").unwrap(),
                    row.try_get("", "artist").unwrap(),
                    row.try_get("", "track_title").unwrap(),
                    row.try_get("", "album").unwrap(),
                    row.try_get("", "album_artist").unwrap(),
                    row.try_get("", "track_number").unwrap(),
                    row.try_get("", "duration_secs").unwrap(),
                    row.try_get("", "started_at_unix_secs").unwrap(),
                    row.try_get("", "attempt_count").unwrap(),
                    row.try_get("", "next_attempt_at_ms").unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn existing_migration_seventeen_database_gains_exact_pause_schema() {
        let database = database_through_queue_migration().await;
        database
            .execute_unprepared(
                "INSERT INTO lastfm_scrobble_queue
                 (occurrence_id, account_binding, artist, track_title, album,
                  album_artist, track_number, duration_secs, started_at_unix_secs,
                  attempt_count, next_attempt_at_ms)
                 VALUES (x'00112233445546778899aabbccddeeff', zeroblob(32),
                         'Artist', 'Track', 'Album', 'Album Artist', 7, 240,
                         1700000000, 3, 90000)",
            )
            .await
            .unwrap();
        let before = queue_snapshot(&database).await;
        assert!(!target_exists(&database).await);
        Migrator::up(&database, Some(1)).await.unwrap();
        validate_schema(&SchemaManager::new(&database))
            .await
            .unwrap();
        assert!(target_exists(&database).await);
        assert_eq!(queue_snapshot(&database).await, before);
        assert_eq!(
            database
                .query_one(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT COUNT(*) AS count FROM {TABLE}"),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<i64>("", "count")
                .unwrap(),
            0
        );
        let queue_type: String = database
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT type FROM sqlite_master WHERE name = 'lastfm_scrobble_queue'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "type")
            .unwrap();
        assert_eq!(queue_type, "table");
    }

    #[tokio::test]
    async fn fixed_singleton_categories_are_exact_and_private() {
        let database = migrated_database().await;
        for category in 1..=4 {
            database
                .execute(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    format!(
                        "INSERT INTO {TABLE} (slot, account_binding, pause_category)
                         VALUES (1, ?, ?)
                         ON CONFLICT(slot) DO UPDATE SET pause_category = excluded.pause_category"
                    ),
                    [vec![0xa5; ACCOUNT_BINDING_BYTES].into(), category.into()],
                ))
                .await
                .unwrap();
        }
        for sql in [
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, account_binding, pause_category) VALUES (2, zeroblob(32), 1)"
            ),
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, account_binding, pause_category) VALUES (1, zeroblob(31), 1)"
            ),
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, account_binding, pause_category) VALUES (1, zeroblob(32), 5)"
            ),
        ] {
            database.execute_unprepared(&sql).await.unwrap_err();
        }
        assert_eq!(
            database
                .query_one(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT COUNT(*) AS count FROM {TABLE}"),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<i64>("", "count")
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn exact_retry_is_idempotent_and_near_matches_fail_closed() {
        let exact = database_through_queue_migration().await;
        Migration.up(&SchemaManager::new(&exact)).await.unwrap();
        Migration.up(&SchemaManager::new(&exact)).await.unwrap();

        let partial = database_through_queue_migration().await;
        partial
            .execute_unprepared(&format!("CREATE TABLE {TABLE} (slot INTEGER PRIMARY KEY)"))
            .await
            .unwrap();
        let error = Migrator::up(&partial, Some(1)).await.unwrap_err();
        assert!(error.to_string().contains("column schema"));
        assert!(target_exists(&partial).await);

        let weakened = database_through_queue_migration().await;
        weakened
            .execute_unprepared(&canonical_table_sql().replace(
                "pause_category BETWEEN 1 AND 4",
                "pause_category BETWEEN 0 AND 4",
            ))
            .await
            .unwrap();
        let error = Migrator::up(&weakened, Some(1)).await.unwrap_err();
        assert!(error.to_string().contains("exact canonical"));
        assert!(target_exists(&weakened).await);
    }

    #[tokio::test]
    async fn downgrade_is_lossless_or_refused_and_preserves_queue_schema() {
        let empty = migrated_database().await;
        Migration.down(&SchemaManager::new(&empty)).await.unwrap();
        assert!(!target_exists(&empty).await);
        assert!(empty
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT type FROM sqlite_master WHERE name = 'lastfm_scrobble_queue'".to_owned(),
            ))
            .await
            .unwrap()
            .is_some());

        let paused = migrated_database().await;
        paused
            .execute_unprepared(&format!(
                "INSERT INTO {TABLE} (slot, account_binding, pause_category)
                 VALUES (1, zeroblob(32), 1)"
            ))
            .await
            .unwrap();
        let error = Migration
            .down(&SchemaManager::new(&paused))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot downgrade"));
        assert!(target_exists(&paused).await);

        let queued = migrated_database().await;
        queued
            .execute_unprepared(
                "INSERT INTO lastfm_scrobble_queue
                 (occurrence_id, account_binding, artist, track_title, duration_secs,
                  started_at_unix_secs, attempt_count, next_attempt_at_ms)
                 VALUES (randomblob(16), zeroblob(32), 'Artist', 'Track', 31, 1, 0, 0)",
            )
            .await
            .unwrap();
        let error = Migration
            .down(&SchemaManager::new(&queued))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("1 pending"));
        assert!(target_exists(&queued).await);
    }

    #[tokio::test]
    async fn unexpected_schema_objects_are_rejected_without_destruction() {
        let database = migrated_database().await;
        database
            .execute_unprepared(&format!(
                "CREATE TRIGGER unexpected_lastfm_pause_trigger
                 AFTER INSERT ON {TABLE} BEGIN SELECT 1; END"
            ))
            .await
            .unwrap();
        let error = Migration
            .up(&SchemaManager::new(&database))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unexpected trigger"));
        assert!(target_exists(&database).await);
    }
}
