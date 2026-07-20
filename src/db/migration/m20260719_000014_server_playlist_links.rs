//! Migration: persist pull-only Subsonic server-playlist links.
//!
//! The link is deliberately separate from regular-playlist occurrences. It
//! stores only durable, non-secret synchronization identity and state; source
//! sessions, routes, locators, credentials, errors, and operation generations
//! remain transient authority.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, Statement, TransactionTrait};

const TABLE: &str = "server_playlist_links";
const SOURCE_NATIVE_INDEX: &str = "idx_server_playlist_links_source_native";

// Frozen persistent-format values. These intentionally do not call mutable
// application helpers from a historical migration.
const NIL_SOURCE_ID: &str = "00000000-0000-0000-0000-000000000000";
const LOCAL_SOURCE_ID: &str = "dbae1f16-7921-5209-939e-ce3177ec7b57";
const RADIO_BROWSER_SOURCE_ID: &str = "39f5ad82-6349-5d36-b498-3b8904e9dcb4";
const LINK_MODE: &str = "pull_read_only_v1";
const DIGEST_VERSION: i32 = 1;
const DIGEST_BYTES: usize = 32;
const MAX_LOCAL_PLAYLIST_ID_BYTES: usize = 4 * 1024;
const MAX_NATIVE_PLAYLIST_ID_BYTES: usize = 4 * 1024;
const MAX_SYNCED_NAME_BYTES: usize = 16 * 1024;
const MAX_SUCCESS_AT_MS: i64 = 253_402_300_799_999;

const PLAYLIST_ID_CHECK: &str = "ck_server_playlist_links_playlist_id";
const SOURCE_ID_CHECK: &str = "ck_server_playlist_links_source_id";
const NATIVE_ID_CHECK: &str = "ck_server_playlist_links_native_id";
const MODE_CHECK: &str = "ck_server_playlist_links_mode";
const NAME_CHECK: &str = "ck_server_playlist_links_name";
const DIGEST_CHECK: &str = "ck_server_playlist_links_digest";
const SUCCESS_CHECK: &str = "ck_server_playlist_links_success";
const LOCAL_STATE_CHECK: &str = "ck_server_playlist_links_local_state";
const REMOTE_STATE_CHECK: &str = "ck_server_playlist_links_remote_state";
const REVISION_CHECK: &str = "ck_server_playlist_links_revision";
const PLAYLIST_FOREIGN_KEY: &str = "fk_server_playlist_links_playlist";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_links(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_links(manager, false).await
    }
}

