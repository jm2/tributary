//! Migration: persist the playlist-sidebar presentation order.
//!
//! The sidebar order is derived UI state, separate from playlist content and
//! from the durable revision table installed by migration 15. Rows record a
//! contiguous position for every playlist that has been explicitly reordered.
//! A missing row means "fall back to `created_at` order", so installs that
//! never reorder keep their historical ordering exactly.
//!
//! Three triggers advance the migration-15 revision on every insert, effective
//! update, and delete so the engine republishes the sidebar snapshot after a
//! reorder, including when the write arrives through raw SQL.

use std::fmt;

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{
    ConnectionTrait, DatabaseConnection, Statement, TransactionTrait,
};

const TABLE: &str = "playlist_sidebar_order";
const REVISION_TABLE: &str = "playlist_sidebar_revision";
const REVISION_SINGLETON: i64 = 1;
const POSITION_CHECK: &str = "ck_playlist_sidebar_order_position";

const INSERT_TRIGGER: &str = "trg_playlist_sidebar_revision_sidebar_order_insert";
const UPDATE_TRIGGER: &str = "trg_playlist_sidebar_revision_sidebar_order_update";
const DELETE_TRIGGER: &str = "trg_playlist_sidebar_revision_sidebar_order_delete";

const TRIGGERS: [TriggerDefinition; 3] = [
    TriggerDefinition::new(INSERT_TRIGGER, "INSERT"),
    TriggerDefinition::new(UPDATE_TRIGGER, "UPDATE"),
    TriggerDefinition::new(DELETE_TRIGGER, "DELETE"),
];

#[derive(Clone, Copy)]
struct TriggerDefinition {
    name: &'static str,
    operation: &'static str,
}

impl TriggerDefinition {
    const fn new(name: &'static str, operation: &'static str) -> Self {
        Self { name, operation }
    }
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate(manager, false).await
    }
}

/// Revalidate the mutable order boundary even when the migration ledger is
/// current. The revision triggers are critical schema objects; a missing or
/// altered trigger would silently stop the sidebar from republishing after a
/// reorder.
pub(super) async fn revalidate(connection: &DatabaseConnection) -> Result<(), DbErr> {
    validate_installation(&SchemaManager::new(connection)).await
}

/// Own the complete DDL transaction so validation failures cannot leave a
/// partially altered boundary.
async fn migrate(manager: &SchemaManager<'_>, install: bool) -> Result<(), DbErr> {
    let transaction = manager.get_connection().begin().await?;
    let result = {
        let manager = SchemaManager::new(&transaction);
        if install {
            create_or_validate(&manager).await
        } else {
            drop_or_validate_absent(&manager).await
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
            "{original}; additionally failed to roll back playlist-sidebar order migration: \
             {rollback_error}"
        )),
    }
}

async fn create_or_validate(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    if target_objects_absent(manager).await? {
        manager
            .get_connection()
            .execute_unprepared(&canonical_table_sql())
            .await?;
        for trigger in TRIGGERS {
            manager
                .get_connection()
                .execute_unprepared(&canonical_trigger_sql(trigger))
                .await?;
        }
    }

    validate_installation(manager).await
}

async fn drop_or_validate_absent(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    if target_objects_absent(manager).await? {
        return Ok(());
    }

    validate_installation(manager).await?;
    for trigger in TRIGGERS {
        manager
            .get_connection()
            .execute_unprepared(&format!("DROP TRIGGER {}", trigger.name))
            .await?;
    }
    manager
        .get_connection()
        .execute_unprepared(&format!("DROP TABLE {TABLE}"))
        .await?;

    if !target_objects_absent(manager).await? {
        return Err(DbErr::Migration(
            "playlist-sidebar order objects remained after downgrade".to_string(),
        ));
    }
    Ok(())
}

