//! Migration: install a durable invalidation revision for the playlist sidebar.
//!
//! The revision is derived state. Six row triggers advance it after every
//! committed mutation that can change the sidebar projection: inserts,
//! effective updates, and deletes on `playlists` and
//! `server_playlist_links`. The trigger boundary also covers raw SQL and
//! foreign-key cascades, so callers cannot accidentally bypass invalidation.

use std::fmt;

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{
    ConnectionTrait, DatabaseConnection, Statement, TransactionTrait,
};

const TABLE: &str = "playlist_sidebar_revision";
const SINGLETON: i64 = 1;
const SINGLETON_CHECK: &str = "ck_playlist_sidebar_revision_singleton";
const REVISION_CHECK: &str = "ck_playlist_sidebar_revision_revision";

const PLAYLIST_INSERT_TRIGGER: &str = "trg_playlist_sidebar_revision_playlists_insert";
const PLAYLIST_UPDATE_TRIGGER: &str = "trg_playlist_sidebar_revision_playlists_update";
const PLAYLIST_DELETE_TRIGGER: &str = "trg_playlist_sidebar_revision_playlists_delete";
const LINK_INSERT_TRIGGER: &str = "trg_playlist_sidebar_revision_server_playlist_links_insert";
const LINK_UPDATE_TRIGGER: &str = "trg_playlist_sidebar_revision_server_playlist_links_update";
const LINK_DELETE_TRIGGER: &str = "trg_playlist_sidebar_revision_server_playlist_links_delete";

const TRIGGERS: [TriggerDefinition; 6] = [
    TriggerDefinition::new(PLAYLIST_INSERT_TRIGGER, "INSERT", "playlists", None),
    TriggerDefinition::new(
        PLAYLIST_UPDATE_TRIGGER,
        "UPDATE",
        "playlists",
        Some(
            "OLD.id IS NOT NEW.id
             OR OLD.name IS NOT NEW.name
             OR OLD.is_smart IS NOT NEW.is_smart
             OR OLD.smart_rules_json IS NOT NEW.smart_rules_json
             OR OLD.limit_enabled IS NOT NEW.limit_enabled
             OR OLD.limit_value IS NOT NEW.limit_value
             OR OLD.limit_unit IS NOT NEW.limit_unit
             OR OLD.limit_sort IS NOT NEW.limit_sort
             OR OLD.match_mode IS NOT NEW.match_mode
             OR OLD.live_updating IS NOT NEW.live_updating
             OR OLD.created_at IS NOT NEW.created_at
             OR OLD.updated_at IS NOT NEW.updated_at",
        ),
    ),
    TriggerDefinition::new(PLAYLIST_DELETE_TRIGGER, "DELETE", "playlists", None),
    TriggerDefinition::new(LINK_INSERT_TRIGGER, "INSERT", "server_playlist_links", None),
    TriggerDefinition::new(
        LINK_UPDATE_TRIGGER,
        "UPDATE",
        "server_playlist_links",
        Some(
            "OLD.playlist_id IS NOT NEW.playlist_id
             OR OLD.source_id IS NOT NEW.source_id
             OR OLD.native_playlist_id IS NOT NEW.native_playlist_id
             OR OLD.mode IS NOT NEW.mode
             OR OLD.last_synced_name IS NOT NEW.last_synced_name
             OR OLD.digest_version IS NOT NEW.digest_version
             OR OLD.membership_digest IS NOT NEW.membership_digest
             OR OLD.last_success_at_ms IS NOT NEW.last_success_at_ms
             OR OLD.local_state IS NOT NEW.local_state
             OR OLD.remote_state IS NOT NEW.remote_state
             OR OLD.state_revision IS NOT NEW.state_revision",
        ),
    ),
    TriggerDefinition::new(LINK_DELETE_TRIGGER, "DELETE", "server_playlist_links", None),
];

#[derive(Clone, Copy)]
struct TriggerDefinition {
    name: &'static str,
    operation: &'static str,
    table: &'static str,
    when: Option<&'static str>,
}