/// Own the complete SQLite DDL operation. A table/index collision, validation
/// failure, or downgrade refusal therefore leaves both schema and rows intact.
async fn migrate_links(manager: &SchemaManager<'_>, create: bool) -> Result<(), DbErr> {
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
            "{original}; additionally failed to roll back server-playlist link migration: \
             {rollback_error}"
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
                .execute_unprepared(&canonical_index_sql())
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
    let Some(object_type) = object_type(manager, TABLE).await? else {
        return Ok(());
    };
    if object_type != "table" {
        return Err(DbErr::Migration(format!(
            "{TABLE} must be a table, found {object_type}"
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
        .ok_or_else(|| DbErr::Migration("failed to inspect server-playlist links".to_string()))?;
    let count: i64 = row.try_get("", "count")?;
    if count != 0 {
        return Err(DbErr::Migration(format!(
            "cannot downgrade {count} linked server playlist row(s) losslessly; unlink them first"
        )));
    }

    manager
        .get_connection()
        .execute_unprepared(&format!("DROP TABLE {TABLE}"))
        .await?;
    Ok(())
}

fn canonical_table_sql() -> String {
    format!(
        "CREATE TABLE {TABLE} (
             playlist_id VARCHAR COLLATE BINARY PRIMARY KEY NOT NULL,
             source_id VARCHAR COLLATE BINARY NOT NULL,
             native_playlist_id VARCHAR COLLATE BINARY NOT NULL,
             mode VARCHAR NOT NULL,
             last_synced_name VARCHAR NOT NULL,
             digest_version INTEGER NOT NULL,
             membership_digest BLOB NOT NULL,
             last_success_at_ms INTEGER NOT NULL,
             local_state VARCHAR NOT NULL,
             remote_state VARCHAR NOT NULL,
             state_revision INTEGER NOT NULL,
             CONSTRAINT {PLAYLIST_ID_CHECK} CHECK (
                 typeof(playlist_id) = 'text'
                 AND length(CAST(playlist_id AS BLOB)) BETWEEN 1 AND {MAX_LOCAL_PLAYLIST_ID_BYTES}
             ),
             CONSTRAINT {SOURCE_ID_CHECK} CHECK (
                 typeof(source_id) = 'text'
                 AND length(CAST(source_id AS BLOB)) = 36
                 AND source_id = lower(source_id)
                 AND substr(source_id, 9, 1) = '-'
                 AND substr(source_id, 14, 1) = '-'
                 AND substr(source_id, 19, 1) = '-'
                 AND substr(source_id, 24, 1) = '-'
                 AND length(replace(source_id, '-', '')) = 32
                 AND replace(source_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                 AND source_id NOT IN (
                     '{NIL_SOURCE_ID}', '{LOCAL_SOURCE_ID}', '{RADIO_BROWSER_SOURCE_ID}'
                 )
             ),
             CONSTRAINT {NATIVE_ID_CHECK} CHECK (
                 typeof(native_playlist_id) = 'text'
                 AND length(CAST(native_playlist_id AS BLOB))
                     BETWEEN 1 AND {MAX_NATIVE_PLAYLIST_ID_BYTES}
             ),
             CONSTRAINT {MODE_CHECK} CHECK (
                 typeof(mode) = 'text' AND mode = '{LINK_MODE}'
             ),
             CONSTRAINT {NAME_CHECK} CHECK (
                 typeof(last_synced_name) = 'text'
                 AND length(CAST(last_synced_name AS BLOB)) <= {MAX_SYNCED_NAME_BYTES}
             ),
             CONSTRAINT {DIGEST_CHECK} CHECK (
                 typeof(digest_version) = 'integer'
                 AND digest_version = {DIGEST_VERSION}
                 AND typeof(membership_digest) = 'blob'
                 AND length(membership_digest) = {DIGEST_BYTES}
             ),
             CONSTRAINT {SUCCESS_CHECK} CHECK (
                 typeof(last_success_at_ms) = 'integer'
                 AND last_success_at_ms BETWEEN 0 AND {MAX_SUCCESS_AT_MS}
             ),
             CONSTRAINT {LOCAL_STATE_CHECK} CHECK (
                 typeof(local_state) = 'text'
                 AND local_state IN ('clean', 'conflict')
             ),
             CONSTRAINT {REMOTE_STATE_CHECK} CHECK (
                 typeof(remote_state) = 'text'
                 AND remote_state IN ('present', 'missing')
             ),
             CONSTRAINT {REVISION_CHECK} CHECK (
                 typeof(state_revision) = 'integer' AND state_revision >= 0
             ),
             CONSTRAINT {PLAYLIST_FOREIGN_KEY}
                 FOREIGN KEY (playlist_id)
                 REFERENCES playlists (id)
                 ON DELETE CASCADE
         )"
    )
}

fn canonical_index_sql() -> String {
    format!(
        "CREATE UNIQUE INDEX {SOURCE_NATIVE_INDEX}
         ON {TABLE} (source_id COLLATE BINARY, native_playlist_id COLLATE BINARY)"
    )
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

type ColumnSchema = (String, String, i32, Option<String>, i32);

async fn validate_schema(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    validate_columns(manager).await?;
    validate_table_sql(manager).await?;
    validate_foreign_key(manager).await?;
    validate_indexes(manager).await?;
    validate_no_triggers(manager).await?;
    validate_foreign_key_rows(manager).await
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
                row.try_get("", "name")?,
                row.try_get::<String>("", "type")?.to_ascii_lowercase(),
                row.try_get("", "notnull")?,
                row.try_get("", "dflt_value")?,
                row.try_get("", "pk")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let column = |name: &str, sql_type: &str, primary_key: i32| {
        (name.to_string(), sql_type.to_string(), 1, None, primary_key)
    };
    let expected = vec![
        column("playlist_id", "varchar", 1),
        column("source_id", "varchar", 0),
        column("native_playlist_id", "varchar", 0),
        column("mode", "varchar", 0),
        column("last_synced_name", "varchar", 0),
        column("digest_version", "integer", 0),
        column("membership_digest", "blob", 0),
        column("last_success_at_ms", "integer", 0),
        column("local_state", "varchar", 0),
        column("remote_state", "varchar", 0),
        column("state_revision", "integer", 0),
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
        .query_one(Statement::from_string(
            manager.get_database_backend(),
            format!("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = '{TABLE}'"),
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

async fn validate_foreign_key(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA foreign_key_list('{TABLE}')"),
        ))
        .await?;
    let actual = rows
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>((
                row.try_get::<String>("", "from")?,
                row.try_get::<String>("", "table")?,
                row.try_get::<String>("", "to")?,
                row.try_get::<String>("", "on_update")?.to_ascii_uppercase(),
                row.try_get::<String>("", "on_delete")?.to_ascii_uppercase(),
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let expected = vec![(
        "playlist_id".to_string(),
        "playlists".to_string(),
        "id".to_string(),
        "NO ACTION".to_string(),
        "CASCADE".to_string(),
    )];
    if actual != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has unexpected foreign keys: {actual:?}"
        )));
    }
    Ok(())
}

async fn validate_indexes(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_list('{TABLE}')"),
        ))
        .await?;
    let mut explicit = Vec::new();
    let mut implicit = Vec::new();
    for row in rows {
        let index = (
            row.try_get::<String>("", "name")?,
            row.try_get::<i32>("", "unique")? == 1,
            row.try_get::<String>("", "origin")?,
            row.try_get::<i32>("", "partial")? == 1,
        );
        if index.2 == "c" {
            explicit.push(index);
        } else {
            implicit.push(index);
        }
    }
    if explicit
        != [(
            SOURCE_NATIVE_INDEX.to_string(),
            true,
            "c".to_string(),
            false,
        )]
    {
        return Err(DbErr::Migration(format!(
            "{TABLE} has unexpected explicit indexes: {explicit:?}"
        )));
    }
    let [(primary_name, true, origin, false)] = implicit.as_slice() else {
        return Err(DbErr::Migration(format!(
            "{TABLE} has unexpected implicit indexes: {implicit:?}"
        )));
    };
    if origin != "pk" {
        return Err(DbErr::Migration(format!(
            "{TABLE} primary-key index has origin {origin}"
        )));
    }

    validate_index_columns(manager, primary_name, &["playlist_id"]).await?;
    validate_index_columns(
        manager,
        SOURCE_NATIVE_INDEX,
        &["source_id", "native_playlist_id"],
    )
    .await?;

    let row = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT sql FROM sqlite_master
             WHERE type = 'index' AND name = ? AND tbl_name = ?",
            [SOURCE_NATIVE_INDEX.into(), TABLE.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("{SOURCE_NATIVE_INDEX} SQL is missing")))?;
    let actual_sql: String = row.try_get("", "sql")?;
    if canonical_sql(&actual_sql) != canonical_sql(&canonical_index_sql()) {
        return Err(DbErr::Migration(format!(
            "{SOURCE_NATIVE_INDEX} does not have the exact canonical definition"
        )));
    }

    validate_index_collations(manager).await
}

async fn validate_index_columns(
    manager: &SchemaManager<'_>,
    index: &str,
    expected: &[&str],
) -> Result<(), DbErr> {
    let quoted = index.replace('\'', "''");
    let columns = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_info('{quoted}')"),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>((
                row.try_get::<i32>("", "seqno")?,
                row.try_get::<String>("", "name")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let actual: Vec<_> = columns.into_iter().map(|(_, name)| name).collect();
    let expected: Vec<_> = expected.iter().map(|value| (*value).to_string()).collect();
    if actual != expected {
        return Err(DbErr::Migration(format!(
            "{index} has columns {actual:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

async fn validate_index_collations(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_xinfo('{SOURCE_NATIVE_INDEX}')"),
        ))
        .await?;
    let keyed = rows
        .into_iter()
        .filter_map(|row| {
            let key = row.try_get::<i32>("", "key");
            match key {
                Ok(1) => Some(Ok((
                    row.try_get::<i32>("", "seqno"),
                    row.try_get::<String>("", "name"),
                    row.try_get::<String>("", "coll"),
                    row.try_get::<i32>("", "desc"),
                ))),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .map(|row: Result<_, DbErr>| {
            let (sequence, name, collation, descending) = row?;
            Ok((sequence?, name?, collation?, descending?))
        })
        .collect::<Result<Vec<_>, DbErr>>()?;
    let expected = vec![
        (0, "source_id".to_string(), "BINARY".to_string(), 0),
        (1, "native_playlist_id".to_string(), "BINARY".to_string(), 0),
    ];
    if keyed != expected {
        return Err(DbErr::Migration(format!(
            "{SOURCE_NATIVE_INDEX} has unexpected key/collation metadata: {keyed:?}"
        )));
    }
    Ok(())
}

async fn validate_foreign_key_rows(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let violations = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA foreign_key_check('{TABLE}')"),
        ))
        .await?;
    if !violations.is_empty() {
        return Err(DbErr::Migration(format!(
            "{TABLE} has {} foreign-key violation(s)",
            violations.len()
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

/// Normalize harmless SQLite formatting and identifier quoting while
/// preserving string literal contents and unknown double-quoted tokens.
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
            '"' => {
                let mut token = String::new();
                let mut raw = String::from("\"");
                while let Some(quoted_character) = characters.next() {
                    raw.push(quoted_character);
                    if quoted_character == '"' {
                        if characters.peek() == Some(&'"') {
                            raw.push(characters.next().expect("peeked quote exists"));
                            token.push('"');
                        } else {
                            break;
                        }
                    } else {
                        token.push(quoted_character);
                    }
                }
                if is_known_identifier(&token) {
                    canonical.extend(token.chars().flat_map(char::to_lowercase));
                } else {
                    canonical.push_str(&raw);
                }
            }
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

fn is_known_identifier(identifier: &str) -> bool {
    matches!(
        identifier.to_ascii_lowercase().as_str(),
        TABLE
            | "playlists"
            | "playlist_id"
            | "source_id"
            | "native_playlist_id"
            | "mode"
            | "last_synced_name"
            | "digest_version"
            | "membership_digest"
            | "last_success_at_ms"
            | "local_state"
            | "remote_state"
            | "state_revision"
            | PLAYLIST_ID_CHECK
            | SOURCE_ID_CHECK
            | NATIVE_ID_CHECK
            | MODE_CHECK
            | NAME_CHECK
            | DIGEST_CHECK
            | SUCCESS_CHECK
            | LOCAL_STATE_CHECK
            | REMOTE_STATE_CHECK
            | REVISION_CHECK
            | PLAYLIST_FOREIGN_KEY
            | SOURCE_NATIVE_INDEX
    )
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend, EntityTrait,
        PaginatorTrait, Statement,
    };

    use super::*;
    use crate::db::entities::server_playlist_link::{
        self, ServerPlaylistLinkMode, ServerPlaylistLocalState, ServerPlaylistRemoteState,
        StoredServerPlaylistLink,
    };
    use crate::db::migration::Migrator;

    const REMOTE_SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND_REMOTE_SOURCE_ID: &str = "22222222-2222-4222-8222-222222222222";

    async fn database_before_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory SQLite database");
        Migrator::up(&db, Some(13))
            .await
            .expect("apply migrations preceding server-playlist links");
        db
    }

    async fn migrated_database() -> DatabaseConnection {
        let db = database_before_migration().await;
        Migrator::up(&db, Some(1))
            .await
            .expect("apply server-playlist link migration");
        db
    }

    async fn insert_playlist(db: &DatabaseConnection, id: &str) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlists (id, name, created_at, updated_at)
             VALUES (?, 'Playlist', '2026-07-19T00:00:00Z', '2026-07-19T00:00:00Z')",
            [id.into()],
        ))
        .await
        .expect("insert playlist");
    }

    fn link_model(playlist_id: &str, native_id: &str) -> server_playlist_link::Model {
        server_playlist_link::Model {
            playlist_id: playlist_id.to_string(),
            source_id: REMOTE_SOURCE_ID.to_string(),
            native_playlist_id: native_id.to_string(),
            mode: LINK_MODE.to_string(),
            last_synced_name: "Server Playlist".to_string(),
            digest_version: DIGEST_VERSION,
            membership_digest: vec![0x5a; DIGEST_BYTES],
            last_success_at_ms: 1_752_937_200_123,
            local_state: "clean".to_string(),
            remote_state: "present".to_string(),
            state_revision: 0,
        }
    }

    async fn insert_link(
        db: &DatabaseConnection,
        model: server_playlist_link::Model,
    ) -> Result<server_playlist_link::Model, DbErr> {
        let active: server_playlist_link::ActiveModel = model.into();
        active.insert(db).await
    }

    async fn migration_is_applied(db: &DatabaseConnection) -> bool {
        let name = Migration.name().to_string();
        Migrator::get_migration_models(db)
            .await
            .expect("read migration ledger")
            .iter()
            .any(|migration| migration.version == name)
    }

    async fn target_exists(db: &DatabaseConnection) -> bool {
        object_type(&SchemaManager::new(db), TABLE)
            .await
            .expect("inspect target")
            == Some("table".to_string())
    }

    #[tokio::test]
    async fn fresh_up_creates_exact_schema_and_entity_round_trips() {
        let db = migrated_database().await;
        let manager = SchemaManager::new(&db);
        validate_schema(&manager).await.expect("canonical schema");
        assert!(migration_is_applied(&db).await);

        assert_eq!(
            LINK_MODE,
            crate::db::entities::server_playlist_link::SERVER_PLAYLIST_LINK_MODE
        );
        assert_eq!(
            DIGEST_VERSION,
            crate::db::entities::server_playlist_link::SERVER_PLAYLIST_DIGEST_VERSION
        );
        assert_eq!(
            DIGEST_BYTES,
            crate::db::entities::server_playlist_link::SERVER_PLAYLIST_DIGEST_BYTES
        );
        assert_eq!(
            MAX_SYNCED_NAME_BYTES,
            crate::db::entities::server_playlist_link::MAX_SERVER_PLAYLIST_LINK_NAME_BYTES
        );
        assert_eq!(
            MAX_LOCAL_PLAYLIST_ID_BYTES,
            crate::db::entities::server_playlist_link::MAX_SERVER_PLAYLIST_LOCAL_ID_BYTES
        );
        assert_eq!(
            MAX_NATIVE_PLAYLIST_ID_BYTES,
            crate::architecture::identity::MAX_NATIVE_PLAYLIST_ID_BYTES
        );
        assert_eq!(
            MAX_SUCCESS_AT_MS,
            crate::db::entities::server_playlist_link::MAX_SERVER_PLAYLIST_SUCCESS_AT_MS
        );
        assert_eq!(
            LOCAL_SOURCE_ID,
            crate::architecture::SourceId::local().to_string()
        );
        assert_eq!(
            RADIO_BROWSER_SOURCE_ID,
            crate::architecture::SourceId::radio_browser().to_string()
        );

        insert_playlist(&db, "playlist-1").await;
        let mut expected = link_model("playlist-1", " Case/Sensitive Native ☃");
        expected.last_synced_name = " Exact synchronized name ☃".to_string();
        expected.local_state = "conflict".to_string();
        expected.remote_state = "missing".to_string();
        expected.state_revision = 41;
        insert_link(&db, expected.clone())
            .await
            .expect("insert canonical entity");

        let raw = server_playlist_link::Entity::find_by_id("playlist-1")
            .one(&db)
            .await
            .expect("query link")
            .expect("link exists");
        assert_eq!(raw, expected);
        let stored = StoredServerPlaylistLink::try_from(raw).expect("validate stored entity");
        assert_eq!(
            stored.native_playlist_id.as_str(),
            " Case/Sensitive Native ☃"
        );
        assert_eq!(stored.mode, ServerPlaylistLinkMode::PullReadOnly);
        assert_eq!(stored.local_state, ServerPlaylistLocalState::Conflict);
        assert_eq!(stored.remote_state, ServerPlaylistRemoteState::Missing);
        assert_eq!(stored.state_revision, 41);
    }

    #[tokio::test]
    async fn up_preserves_every_predecessor_playlist_and_occurrence_field() {
        let db = database_before_migration().await;
        insert_playlist(&db, "preserved-playlist").await;
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, source_id, track_id, local_track_id,
                 match_title, match_artist, match_album, match_duration_secs, match_file_path
             ) VALUES (
                 'preserved-entry', 'preserved-playlist', 7, ?, 'native-track', NULL,
                 'title evidence', 'artist evidence', 'album evidence', 123, NULL
             )",
            [REMOTE_SOURCE_ID.into()],
        ))
        .await
        .expect("insert predecessor occurrence");

        Migrator::up(&db, Some(1)).await.expect("migrate");
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT * FROM playlist_entries WHERE id = 'preserved-entry'".to_string(),
            ))
            .await
            .expect("query occurrence")
            .expect("occurrence exists");
        assert_eq!(
            row.try_get::<String>("", "playlist_id").unwrap(),
            "preserved-playlist"
        );
        assert_eq!(row.try_get::<i32>("", "position").unwrap(), 7);
        assert_eq!(
            row.try_get::<String>("", "source_id").unwrap(),
            REMOTE_SOURCE_ID
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "track_id")
                .unwrap()
                .as_deref(),
            Some("native-track")
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "local_track_id").unwrap(),
            None
        );
        assert_eq!(
            row.try_get::<String>("", "match_title").unwrap(),
            "title evidence"
        );
        assert_eq!(
            row.try_get::<String>("", "match_artist").unwrap(),
            "artist evidence"
        );
        assert_eq!(
            row.try_get::<String>("", "match_album").unwrap(),
            "album evidence"
        );
        assert_eq!(
            row.try_get::<Option<i32>>("", "match_duration_secs")
                .unwrap(),
            Some(123)
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "match_file_path")
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn exact_interrupted_target_is_retryable_but_partial_or_near_match_is_refused() {
        let compatible = database_before_migration().await;
        Migration
            .up(&SchemaManager::new(&compatible))
            .await
            .expect("install exact interrupted target");
        assert!(!migration_is_applied(&compatible).await);
        Migrator::up(&compatible, Some(1))
            .await
            .expect("record exact interrupted target");
        assert!(migration_is_applied(&compatible).await);
        Migration
            .up(&SchemaManager::new(&compatible))
            .await
            .expect("exact retry is idempotent");

        let partial = database_before_migration().await;
        partial
            .execute_unprepared(
                "CREATE TABLE server_playlist_links (
                     playlist_id VARCHAR PRIMARY KEY NOT NULL
                 )",
            )
            .await
            .expect("install partial target");
        let error = Migrator::up(&partial, Some(1))
            .await
            .expect_err("partial target must fail closed");
        assert!(error.to_string().contains("column schema"));
        assert!(!migration_is_applied(&partial).await);
        assert_eq!(
            partial
                .query_one(Statement::from_string(
                    DbBackend::Sqlite,
                    "SELECT COUNT(*) AS count FROM server_playlist_links".to_string(),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<i64>("", "count")
                .unwrap(),
            0
        );

        let near_match = database_before_migration().await;
        let altered = canonical_table_sql().replace("state_revision >= 0", "state_revision > 0");
        near_match
            .execute_unprepared(&altered)
            .await
            .expect("install restrictive lookalike");
        near_match
            .execute_unprepared(&canonical_index_sql())
            .await
            .expect("install expected index");
        let error = Migrator::up(&near_match, Some(1))
            .await
            .expect_err("near-match CHECK must fail closed");
        assert!(error.to_string().contains("exact canonical"));
        assert!(!migration_is_applied(&near_match).await);

        let missing_index = database_before_migration().await;
        missing_index
            .execute_unprepared(&canonical_table_sql())
            .await
            .expect("install table without index");
        let error = Migrator::up(&missing_index, Some(1))
            .await
            .expect_err("partial index target must fail closed");
        assert!(error.to_string().contains("explicit indexes"));
        assert!(!migration_is_applied(&missing_index).await);
    }

    #[tokio::test]
    async fn object_and_index_collisions_roll_back_without_replacing_existing_objects() {
        let view_collision = database_before_migration().await;
        view_collision
            .execute_unprepared(
                "CREATE VIEW server_playlist_links AS SELECT id AS playlist_id FROM playlists",
            )
            .await
            .expect("create conflicting view");
        let error = Migrator::up(&view_collision, Some(1))
            .await
            .expect_err("view collision must fail");
        assert!(error.to_string().contains("must be a table"));
        assert_eq!(
            object_type(&SchemaManager::new(&view_collision), TABLE)
                .await
                .unwrap(),
            Some("view".to_string())
        );

        let index_collision = database_before_migration().await;
        index_collision
            .execute_unprepared("CREATE TABLE collision_owner (id VARCHAR NOT NULL)")
            .await
            .expect("create collision table");
        index_collision
            .execute_unprepared(&format!(
                "CREATE UNIQUE INDEX {SOURCE_NATIVE_INDEX} ON collision_owner (id)"
            ))
            .await
            .expect("reserve migration index name");
        Migrator::up(&index_collision, Some(1))
            .await
            .expect_err("index collision must roll back table creation");
        assert!(!target_exists(&index_collision).await);
        assert_eq!(
            object_type(&SchemaManager::new(&index_collision), SOURCE_NATIVE_INDEX)
                .await
                .unwrap(),
            Some("index".to_string())
        );
        assert!(!migration_is_applied(&index_collision).await);
    }

    #[tokio::test]
    async fn database_constraints_reject_every_noncanonical_state() {
        let db = migrated_database().await;
        insert_playlist(&db, "target").await;
        insert_playlist(&db, "").await;
        let oversized_playlist_id = "p".repeat(MAX_LOCAL_PLAYLIST_ID_BYTES + 1);
        insert_playlist(&db, &oversized_playlist_id).await;

        let mut invalid = Vec::new();

        let mut row = link_model("", "empty-playlist-id");
        invalid.push(row.clone());
        row.playlist_id = oversized_playlist_id;
        row.native_playlist_id = "oversized-playlist-id".to_string();
        invalid.push(row);

        for source_id in [
            NIL_SOURCE_ID,
            LOCAL_SOURCE_ID,
            RADIO_BROWSER_SOURCE_ID,
            "11111111-1111-4111-8111-11111111111A",
            "not-a-source-id",
        ] {
            let mut row = link_model("target", "bad-source");
            row.source_id = source_id.to_string();
            invalid.push(row);
        }

        let mut row = link_model("target", "");
        invalid.push(row.clone());
        row.native_playlist_id = "n".repeat(MAX_NATIVE_PLAYLIST_ID_BYTES + 1);
        invalid.push(row);

        let mut row = link_model("target", "bad-mode");
        row.mode = "push_read_write".to_string();
        invalid.push(row);
        let mut row = link_model("target", "bad-name");
        row.last_synced_name = "n".repeat(MAX_SYNCED_NAME_BYTES + 1);
        invalid.push(row);
        let mut row = link_model("target", "bad-version");
        row.digest_version = DIGEST_VERSION + 1;
        invalid.push(row);
        let mut row = link_model("target", "short-digest");
        row.membership_digest.pop();
        invalid.push(row);
        let mut row = link_model("target", "long-digest");
        row.membership_digest.push(0);
        invalid.push(row);
        for timestamp in [-1, MAX_SUCCESS_AT_MS + 1] {
            let mut row = link_model("target", "bad-time");
            row.last_success_at_ms = timestamp;
            invalid.push(row);
        }
        for state in ["dirty", "", "CLEAN"] {
            let mut row = link_model("target", "bad-local-state");
            row.local_state = state.to_string();
            invalid.push(row);
        }
        for state in ["offline", "", "PRESENT"] {
            let mut row = link_model("target", "bad-remote-state");
            row.remote_state = state.to_string();
            invalid.push(row);
        }
        let mut row = link_model("target", "bad-revision");
        row.state_revision = -1;
        invalid.push(row);

        for row in invalid {
            insert_link(&db, row)
                .await
                .expect_err("noncanonical link row must violate a CHECK");
        }
        assert_eq!(
            server_playlist_link::Entity::find()
                .count(&db)
                .await
                .expect("count rejected rows"),
            0
        );

        db.execute_unprepared(
            "INSERT INTO server_playlist_links (
                 playlist_id, source_id, native_playlist_id, mode, last_synced_name,
                 digest_version, membership_digest, last_success_at_ms,
                 local_state, remote_state, state_revision
             ) VALUES (
                 'target', '11111111-1111-4111-8111-111111111111', 'text-digest',
                 'pull_read_only_v1', '', 1, '01234567890123456789012345678901',
                 1, 'clean', 'present', 0
             )",
        )
        .await
        .expect_err("TEXT digest must not masquerade as canonical BLOB storage");
        db.execute_unprepared(
            "INSERT INTO server_playlist_links (
                 playlist_id, source_id, native_playlist_id, mode, last_synced_name,
                 digest_version, membership_digest, last_success_at_ms,
                 local_state, remote_state, state_revision
             ) VALUES (
                 'target', '11111111-1111-4111-8111-111111111111', 'real-time',
                 'pull_read_only_v1', '', 1, zeroblob(32),
                 1.5, 'clean', 'present', 0
             )",
        )
        .await
        .expect_err("REAL timestamp storage must fail closed");
    }

    #[tokio::test]
    async fn exact_boundaries_states_and_binary_identity_are_preserved() {
        let db = migrated_database().await;
        let cases = [
            ("clean-present", "clean", "present"),
            ("clean-missing", "clean", "missing"),
            ("conflict-present", "conflict", "present"),
            ("conflict-missing", "conflict", "missing"),
        ];
        for (playlist_id, local, remote) in cases {
            insert_playlist(&db, playlist_id).await;
            let mut row = link_model(playlist_id, playlist_id);
            row.local_state = local.to_string();
            row.remote_state = remote.to_string();
            insert_link(&db, row)
                .await
                .expect("orthogonal state combination");
        }

        insert_playlist(&db, "maximums").await;
        let mut maximums = link_model("maximums", &"n".repeat(MAX_NATIVE_PLAYLIST_ID_BYTES));
        maximums.last_synced_name = "x".repeat(MAX_SYNCED_NAME_BYTES);
        maximums.last_success_at_ms = MAX_SUCCESS_AT_MS;
        maximums.state_revision = i64::MAX;
        insert_link(&db, maximums).await.expect("exact maxima");

        insert_playlist(&db, "empty-hints").await;
        let mut empty_hints = link_model("empty-hints", " ");
        empty_hints.last_synced_name.clear();
        empty_hints.last_success_at_ms = 0;
        insert_link(&db, empty_hints)
            .await
            .expect("identity whitespace and an empty synchronized name remain exact");

        insert_playlist(&db, "case-upper").await;
        insert_playlist(&db, "case-lower").await;
        insert_link(&db, link_model("case-upper", "Native-ID"))
            .await
            .expect("uppercase native identity");
        insert_link(&db, link_model("case-lower", "native-id"))
            .await
            .expect("binary-distinct lowercase native identity");

        insert_playlist(&db, "duplicate").await;
        insert_link(&db, link_model("duplicate", "Native-ID"))
            .await
            .expect_err("one exact source/native identity may own only one mirror");

        insert_playlist(&db, "other-source").await;
        let mut other_source = link_model("other-source", "Native-ID");
        other_source.source_id = SECOND_REMOTE_SOURCE_ID.to_string();
        insert_link(&db, other_source)
            .await
            .expect("same native ID in another source namespace");
    }

    #[tokio::test]
    async fn playlist_foreign_key_is_the_only_relation_and_cascades_link_only() {
        let db = migrated_database().await;
        insert_link(&db, link_model("missing-playlist", "orphan"))
            .await
            .expect_err("link must name an existing playlist");

        insert_playlist(&db, "owned").await;
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, source_id, track_id, local_track_id,
                 match_title, match_artist, match_album, match_duration_secs, match_file_path
             ) VALUES (
                 'owned-entry', 'owned', 0, ?, 'remote-track', NULL, '', '', '', NULL, NULL
             )",
            [REMOTE_SOURCE_ID.into()],
        ))
        .await
        .expect("insert owned entry");
        insert_link(&db, link_model("owned", "native"))
            .await
            .expect("insert owned link");

        db.execute_unprepared("DELETE FROM playlists WHERE id = 'owned'")
            .await
            .expect("delete owning playlist");
        assert!(server_playlist_link::Entity::find_by_id("owned")
            .one(&db)
            .await
            .unwrap()
            .is_none());
        let entries = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM playlist_entries WHERE playlist_id = 'owned'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "count")
            .unwrap();
        assert_eq!(entries, 0);

        validate_foreign_key(&SchemaManager::new(&db))
            .await
            .expect("only playlist ownership is a foreign key");
    }

    #[tokio::test]
    async fn altered_index_uniqueness_or_collation_is_rejected() {
        let non_unique = migrated_database().await;
        non_unique
            .execute_unprepared(&format!("DROP INDEX {SOURCE_NATIVE_INDEX}"))
            .await
            .unwrap();
        non_unique
            .execute_unprepared(&format!(
                "CREATE INDEX {SOURCE_NATIVE_INDEX}
                 ON {TABLE} (source_id COLLATE BINARY, native_playlist_id COLLATE BINARY)"
            ))
            .await
            .unwrap();
        assert!(validate_schema(&SchemaManager::new(&non_unique))
            .await
            .is_err());

        let no_case = migrated_database().await;
        no_case
            .execute_unprepared(&format!("DROP INDEX {SOURCE_NATIVE_INDEX}"))
            .await
            .unwrap();
        no_case
            .execute_unprepared(&format!(
                "CREATE UNIQUE INDEX {SOURCE_NATIVE_INDEX}
                 ON {TABLE} (source_id COLLATE BINARY, native_playlist_id COLLATE NOCASE)"
            ))
            .await
            .unwrap();
        assert!(validate_schema(&SchemaManager::new(&no_case))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn unexpected_trigger_is_never_accepted_or_destroyed_by_down() {
        let db = migrated_database().await;
        db.execute_unprepared(
            "CREATE TRIGGER unexpected_server_playlist_link_trigger
             AFTER INSERT ON server_playlist_links
             BEGIN
                 SELECT 1;
             END",
        )
        .await
        .expect("install unexpected trigger");

        let error = Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect_err("retry must reject an unexpected trigger");
        assert!(error.to_string().contains("unexpected trigger"));
        let error = Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect_err("down must not destroy an unexpected trigger");
        assert!(error.to_string().contains("unexpected trigger"));
        assert!(target_exists(&db).await);
        assert_eq!(
            object_type(
                &SchemaManager::new(&db),
                "unexpected_server_playlist_link_trigger"
            )
            .await
            .unwrap(),
            Some("trigger".to_string())
        );
    }

    #[tokio::test]
    async fn empty_downgrade_is_exact_repeatable_and_preserves_regular_data() {
        let db = migrated_database().await;
        insert_playlist(&db, "ordinary").await;
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, source_id, track_id, local_track_id,
                 match_title, match_artist, match_album, match_duration_secs, match_file_path
             ) VALUES (
                 'ordinary-entry', 'ordinary', 0, ?, 'remote-track', NULL, '', '', '', NULL, NULL
             )",
            [REMOTE_SOURCE_ID.into()],
        ))
        .await
        .expect("insert ordinary entry");

        Migrator::down(&db, Some(1))
            .await
            .expect("empty link table is losslessly removable");
        assert!(!target_exists(&db).await);
        assert!(!migration_is_applied(&db).await);
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("repeated direct down is idempotent");
        assert_eq!(
            db.query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM playlist_entries WHERE id = 'ordinary-entry'"
                    .to_string(),
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
    async fn nonempty_downgrade_refuses_without_losing_link_playlist_or_entries() {
        let db = migrated_database().await;
        insert_playlist(&db, "mirror").await;
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, source_id, track_id, local_track_id,
                 match_title, match_artist, match_album, match_duration_secs, match_file_path
             ) VALUES (
                 'mirror-entry', 'mirror', 0, ?, 'remote-track', NULL, '', '', '', NULL, NULL
             )",
            [REMOTE_SOURCE_ID.into()],
        ))
        .await
        .expect("insert mirror entry");
        insert_link(&db, link_model("mirror", "native-secret"))
            .await
            .expect("insert link");

        let error = Migrator::down(&db, Some(1))
            .await
            .expect_err("linked mirror cannot downgrade losslessly");
        let diagnostics = error.to_string();
        assert!(diagnostics.contains("unlink them first"));
        assert!(!diagnostics.contains("native-secret"));
        assert!(migration_is_applied(&db).await);
        validate_schema(&SchemaManager::new(&db))
            .await
            .expect("refusal retains exact schema");
        assert!(server_playlist_link::Entity::find_by_id("mirror")
            .one(&db)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            db.query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM playlist_entries WHERE id = 'mirror-entry'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "count")
            .unwrap(),
            1
        );

        server_playlist_link::Entity::delete_by_id("mirror")
            .exec(&db)
            .await
            .expect("explicitly unlink");
        Migrator::down(&db, Some(1))
            .await
            .expect("downgrade after unlink");
        assert!(!target_exists(&db).await);
        assert_eq!(
            db.query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM playlist_entries WHERE id = 'mirror-entry'"
                    .to_string(),
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
    async fn malformed_target_is_never_dropped_by_down() {
        let db = database_before_migration().await;
        db.execute_unprepared(
            "CREATE TABLE server_playlist_links (
                 playlist_id VARCHAR PRIMARY KEY NOT NULL,
                 opaque_data VARCHAR NOT NULL
             )",
        )
        .await
        .expect("install unknown target");
        db.execute_unprepared(
            "INSERT INTO server_playlist_links (playlist_id, opaque_data)
             VALUES ('do-not-drop', 'opaque-secret')",
        )
        .await
        .expect("insert unknown data");
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect_err("down must not guess at a partial target");
        assert_eq!(
            db.query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT opaque_data FROM server_playlist_links WHERE playlist_id = 'do-not-drop'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<String>("", "opaque_data")
            .unwrap(),
            "opaque-secret"
        );
    }
}