async fn target_objects_absent(manager: &SchemaManager<'_>) -> Result<bool, DbErr> {
    let mut names = Vec::with_capacity(TRIGGERS.len() + 1);
    names.push(TABLE);
    names.extend(TRIGGERS.iter().map(|trigger| trigger.name));

    for name in names {
        if !objects_named(manager, name).await?.is_empty() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn canonical_table_sql() -> String {
    format!(
        "CREATE TABLE {TABLE} (
             playlist_id TEXT PRIMARY KEY NOT NULL
                 REFERENCES playlists(id) ON DELETE CASCADE,
             position INTEGER NOT NULL,
             CONSTRAINT {POSITION_CHECK} CHECK (
                 typeof(position) = 'integer' AND position >= 0
             )
         ) WITHOUT ROWID"
    )
}

fn canonical_trigger_sql(trigger: TriggerDefinition) -> String {
    let when = if trigger.operation == "UPDATE" {
        "WHEN OLD.playlist_id IS NOT NEW.playlist_id
              OR OLD.position IS NOT NEW.position"
    } else {
        ""
    };
    format!(
        "CREATE TRIGGER {name}
         AFTER {operation} ON {TABLE}
         {when}
         BEGIN
             SELECT CASE
                 WHEN NOT EXISTS (
                     SELECT 1 FROM {revision_table} WHERE singleton = {singleton}
                 ) THEN RAISE(ABORT, 'playlist sidebar revision singleton missing')
                 WHEN (
                     SELECT revision FROM {revision_table} WHERE singleton = {singleton}
                 ) = {max_revision}
                 THEN RAISE(ABORT, 'playlist sidebar revision exhausted')
             END;
             UPDATE {revision_table}
             SET revision = revision + 1
             WHERE singleton = {singleton};
         END",
        name = trigger.name,
        operation = trigger.operation,
        TABLE = TABLE,
        when = when,
        revision_table = REVISION_TABLE,
        singleton = REVISION_SINGLETON,
        max_revision = i64::MAX,
    )
}

#[derive(Eq, PartialEq)]
struct SchemaObject {
    object_type: String,
    table_name: String,
    sql: Option<String>,
}

impl fmt::Debug for SchemaObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SchemaObject")
            .field("object_type", &self.object_type)
            .field("table_name_byte_len", &self.table_name.len())
            .field("sql_present", &self.sql.is_some())
            .finish()
    }
}

async fn objects_named(
    manager: &SchemaManager<'_>,
    name: &str,
) -> Result<Vec<SchemaObject>, DbErr> {
    manager
        .get_connection()
        .query_all_raw(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT type, tbl_name, sql FROM sqlite_master WHERE name = ? ORDER BY type, tbl_name",
            [name.into()],
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok(SchemaObject {
                object_type: row.try_get("", "type")?,
                table_name: row.try_get("", "tbl_name")?,
                sql: row.try_get("", "sql")?,
            })
        })
        .collect()
}

async fn validate_installation(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    validate_table_object(manager).await?;
    validate_columns(manager).await?;
    validate_triggers(manager).await?;
    validate_revision_boundary(manager).await
}

async fn validate_table_object(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let objects = objects_named(manager, TABLE).await?;
    let [object] = objects.as_slice() else {
        return Err(DbErr::Migration(format!(
            "{TABLE} must resolve to exactly one table object, found {objects:?}"
        )));
    };
    if object.object_type != "table" || object.table_name != TABLE {
        return Err(DbErr::Migration(format!(
            "{TABLE} must be a table owned by itself, found {object:?}"
        )));
    }
    let actual = object
        .sql
        .as_deref()
        .ok_or_else(|| DbErr::Migration(format!("{TABLE} SQL is missing")))?;
    if canonical_sql(actual) != canonical_sql(&canonical_table_sql()) {
        return Err(DbErr::Migration(format!(
            "{TABLE} does not have the exact canonical table definition"
        )));
    }
    Ok(())
}

type ColumnSchema = (i32, String, String, i32, Option<String>, i32);

async fn validate_columns(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let columns = manager
        .get_connection()
        .query_all_raw(Statement::from_string(
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
        (0, "playlist_id".to_string(), "text".to_string(), 1, None, 1),
        (1, "position".to_string(), "integer".to_string(), 1, None, 0),
    ];
    if columns != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has an unexpected column schema: {columns:?}"
        )));
    }
    Ok(())
}