impl TriggerDefinition {
    const fn new(
        name: &'static str,
        operation: &'static str,
        table: &'static str,
        when: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            operation,
            table,
            when,
        }
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

/// Revalidate the objects even when the migration ledger says there is no
/// work to do. Critical triggers are mutable SQLite schema objects; silently
/// continuing after one is removed would make the UI projection stale.
pub(super) async fn revalidate(connection: &DatabaseConnection) -> Result<(), DbErr> {
    validate_installation(&SchemaManager::new(connection)).await
}

/// Own the complete DDL transaction. SeaORM does not guarantee that a
/// migration's individual SQLite statements and validation queries are one
/// atomic unit.
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
            "{original}; additionally failed to roll back playlist-sidebar revision migration: \
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
        manager
            .get_connection()
            .execute_unprepared(&format!(
                "INSERT INTO {TABLE} (singleton, revision) VALUES ({SINGLETON}, 0)"
            ))
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
            "playlist-sidebar revision objects remained after downgrade".to_string(),
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
             singleton INTEGER PRIMARY KEY NOT NULL,
             revision INTEGER NOT NULL,
             CONSTRAINT {SINGLETON_CHECK} CHECK (
                 typeof(singleton) = 'integer' AND singleton = {SINGLETON}
             ),
             CONSTRAINT {REVISION_CHECK} CHECK (
                 typeof(revision) = 'integer'
                 AND revision BETWEEN 0 AND {max_revision}
             )
         ) WITHOUT ROWID",
        max_revision = i64::MAX,
    )
}

fn canonical_trigger_sql(trigger: TriggerDefinition) -> String {
    let when = trigger
        .when
        .map(|condition| format!("WHEN {condition}"))
        .unwrap_or_default();
    format!(
        "CREATE TRIGGER {name}
         AFTER {operation} ON {source_table}
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
        source_table = trigger.table,
        revision_table = TABLE,
        singleton = SINGLETON,
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
        .query_all(Statement::from_sql_and_values(
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
    validate_indexes(manager).await?;
    validate_singleton_row(manager).await?;
    validate_triggers(manager).await
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
        (
            0,
            "singleton".to_string(),
            "integer".to_string(),
            1,
            None,
            1,
        ),
        (1, "revision".to_string(), "integer".to_string(), 1, None, 0),
    ];
    if columns != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has an unexpected column schema: {columns:?}"
        )));
    }
    Ok(())
}

async fn validate_indexes(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let indexes = manager
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
                row.try_get::<i32>("", "unique")?,
                row.try_get::<String>("", "origin")?,
                row.try_get::<i32>("", "partial")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let [(index_name, 1, origin, 0)] = indexes.as_slice() else {
        return Err(DbErr::Migration(format!(
            "{TABLE} has unexpected indexes: {indexes:?}"
        )));
    };
    if origin != "pk" {
        return Err(DbErr::Migration(format!(
            "{TABLE} primary-key index has origin {origin}"
        )));
    }

    let columns = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_info('{}')", index_name.replace('\'', "''")),
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    if columns != ["singleton"] {
        return Err(DbErr::Migration(format!(
            "{TABLE} primary-key index has unexpected columns: {columns:?}"
        )));
    }
    Ok(())
}