async fn validate_triggers(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for trigger in TRIGGERS {
        let objects = objects_named(manager, trigger.name).await?;
        let [object] = objects.as_slice() else {
            return Err(DbErr::Migration(format!(
                "{} must resolve to exactly one trigger object, found {objects:?}",
                trigger.name
            )));
        };
        if object.object_type != "trigger" || object.table_name != TABLE {
            return Err(DbErr::Migration(format!(
                "{} has an unexpected object type or owner: {object:?}",
                trigger.name
            )));
        }
        let actual = object
            .sql
            .as_deref()
            .ok_or_else(|| DbErr::Migration(format!("{} SQL is missing", trigger.name)))?;
        if canonical_sql(actual) != canonical_sql(&canonical_trigger_sql(trigger)) {
            return Err(DbErr::Migration(format!(
                "{} does not have the exact canonical trigger definition",
                trigger.name
            )));
        }
    }

    let mut actual = manager
        .get_connection()
        .query_all_raw(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?",
            [TABLE.into()],
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    actual.sort();
    let mut expected = TRIGGERS
        .iter()
        .map(|trigger| (*trigger.name).to_string())
        .collect::<Vec<_>>();
    expected.sort();
    if actual != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has an unexpected trigger set (found {}, expected {})",
            actual.len(),
            expected.len()
        )));
    }
    Ok(())
}

/// The order boundary must stay attached to the revision singleton that
/// migration 15 owns. Losing it would leave reorders invisible to the engine.
async fn validate_revision_boundary(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all_raw(Statement::from_string(
            manager.get_database_backend(),
            format!("SELECT singleton FROM {REVISION_TABLE}"),
        ))
        .await?;
    let [row] = rows.as_slice() else {
        return Err(DbErr::Migration(format!(
            "{TABLE} requires exactly one {REVISION_TABLE} singleton row, found {}",
            rows.len()
        )));
    };
    let singleton: i64 = row.try_get("", "singleton")?;
    if singleton != REVISION_SINGLETON {
        return Err(DbErr::Migration(format!(
            "{TABLE} revision boundary has an invalid singleton"
        )));
    }
    Ok(())
}

/// Normalize formatting and identifier quoting while preserving SQL string
/// literal contents. Validation still compares the complete statement.
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

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement,
    };

    use super::*;
    use crate::db::migration::Migrator;

    async fn migrated_database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory SQLite database");
        Migrator::up(&db, None)
            .await
            .expect("run playlist sidebar order migrations");
        db
    }

    async fn revision(connection: &impl ConnectionTrait) -> i64 {
        connection
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT revision FROM {REVISION_TABLE} WHERE singleton = {REVISION_SINGLETON}"
                ),
            ))
            .await
            .expect("query revision")
            .expect("singleton revision exists")
            .try_get("", "revision")
            .expect("revision is an integer")
    }

    async fn insert_playlist(connection: &impl ConnectionTrait, id: &str) {
        connection
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO playlists (id, name, created_at, updated_at)
                 VALUES (?, 'Playlist', '2026-08-07T00:00:00Z', '2026-08-07T00:00:00Z')",
                [id.into()],
            ))
            .await
            .expect("insert fixture playlist");
    }

    fn order_insert_sql(playlist_id: &str, position: i64) -> Statement {
        Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!("INSERT INTO {TABLE} (playlist_id, position) VALUES (?, ?)"),
            [playlist_id.into(), position.into()],
        )
    }

    async fn row_count(connection: &impl ConnectionTrait) -> i64 {
        connection
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT COUNT(*) AS count FROM {TABLE}"),
            ))
            .await
            .expect("count rows")
            .expect("count returns one row")
            .try_get("", "count")
            .expect("count is integer")
    }

    #[tokio::test]
    async fn fresh_up_creates_and_revalidates_the_exact_table_and_three_triggers() {
        let db = migrated_database().await;
        let manager = SchemaManager::new(&db);

        validate_installation(&manager)
            .await
            .expect("fresh installation is exact");
        revalidate(&db)
            .await
            .expect("startup revalidation accepts objects");
        assert_eq!(row_count(&db).await, 0);

        let trigger_rows = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT name, tbl_name FROM sqlite_master
                     WHERE type = 'trigger' AND tbl_name = '{TABLE}' ORDER BY name"
                ),
            ))
            .await
            .expect("inspect triggers");
        assert_eq!(trigger_rows.len(), 3);
        for definition in TRIGGERS {
            let object = objects_named(&manager, definition.name)
                .await
                .expect("inspect canonical trigger");
            assert_eq!(object.len(), 1);
            assert_eq!(object[0].object_type, "trigger");
            assert_eq!(object[0].table_name, TABLE);
        }
    }

    #[tokio::test]
    async fn order_mutations_advance_and_no_op_updates_do_not() {
        let db = migrated_database().await;
        insert_playlist(&db, "playlist-a").await;
        insert_playlist(&db, "playlist-b").await;
        assert_eq!(revision(&db).await, 2);

        db.execute_raw(order_insert_sql("playlist-a", 0))
            .await
            .expect("insert order row");
        assert_eq!(revision(&db).await, 3);
        db.execute_raw(order_insert_sql("playlist-b", 1))
            .await
            .expect("insert order row");
        assert_eq!(revision(&db).await, 4);

        db.execute_unprepared(&format!(
            "UPDATE {TABLE} SET position = 1 WHERE playlist_id = 'playlist-a'"
        ))
        .await
        .expect("effective position update");
        assert_eq!(revision(&db).await, 5);
        db.execute_unprepared(&format!(
            "UPDATE {TABLE} SET position = position WHERE playlist_id = 'playlist-a'"
        ))
        .await
        .expect("actual no-op update");
        assert_eq!(revision(&db).await, 5);

        db.execute_unprepared(&format!(
            "DELETE FROM {TABLE} WHERE playlist_id = 'playlist-a'"
        ))
        .await
        .expect("delete order row");
        assert_eq!(revision(&db).await, 6);
    }

    #[tokio::test]
    async fn negative_positions_are_rejected_and_cascade_removes_order_rows() {
        let db = migrated_database().await;
        insert_playlist(&db, "playlist-a").await;
        insert_playlist(&db, "playlist-b").await;
        db.execute_raw(order_insert_sql("playlist-a", 0))
            .await
            .expect("insert order row");

        db.execute_unprepared(&format!(
            "INSERT INTO {TABLE} (playlist_id, position) VALUES ('playlist-b', -1)"
        ))
        .await
        .expect_err("negative position is forbidden");
        assert_eq!(revision(&db).await, 3);
        assert_eq!(row_count(&db).await, 1);

        db.execute_unprepared("DELETE FROM playlists WHERE id = 'playlist-a'")
            .await
            .expect("cascade from parent playlist delete");
        assert_eq!(row_count(&db).await, 0);
        // The parent delete and the order-row cascade each advance the
        // migration-15 revision once, mirroring the per-changed-row trigger
        // contract.
        assert_eq!(revision(&db).await, 5);
    }

    #[tokio::test]
    async fn missing_singleton_aborts_order_writes() {
        let db = migrated_database().await;
        insert_playlist(&db, "playlist-a").await;
        db.execute_unprepared(&format!(
            "DELETE FROM {REVISION_TABLE} WHERE singleton = {REVISION_SINGLETON}"
        ))
        .await
        .expect("simulate deleted singleton");

        let error = db
            .execute_raw(order_insert_sql("playlist-a", 0))
            .await
            .expect_err("missing singleton must abort order insert");
        assert!(error.to_string().contains("singleton missing"));
        assert_eq!(row_count(&db).await, 0);
        revalidate(&db)
            .await
            .expect_err("startup revalidation detects missing singleton");
    }

    #[tokio::test]
    async fn down_removes_objects_and_keeps_playlists() {
        let db = migrated_database().await;
        insert_playlist(&db, "playlist-a").await;
        db.execute_raw(order_insert_sql("playlist-a", 0))
            .await
            .expect("insert order row");

        Migrator::down(&db, Some(1))
            .await
            .expect("downgrade playlist-sidebar order");
        assert!(target_objects_absent(&SchemaManager::new(&db))
            .await
            .unwrap());
        assert!(db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id FROM playlists WHERE id = 'playlist-a'".to_string(),
            ))
            .await
            .unwrap()
            .is_some());
    }
}