async fn validate_singleton_row(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("SELECT singleton, revision FROM {TABLE}"),
        ))
        .await?;
    let [row] = rows.as_slice() else {
        return Err(DbErr::Migration(format!(
            "{TABLE} must contain exactly one singleton row, found {}",
            rows.len()
        )));
    };
    let singleton: i64 = row.try_get("", "singleton")?;
    let revision: i64 = row.try_get("", "revision")?;
    if singleton != SINGLETON || revision < 0 {
        return Err(DbErr::Migration(format!(
            "{TABLE} has an invalid singleton row"
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
        if object.object_type != "trigger" || object.table_name != trigger.table {
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

    validate_table_trigger_set(
        manager,
        "playlists",
        &[
            PLAYLIST_DELETE_TRIGGER,
            PLAYLIST_INSERT_TRIGGER,
            PLAYLIST_UPDATE_TRIGGER,
        ],
    )
    .await?;
    validate_table_trigger_set(
        manager,
        "server_playlist_links",
        &[
            LINK_DELETE_TRIGGER,
            LINK_INSERT_TRIGGER,
            LINK_UPDATE_TRIGGER,
        ],
    )
    .await?;
    validate_table_trigger_set(manager, TABLE, &[]).await
}

async fn validate_table_trigger_set(
    manager: &SchemaManager<'_>,
    table: &str,
    expected: &[&str],
) -> Result<(), DbErr> {
    let mut actual = manager
        .get_connection()
        .query_all(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?",
            [table.into()],
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    actual.sort();
    let mut expected = expected
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    expected.sort();
    if actual != expected {
        return Err(DbErr::Migration(format!(
            "{table} has an unexpected trigger set (found {}, expected {})",
            actual.len(),
            expected.len()
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
        ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement, TransactionTrait,
    };

    use super::*;
    use crate::db::migration::Migrator;

    const REMOTE_SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";

    async fn database_at_migration_14() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory SQLite database");
        Migrator::up(&db, Some(14))
            .await
            .expect("apply migrations through server-playlist links");
        db
    }

    async fn migrated_database() -> DatabaseConnection {
        let db = database_at_migration_14().await;
        Migrator::up(&db, Some(1))
            .await
            .expect("apply playlist-sidebar revision migration");
        db
    }

    async fn migration_is_applied(db: &DatabaseConnection) -> bool {
        let name = Migration.name().to_string();
        Migrator::get_migration_models(db)
            .await
            .expect("read migration ledger")
            .iter()
            .any(|migration| migration.version == name)
    }

    async fn revision(connection: &impl ConnectionTrait) -> i64 {
        connection
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT revision FROM {TABLE} WHERE singleton = {SINGLETON}"),
            ))
            .await
            .expect("query revision")
            .expect("singleton revision exists")
            .try_get("", "revision")
            .expect("revision is an integer")
    }

    fn playlist_insert_sql(id: &str) -> Statement {
        Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlists (id, name, created_at, updated_at)
             VALUES (?, 'Playlist', '2026-07-20T00:00:00Z', '2026-07-20T00:00:00Z')",
            [id.into()],
        )
    }

    fn link_insert_sql(playlist_id: &str, native_id: &str) -> Statement {
        Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO server_playlist_links (
                 playlist_id, source_id, native_playlist_id, mode, last_synced_name,
                 digest_version, membership_digest, last_success_at_ms,
                 local_state, remote_state, state_revision
             ) VALUES (?, ?, ?, 'pull_read_only_v1', 'Server Playlist',
                       1, zeroblob(32), 1, 'clean', 'present', 0)",
            [
                playlist_id.into(),
                REMOTE_SOURCE_ID.into(),
                native_id.into(),
            ],
        )
    }

    async fn row_count(connection: &impl ConnectionTrait, table: &str) -> i64 {
        connection
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT COUNT(*) AS count FROM {table}"),
            ))
            .await
            .expect("count rows")
            .expect("count returns one row")
            .try_get("", "count")
            .expect("count is integer")
    }

    async fn execute_effective_update(
        db: &DatabaseConnection,
        sql: &str,
        expected_revision: &mut i64,
    ) {
        db.execute_unprepared(sql)
            .await
            .expect("execute effective update");
        *expected_revision += 1;
        assert_eq!(revision(db).await, *expected_revision, "SQL: {sql}");
    }

    #[tokio::test]
    async fn fresh_up_creates_and_revalidates_the_exact_singleton_and_six_triggers() {
        let db = migrated_database().await;
        let manager = SchemaManager::new(&db);

        validate_installation(&manager)
            .await
            .expect("fresh installation is exact");
        revalidate(&db)
            .await
            .expect("startup revalidation accepts canonical objects");
        assert!(migration_is_applied(&db).await);
        assert_eq!(revision(&db).await, 0);

        let trigger_rows = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT name, tbl_name FROM sqlite_master
                 WHERE type = 'trigger'
                   AND name LIKE 'trg_playlist_sidebar_revision_%'
                 ORDER BY name"
                    .to_string(),
            ))
            .await
            .expect("inspect triggers");
        assert_eq!(trigger_rows.len(), 6);
        for definition in TRIGGERS {
            let object = objects_named(&manager, definition.name)
                .await
                .expect("inspect canonical trigger");
            assert_eq!(object.len(), 1);
            assert_eq!(object[0].object_type, "trigger");
            assert_eq!(object[0].table_name, definition.table);
        }

        db.execute_unprepared(&format!(
            "INSERT INTO {TABLE} (singleton, revision) VALUES (2, 0)"
        ))
        .await
        .expect_err("a second singleton identity is forbidden");
        db.execute_unprepared(&format!(
            "UPDATE {TABLE} SET revision = 1.5 WHERE singleton = {SINGLETON}"
        ))
        .await
        .expect_err("non-integer revision storage is forbidden");
        db.execute_unprepared(&format!(
            "UPDATE {TABLE} SET revision = -1 WHERE singleton = {SINGLETON}"
        ))
        .await
        .expect_err("negative revisions are forbidden");
        assert_eq!(revision(&db).await, 0);
    }

    #[tokio::test]
    async fn every_insert_effective_update_and_delete_advances_but_no_op_updates_do_not() {
        let db = migrated_database().await;

        db.execute(playlist_insert_sql("playlist-1"))
            .await
            .expect("insert playlist");
        assert_eq!(revision(&db).await, 1);

        db.execute_unprepared("UPDATE playlists SET name = 'Renamed' WHERE id = 'playlist-1'")
            .await
            .expect("update playlist");
        assert_eq!(revision(&db).await, 2);
        db.execute_unprepared(
            "UPDATE playlists
             SET id = id, name = name, is_smart = is_smart,
                 smart_rules_json = smart_rules_json, limit_enabled = limit_enabled,
                 limit_value = limit_value, limit_unit = limit_unit,
                 limit_sort = limit_sort, match_mode = match_mode,
                 live_updating = live_updating, created_at = created_at,
                 updated_at = updated_at
             WHERE id = 'playlist-1'",
        )
        .await
        .expect("actual playlist no-op");
        assert_eq!(revision(&db).await, 2);

        db.execute(link_insert_sql("playlist-1", "native-1"))
            .await
            .expect("insert link");
        assert_eq!(revision(&db).await, 3);
        db.execute_unprepared(
            "UPDATE server_playlist_links
             SET local_state = 'conflict' WHERE playlist_id = 'playlist-1'",
        )
        .await
        .expect("update link");
        assert_eq!(revision(&db).await, 4);
        db.execute_unprepared(
            "UPDATE server_playlist_links
             SET playlist_id = playlist_id, source_id = source_id,
                 native_playlist_id = native_playlist_id, mode = mode,
                 last_synced_name = last_synced_name,
                 digest_version = digest_version, membership_digest = membership_digest,
                 last_success_at_ms = last_success_at_ms, local_state = local_state,
                 remote_state = remote_state, state_revision = state_revision
             WHERE playlist_id = 'playlist-1'",
        )
        .await
        .expect("actual link no-op");
        assert_eq!(revision(&db).await, 4);

        db.execute_unprepared("DELETE FROM server_playlist_links WHERE playlist_id = 'playlist-1'")
            .await
            .expect("delete link");
        assert_eq!(revision(&db).await, 5);
        db.execute_unprepared("DELETE FROM playlists WHERE id = 'playlist-1'")
            .await
            .expect("delete playlist");
        assert_eq!(revision(&db).await, 6);
    }

    #[tokio::test]
    async fn every_mutable_projection_column_is_covered_by_the_update_guards() {
        let db = migrated_database().await;
        db.execute_unprepared(
            "INSERT INTO playlists (id, name, created_at, updated_at) VALUES
                 ('playlist-1', 'One', 'created-1', 'updated-1'),
                 ('playlist-2', 'Two', 'created-2', 'updated-2')",
        )
        .await
        .expect("insert parent playlists");
        let mut expected = 2;

        for sql in [
            "UPDATE playlists SET id = 'playlist-renamed' WHERE id = 'playlist-1'",
            "UPDATE playlists SET name = 'Renamed' WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET is_smart = 1 WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET smart_rules_json = '{}' WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET limit_enabled = 1 WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET limit_value = 25 WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET limit_unit = 'songs' WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET limit_sort = 'random' WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET match_mode = 'any' WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET live_updating = 0 WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET created_at = 'created-changed' WHERE id = 'playlist-renamed'",
            "UPDATE playlists SET updated_at = 'updated-changed' WHERE id = 'playlist-renamed'",
        ] {
            execute_effective_update(&db, sql, &mut expected).await;
        }

        db.execute(link_insert_sql("playlist-renamed", "native-1"))
            .await
            .expect("insert link");
        expected += 1;
        assert_eq!(revision(&db).await, expected);
        for sql in [
            "UPDATE server_playlist_links SET playlist_id = 'playlist-2'
             WHERE playlist_id = 'playlist-renamed'",
            "UPDATE server_playlist_links
             SET source_id = '22222222-2222-4222-8222-222222222222'
             WHERE playlist_id = 'playlist-2'",
            "UPDATE server_playlist_links SET native_playlist_id = 'native-2'
             WHERE playlist_id = 'playlist-2'",
            "UPDATE server_playlist_links SET last_synced_name = 'Renamed Server Playlist'
             WHERE playlist_id = 'playlist-2'",
            "UPDATE server_playlist_links
             SET membership_digest = X'0100000000000000000000000000000000000000000000000000000000000000'
             WHERE playlist_id = 'playlist-2'",
            "UPDATE server_playlist_links SET last_success_at_ms = 2
             WHERE playlist_id = 'playlist-2'",
            "UPDATE server_playlist_links SET local_state = 'conflict'
             WHERE playlist_id = 'playlist-2'",
            "UPDATE server_playlist_links SET remote_state = 'missing'
             WHERE playlist_id = 'playlist-2'",
            "UPDATE server_playlist_links SET state_revision = 1
             WHERE playlist_id = 'playlist-2'",
        ] {
            execute_effective_update(&db, sql, &mut expected).await;
        }

        // `mode` and `digest_version` are immutable by migration-14 CHECKs;
        // assigning their sole canonical values is an actual no-op.
        db.execute_unprepared(
            "UPDATE server_playlist_links
             SET mode = 'pull_read_only_v1', digest_version = 1
             WHERE playlist_id = 'playlist-2'",
        )
        .await
        .expect("immutable-column no-op");
        assert_eq!(revision(&db).await, expected);
    }

    #[tokio::test]
    async fn raw_multirow_dml_and_foreign_key_cascades_advance_per_changed_row() {
        let db = migrated_database().await;

        db.execute_unprepared(
            "INSERT INTO playlists (id, name, created_at, updated_at) VALUES
                 ('playlist-1', 'One', '2026-07-20', '2026-07-20'),
                 ('playlist-2', 'Two', '2026-07-20', '2026-07-20')",
        )
        .await
        .expect("raw multirow playlist insert");
        assert_eq!(revision(&db).await, 2);

        db.execute_unprepared(&format!(
            "INSERT INTO server_playlist_links (
                 playlist_id, source_id, native_playlist_id, mode, last_synced_name,
                 digest_version, membership_digest, last_success_at_ms,
                 local_state, remote_state, state_revision
             ) VALUES
                 ('playlist-1', '{REMOTE_SOURCE_ID}', 'native-1', 'pull_read_only_v1',
                  'One', 1, zeroblob(32), 1, 'clean', 'present', 0),
                 ('playlist-2', '{REMOTE_SOURCE_ID}', 'native-2', 'pull_read_only_v1',
                  'Two', 1, zeroblob(32), 1, 'clean', 'present', 0)"
        ))
        .await
        .expect("raw multirow link insert");
        assert_eq!(revision(&db).await, 4);

        db.execute_unprepared("UPDATE playlists SET name = name || ' changed'")
            .await
            .expect("raw multirow effective update");
        assert_eq!(revision(&db).await, 6);
        db.execute_unprepared("UPDATE playlists SET name = name")
            .await
            .expect("raw multirow no-op update");
        assert_eq!(revision(&db).await, 6);

        db.execute_unprepared("DELETE FROM playlists")
            .await
            .expect("cascade link deletion from multirow playlist delete");
        assert_eq!(row_count(&db, "playlists").await, 0);
        assert_eq!(row_count(&db, "server_playlist_links").await, 0);
        assert_eq!(revision(&db).await, 10);
    }

    #[tokio::test]
    async fn statement_and_transaction_rollbacks_restore_domain_rows_and_revision() {
        let db = migrated_database().await;

        db.execute_unprepared(
            "INSERT INTO playlists (id, name, created_at, updated_at) VALUES
                 ('statement-rollback', 'First', 'created', 'updated'),
                 ('statement-rollback', 'Duplicate', 'created', 'updated')",
        )
        .await
        .expect_err("later row failure rolls back prior row trigger effects");
        assert_eq!(revision(&db).await, 0);
        assert_eq!(row_count(&db, "playlists").await, 0);

        let transaction = db.begin().await.expect("begin transaction");

        transaction
            .execute(playlist_insert_sql("rolled-back"))
            .await
            .expect("insert in transaction");
        transaction
            .execute(link_insert_sql("rolled-back", "native-rollback"))
            .await
            .expect("insert link in transaction");
        assert_eq!(revision(&transaction).await, 2);
        transaction.rollback().await.expect("roll back transaction");

        assert_eq!(revision(&db).await, 0);
        assert_eq!(row_count(&db, "playlists").await, 0);
        assert_eq!(row_count(&db, "server_playlist_links").await, 0);
    }

    #[tokio::test]
    async fn missing_singleton_aborts_and_rolls_back_the_complete_mutation() {
        let db = migrated_database().await;
        db.execute(playlist_insert_sql("survivor"))
            .await
            .expect("insert precondition playlist");
        db.execute(link_insert_sql("survivor", "native-survivor"))
            .await
            .expect("insert precondition link");
        db.execute_unprepared(&format!(
            "DELETE FROM {TABLE} WHERE singleton = {SINGLETON}"
        ))
        .await
        .expect("simulate deleted singleton");

        let insert_error = db
            .execute(playlist_insert_sql("must-not-exist"))
            .await
            .expect_err("missing singleton must abort insert");
        assert!(insert_error.to_string().contains("singleton missing"));
        assert_eq!(row_count(&db, "playlists").await, 1);

        let cascade_error = db
            .execute_unprepared("DELETE FROM playlists WHERE id = 'survivor'")
            .await
            .expect_err("missing singleton must abort cascade and parent delete");
        assert!(cascade_error.to_string().contains("singleton missing"));
        assert_eq!(row_count(&db, "playlists").await, 1);
        assert_eq!(row_count(&db, "server_playlist_links").await, 1);
        revalidate(&db)
            .await
            .expect_err("startup revalidation detects missing singleton");
    }

    #[tokio::test]
    async fn exhausted_revision_aborts_changes_but_still_allows_actual_no_op_updates() {
        let db = migrated_database().await;
        db.execute(playlist_insert_sql("survivor"))
            .await
            .expect("insert precondition playlist");
        db.execute_unprepared(&format!(
            "UPDATE {TABLE} SET revision = {} WHERE singleton = {SINGLETON}",
            i64::MAX - 1
        ))
        .await
        .expect("set penultimate revision");
        db.execute_unprepared("UPDATE playlists SET name = 'Last allowed' WHERE id = 'survivor'")
            .await
            .expect("the last representable increment succeeds");
        assert_eq!(revision(&db).await, i64::MAX);

        db.execute_unprepared("UPDATE playlists SET name = name WHERE id = 'survivor'")
            .await
            .expect("no-op update does not need a new revision");
        assert_eq!(revision(&db).await, i64::MAX);

        let update_error = db
            .execute_unprepared("UPDATE playlists SET name = 'Changed' WHERE id = 'survivor'")
            .await
            .expect_err("effective update must fail at maximum revision");
        assert!(update_error.to_string().contains("revision exhausted"));
        let name: String = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT name FROM playlists WHERE id = 'survivor'".to_string(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "name")
            .unwrap();
        assert_eq!(name, "Last allowed");

        let delete_error = db
            .execute_unprepared("DELETE FROM playlists WHERE id = 'survivor'")
            .await
            .expect_err("delete must fail at maximum revision");
        assert!(delete_error.to_string().contains("revision exhausted"));
        assert_eq!(row_count(&db, "playlists").await, 1);

        let multirow_error = db
            .execute_unprepared(
                "INSERT INTO playlists (id, name, created_at, updated_at) VALUES
                     ('first-aborted', 'First', 'created', 'updated'),
                     ('second-aborted', 'Second', 'created', 'updated')",
            )
            .await
            .expect_err("revision exhaustion must roll back the complete multirow statement");
        assert!(multirow_error.to_string().contains("revision exhausted"));
        assert_eq!(row_count(&db, "playlists").await, 1);
        assert_eq!(revision(&db).await, i64::MAX);
    }

    #[tokio::test]
    async fn exact_interrupted_install_is_retryable_but_partial_and_altered_targets_are_refused() {
        let exact = database_at_migration_14().await;
        Migration
            .up(&SchemaManager::new(&exact))
            .await
            .expect("install DDL before migration ledger write");
        assert!(!migration_is_applied(&exact).await);
        Migrator::up(&exact, Some(1))
            .await
            .expect("retry exact interrupted installation");
        assert!(migration_is_applied(&exact).await);
        assert_eq!(revision(&exact).await, 0);
        Migration
            .up(&SchemaManager::new(&exact))
            .await
            .expect("direct exact retry is idempotent");

        let partial = database_at_migration_14().await;
        partial
            .execute_unprepared(&canonical_table_sql())
            .await
            .expect("install only the derived table");
        partial
            .execute_unprepared(&format!(
                "INSERT INTO {TABLE} (singleton, revision) VALUES ({SINGLETON}, 0)"
            ))
            .await
            .expect("seed partial table");
        let partial_error = Migrator::up(&partial, Some(1))
            .await
            .expect_err("partial installation must fail closed");
        assert!(partial_error.to_string().contains("trigger object"));
        assert!(!migration_is_applied(&partial).await);
        assert_eq!(revision(&partial).await, 0);

        let near_match_table = database_at_migration_14().await;
        near_match_table
            .execute_unprepared(
                &canonical_table_sql().replace("revision BETWEEN 0", "revision BETWEEN 1"),
            )
            .await
            .expect("install near-match table");
        near_match_table
            .execute_unprepared(&format!(
                "INSERT INTO {TABLE} (singleton, revision) VALUES ({SINGLETON}, 1)"
            ))
            .await
            .expect("seed near-match table");
        let table_error = Migrator::up(&near_match_table, Some(1))
            .await
            .expect_err("near-match table SQL must fail closed");
        assert!(table_error.to_string().contains("exact canonical table"));
        assert!(!migration_is_applied(&near_match_table).await);
        assert_eq!(revision(&near_match_table).await, 1);

        let altered = database_at_migration_14().await;
        Migration
            .up(&SchemaManager::new(&altered))
            .await
            .expect("install canonical target before alteration");
        altered
            .execute_unprepared(&format!("DROP TRIGGER {PLAYLIST_UPDATE_TRIGGER}"))
            .await
            .expect("drop canonical trigger");
        let altered_sql = canonical_trigger_sql(TriggerDefinition::new(
            PLAYLIST_UPDATE_TRIGGER,
            "UPDATE",
            "playlists",
            Some("OLD.name IS NOT NEW.name"),
        ));
        altered
            .execute_unprepared(&altered_sql)
            .await
            .expect("install near-match trigger");
        let altered_error = Migrator::up(&altered, Some(1))
            .await
            .expect_err("altered trigger must fail closed");
        assert!(altered_error
            .to_string()
            .contains("exact canonical trigger"));
        assert!(!migration_is_applied(&altered).await);
    }

    #[tokio::test]
    async fn object_collisions_are_refused_without_replacing_or_completing_them() {
        let table_collision = database_at_migration_14().await;
        table_collision
            .execute_unprepared(&format!(
                "CREATE VIEW {TABLE} AS SELECT id AS singleton, 0 AS revision FROM playlists"
            ))
            .await
            .expect("create colliding view");
        let error = Migrator::up(&table_collision, Some(1))
            .await
            .expect_err("view collision must fail");
        assert!(error.to_string().contains("must be a table"));
        assert_eq!(
            objects_named(&SchemaManager::new(&table_collision), TABLE)
                .await
                .unwrap()[0]
                .object_type,
            "view"
        );
        assert!(!migration_is_applied(&table_collision).await);

        let trigger_collision = database_at_migration_14().await;
        trigger_collision
            .execute_unprepared(&format!(
                "CREATE TRIGGER {PLAYLIST_INSERT_TRIGGER}
                 AFTER INSERT ON playlists BEGIN SELECT 1; END"
            ))
            .await
            .expect("reserve canonical trigger name with noncanonical SQL");
        let error = Migrator::up(&trigger_collision, Some(1))
            .await
            .expect_err("trigger collision must fail");
        assert!(error
            .to_string()
            .contains("must resolve to exactly one table"));
        assert!(
            objects_named(&SchemaManager::new(&trigger_collision), TABLE)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            objects_named(
                &SchemaManager::new(&trigger_collision),
                PLAYLIST_INSERT_TRIGGER
            )
            .await
            .unwrap()[0]
                .object_type,
            "trigger"
        );
    }

    #[tokio::test]
    async fn migration_14_boundary_is_enforced_and_failed_early_ddl_is_atomic() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open database");
        Migrator::up(&db, Some(13))
            .await
            .expect("stop immediately before server-playlist links");

        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect_err("migration 15 cannot precede its migration-14 dependency");
        assert!(objects_named(&SchemaManager::new(&db), TABLE)
            .await
            .unwrap()
            .is_empty());
        for trigger in TRIGGERS {
            assert!(objects_named(&SchemaManager::new(&db), trigger.name)
                .await
                .unwrap()
                .is_empty());
        }

        Migrator::up(&db, Some(1))
            .await
            .expect("apply migration 14");
        assert!(
            objects_named(&SchemaManager::new(&db), "server_playlist_links")
                .await
                .unwrap()
                .iter()
                .any(|object| object.object_type == "table")
        );
        Migrator::up(&db, Some(1))
            .await
            .expect("apply migration 15 at its exact boundary");
        validate_installation(&SchemaManager::new(&db))
            .await
            .expect("boundary migration is canonical");
    }

    #[tokio::test]
    async fn down_then_up_is_lossless_for_playlists_and_links_and_resets_only_derived_state() {
        let db = migrated_database().await;
        db.execute(playlist_insert_sql("preserved"))
            .await
            .expect("insert preserved playlist");
        db.execute(link_insert_sql("preserved", "native-preserved"))
            .await
            .expect("insert preserved link");
        assert_eq!(revision(&db).await, 2);

        let playlist_before = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT * FROM playlists WHERE id = 'preserved'".to_string(),
            ))
            .await
            .unwrap()
            .unwrap();
        let link_before = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT * FROM server_playlist_links WHERE playlist_id = 'preserved'".to_string(),
            ))
            .await
            .unwrap()
            .unwrap();

        Migrator::down(&db, Some(1))
            .await
            .expect("downgrade derived revision");
        assert!(target_objects_absent(&SchemaManager::new(&db))
            .await
            .unwrap());
        assert_eq!(row_count(&db, "playlists").await, 1);
        assert_eq!(row_count(&db, "server_playlist_links").await, 1);
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("already-absent direct downgrade is idempotent");

        Migrator::up(&db, Some(1))
            .await
            .expect("reinstall derived revision");
        assert_eq!(revision(&db).await, 0);
        let playlist_after = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT * FROM playlists WHERE id = 'preserved'".to_string(),
            ))
            .await
            .unwrap()
            .unwrap();
        let link_after = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT * FROM server_playlist_links WHERE playlist_id = 'preserved'".to_string(),
            ))
            .await
            .unwrap()
            .unwrap();

        for column in ["id", "name", "match_mode", "created_at", "updated_at"] {
            assert_eq!(
                playlist_before.try_get::<String>("", column).unwrap(),
                playlist_after.try_get::<String>("", column).unwrap()
            );
        }
        for column in ["is_smart", "limit_enabled", "live_updating"] {
            assert_eq!(
                playlist_before.try_get::<i64>("", column).unwrap(),
                playlist_after.try_get::<i64>("", column).unwrap()
            );
        }
        for column in ["smart_rules_json", "limit_unit", "limit_sort"] {
            assert_eq!(
                playlist_before
                    .try_get::<Option<String>>("", column)
                    .unwrap(),
                playlist_after
                    .try_get::<Option<String>>("", column)
                    .unwrap()
            );
        }
        assert_eq!(
            playlist_before
                .try_get::<Option<i64>>("", "limit_value")
                .unwrap(),
            playlist_after
                .try_get::<Option<i64>>("", "limit_value")
                .unwrap()
        );
        for column in [
            "playlist_id",
            "source_id",
            "native_playlist_id",
            "mode",
            "last_synced_name",
            "local_state",
            "remote_state",
        ] {
            assert_eq!(
                link_before.try_get::<String>("", column).unwrap(),
                link_after.try_get::<String>("", column).unwrap()
            );
        }
        for column in ["digest_version", "last_success_at_ms", "state_revision"] {
            assert_eq!(
                link_before.try_get::<i64>("", column).unwrap(),
                link_after.try_get::<i64>("", column).unwrap()
            );
        }
        assert_eq!(
            link_before
                .try_get::<Vec<u8>>("", "membership_digest")
                .unwrap(),
            link_after
                .try_get::<Vec<u8>>("", "membership_digest")
                .unwrap()
        );
    }

    #[tokio::test]
    async fn down_refuses_partial_or_tampered_objects_atomically() {
        let db = migrated_database().await;
        db.execute(playlist_insert_sql("preserved"))
            .await
            .expect("insert preserved playlist");
        db.execute_unprepared(&format!("DROP TRIGGER {LINK_DELETE_TRIGGER}"))
            .await
            .expect("tamper with installation");

        let error = Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect_err("partial installation must not be partially dropped");
        assert!(error.to_string().contains("trigger object"));
        assert_eq!(row_count(&db, "playlists").await, 1);
        assert_eq!(revision(&db).await, 1);
        assert_eq!(
            objects_named(&SchemaManager::new(&db), PLAYLIST_INSERT_TRIGGER)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            objects_named(&SchemaManager::new(&db), TABLE)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn startup_revalidation_rejects_missing_or_noncanonical_critical_objects() {
        let missing_trigger = migrated_database().await;
        missing_trigger
            .execute_unprepared(&format!("DROP TRIGGER {LINK_UPDATE_TRIGGER}"))
            .await
            .expect("remove critical trigger");
        let error = revalidate(&missing_trigger)
            .await
            .expect_err("startup must reject missing trigger");
        assert!(error.to_string().contains("trigger object"));

        let missing_row = migrated_database().await;
        missing_row
            .execute_unprepared(&format!("DELETE FROM {TABLE}"))
            .await
            .expect("remove critical singleton row");
        let error = revalidate(&missing_row)
            .await
            .expect_err("startup must reject missing singleton row");
        assert!(error.to_string().contains("exactly one singleton row"));

        let extra_index = migrated_database().await;
        extra_index
            .execute_unprepared(&format!(
                "CREATE INDEX idx_playlist_sidebar_revision_extra ON {TABLE} (revision)"
            ))
            .await
            .expect("add noncanonical index");
        let error = revalidate(&extra_index)
            .await
            .expect_err("startup must reject noncanonical schema additions");
        assert!(error.to_string().contains("unexpected indexes"));

        for table in ["playlists", "server_playlist_links"] {
            let extra_trigger = database_at_migration_14().await;
            let trigger_name = format!("trg_unowned_{table}_mutation");
            extra_trigger
                .execute_unprepared(&format!(
                    "CREATE TRIGGER {trigger_name}
                     AFTER UPDATE ON {table} BEGIN SELECT 1; END"
                ))
                .await
                .expect("install unowned trigger before migration");
            let error = Migrator::up(&extra_trigger, Some(1))
                .await
                .expect_err("migration must refuse an unowned source-table trigger");
            let message = error.to_string();
            assert!(message.contains("unexpected trigger set"));
            assert!(!message.contains(&trigger_name));
            assert!(!migration_is_applied(&extra_trigger).await);
            assert!(objects_named(&SchemaManager::new(&extra_trigger), TABLE)
                .await
                .unwrap()
                .is_empty());
            assert_eq!(
                objects_named(&SchemaManager::new(&extra_trigger), &trigger_name)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
    }
}
